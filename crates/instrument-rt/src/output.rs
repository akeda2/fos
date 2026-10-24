//! The playback side of an instrument tape.

use audio_sample::Format;
use gfx::Canvas;
use tape_rt::{AudioInfo, Line, TapeApi};

/// Frames rendered per `pump` call at most.
pub const CHUNK: usize = 256;
/// Room for a 32-channel device with 4-byte slots.
pub const MAX_FRAME_BYTES: usize = 128;
pub const SCOPE_LEN: usize = 1024;

/// Writes stereo `frames` into `out` as device frames of `channels` samples in `format`: a mono device gets the left and
/// right mixed, a multichannel one gets the pair starting at channel `first` (0-based) and silence elsewhere.
pub fn pack(frames: &[[f32; 2]], channels: usize, first: usize, format: Format, out: &mut [u8]) {
    let slot = format.slot;
    let frame_bytes = channels * slot;
    for (n, frame) in frames.iter().enumerate() {
        for channel in 0..channels {
            let value = if channels == 1 {
                0.5 * (frame[0] + frame[1])
            } else if channel == first {
                frame[0]
            } else if channel == first + 1 {
                frame[1]
            } else {
                0.0
            };
            let at = n * frame_bytes + channel * slot;
            format.write(value, &mut out[at..at + slot]);
        }
    }
}

pub struct Output {
    api: &'static TapeApi,
    pub rate: u32,
    pub channels: usize,
    /// The left channel of the pair (0-based).
    pub first: usize,
    pub format: Format,
    pub latency_ms: u32,
    /// A slowly falling peak of what was played, 0..=1.
    pub peak: f32,
    capacity: u32,
    target: usize,
    scope: [f32; SCOPE_LEN],
    scope_at: usize,
    frames: [[f32; 2]; CHUNK],
    packed: [u8; CHUNK * MAX_FRAME_BYTES],
}

impl Output {
    /// Starts the playback device. Takes `rate=HZ` (default 48000 if offered, else the first rate offered), `latency=MS`
    /// (10-200, default 40) and `out=N` (the left channel of the output pair, 1-based, default 1) from `arguments`.
    pub fn start(api: &'static TapeApi, arguments: &str) -> Result<Output, Line<128>> {
        let mut info = AudioInfo::default();
        let mut error = Line::<128>::new();
        if (api.audio_info)(&mut info) == 0 || info.playback_channels == 0 {
            let _ = core::fmt::Write::write_str(&mut error, "no audio output (USB audio or HD Audio with a playback side)");
            return Err(error);
        }
        let wanted = tape_rt::arg_value(arguments, "rate").and_then(|value| value.parse::<u32>().ok());
        let offered = &info.rates[..(info.rate_count as usize).min(8)];
        let rate = wanted.or_else(|| offered.contains(&48000).then_some(48000)).or_else(|| offered.first().copied()).unwrap_or(48000);
        let latency_ms = tape_rt::arg_value(arguments, "latency").and_then(|value| value.parse::<u32>().ok()).unwrap_or(40).clamp(10, 200);
        if (api.audio_start)(rate, 0, 1) != 0 {
            let _ = core::fmt::Write::write_fmt(&mut error, format_args!("the audio device does not start at {rate} Hz"));
            return Err(error);
        }
        (api.audio_info)(&mut info);
        let format = Format::new(info.playback_slot_bytes as usize, info.playback_bits as usize, tape_rt::audio_float_of(api).1);
        let channels = info.playback_channels as usize;
        if !format.valid() || channels == 0 || channels * format.slot > MAX_FRAME_BYTES {
            (api.audio_stop)();
            let _ = core::fmt::Write::write_fmt(&mut error, format_args!("cannot write this device's sample format ({} byte slots, {} channels)", format.slot, channels));
            return Err(error);
        }
        let first = tape_rt::arg_value(arguments, "out").and_then(|value| value.parse::<usize>().ok()).map_or(0, |n| n.max(1) - 1);
        if channels > 1 && first + 2 > channels {
            (api.audio_stop)();
            let _ = core::fmt::Write::write_fmt(&mut error, format_args!("out={} needs outputs {} and {}, the device has {}", first + 1, first + 1, first + 2, channels));
            return Err(error);
        }
        Ok(Output {
            api,
            rate,
            channels,
            first,
            format,
            latency_ms,
            peak: 0.0,
            capacity: info.playback_space,
            target: (rate * latency_ms / 1000) as usize,
            scope: [0.0; SCOPE_LEN],
            scope_at: 0,
            frames: [[0.0; 2]; CHUNK],
            packed: [0; CHUNK * MAX_FRAME_BYTES],
        })
    }

    /// Tops the device's queue up to the latency with frames from `render` (which fills the slice with stereo frames, each
    /// within -1..=1). True when it wrote something; call it often, and between the bands of a slow picture.
    pub fn pump(&mut self, mut render: impl FnMut(&mut [[f32; 2]])) -> bool {
        let mut info = AudioInfo::default();
        (self.api.audio_info)(&mut info);
        self.capacity = self.capacity.max(info.playback_space);
        let frame_bytes = self.channels * self.format.slot;
        let queued = (self.capacity - info.playback_space) as usize / frame_bytes;
        if queued >= self.target {
            return false;
        }
        let frames = (self.target - queued).min(CHUNK);
        let rendered = &mut self.frames[..frames];
        render(rendered);
        let mut peak = self.peak * 0.98;
        for frame in rendered.iter() {
            peak = peak.max(frame[0].abs()).max(frame[1].abs());
            self.scope[self.scope_at] = frame[0];
            self.scope_at = (self.scope_at + 1) % SCOPE_LEN;
        }
        self.peak = peak;
        pack(rendered, self.channels, self.first, self.format, &mut self.packed);
        (self.api.audio_write)(self.packed.as_ptr(), frames * frame_bytes);
        true
    }

    /// Fills the queue before the first picture: the first draw is the slowest.
    pub fn prime(&mut self, mut render: impl FnMut(&mut [[f32; 2]])) {
        while self.pump(&mut render) {}
    }

    pub fn stop(&self) {
        (self.api.audio_stop)();
    }

    /// The oldest scope sample first, starting at a rising zero crossing within the first half so a steady note stands still.
    fn scope_trigger(&self) -> usize {
        let at = |n: usize| self.scope[(self.scope_at + n) % SCOPE_LEN];
        (0..SCOPE_LEN / 2).find(|&n| at(n) <= 0.0 && at(n + 1) > 0.0).unwrap_or(0)
    }

    /// Draws the last samples of the left channel as a line in the box (`x`, `y`, `w`, `h`).
    pub fn draw_scope(&self, canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, background: u32, line: u32) {
        canvas.fill_rect(x, y, w as u32, h as u32, background);
        let at = |n: usize| self.scope[(self.scope_at + n) % SCOPE_LEN];
        let start = self.scope_trigger();
        let points = (SCOPE_LEN / 2).min(w.max(1) as usize);
        let mid = y + h / 2;
        let point = |n: usize| (x + (n as i32 * w) / points as i32, mid - (at(start + n * (SCOPE_LEN / 2) / points) * (h as f32 * 0.45)) as i32);
        let mut last = point(0);
        for n in 1..points {
            let next = point(n);
            canvas.line(last.0, last.1, next.0, next.1, line);
            last = next;
        }
    }
}
