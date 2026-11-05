//! Serial ports for tapes (`serial_*`, ABI 7): the UARTs and USB adapters of `kernel_core::serialports`, the same code the
//! embedded kernels use. What mf adds is ownership, because several tapes can run at once: the first process to open a port
//! owns it until it closes it or ends (`process_ended`), another process's open gets `SERIAL_BUSY` and its other calls on that
//! port do nothing. Every call runs under the USB lock on CPU 0, like the MIDI and audio calls, and a tape's memory never goes
//! to a driver: data moves through kernel buffers.

use core::sync::atomic::{AtomicU64, Ordering};
use fosfor_abi::tape::{SERIAL_BUSY, SERIAL_NO_PORT, SerialInfo};
use kernel_core::serialports;

use crate::{process, sched, usb};

const MAX_PORTS: usize = 16;
const CHUNK: usize = 4096;
static OWNERS: [AtomicU64; MAX_PORTS] = [const { AtomicU64::new(0) }; MAX_PORTS];

fn owner(number: u64) -> Option<&'static AtomicU64> {
    (number as usize).checked_sub(1).and_then(|index| OWNERS.get(index))
}

/// The port's owner slot if the caller holds it.
fn mine(number: u64) -> Option<usize> {
    let slot = owner(number)?;
    (slot.load(Ordering::Acquire) == sched::current_pid()).then_some(number as usize)
}

pub fn count() -> u64 {
    let _usb = usb::tape_lock();
    serialports::count().min(MAX_PORTS) as u64
}

pub fn info(number: u64, out: u64) -> u64 {
    let mut info = SerialInfo::default();
    let found = {
        let _usb = usb::tape_lock();
        number as usize <= MAX_PORTS && serialports::info(number as usize, &mut info)
    };
    if !found {
        return 0;
    }
    let bytes = unsafe { core::slice::from_raw_parts((&info as *const SerialInfo).cast::<u8>(), core::mem::size_of::<SerialInfo>()) }; // kernel-memory
    u64::from(out != 0 && process::copy_to_user(out, bytes))
}

pub fn open(number: u64, baud: u64, format: u64) -> i64 {
    let Some(slot) = owner(number) else { return i64::from(SERIAL_NO_PORT) };
    let pid = sched::current_pid();
    if slot.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire).is_err() && slot.load(Ordering::Acquire) != pid {
        return i64::from(SERIAL_BUSY);
    }
    let result = {
        let _usb = usb::tape_lock();
        serialports::open(number as usize, baud as u32, format as u32)
    };
    if result != 0 {
        slot.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire).ok();
    }
    i64::from(result)
}

pub fn close(number: u64) {
    if let Some(port) = mine(number) {
        {
            let _usb = usb::tape_lock();
            serialports::close(port);
        }
        OWNERS[port - 1].store(0, Ordering::Release);
    }
}

/// `serial_read`: received bytes into the tape's buffer; never blocks.
pub fn read(number: u64, out: u64, capacity: u64) -> u64 {
    let Some(port) = mine(number) else { return 0 };
    let mut buffer = [0u8; CHUNK];
    let want = (capacity as usize).min(CHUNK);
    let got = {
        let _usb = usb::tape_lock();
        serialports::poll();
        serialports::read(port, &mut buffer[..want])
    };
    if got > 0 && !process::copy_to_user(out, &buffer[..got]) {
        return 0;
    }
    got as u64
}

/// `serial_write`: how many of the bytes the port took.
pub fn write(number: u64, data: u64, length: u64) -> u64 {
    let Some(port) = mine(number) else { return 0 };
    let Some(bytes) = process::user_bytes_up_to(data, length, CHUNK as u64) else { return 0 };
    let _usb = usb::tape_lock();
    serialports::write(port, &bytes) as u64
}

pub fn lines(number: u64) -> u64 {
    let Some(port) = mine(number) else { return 0 };
    let _usb = usb::tape_lock();
    serialports::poll();
    u64::from(serialports::lines(port))
}

pub fn set_lines(number: u64, lines: u64) {
    if let Some(port) = mine(number) {
        let _usb = usb::tape_lock();
        serialports::set_lines(port, lines as u32);
    }
}

pub fn set_break(number: u64, on: u64) {
    if let Some(port) = mine(number) {
        let _usb = usb::tape_lock();
        serialports::set_break(port, on != 0);
    }
}

/// A process ended: close and release every port it held.
pub fn process_ended(pid: u64) {
    for (index, slot) in OWNERS.iter().enumerate() {
        if slot.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let _usb = usb::lock();
            serialports::close(index + 1);
        }
    }
}
