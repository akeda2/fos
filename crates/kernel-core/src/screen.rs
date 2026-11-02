//! What the pointer code needs to know about the screen and the Pi's DSI touch chip. The kernel owns the framebuffer
//! and the I2C bus, so it registers these hooks (`set`); without them there is no pixel screen and no DSI panel.

/// Which way the monitor is turned (a portrait mount): tapes draw on an upright logical screen and the kernel rotates it
/// onto the framebuffer. The text console is not rotated.
#[derive(Clone, Copy, PartialEq)]
pub enum Rotate {
    /// Clockwise: the logical top-left corner lands at the top right of the framebuffer.
    Right,
    /// Anti-clockwise: the logical top-left corner lands at the bottom left.
    Left,
    /// Upside down (180 degrees): the logical top-left corner lands at the bottom right; width and height stay.
    Flip,
}

impl Rotate {
    /// `r`/`right` or `l`/`left`.
    pub fn parse(word: &str) -> Option<Rotate> {
        match word {
            "r" | "right" => Some(Rotate::Right),
            "l" | "left" => Some(Rotate::Left),
            "f" | "flip" | "180" => Some(Rotate::Flip),
            _ => None,
        }
    }

    /// A quarter turn swaps the logical screen's width and height.
    pub fn swaps(self) -> bool {
        self != Rotate::Flip
    }

    /// The framebuffer pixel (of a `screen_width` x `screen_height` one) under logical pixel (x, y).
    pub fn map(self, screen_width: usize, screen_height: usize, x: usize, y: usize) -> (usize, usize) {
        match self {
            Rotate::Right => (screen_width - 1 - y, x),
            Rotate::Left => (y, screen_height - 1 - x),
            Rotate::Flip => (screen_width - 1 - x, screen_height - 1 - y),
        }
    }

    /// The logical screen's size on a `screen_width` x `screen_height` framebuffer.
    pub fn logical(self, screen_width: usize, screen_height: usize) -> (usize, usize) {
        if self.swaps() { (screen_height, screen_width) } else { (screen_width, screen_height) }
    }
}

/// The DSI display's touch chip (FT5406 on the Pi): no interrupts, so whoever wants events polls it.
pub struct Dsi {
    pub width: i32,
    pub height: i32,
    pub present: fn() -> bool,
    /// Reads the chip if it is time and queues the events as panel `panel`.
    pub poll: fn(panel: u8),
    pub reports: fn() -> u64,
}

/// The kernel's screen: the logical pixel size (`None` on a text screen) and the rotation of physical screen 0 or 1.
pub struct Screen {
    pub gfx_info: fn() -> Option<(u32, u32)>,
    pub rotation: fn(usize) -> Option<Rotate>,
    pub dsi: Option<&'static Dsi>,
}

static mut SCREEN: Option<&'static Screen> = None;

/// Registers the screen once at boot (before any pointer event can arrive).
pub fn set(screen: &'static Screen) {
    unsafe { *core::ptr::addr_of_mut!(SCREEN) = Some(screen) };
}

fn screen() -> Option<&'static Screen> {
    unsafe { *core::ptr::addr_of!(SCREEN) }
}

pub fn gfx_info() -> Option<(u32, u32)> {
    screen().and_then(|s| (s.gfx_info)())
}

pub fn rotation(phys: usize) -> Option<Rotate> {
    screen().and_then(|s| (s.rotation)(phys))
}

/// The DSI touch chip, when the kernel has one and it answered.
pub fn dsi() -> Option<&'static Dsi> {
    screen().and_then(|s| s.dsi).filter(|d| (d.present)())
}
