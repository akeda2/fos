//! The colours every chart, meter and scope draws with, as one switchable theme: `dark` (the default), `classic` (black with
//! yellow text, like the older broadcast instruments) and `light` (white, dark ink: for bright rooms and for screenshots that
//! are printed). A tape picks one at start (`theme=NAME`, `set_by_name`) and may cycle with a key (`next`), then redraws its
//! static layer. The current theme is one process-wide index: tapes are single-threaded.

use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub background: u32,
    pub grid: u32,
    pub grid_major: u32,
    pub text: u32,
    pub dim: u32,
    /// Scope graticule: axes, frame, labels.
    pub axis: u32,
    pub frame: u32,
    pub label: u32,
    /// An unlit meter bar or box.
    pub off: u32,
    pub hold: u32,
    pub green: u32,
    pub amber: u32,
    pub red: u32,
    /// LTAS curves (live, exp, ltas, peak), the filled bars under the ltas curve and the "unresolved" marker.
    pub curves: [u32; 4],
    pub bar_fill: u32,
    pub unresolved: u32,
    /// The scope phosphor at intensity 0, 140 and 255.
    pub phosphor: [u32; 3],
}

pub const DARK: Theme = Theme {
    name: "dark",
    background: 0x101418,
    grid: 0x232a31,
    grid_major: 0x3a444d,
    text: 0xc8d0d8,
    dim: 0x7d8791,
    axis: 0x2c363f,
    frame: 0x4b5964,
    label: 0x9aa5af,
    off: 0x1a2026,
    hold: 0xffffff,
    green: 0x3ccf6a,
    amber: 0xffc83d,
    red: 0xff4d4d,
    curves: [0x4aa3ff, 0xffd34a, 0x4ade80, 0xff5c5c],
    bar_fill: 0x1c4a32,
    unresolved: 0xd08000,
    phosphor: [0x004600, 0x00ab11, 0xffffff],
};

pub const CLASSIC: Theme = Theme {
    name: "classic",
    background: 0x000000,
    grid: 0x2a2400,
    grid_major: 0x5c4e00,
    text: 0xffd400,
    dim: 0xb09400,
    axis: 0x3a3200,
    frame: 0x8a7500,
    label: 0xd8b400,
    off: 0x120f00,
    hold: 0xfff0a0,
    ..DARK
};

pub const LIGHT: Theme = Theme {
    name: "light",
    background: 0xffffff,
    grid: 0xdde2e7,
    grid_major: 0xaab2ba,
    text: 0x101820,
    dim: 0x4c5660,
    axis: 0xc4cbd2,
    frame: 0x606a74,
    label: 0x303a44,
    off: 0xeceff2,
    hold: 0x000000,
    green: 0x1a9a45,
    amber: 0xc78000,
    red: 0xd02020,
    curves: [0x1060d0, 0xa87000, 0x1a8a40, 0xc02020],
    bar_fill: 0xbfe3cd,
    unresolved: 0xc07000,
    // light grey-blue when faint, dark blue when bright: ink on paper
    phosphor: [0xb4c8e0, 0x3068a8, 0x001030],
};

pub const THEMES: [Theme; 3] = [DARK, CLASSIC, LIGHT];

static CURRENT: AtomicUsize = AtomicUsize::new(0);

pub fn theme() -> &'static Theme {
    &THEMES[CURRENT.load(Ordering::Relaxed) % THEMES.len()]
}

/// Selects a theme by name; false (and no change) for an unknown one.
pub fn set_by_name(name: &str) -> bool {
    match THEMES.iter().position(|theme| theme.name == name) {
        Some(index) => {
            CURRENT.store(index, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Switches to the next theme and returns it.
pub fn next() -> &'static Theme {
    CURRENT.store((CURRENT.load(Ordering::Relaxed) + 1) % THEMES.len(), Ordering::Relaxed);
    theme()
}

/// Mixes two 0x00RRGGBB colours: `from` at 0, `to` at `of` = `max`.
pub fn mix(from: u32, to: u32, of: u32, max: u32) -> u32 {
    let channel = |shift: u32| {
        let (a, b) = (((from >> shift) & 0xff) as i64, ((to >> shift) & 0xff) as i64);
        (a + (b - a) * of as i64 / max.max(1) as i64) as u32 & 0xff
    };
    channel(16) << 16 | channel(8) << 8 | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_select_and_unknown_names_do_not() {
        assert!(set_by_name("light"));
        assert_eq!(theme().background, 0xffffff);
        assert!(!set_by_name("neon"));
        assert_eq!(theme().name, "light");
        assert!(set_by_name("dark"));
        assert_eq!(next().name, "classic");
        assert_eq!(next().name, "light");
        assert_eq!(next().name, "dark");
    }

    #[test]
    fn mixing_reaches_both_ends() {
        assert_eq!(mix(0x102030, 0xf0e0d0, 0, 10), 0x102030);
        assert_eq!(mix(0x102030, 0xf0e0d0, 10, 10), 0xf0e0d0);
        assert_eq!(mix(0x000000, 0xff00ff, 5, 10), 0x7f007f);
    }
}
