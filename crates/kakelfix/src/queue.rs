//! Mixer writes off the drawing thread. Some devices take milliseconds per write (one USB control transfer per mixer word,
//! with a pause after each), so a fader drag would stall the screen. Where the kernel runs tape threads (the server kernel),
//! the tape gives every write to a worker through a `Link`: one slot per crosspoint, master and control holds the latest
//! value not yet written, so a drag that outruns the device sends only where the fader is now. The drawing thread reads
//! its own `Queued` shadow of the gains instead of asking the device (whose calls wait while a write is in progress), and
//! sees the device's serial move only when the worker is idle, so a burst of its own writes means one re-read at the end.
//! Without threads `Queued` writes straight through and reads the device; the app cannot tell the difference.

use crate::Device;
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use fosfor_abi::tape::MixerControl;
use mixer_model::{Group, MAX_CHANNELS, MAX_SOURCES};

/// Controls the app shows (`App::controls` holds as many).
pub const MAX_CONTROLS: usize = 96;
const GAINS: usize = MAX_CHANNELS * MAX_SOURCES;
/// An empty slot. Raw gains stop far below it (`MixerInfo::max`).
const EMPTY: u32 = u32::MAX;
const EMPTY_CONTROL: u64 = u64::MAX;

/// The writes waiting for the worker, shared by both threads (a `static` in the tape).
pub struct Link {
    gains: [AtomicU32; GAINS],
    masters: [AtomicU32; MAX_CHANNELS],
    controls: [AtomicU64; MAX_CONTROLS],
    /// Filled slots plus writes in progress: 0 means the device holds everything asked of it.
    queued: AtomicU32,
    /// Counts every new request; the worker sleeps on it (`futex_wait`).
    pub work: AtomicU32,
    failed: AtomicU32,
    stop: AtomicBool,
}

impl Default for Link {
    fn default() -> Link {
        Link::new()
    }
}

impl Link {
    pub const fn new() -> Link {
        Link {
            gains: [const { AtomicU32::new(EMPTY) }; GAINS],
            masters: [const { AtomicU32::new(EMPTY) }; MAX_CHANNELS],
            controls: [const { AtomicU64::new(EMPTY_CONTROL) }; MAX_CONTROLS],
            queued: AtomicU32::new(0),
            work: AtomicU32::new(0),
            failed: AtomicU32::new(0),
            stop: AtomicBool::new(false),
        }
    }

    /// Nothing waiting and nothing being written.
    pub fn idle(&self) -> bool {
        self.queued.load(Ordering::Acquire) == 0
    }

    /// Writes the device refused so far.
    pub fn failures(&self) -> u32 {
        self.failed.load(Ordering::Acquire)
    }

    /// Asks the worker to finish what is queued and end (`Worker::run` returns).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.work.fetch_add(1, Ordering::AcqRel);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn put32(&self, slot: &AtomicU32, value: u32) {
        if slot.swap(value, Ordering::AcqRel) == EMPTY {
            self.queued.fetch_add(1, Ordering::AcqRel);
        }
        self.work.fetch_add(1, Ordering::AcqRel);
    }

    fn put_control(&self, index: usize, value: i64) {
        if self.controls[index].swap(value as u64, Ordering::AcqRel) == EMPTY_CONTROL {
            self.queued.fetch_add(1, Ordering::AcqRel);
        }
        self.work.fetch_add(1, Ordering::AcqRel);
    }

    /// The worker's step: writes every filled slot once, masters first, controls last. Each slot is emptied before its
    /// write and counted done after it, so `idle` only turns true once the device has the value. False if there was nothing.
    pub fn drain(&self, device: &mut impl Device) -> bool {
        let mut wrote = false;
        let mut done = |ok: bool| {
            if !ok {
                self.failed.fetch_add(1, Ordering::AcqRel);
            }
            self.queued.fetch_sub(1, Ordering::AcqRel);
            wrote = true;
        };
        for (o, slot) in self.masters.iter().enumerate() {
            let raw = slot.swap(EMPTY, Ordering::AcqRel);
            if raw != EMPTY {
                done(device.set_master(o, raw));
            }
        }
        for (index, slot) in self.gains.iter().enumerate() {
            let raw = slot.swap(EMPTY, Ordering::AcqRel);
            if raw != EMPTY {
                done(device.set_gain(index / MAX_SOURCES, index % MAX_SOURCES, raw));
            }
        }
        for (index, slot) in self.controls.iter().enumerate() {
            let value = slot.swap(EMPTY_CONTROL, Ordering::AcqRel);
            if value != EMPTY_CONTROL {
                done(device.set_control(index, value as i64));
            }
        }
        wrote
    }
}

/// The drawing thread's device: reads the shadow, queues writes on the `Link` (or writes through when `link` is None).
pub struct Queued<'a, D: Device> {
    pub device: D,
    link: Option<&'a Link>,
    outputs: usize,
    sources: usize,
    gains: [Cell<u32>; GAINS],
    masters: [Cell<u32>; MAX_CHANNELS],
    /// The shadow matches the device as of `serial`.
    valid: Cell<bool>,
    serial: Cell<u64>,
}

impl<'a, D: Device> Queued<'a, D> {
    /// `outputs` and `sources` (inputs plus playback channels) bound the shadow.
    pub fn new(device: D, link: Option<&'a Link>, outputs: usize, sources: usize) -> Self {
        Queued { device, link, outputs: outputs.min(MAX_CHANNELS), sources: sources.min(MAX_SOURCES), gains: [const { Cell::new(EMPTY) }; GAINS], masters: [const { Cell::new(EMPTY) }; MAX_CHANNELS], valid: Cell::new(false), serial: Cell::new(0) }
    }

    pub fn threaded(&self) -> bool {
        self.link.is_some()
    }

    fn refresh(&self) {
        if self.valid.get() {
            return;
        }
        for o in 0..self.outputs {
            for s in 0..self.sources {
                self.gains[o * MAX_SOURCES + s].set(self.device.gain(o, s).unwrap_or(EMPTY));
            }
            self.masters[o].set(self.device.master(o).unwrap_or(EMPTY));
        }
        self.valid.set(true);
    }

    /// Forget the shadow (a queued write was refused, so the serial may not have moved): the next read asks the device.
    pub fn invalidate(&self) {
        self.valid.set(false);
    }

    fn known(raw: u32) -> Option<u32> {
        (raw != EMPTY).then_some(raw)
    }
}

impl<D: Device> Device for Queued<'_, D> {
    fn gain(&self, o: usize, s: usize) -> Option<u32> {
        let Some(link) = self.link else { return self.device.gain(o, s) };
        if o >= self.outputs || s >= self.sources {
            return None;
        }
        if link.idle() {
            self.refresh();
        }
        Self::known(self.gains[o * MAX_SOURCES + s].get())
    }
    fn set_gain(&mut self, o: usize, s: usize, raw: u32) -> bool {
        let Some(link) = self.link else { return self.device.set_gain(o, s, raw) };
        if o >= self.outputs || s >= self.sources || raw == EMPTY {
            return false;
        }
        self.gains[o * MAX_SOURCES + s].set(raw);
        link.put32(&link.gains[o * MAX_SOURCES + s], raw);
        true
    }
    fn master(&self, o: usize) -> Option<u32> {
        let Some(link) = self.link else { return self.device.master(o) };
        if o >= self.outputs {
            return None;
        }
        if link.idle() {
            self.refresh();
        }
        Self::known(self.masters[o].get())
    }
    fn set_master(&mut self, o: usize, raw: u32) -> bool {
        let Some(link) = self.link else { return self.device.set_master(o, raw) };
        if o >= self.outputs || raw == EMPTY {
            return false;
        }
        self.masters[o].set(raw);
        link.put32(&link.masters[o], raw);
        true
    }
    /// While writes are queued this is the serial the shadow was read at, so the app's own changes do not make it re-read
    /// a half-written device; once idle a moved serial (ours or anyone's) marks the shadow stale.
    fn serial(&self) -> u64 {
        let Some(link) = self.link else { return self.device.serial() };
        if link.idle() {
            let serial = self.device.serial();
            if serial != self.serial.get() {
                self.serial.set(serial);
                self.valid.set(false);
            }
        }
        self.serial.get()
    }
    fn name(&self, group: Group, index: usize, out: &mut [u8]) -> usize {
        self.device.name(group, index, out)
    }
    fn control(&self, index: usize) -> Option<MixerControl> {
        self.device.control(index)
    }
    fn control_item(&self, index: usize, item: usize, out: &mut [u8]) -> usize {
        self.device.control_item(index, item, out)
    }
    fn set_control(&mut self, index: usize, value: i64) -> bool {
        match self.link {
            Some(link) if index < MAX_CONTROLS => {
                link.put_control(index, value);
                true
            }
            Some(_) => false,
            None => self.device.set_control(index, value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Fake;

    #[test]
    fn writes_wait_for_the_worker_and_coalesce() {
        let link = Link::new();
        let mut ui = Queued::new(Fake::new(), Some(&link), 2, 4);
        let mut worker = Fake::new();
        let before = ui.serial();
        assert_eq!(ui.gain(0, 2), Some(0x8000));
        for raw in [100, 200, 300] {
            assert!(ui.set_gain(0, 2, raw));
        }
        assert!(ui.set_master(1, 0x4000));
        assert!(ui.set_control(0, 1));
        assert_eq!(ui.gain(0, 2), Some(300));
        assert!(!link.idle());
        assert_eq!(ui.serial(), before);
        assert!(link.drain(&mut worker));
        assert!(link.idle());
        assert_eq!(worker.writes, 2);
        assert_eq!((worker.gain(0, 2), worker.master(1), worker.controls[0]), (Some(300), Some(0x4000), 1));
        assert!(!link.drain(&mut worker));
    }

    #[test]
    fn idle_serial_change_rereads_the_device() {
        let link = Link::new();
        let mut ui = Queued::new(Fake::new(), Some(&link), 2, 4);
        let first = ui.serial();
        assert_eq!(ui.gain(1, 3), Some(0x8000));
        ui.device.set_gain(1, 3, 77);
        assert_ne!(ui.serial(), first);
        assert_eq!(ui.gain(1, 3), Some(77));
        assert_eq!(ui.gain(1, 0), None);
    }

    #[test]
    fn refused_writes_are_counted() {
        let link = Link::new();
        let mut ui = Queued::new(Fake::new(), Some(&link), 2, 4);
        let mut worker = Fake::new();
        worker.refuse = true;
        ui.set_gain(0, 0, 5);
        link.drain(&mut worker);
        assert_eq!(link.failures(), 1);
        assert!(link.idle());
        ui.invalidate();
        assert_eq!(ui.gain(0, 0), None);
    }

    #[test]
    fn without_a_link_it_writes_through() {
        let mut ui = Queued::new(Fake::new(), None, 2, 4);
        let first = ui.serial();
        assert!(ui.set_gain(0, 3, 9));
        assert_eq!(ui.device.writes, 1);
        assert_ne!(ui.serial(), first);
    }
}
