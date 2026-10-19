//! Where everything goes, derived from the screen size so any aspect ratio works (16:9 is what it is drawn for): the scope is
//! a square in the middle, the correlation and balance bars under it, the level meters to its right, and the area to its left
//! is kept free for a loudness (LUFS) panel that also reaches down beside the correlation and balance bars.

const TOP: i32 = 34;
const BOTTOM: i32 = 24;
/// Under the scope: its -M label, then the correlation bar and the balance bar with their labels.
const UNDER: i32 = 22 + 16 + 26 + 10 + 26;
/// The least room each side column keeps.
const SIDE_MIN: i32 = 260;

#[derive(Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub scope: Rect,
    pub correlation: Rect,
    pub balance: Rect,
    /// Where the bars are (the scale's labels sit on both sides of it); same top and height as the scope.
    pub bars: Rect,
    pub bar_w: u32,
    /// The loudness panel.
    pub left_panel: Rect,
}

impl Layout {
    pub fn new(width: u32, height: u32) -> Self {
        let (w, h) = (width as i32, height as i32);
        let by_height = h - TOP - 22 - UNDER - BOTTOM;
        let by_width = w - 2 * SIDE_MIN;
        let size = by_height.min(by_width).max(64) & !1;
        let sx = (w - size) / 2;
        let sy = TOP + 22;
        let scope = Rect { x: sx, y: sy, w: size as u32, h: size as u32 };
        let under = sy + size + 22;
        let correlation = Rect { x: sx, y: under, w: size as u32, h: 16 };
        let balance = Rect { x: sx, y: under + 16 + 26, w: size as u32, h: 10 };
        let right = sx + size + 28;
        let label = 56;
        let free = w - right - label - 6 - 60 - 34 - 16;
        let bar_w = ((free - 10) / 2).clamp(14, 40);
        let bars = Rect { x: right + label, y: sy, w: (2 * bar_w + 10) as u32, h: size as u32 };
        let left_panel = Rect { x: 16, y: sy, w: (sx - 28 - 16).max(0) as u32, h: (size + UNDER) as u32 };
        Self { width, height, scope, correlation, balance, bars, bar_w: bar_w as u32, left_panel }
    }

    /// The x of the left bar and of the right bar.
    pub fn bar_x(&self, channel: usize) -> i32 {
        self.bars.x + channel as i32 * (self.bar_w as i32 + 10)
    }
}
