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

/// Writes `width` channels per frame of `frames` into `out` as device frames of `channels` samples, starting at channel
/// `first` (0-based); silence elsewhere. A mono device gets the first two mixed.
pub fn pack_wide(frames: &[[f32; 6]], width: usize, channels: usize, first: usize, format: Format, out: &mut [u8]) {
    let slot = format.slot;
    let frame_bytes = channels * slot;
    for (n, frame) in frames.iter().enumerate() {
        for channel in 0..channels {
            let value = if channels == 1 {
                0.5 * (frame[0] + frame[1])
            } else if channel >= first && channel < first + width.min(6) {
                frame[channel - first]
            } else {
                0.0
            };
            let at = n * frame_bytes + channel * slot;
            format.write(value, &mut out[at..at + slot]);
        }
    }
}

/// The capture side of a duplex `Output`: the input pair and the device's sample layout.
struct Capture {
    channels: usize,
    first: usize,
    format: Format,
    bytes: [u8; CHUNK * MAX_FRAME_BYTES],
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
    /// The same for what was captured (a duplex `Output` only).
    pub input_peak: f32,
    capture: Option<Capture>,
    capacity: u32,
    target: usize,
    /// Extra captured frames tolerated before the oldest are dropped (0 = live use; raised while archiving).
    headroom: usize,
    scope: [f32; SCOPE_LEN],
    scope_at: usize,
    frames: [[f32; 2]; CHUNK],
    wide: [[f32; 6]; CHUNK],
    packed: [u8; CHUNK * MAX_FRAME_BYTES],
}

impl Output {
    /// Starts the playback device. Takes `rate=HZ` (default 48000 if offered, else the first rate offered), `latency=MS`
    /// (10-200, default 40) and `out=N` (the left channel of the output pair, 1-based, default 1) from `arguments`.
    pub fn start(api: &'static TapeApi, arguments: &str) -> Result<Output, Line<128>> {
        Self::open(api, arguments, false, 40)
    }

    /// Like `start`, with another default latency (still overridden by `latency=`): for instruments played live.
    pub fn start_with_latency(api: &'static TapeApi, arguments: &str, default_ms: u32) -> Result<Output, Line<128>> {
        Self::open(api, arguments, false, default_ms)
    }

    /// Like `start`, and the device also captures: `in=N` is the left channel of the input pair (1-based, default 1; a
    /// mono input feeds both sides). Use `pump_duplex` to run the signal through.
    pub fn start_duplex(api: &'static TapeApi, arguments: &str) -> Result<Output, Line<128>> {
        Self::open(api, arguments, true, 40)
    }

    fn open(api: &'static TapeApi, arguments: &str, duplex: bool, default_ms: u32) -> Result<Output, Line<128>> {
        let mut info = AudioInfo::default();
        let mut error = Line::<128>::new();
        if (api.audio_info)(&mut info) == 0 || info.playback_channels == 0 {
            let _ = core::fmt::Write::write_str(&mut error, "no audio output (USB audio or HD Audio with a playback side)");
            return Err(error);
        }
        if duplex && info.capture_channels == 0 {
            let _ = core::fmt::Write::write_str(&mut error, "no audio input (the device has no capture side)");
            return Err(error);
        }
        let wanted = tape_rt::arg_value(arguments, "rate").and_then(|value| value.parse::<u32>().ok());
        let offered = &info.rates[..(info.rate_count as usize).min(8)];
        let rate = wanted.or_else(|| offered.contains(&48000).then_some(48000)).or_else(|| offered.first().copied()).unwrap_or(48000);
        let latency_ms = tape_rt::arg_value(arguments, "latency").and_then(|value| value.parse::<u32>().ok()).unwrap_or(default_ms).clamp(10, 200);
        if (api.audio_start)(rate, u32::from(duplex), 1) != 0 {
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
        let capture = if duplex {
            let format = Format::new(info.capture_slot_bytes as usize, info.capture_bits as usize, tape_rt::audio_float_of(api).0);
            let channels = info.capture_channels as usize;
            let first = tape_rt::arg_value(arguments, "in").and_then(|value| value.parse::<usize>().ok()).map_or(0, |n| n.max(1) - 1);
            let problem = if !format.valid() || channels * format.slot > MAX_FRAME_BYTES {
                Some(format_args!("cannot read this device's input format ({} byte slots, {} channels)", format.slot, channels))
            } else if channels > 1 && first + 2 > channels {
                Some(format_args!("in={} needs inputs {} and {}, the device has {}", first + 1, first + 1, first + 2, channels))
            } else {
                None
            };
            if let Some(problem) = problem {
                (api.audio_stop)();
                let _ = core::fmt::Write::write_fmt(&mut error, problem);
                return Err(error);
            }
            Some(Capture { channels, first, format, bytes: [0; CHUNK * MAX_FRAME_BYTES] })
        } else {
            None
        };
        Ok(Output {
            api,
            rate,
            channels,
            first,
            format,
            latency_ms,
            peak: 0.0,
            input_peak: 0.0,
            capture,
            capacity: info.playback_space,
            target: (rate * latency_ms / 1000) as usize,
            headroom: 0,
            scope: [0.0; SCOPE_LEN],
            scope_at: 0,
            frames: [[0.0; 2]; CHUNK],
            wide: [[0.0; 6]; CHUNK],
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
        render(&mut self.frames[..frames]);
        self.finish_rendered(frames);
        true
    }

    fn finish(&mut self, frames: usize, process: &mut impl FnMut(&mut [[f32; 2]])) {
        process(&mut self.frames[..frames]);
        self.finish_rendered(frames);
    }

    /// Meters, scopes and queues the first `frames` of `self.frames`.
    fn finish_rendered(&mut self, frames: usize) {
        let frame_bytes = self.channels * self.format.slot;
        let rendered = &mut self.frames[..frames];
        let mut peak = self.peak * 0.98;
        for frame in rendered.iter() {
            peak = peak.max(frame[0].abs()).max(frame[1].abs());
            self.scope[self.scope_at] = frame[0];
            self.scope_at = (self.scope_at + 1) % SCOPE_LEN;
        }
        self.peak = peak;
        pack(rendered, self.channels, self.first, self.format, &mut self.packed);
        (self.api.audio_write)(self.packed.as_ptr(), frames * frame_bytes);
    }

    /// One duplex step: takes the captured frames that are waiting (as many as the playback queue has room for), hands them to
    /// `process` as stereo frames to be changed in place, and queues the result. Every captured frame becomes exactly one
    /// played frame, so the input-to-output delay is the queue the tape primed with (`prime`) plus the device's own buffers,
    /// and it stays put because the one device clocks both sides. If the loop stalled and a backlog grew past the latency,
    /// the oldest frames are dropped (a glitch) instead of keeping the delay for good. True when it wrote something.
    pub fn pump_duplex(&mut self, mut process: impl FnMut(&mut [[f32; 2]])) -> bool {
        let Some(got) = self.take_capture() else { return false };
        self.finish(got, &mut process);
        true
    }

    /// Like `pump_duplex`, but `process` gets the captured stereo frames (to change in place, as there) and fills frames of up to six channels, of which
    /// the first `width` are played on the device's channels from `first` on (the caller checks the device has them).
    /// The meter and the scope follow those channels. True when it wrote something.
    pub fn pump_duplex_wide(&mut self, width: usize, mut process: impl FnMut(&mut [[f32; 2]], &mut [[f32; 6]])) -> bool {
        let Some(got) = self.take_capture() else { return false };
        let width = width.clamp(1, 6);
        process(&mut self.frames[..got], &mut self.wide[..got]);
        let mut peak = self.peak * 0.98;
        for frame in &self.wide[..got] {
            for &value in &frame[..width] {
                peak = peak.max(value.abs());
            }
            self.scope[self.scope_at] = frame[0];
            self.scope_at = (self.scope_at + 1) % SCOPE_LEN;
        }
        self.peak = peak;
        pack_wide(&self.wide[..got], width, self.channels, self.first, self.format, &mut self.packed);
        (self.api.audio_write)(self.packed.as_ptr(), got * self.channels * self.format.slot);
        true
    }

    /// Reads the waiting capture into `self.frames` (the part `pump_duplex` and `pump_duplex_wide` share); the number of
    /// frames, or `None` when nothing was read.
    fn take_capture(&mut self) -> Option<usize> {
        let capture = self.capture.as_mut()?;
        let mut info = AudioInfo::default();
        (self.api.audio_info)(&mut info);
        self.capacity = self.capacity.max(info.playback_space);
        let in_bytes = capture.channels * capture.format.slot;
        let out_bytes = self.channels * self.format.slot;
        let room = info.playback_space as usize / out_bytes;
        let mut waiting = info.capture_available as usize / in_bytes;
        while waiting > self.target + CHUNK + self.headroom {
            let drop = (waiting - self.target).min(CHUNK);
            (self.api.audio_read)(capture.bytes.as_mut_ptr(), drop * in_bytes);
            waiting -= drop;
        }
        let frames = waiting.min(room).min(CHUNK);
        if frames == 0 {
            return None;
        }
        let got = (self.api.audio_read)(capture.bytes.as_mut_ptr(), frames * in_bytes) / in_bytes;
        if got == 0 {
            return None;
        }
        let slot = capture.format.slot;
        let mut input_peak = self.input_peak * 0.98;
        for (n, frame) in self.frames[..got].iter_mut().enumerate() {
            let at = n * in_bytes;
            let left = capture.format.read(&capture.bytes[at + capture.first * slot..]);
            let right = if capture.channels == 1 { left } else { capture.format.read(&capture.bytes[at + (capture.first + 1) * slot..]) };
            input_peak = input_peak.max(left.abs()).max(right.abs());
            *frame = [left, right];
        }
        self.input_peak = input_peak;
        Some(got)
    }

    /// Lets up to `frames` more captured frames wait before the oldest are dropped. A live instrument wants none (a
    /// backlog is latency); an archive must not lose samples when the disk stalls for a moment.
    pub fn capture_headroom(&mut self, frames: usize) {
        self.headroom = frames;
    }

    /// Significant bits of the captured samples as integers (at most 24; a float device gives 24), 0 without capture.
    pub fn capture_bits(&self) -> u32 {
        self.capture.as_ref().map_or(0, |c| if c.format.float { 24 } else { c.format.bits.min(24) as u32 })
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
