//! USB for the server kernel: xHCI, EHCI and OHCI controllers, keyboards, MIDI and touch panels, all from `kernel-core`.
//! The controller code is unlocked and written for one CPU, and tapes call it from any CPU (`midi_send`, `touch_read`), so
//! two rules keep it safe: events are never taken by interrupt (the controllers stay in the polled mode, where whoever holds
//! `lock` drains the rings), and every entry point here holds that lock. It is re-entrant per thread because a transfer waits
//! by polling, which comes back through `poll`. The service thread polls on CPU 0 and drains typed bytes to the shell.

use core::sync::atomic::{AtomicUsize, Ordering};
use fosfor_abi::keymap::Layout;
use kernel_core::usb_class::{self, Class, Host, NONE};
#[cfg(target_arch = "x86_64")]
use kernel_core::usb_hid;
// The Pi's enumerator: xHCI boot keyboards, the same calls as `usb_hid` (which carries hubs, EHCI and OHCI besides).
#[cfg(target_arch = "aarch64")]
use kernel_core::usb_kbd as usb_hid;
#[cfg(target_arch = "x86_64")]
use kernel_core::ehci;
use kernel_core::{usb_audio, usb_midi, usb_pointer, usb_serial, usb_storage, xhci};

use crate::sync::SpinLock;

/// The thread (id + 1) that holds the USB lock, 0 for none. A plain flag, not a `SpinLock`: that one keeps interrupts off, and the
/// controller code waits on timer ticks, which only advance while interrupts are on.
static OWNER: AtomicUsize = AtomicUsize::new(0);
/// A ticket lock: a caller takes the next ticket (`NEXT`) and holds the lock when `SERVING` reaches it. Callers get the lock in the
/// order they asked. A plain compare-and-swap was not fair: a thread that releases and at once takes it again (a disk writer, one
/// transfer after another) nearly always won over a waiter, worst of all when both are pinned to CPU 0, where the waiter only runs
/// while the holder is preempted mid-transfer. On aneby that starved a tape's `audio_read` for seconds while it wrote to a USB
/// stick on the USB audio interface's controller, and the capture FIFO overflowed. Now a waiter is next after at most the transfer in
/// flight. A ticket is never abandoned: a killed thread ends only on its way back to ring 3 (`process::check_killed`).
static NEXT: AtomicUsize = AtomicUsize::new(0);
static SERVING: AtomicUsize = AtomicUsize::new(0);

pub struct UsbGuard {
    taken: bool,
}

/// The lock for a tape call. The drivers and their interrupt handlers are CPU-0 oriented, so the tape moves there first.
pub fn tape_lock() -> UsbGuard {
    crate::sched::pin_current(0);
    lock()
}

pub fn lock() -> UsbGuard {
    let me = crate::sched::current_id() + 1;
    if OWNER.load(Ordering::Acquire) == me {
        return UsbGuard { taken: false };
    }
    let ticket = NEXT.fetch_add(1, Ordering::AcqRel);
    if SERVING.load(Ordering::Acquire) != ticket {
        // A thread gives its CPU away while it waits: the holder is often on the same CPU (both pinned to CPU 0). The boot
        // context and idle threads cannot be queued, so they spin, letting the timer tick through the wait: a tape's system
        // call arrives with interrupts off, and the holder may be a thread this CPU preempted.
        let yielding = crate::sched::may_yield();
        while SERVING.load(Ordering::Acquire) != ticket {
            if yielding {
                let was_on = crate::cpu::interrupts_save_and_disable();
                crate::cpu::interrupts_enable();
                crate::sched::yield_now();
                core::hint::spin_loop();
                if !was_on {
                    crate::cpu::interrupts_disable();
                }
            } else {
                let was_on = crate::cpu::interrupts_save_and_disable();
                crate::cpu::interrupts_enable();
                core::hint::spin_loop();
                if !was_on {
                    crate::cpu::interrupts_disable();
                }
            }
        }
    }
    OWNER.store(me, Ordering::Release);
    UsbGuard { taken: true }
}

/// The lock if nobody holds it or waits for it, without waiting (an interrupt handler's poll, the PS/2 mouse).
pub fn try_lock() -> Option<UsbGuard> {
    let me = crate::sched::current_id() + 1;
    if OWNER.load(Ordering::Acquire) == me {
        return Some(UsbGuard { taken: false });
    }
    // Free means every ticket handed out has been served and given back: then `SERVING` cannot move under us.
    let serving = SERVING.load(Ordering::Acquire);
    NEXT.compare_exchange(serving, serving.wrapping_add(1), Ordering::AcqRel, Ordering::Relaxed).ok()?;
    OWNER.store(me, Ordering::Release);
    Some(UsbGuard { taken: true })
}

impl Drop for UsbGuard {
    fn drop(&mut self) {
        if self.taken {
            OWNER.store(0, Ordering::Release);
            SERVING.fetch_add(1, Ordering::AcqRel);
        }
    }
}

const KEYS: usize = 64;
static TYPED: SpinLock<([u8; KEYS], usize, usize)> = SpinLock::new(([0; KEYS], 0, 0));

fn key_push(byte: u8) {
    let mut queue = TYPED.lock();
    let (bytes, head, tail) = &mut *queue;
    if (*head + 1) % KEYS != *tail {
        bytes[*head] = byte;
        *head = (*head + 1) % KEYS;
    }
}

fn key_layout() -> Layout {
    if crate::config::swedish() { Layout::Swedish } else { Layout::Us }
}

static ENV: kernel_core::usb_env::Env = kernel_core::usb_env::Env { key_push, key_layout, boot_disk_known: || crate::storage::boot_disk().is_some() };

static HOST: Host = Host {
    set_step: usb_hid::set_step,
    step: usb_hid::step,
    release_port: usb_hid::release_port,
    probe_device: |index, controller, device| usb_hid::probe_device(index, controller, device),
    background_event: usb_hid::background_event,
    poll: poll_nowait,
};

static AUDIO: Class = Class { xhci_departed: Some(usb_audio::xhci_departed), ehci_departed: Some(usb_audio::ehci_departed), irq_event: Some(usb_audio::irq_event), ehci_event: Some(usb_audio::ehci_event), ..NONE };
static MIDI: Class = Class { release: Some(usb_midi::release), background_event: Some(usb_midi::background_event), ..NONE };
static POINTER: Class = Class { release: Some(usb_pointer::release), background_event: Some(usb_pointer::background_event), ..NONE };
static SERIAL: Class = Class {
    background_event: Some(usb_serial::background_event),
    xhci_departed: Some(usb_serial::xhci_departed),
    ehci_departed: Some(usb_serial::ehci_departed),
    ehci_root_departed: Some(usb_serial::ehci_root_departed),
    ..NONE
};

pub fn init() {
    kernel_core::usb_env::set(&ENV);
    usb_class::set_host(&HOST);
    // The private drivers' classes (docs/private.md) first: their `claim` must see a device before the generic drivers do.
    for class in crate::private::USB_CLASSES.iter().copied().chain([&AUDIO, &MIDI, &POINTER, &SERIAL]) {
        usb_class::register(class);
    }
    let _usb = lock();
    let x = xhci::init_all();
    #[cfg(target_arch = "x86_64")]
    let e = ehci::init_all();
    #[cfg(not(target_arch = "x86_64"))]
    let e = 0;
    #[cfg(target_arch = "x86_64")]
    let o = kernel_core::ohci::init_all();
    #[cfg(not(target_arch = "x86_64"))]
    let o = 0;
    if x + e + o == 0 {
        println!("USB: no xHCI, EHCI or OHCI controller.");
        return;
    }
    let devices = usb_hid::probe_all();
    // Events by MSI like the embedded kernel (isochronous audio refills need them); no-msi in settings keeps them polled.
    xhci::enable_interrupts_all(crate::config::no_msi());
    println!("USB: {x} xHCI, {e} EHCI and {o} OHCI controller(s), {devices} usable device(s) (keyboards, MIDI, touch, disks).");
}

/// The host's `poll` hook, which kernel-core also calls from interrupt handlers: waiting for the lock there could spin forever
/// (the holder is a thread this CPU cannot resume), so a busy lock means someone is polling already and this pass is skipped.
fn poll_nowait() {
    if let Some(_usb) = try_lock() {
        usb_hid::poll();
    }
}

/// One pass over the controllers' rings, then typed bytes to the shell (the service thread calls this).
pub fn poll() {
    {
        let _usb = lock();
        usb_hid::poll();
    }
    loop {
        let byte = {
            let mut queue = TYPED.lock();
            let (bytes, head, tail) = &mut *queue;
            if *head == *tail {
                break;
            }
            let byte = bytes[*tail];
            *tail = (*tail + 1) % KEYS;
            byte
        };
        crate::shell::feed_key(byte);
    }
}

// ---- tape calls (ABI 24-26): the kernel-core implementations, on kernel buffers, under the lock ----

pub fn midi_devices() -> u64 {
    let _usb = tape_lock();
    u64::from(usb_midi::api_midi_devices())
}

/// A disk transfer. The controllers are CPU-0 oriented like every other USB entry, so a thread past boot moves there first; the
/// transfer polls with the USB lock held and interrupts on (the caller's disk lock, `tapefs::lock`, already allows that).
pub fn storage_read(device: usize, lba: u64, buffer: &mut [u8]) -> Result<(), usb_storage::Error> {
    let _usb = storage_lock();
    usb_storage::read(device, lba, buffer)
}

pub fn storage_write(device: usize, lba: u64, buffer: &[u8]) -> Result<(), usb_storage::Error> {
    let _usb = storage_lock();
    usb_storage::write(device, lba, buffer)
}

fn storage_lock() -> UsbGuard {
    if crate::sched::ready() {
        crate::sched::pin_current(0);
    }
    lock()
}

/// `midi_read`: six bytes (device, cable, count, three data bytes) to `out`.
pub fn midi_read(out: u64) -> i64 {
    let mut packet = [0u8; 6];
    let got = {
        let _usb = tape_lock();
        usb_midi::api_midi_read(packet.as_mut_ptr())
    };
    if got > 0 && !crate::process::copy_to_user(out, &packet) {
        return -1;
    }
    i64::from(got)
}

pub fn midi_send(device: u64, message: u64, length: u64) -> i64 {
    if length == 0 || length > 3 {
        return -1;
    }
    let Some(bytes) = crate::process::user_bytes_up_to(message, length, 3) else { return -1 };
    let _usb = tape_lock();
    i64::from(usb_midi::api_midi_send(device as u32, bytes.as_ptr(), length as u32))
}

pub fn touch_info(panel: u64) -> u64 {
    let _usb = tape_lock();
    usb_pointer::api_touch_info(panel as u32)
}

/// `touch_read`: six i32 (panel, id, x, y, down, screen) to `out`.
pub fn touch_read(out: u64) -> i64 {
    let mut event = [0i32; 6];
    let got = {
        let _usb = tape_lock();
        crate::ps2::deliver_mouse();
        usb_pointer::api_touch_read(event.as_mut_ptr())
    };
    if got > 0 {
        let mut bytes = [0u8; 24];
        for (chunk, value) in bytes.chunks_mut(4).zip(event) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        if !crate::process::copy_to_user(out, &bytes) {
            return -1;
        }
    }
    i64::from(got)
}
