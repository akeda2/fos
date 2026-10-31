//! Finger and typed-key bookkeeping.

use tape_rt::TouchEvent;

/// What a touch screen drives: a set of widgets that a finger can grab. `Grab` is whatever the target needs to remember about
/// the widget a finger holds (a drawbar index, a key note...).
pub trait TouchTarget {
    type Grab: Copy;
    /// A finger came down at (`x`, `y`) in the tape's own pixels; `Some` when it landed on something that wants to be held.
    fn down(&mut self, x: i32, y: i32) -> Option<Self::Grab>;
    /// The held finger moved; returns the grab to hold from now on (a key slid along the keyboard hands over to the next).
    fn moved(&mut self, grab: Self::Grab, x: i32, y: i32) -> Self::Grab;
    fn up(&mut self, grab: Self::Grab);
}

const FINGERS: usize = 8;

/// Maps a panel's coordinates onto a tape screen's pixels and follows the mouse or touchpad (the kernel already applies
/// the `mouse-speed` setting and the acceleration curve), so every tape draws its cursor the same way.
#[derive(Clone, Copy)]
pub struct Pointer {
    width: i32,
    height: i32,
    screen: u32,
    at: Option<(i32, i32)>,
}

impl Pointer {
    /// For a tape picture of `width` x `height` pixels on tape screen `screen`.
    pub fn new(width: i32, height: i32, screen: u32) -> Self {
        Pointer { width, height, screen, at: None }
    }

    /// The event's position in the tape's pixels (panel measuring `extent`); `None` for a panel on another screen.
    pub fn locate(&self, event: &TouchEvent, extent: (i32, i32)) -> Option<(i32, i32)> {
        if event.screen >= 0 && event.screen != self.screen as i32 {
            return None;
        }
        Some((event.x * self.width / extent.0.max(1), event.y * self.height / extent.1.max(1)))
    }

    /// Notes where the mouse is when `event` is a mouse event on this screen; true if the cursor moved.
    pub fn track(&mut self, event: &TouchEvent, extent: (i32, i32)) -> bool {
        match self.locate(event, extent) {
            Some(at) if event.is_mouse() && self.at != Some(at) => {
                self.at = Some(at);
                true
            }
            _ => false,
        }
    }

    /// Where the mouse last was; `None` until one has moved on this screen.
    pub fn at(&self) -> Option<(i32, i32)> {
        self.at
    }
}

/// Up to eight fingers, each holding at most one widget from the moment it lands until it lifts.
pub struct Touches<G: Copy> {
    grabs: [Option<(i32, G)>; FINGERS],
    pointer: Pointer,
}

impl<G: Copy> Touches<G> {
    /// For a tape picture of `width` x `height` pixels on tape screen `screen`.
    pub fn new(width: i32, height: i32, screen: u32) -> Self {
        Touches { grabs: [None; FINGERS], pointer: Pointer::new(width, height, screen) }
    }

    /// Handles one finger event whose panel measures `extent` (the panel's own units). False when it was ignored: a panel
    /// on another screen. True when the instrument may look different afterwards.
    pub fn handle<T: TouchTarget<Grab = G>>(&mut self, event: &TouchEvent, extent: (i32, i32), target: &mut T) -> bool {
        let Some((x, y)) = self.pointer.locate(event, extent) else { return false };
        self.pointer.track(event, extent);
        let held = self.grabs.iter().position(|g| g.is_some_and(|(id, _)| id == event.id));
        match (event.down, held) {
            (true, None) => {
                if let Some(free) = self.grabs.iter().position(|g| g.is_none()) {
                    if let Some(grab) = target.down(x, y) {
                        self.grabs[free] = Some((event.id, grab));
                    }
                }
            }
            (true, Some(slot)) => {
                if let Some((id, grab)) = self.grabs[slot] {
                    self.grabs[slot] = Some((id, target.moved(grab, x, y)));
                }
            }
            (false, Some(slot)) => {
                if let Some((_, grab)) = self.grabs[slot] {
                    target.up(grab);
                }
                self.grabs[slot] = None;
            }
            (false, None) => {}
        }
        true
    }

    /// Where the mouse (or touchpad) last was, in the tape's pixels; `None` until one has moved on this screen.
    pub fn pointer(&self) -> Option<(i32, i32)> {
        self.pointer.at()
    }

    /// Drains the touch queue into `target`; true when anything on this screen happened.
    pub fn poll<T: TouchTarget<Grab = G>>(&mut self, target: &mut T) -> bool {
        let mut changed = false;
        while let Some(event) = tape_rt::touch_event() {
            let Some(extent) = tape_rt::touch_extent(event.panel) else { continue };
            changed |= self.handle(&event, extent, target);
        }
        changed
    }
}

/// Notes played from a terminal's keyboard: it sends a key press and never a release, so each note rings for a fixed time.
pub struct TypedKeys {
    notes: [Option<(u8, u64)>; 8],
    ring_us: u64,
}

impl TypedKeys {
    pub fn new(ring_us: u64) -> Self {
        TypedKeys { notes: [None; 8], ring_us }
    }

    /// A note was started at `now` (microseconds): it ends `ring_us` later; a repeat of the same note extends it.
    pub fn press(&mut self, note: u8, now: u64) {
        let slot = self.notes.iter().position(|t| t.is_some_and(|(n, _)| n == note)).or_else(|| self.notes.iter().position(|t| t.is_none()));
        if let Some(slot) = slot {
            self.notes[slot] = Some((note, now + self.ring_us));
        }
    }

    /// Calls `end` for every note whose time is up; true if there were any.
    pub fn expire(&mut self, now: u64, mut end: impl FnMut(u8)) -> bool {
        let mut any = false;
        for slot in self.notes.iter_mut() {
            if let Some((note, until)) = *slot {
                if now >= until {
                    end(note);
                    *slot = None;
                    any = true;
                }
            }
        }
        any
    }
}
