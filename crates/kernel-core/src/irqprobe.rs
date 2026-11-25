//! The interrupt self-test of the Realtek drivers (`rtl8168`, `rtl8125`), taken one step at a time.
//!
//! A NIC is switched from polling to interrupts only when its delivery path is proven: MSI first, then the legacy INTx
//! pins an Intel/QEMU chipset routes to the I/O APIC (the ACPI routing table is not read, so candidate pins are tried).
//! For each path the driver unmasks its transmit-OK interrupt and sends one broadcast frame (the RTL8168G/H and the 8125
//! have no forced software interrupt that works), and the path is kept if the interrupt counter moved.
//!
//! The waiting is what makes this a state machine rather than a loop. At boot the drivers run it with interrupts on and
//! nothing locked, and `run` waits a tick at a time. But a card with no link at boot cannot send its test frame, so the
//! test is retried from `receive` once the link is up, and on the server kernel `receive` always runs under the NIC
//! lock, a spinlock with interrupts off: a loop there would wait for ticks and an interrupt that cannot arrive on this
//! CPU until the lock is let go (and the lock may not be let go in the middle, since the scheduler preempts and the NIC
//! lock is re-entrant per CPU). So `step` arms one path and returns; a later `receive`, after the lock was released and
//! the interrupt (if the path works) delivered, looks at the counter and either keeps the path or arms the next one.

use core::sync::atomic::Ordering;

use crate::e1000::{Delivery, ISR};
use crate::{pci, timer};

/// How long an armed path has to show its interrupt. At boot the interrupt arrives in microseconds; from `receive` it
/// arrives when the NIC lock is let go, which is before the next `receive`, and the counter is looked at before the
/// clock, so a late look never fails a path that worked.
const WAIT_TICKS: u64 = 3;

/// What a driver gives the probe.
pub trait SelfTest {
    /// The interface index the handler is registered with.
    fn probe_index(&self) -> usize;
    /// The driver's interrupt handler.
    fn probe_handler(&self) -> fn(usize);
    fn probe_link_up(&self) -> bool;
    /// Clears pending status, unmasks the card's interrupts with transmit OK added, and sends one broadcast frame whose
    /// completion raises the interrupt.
    fn probe_arm(&mut self);
    /// Masks every interrupt again (the path did not work).
    fn probe_disarm(&mut self);
}

/// Where the search is.
#[derive(Clone, Copy)]
enum Stage {
    Msi,
    /// The next INTx candidate to try (an index into `msi::intx_candidates`).
    Intx(usize),
}

#[derive(Clone, Copy)]
struct Armed {
    delivery: Delivery,
    /// The path's interrupt count before the test frame went out.
    before: u64,
    sent_at: u64,
}

pub enum Step {
    /// A path is armed; call `step` again later.
    Pending,
    /// The path works: the caller unmasks its normal interrupts and keeps it.
    Passed(Delivery),
    /// No path worked (or the link went away); the NIC stays polled.
    Failed(&'static str),
}

pub struct Probe {
    device: pci::Device,
    stage: Stage,
    armed: Option<Armed>,
    reason: &'static str,
}

impl Probe {
    pub fn new(device: pci::Device) -> Self {
        Probe { device, stage: Stage::Msi, armed: None, reason: "no interrupt path passed its self-test" }
    }

    /// The whole search at once, for a caller with interrupts on (boot): waits a tick at a time while a path is armed.
    pub fn run<N: SelfTest>(mut self, nic: &mut N) -> Result<Delivery, &'static str> {
        loop {
            match self.step(nic) {
                Step::Pending => {
                    let tick = timer::ticks();
                    while timer::ticks() == tick {
                        core::hint::spin_loop();
                    }
                }
                Step::Passed(delivery) => return Ok(delivery),
                Step::Failed(reason) => return Err(reason),
            }
        }
    }

    /// One step of the search: looks at the armed path (kept if its interrupt came, given up after `WAIT_TICKS`),
    /// then arms the next one. Never waits, so it may run with interrupts off.
    pub fn step<N: SelfTest>(&mut self, nic: &mut N) -> Step {
        if let Some(armed) = self.armed {
            if count(armed.delivery, nic.probe_index()) != armed.before {
                self.armed = None;
                return Step::Passed(armed.delivery);
            }
            if timer::ticks().wrapping_sub(armed.sent_at) < WAIT_TICKS {
                return Step::Pending;
            }
            self.armed = None;
            nic.probe_disarm();
            match armed.delivery {
                Delivery::Msi(vector) => {
                    self.device.disable_msi();
                    crate::msi::release(vector);
                    self.reason = "the MSI self-test message never arrived";
                }
                Delivery::Intx { vector, .. } => {
                    crate::msi::detach_intx(vector, nic.probe_index());
                    self.reason = "no I/O APIC pin delivered the self-test interrupt";
                }
                _ => {}
            }
        }
        if !nic.probe_link_up() {
            return Step::Failed("the link went down during the self-test");
        }
        match self.next_path(nic) {
            Some(delivery) => {
                let before = count(delivery, nic.probe_index());
                nic.probe_arm();
                self.armed = Some(Armed { delivery, before, sent_at: timer::ticks() });
                Step::Pending
            }
            None => Step::Failed(self.reason),
        }
    }

    /// Sets up the next untried path (its handler attached, the device told to use it); None when all are tried.
    fn next_path<N: SelfTest>(&mut self, nic: &N) -> Option<Delivery> {
        let (index, handler) = (nic.probe_index(), nic.probe_handler());
        if let Stage::Msi = self.stage {
            self.stage = Stage::Intx(0);
            if self.device.find_capability(0x05).is_some() {
                match crate::msi::allocate(handler, index) {
                    Some(vector) if self.device.enable_msi(vector, crate::hal::interrupt_target()) => return Some(Delivery::Msi(vector)),
                    Some(vector) => crate::msi::release(vector),
                    None => self.reason = "no free MSI vector",
                }
            } else {
                self.reason = "no MSI capability";
            }
        }
        let Stage::Intx(start) = self.stage else { return None };
        // The firmware's interrupt line (a hint) and the PCI pins the chipset routes to the I/O APIC (GSI 16-23).
        let (first, end) = crate::hal::gsi_range();
        let candidates = crate::msi::intx_candidates(self.device.interrupt_line());
        for position in start..candidates.len() {
            let gsi = candidates[position];
            self.stage = Stage::Intx(position + 1);
            if gsi == u32::MAX || gsi == 0 || gsi < first || gsi >= end || candidates[..position].contains(&gsi) {
                continue;
            }
            if let Some(vector) = crate::msi::attach_intx(gsi, handler, index) {
                return Some(Delivery::Intx { gsi, vector });
            }
        }
        None
    }
}

/// The interrupt count a path's self-test watches: the MSI vector's own count, or the driver's handler count for INTx
/// (whose pin may be shared, so only the handler knows the interrupt was this card's).
fn count(delivery: Delivery, index: usize) -> u64 {
    match delivery {
        Delivery::Msi(vector) => crate::msi::count(vector),
        _ => ISR[index].interrupts.load(Ordering::Relaxed),
    }
}
