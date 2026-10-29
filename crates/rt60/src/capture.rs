//! Waiting for a pop: a trigger with a pre-roll. While armed it keeps the last `pre` samples in a ring; the first sample at or
//! above the threshold starts the recording, which then contains that history (the sound just before the trigger, so the build-up
//! of the pop is not cut) and carries on until the buffer is full.

/// Capture state over a caller-owned buffer of `pre + length` samples.
pub struct Capture<'a> {
    buf: &'a mut [f32],
    pre: usize,
    head: usize,
    pos: usize,
    threshold: f32,
    recording: bool,
    /// The largest magnitude seen since `arm`.
    pub peak: f32,
}

impl<'a> Capture<'a> {
    pub fn new(buf: &'a mut [f32], pre: usize) -> Self {
        let pre = pre.min(buf.len() / 2);
        Self { buf, pre, head: 0, pos: 0, threshold: 0.1, recording: false, peak: 0.0 }
    }

    /// Starts over, waiting for a sample at or above `threshold` (0..1).
    pub fn arm(&mut self, threshold: f32) {
        self.threshold = threshold;
        self.recording = false;
        self.head = 0;
        self.pos = 0;
        self.peak = 0.0;
        self.buf[..self.pre].fill(0.0);
    }

    pub fn triggered(&self) -> bool {
        self.recording
    }

    pub fn done(&self) -> bool {
        self.recording && self.pos >= self.buf.len()
    }

    /// Fraction (0..1) of the recording that is in.
    pub fn progress(&self) -> f32 {
        if self.recording { self.pos as f32 / self.buf.len() as f32 } else { 0.0 }
    }

    /// Feeds samples; true once the buffer is full.
    pub fn feed(&mut self, samples: &[f32]) -> bool {
        for &x in samples {
            let m = x.abs();
            if m > self.peak {
                self.peak = m;
            }
            if self.recording {
                if self.pos < self.buf.len() {
                    self.buf[self.pos] = x;
                    self.pos += 1;
                }
            } else if self.pre > 0 {
                self.buf[self.head] = x;
                self.head = (self.head + 1) % self.pre;
                if m >= self.threshold {
                    // The ring ends with the trigger sample: put it in time order.
                    self.buf[..self.pre].rotate_left(self.head);
                    self.pos = self.pre;
                    self.recording = true;
                }
            } else if m >= self.threshold {
                self.buf[0] = x;
                self.pos = 1;
                self.recording = true;
            }
        }
        self.done()
    }

    /// The recording (the pre-roll first, the trigger sample at index `pre - 1`) once done.
    pub fn recorded(&self) -> &[f32] {
        &self.buf[..self.pos]
    }

    pub fn pre_roll(&self) -> usize {
        self.pre
    }

    /// True when any sample reached full scale (the response is clipped and the decay unreliable).
    pub fn clipped(&self) -> bool {
        self.peak >= 0.99
    }
}
