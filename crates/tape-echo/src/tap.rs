//! Tap tempo: the average gap between the last few taps, forgetting a tap sequence that has gone cold.

/// A gap longer than this starts a new sequence, microseconds.
const COLD_US: u64 = 2_000_000;
/// Gaps shorter than this are bounces, not taps.
const MIN_US: u64 = 80_000;
const KEEP: usize = 4;

pub struct TapTempo {
    last: Option<u64>,
    gaps: [u64; KEEP],
    count: usize,
}

impl TapTempo {
    pub const fn new() -> Self {
        TapTempo { last: None, gaps: [0; KEEP], count: 0 }
    }

    /// A tap at `now_us` (any steady clock). From the second tap of a sequence on, returns the average gap in seconds.
    pub fn tap(&mut self, now_us: u64) -> Option<f32> {
        let Some(last) = self.last.filter(|&last| now_us >= last && now_us - last <= COLD_US) else {
            self.last = Some(now_us);
            self.count = 0;
            return None;
        };
        let gap = now_us - last;
        if gap < MIN_US {
            return None;
        }
        self.last = Some(now_us);
        self.gaps[self.count % KEEP] = gap;
        self.count += 1;
        let used = self.count.min(KEEP);
        let total: u64 = self.gaps[..used].iter().sum();
        Some(total as f32 / used as f32 / 1_000_000.0)
    }

    pub fn reset(&mut self) {
        *self = TapTempo::new();
    }
}

impl Default for TapTempo {
    fn default() -> Self {
        Self::new()
    }
}
