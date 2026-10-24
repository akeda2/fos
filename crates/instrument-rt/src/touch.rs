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

/// Up to eight fingers, each holding at most one widget from the moment it lands until it lifts.
pub struct Touches<G: Copy> {
    grabs: [Option<(i32, G)>; FINGERS],
    width: i32,
    height: i32,
    screen: u32,
}

impl<G: Copy> Touches<G> {
    /// For a tape picture of `width` x `height` pixels on tape screen `screen`.
    pub fn new(width: i32, height: i32, screen: u32) -> Self {
        Touches { grabs: [None; FINGERS], width, height, screen }
    }

    /// Handles one finger event whose panel measures `extent` (the panel's own units). False when it was ignored: a panel
    /// on another screen. True when the instrument may look different afterwards.
    pub fn handle<T: TouchTarget<Grab = G>>(&mut self, event: &TouchEvent, extent: (i32, i32), target: &mut T) -> bool {
        if event.screen >= 0 && event.screen != self.screen as i32 {
            return false;
        }
        let x = event.x * self.width / extent.0.max(1);
        let y = event.y * self.height / extent.1.max(1);
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
