//! The scaffold of a tape that listens on an audio input, shows what it hears on the pixel screen, and may play a test signal
//! (`ltas`, and the tapes that follow it): `startup` handles the common arguments and checks, `Input` is the capture side with
//! the choice of channel, `Playback` pumps a generator to the output, `Screen` owns the grid and frame buffers and the 20
//! frames-a-second rhythm, and `poll_keys` reads the keyboard. What is measured and drawn stays in the tape.
#![cfg_attr(not(test), no_std)]

use dsp::pcm::unpack_channel;
use gfx::Canvas;
use tape_rt::{AudioInfo, Key, KeyDecoder, TapeApi, alloc_pixels, alloc_zeroed};

/// What `startup` found out.
pub struct Startup<'a> {
    /// The arguments with `device=` taken out (and applied) and `outdev=` applied.
    pub arguments: &'a str,
    pub width: u32,
    pub height: u32,
}

/// The shared beginning of `tape_main`: `device=N` and `outdev=M`, the hard-float check and the pixel screen size (at most
/// `max_width` x `max_height`). The error text is ready to print after the tape's name.
pub fn startup(api: &'static TapeApi, max_width: u32, max_height: u32) -> Result<Startup<'static>, &'static str> {
    tape_rt::init(api);
    let arguments = tape_rt::take_audio_device_arg(api, core::str::from_utf8(tape_rt::args()).unwrap_or("")).map_err(|reason| reason)?;
    if let Some(number) = tape_rt::arg_value(arguments, "outdev") {
        let number = number.parse::<u32>().map_err(|_| "outdev=N needs a number (`audio` lists the devices)")?;
        tape_rt::use_audio_output(api, number)?;
    }
    if tape_rt::fp_level() == 0 {
        return Err("needs a hard-float tape build (kernel without FP support?)");
    }
    let Some((width, height)) = tape_rt::gfx_info() else {
        return Err("no pixel screen (BIOS text mode). Use a UEFI boot or a VBE mode");
    };
    Ok(Startup { arguments, width: width.min(max_width), height: height.min(max_height) })
}

/// The capture side of the active audio device, and which channel (or, for a stereo tool, which first channel) is chosen.
pub struct Input {
    pub channels: usize,
    pub slot: usize,
    pub bits: usize,
    /// 0-based.
    pub channel: usize,
    pub playback_channels: usize,
    pub playback_slot: usize,
    pub playback_bits: usize,
    pub rates: [u32; 16],
    pub rate_count: usize,
}

impl Input {
    /// Asks the device; `channel` is the 1-based `chan=` argument value (0 is taken as 1).
    pub fn query(api: &TapeApi, chan: usize) -> Result<Self, &'static str> {
        let mut info = AudioInfo::default();
        if (api.audio_info)(&mut info) == 0 || info.capture_channels == 0 {
            return Err("no audio input (USB audio or HD Audio with a capture side)");
        }
        let channel = chan.max(1) - 1;
        if channel >= info.capture_channels as usize {
            return Err("chan= is out of range for this device's input channels");
        }
        let mut rates = [0u32; 16];
        let rate_count = (info.rate_count as usize).min(16);
        rates[..rate_count].copy_from_slice(&info.rates[..rate_count]);
        Ok(Self {
            channels: info.capture_channels as usize,
            slot: info.capture_slot_bytes as usize,
            bits: info.capture_bits as usize,
            channel,
            playback_channels: info.playback_channels as usize,
            playback_slot: info.playback_slot_bytes as usize,
            playback_bits: info.playback_bits as usize,
            rates,
            rate_count,
        })
    }

    pub fn playback_ok(&self) -> bool {
        self.playback_channels > 0 && self.playback_slot > 0
    }

    pub fn frame_bytes(&self) -> usize {
        self.channels * self.slot
    }

    /// Starts audio at `rate` Hz (capture, and playback when the device has an output); the tape's non-zero status on failure.
    pub fn start(&self, api: &TapeApi, rate: u32) -> Result<(), u32> {
        match (api.audio_start)(rate, 1, self.playback_ok() as u32) {
            0 => Ok(()),
            status => Err(status as u32),
        }
    }

    /// The `,` (previous) and `.` (next) keys: steps the channel with wrap-around. True when it changed.
    pub fn step_channel(&mut self, key: Key) -> bool {
        if self.channels < 2 {
            return false;
        }
        let last = self.channels - 1;
        match key {
            Key::Char(b'.') => self.channel = if self.channel >= last { 0 } else { self.channel + 1 },
            Key::Char(b',') => self.channel = if self.channel == 0 { last } else { self.channel - 1 },
            _ => return false,
        }
        true
    }

    /// Reads what the device has captured (at most `scratch.len()` bytes, whole frames) and puts `channel` (0-based) of it
    /// in `out` as -1..1. Returns the samples written, 0 when nothing new.
    pub fn read(&self, api: &TapeApi, scratch: &mut [u8], out: &mut [f32], channel: usize) -> usize {
        let frame = self.frame_bytes();
        if frame == 0 {
            return 0;
        }
        let got = (api.audio_read)(scratch.as_mut_ptr(), scratch.len() / frame * frame);
        if got < frame {
            return 0;
        }
        unpack_channel(&scratch[..got], out, self.channels, self.slot, self.bits, channel)
    }

    /// Like `read` for two channels at once (a stereo pair), each into its own buffer; the shorter count of the two.
    pub fn read_pair(&self, api: &TapeApi, scratch: &mut [u8], left: &mut [f32], right: &mut [f32], channels: (usize, usize)) -> usize {
        let frame = self.frame_bytes();
        if frame == 0 {
            return 0;
        }
        let got = (api.audio_read)(scratch.as_mut_ptr(), scratch.len() / frame * frame);
        if got < frame {
            return 0;
        }
        let a = unpack_channel(&scratch[..got], left, self.channels, self.slot, self.bits, channels.0);
        let b = unpack_channel(&scratch[..got], right, self.channels, self.slot, self.bits, channels.1);
        a.min(b)
    }
}

/// A generator (or anything that makes samples) pumped to the device's output.
pub struct Playback {
    mono: &'static mut [f32; 4096],
    packed: &'static mut [u8; 4096 * 4 * 24],
    /// Output channels the signal goes to (bit 0 = channel 1).
    pub mask: u32,
}

impl Playback {
    /// None without an output side or when the kernel has no memory. A zero `mask` means the first two channels.
    pub fn new(api: &TapeApi, input: &Input, mask: u32) -> Option<Self> {
        if !input.playback_ok() {
            return None;
        }
        Some(Self { mono: alloc_zeroed(api)?, packed: alloc_zeroed(api)?, mask: if mask == 0 { 0b11 } else { mask } })
    }

    /// Fills as many frames as the device has room for with `fill` and writes them. True when it wrote something.
    pub fn pump(&mut self, api: &TapeApi, input: &Input, fill: impl FnOnce(&mut [f32])) -> bool {
        let frame = (input.playback_channels * input.playback_slot).max(1);
        let mut now = AudioInfo::default();
        (api.audio_info)(&mut now);
        let room = now.playback_space as usize / frame;
        let frames = room.min(self.mono.len()).min(self.packed.len() / frame);
        if frames == 0 {
            return false;
        }
        fill(&mut self.mono[..frames]);
        let bytes = noise::pack(&self.mono[..frames], &mut self.packed[..], input.playback_channels, input.playback_slot, input.playback_bits, self.mask);
        (api.audio_write)(self.packed.as_ptr(), bytes);
        true
    }
}

/// The screen: a `grid` buffer for what changes rarely (background, axes), a `frame` buffer the per-frame drawing goes in, and
/// one `gfx_blit` of the frame at most every 50 ms (a framebuffer write is slow).
pub struct Screen {
    pub width: u32,
    pub height: u32,
    grid: &'static mut [u32],
    frame: &'static mut [u32],
    dirty: bool,
    next_draw: u64,
}

impl Screen {
    pub fn new(api: &TapeApi, width: u32, height: u32) -> Option<Self> {
        let pixels = (width * height) as usize;
        Some(Self { width, height, grid: alloc_pixels(api, pixels)?, frame: alloc_pixels(api, pixels)?, dirty: true, next_draw: 0 })
    }

    /// Takes the screen from the console. False when the kernel will not give it.
    pub fn open(&self) -> bool {
        tape_rt::gfx_open()
    }

    /// Gives it back (the console is repainted).
    pub fn close(&self) {
        tape_rt::gfx_close();
    }

    /// The canvas of the static part, to be redrawn after the layout or scale changes.
    pub fn grid(&mut self) -> Canvas<'_> {
        self.dirty = true;
        Canvas::new(self.grid, self.width, self.height).expect("buffer sized by new")
    }

    /// Asks for a redraw at the next `due` frame.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// True when something changed and 50 ms have passed; the caller then draws with `frame` and calls `present`.
    pub fn due(&mut self) -> bool {
        let now = tape_rt::micros();
        if self.dirty && now >= self.next_draw {
            self.next_draw = now + 50_000;
            self.dirty = false;
            return true;
        }
        false
    }

    /// A canvas holding a copy of the grid, ready for the moving parts.
    pub fn frame(&mut self) -> Canvas<'_> {
        let pixels = (self.width * self.height) as usize;
        self.frame[..pixels].copy_from_slice(&self.grid[..pixels]);
        Canvas::new(self.frame, self.width, self.height).expect("buffer sized by new")
    }

    /// Sends the frame to the screen.
    pub fn present(&self) {
        let pixels = (self.width * self.height) as usize;
        tape_rt::gfx_blit(0, 0, self.width, self.height, &self.frame[..pixels], self.width);
    }

    /// The last drawn frame (for a PNG).
    pub fn pixels(&self) -> &[u32] {
        &self.frame[..(self.width * self.height) as usize]
    }
}

/// Feeds the pending keys to `decoder` and calls `on_key` for each decoded one. False when the user asked to leave (the
/// kernel's quit event), true otherwise.
pub fn poll_keys(api: &TapeApi, decoder: &mut KeyDecoder, mut on_key: impl FnMut(Key)) -> bool {
    loop {
        let event = (api.read_key)();
        if event == 0 {
            return true;
        }
        if event == 1 {
            return false;
        }
        if let Some(key) = decoder.feed((event & 0xff) as u8) {
            on_key(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(channels: usize) -> Input {
        Input { channels, slot: 4, bits: 24, channel: 0, playback_channels: 0, playback_slot: 0, playback_bits: 0, rates: [0; 16], rate_count: 0 }
    }

    #[test]
    fn channel_keys_wrap() {
        let mut one = input(1);
        assert!(!one.step_channel(Key::Char(b'.')));
        let mut many = input(10);
        assert!(many.step_channel(Key::Char(b',')));
        assert_eq!(many.channel, 9);
        assert!(many.step_channel(Key::Char(b'.')));
        assert_eq!(many.channel, 0);
        assert!(!many.step_channel(Key::Char(b'x')));
    }
}
