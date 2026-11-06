//! The server kernel's network side: NIC bring-up (the `kernel-core` drivers), the `term net` console and KUP, in the wire
//! formats the embedded kernel speaks, so `tools/term_net.py`, `tools/peers.py`, `tools/devloop.py` and
//! `tools/send_kernel_update.py` work unchanged (raw Ethernet, ethertype 0x88b6, one tag byte after the header).
//!
//! - Discovery: a probe (tag 0x02) is answered with the host name (0x03); the name is also broadcast at boot and every 10 s.
//! - Terminal: HELLO (0x20) opens a session, DATA (0x21) carries keystrokes one way and console output the other, BYE
//!   (0x22) ends it. Console output reaches the client through a relay ring that `println!` fills and `poll` flushes in
//!   1 KiB DATA frames, so nothing here transmits while the console lock is held.
//! - KUP (0x3a-0x3e): a kernel image is staged in RAM, checked (length, RFC 1071 checksum), written to the inactive slot
//!   and made pending; the loader gives it one try. Only with `kernel-update` in the `debug` file.
//!
//! - NETFS (0x30-0x39) and the tape-sync manifest (0x40): `netfs.rs`, `tapesync.rs`.
//!
//! Not served yet: GPIO and the `tapesync` client command (`DEFERRED.md`). Everything runs on the boot CPU.

use core::sync::atomic::{AtomicUsize, Ordering};

use kernel_core::nic::{self, Nic};
use kernel_core::nicctl;
use kernel_core::{e1000, netdump::Mac, pci, rtl8139, rtl8168, threecom};

use crate::apic::{self, TICK_HZ};
use crate::sync::SpinLock;
use crate::{config, netfs, pmm, power, shell, slots, vm};

const ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const TAG_DISCOVER_REQUEST: u8 = 0x02;
const TAG_DISCOVER_REPLY: u8 = 0x03;
const TAG_HELLO: u8 = 0x20;
const TAG_DATA: u8 = 0x21;
const TAG_BYE: u8 = 0x22;
const TAG_KUP_REQUEST: u8 = 0x3a;
const TAG_KUP_DATA: u8 = 0x3b;
const TAG_KUP_STOP: u8 = 0x3c;
const TAG_KUP_ACK: u8 = 0x3d;
const TAG_KUP_ERROR: u8 = 0x3e;
const KUP_TOO_LARGE: u8 = 1;
const KUP_CHECKSUM_MISMATCH: u8 = 2;
const KUP_DISK_ERROR: u8 = 3;
const KUP_BUSY: u8 = 4;

/// Bytes per DATA frame sent to the client.
const DATA_CAPACITY: usize = 1024;
/// A session whose client has been silent this long can be taken over by another HELLO.
const STALE_TICKS: u64 = 5 * 60 * TICK_HZ;
/// A KUP transfer that has been silent this long is dead.
const KUP_STALE_TICKS: u64 = 5 * TICK_HZ;

// ---- bring-up ----

fn report(delivery: Result<impl core::fmt::Debug, &str>) {
    match delivery {
        Ok(delivery) => println!("  interrupts: {delivery:?} (the self-test interrupt arrived)."),
        Err(reason) => println!("  interrupts: polled ({reason})."),
    }
}

/// PCI location (bus, device, function) and device id per interface, for `nic`.
static mut LOCATIONS: [(u8, u8, u8, u16); nic::MAX_NICS] = [(0, 0, 0, 0); nic::MAX_NICS];

fn locate(index: usize, device: &pci::Device) {
    if index < nic::MAX_NICS {
        unsafe { (*core::ptr::addr_of_mut!(LOCATIONS))[index] = (device.bus, device.device, device.function, device.id) };
    }
}

/// `nic`: one block per interface: driver, PCI location, MAC, link, speed, frame counts and the driver's own line.
pub fn print_list() {
    let _nics = lock_nics();
    for index in 0..nic::count() {
        let Some(nic) = nic::at(index) else { continue };
        if nic::is_virtual(index) {
            if let (Some(parent), Some(name)) = (nic::virtual_parent(index), crate::vnic::name(index)) {
                println!("nic{}: vnic {name} on nic{} {} link {}", index + 1, parent + 1, Mac(&nic.mac()), if nic.link_up() { "up" } else { "DOWN" });
                let (received, sent, _) = nic.counters();
                println!("   frames: {received} received, {sent} sent");
            }
            continue;
        }
        let (bus, device, function, id) = unsafe { (*core::ptr::addr_of!(LOCATIONS))[index] };
        println!("nic{}: {} at {bus:02x}:{device:02x}.{function} (device {id:#06x}) {} link {}", index + 1, nic.family(), Mac(&nic.mac()), if nic.link_up() { "up" } else { "DOWN" });
        if let Some((speed, full)) = nic.link_speed() {
            println!("   negotiated {speed} Mbit/s {}", if full { "full duplex" } else { "half duplex" });
        }
        let (received, sent, dropped) = nic.counters();
        println!("   frames: {received} received, {sent} sent{}", if dropped != 0 { " (some dropped: no link)" } else { "" });
        nic.describe();
    }
    if nic::count() == 0 {
        println!("No network interface.");
    }
}

/// Starts every NIC the drivers know and puts it in the table. Needs interrupts on (the drivers prove their interrupt path).
pub fn init() {
    nicctl::set_frame_handler(frame_hook);
    let mut interfaces = 0;
    for device in pci::devices().filter(|d| d.vendor == e1000::VENDOR_INTEL && e1000::DEVICE_IDS.contains(&d.id)) {
        match e1000::E1000::init(&device, interfaces) {
            Ok(mut nic) => {
                println!("e1000 ({:?}) at {:02x}:{:02x}.{}: MAC {}, link {}.", nic.variant, device.bus, device.device, device.function, Mac(&nic.mac), if nic.link_up() { "up" } else { "down" });
                report(nic.enable_interrupts(&device, false));
                if nic::install(Nic::E1000(nic)).is_some() {
                    locate(interfaces, &device);
                    interfaces += 1;
                }
            }
            Err(error) => println!("e1000 at {:02x}:{:02x}.{}: initialisation failed ({error:?}).", device.bus, device.device, device.function),
        }
    }
    for device in pci::devices().filter(|d| d.vendor == rtl8168::VENDOR_REALTEK && rtl8168::DEVICE_IDS.contains(&d.id)) {
        match rtl8168::Rtl8168::init(&device, interfaces) {
            Ok(mut nic) => {
                println!("rtl8168 at {:02x}:{:02x}.{} (chip version {:#05x}): MAC {}.", device.bus, device.device, device.function, nic.version, Mac(&nic.mac));
                report(nic.enable_interrupts(&device));
                if nic::install(Nic::Rtl8168(nic)).is_some() {
                    locate(interfaces, &device);
                    interfaces += 1;
                }
            }
            Err(error) => println!("rtl8168 at {:02x}:{:02x}.{}: initialisation failed ({error:?}).", device.bus, device.device, device.function),
        }
    }
    for device in pci::devices().filter(|d| d.vendor == rtl8139::VENDOR_REALTEK && rtl8139::DEVICE_IDS.contains(&d.id)) {
        match rtl8139::Rtl8139::init(&device, interfaces) {
            Ok(mut nic) => {
                println!("rtl8139 at {:02x}:{:02x}.{}: MAC {}, link {}.", device.bus, device.device, device.function, Mac(&nic.mac), if nic.link_up() { "up" } else { "down" });
                report(nic.enable_interrupts(&device));
                if nic::install(Nic::Rtl8139(nic)).is_some() {
                    locate(interfaces, &device);
                    interfaces += 1;
                }
            }
            Err(error) => println!("rtl8139 at {:02x}:{:02x}.{}: initialisation failed ({error:?}).", device.bus, device.device, device.function),
        }
    }
    for device in pci::devices().filter(|d| d.vendor == threecom::VENDOR_3COM && threecom::DEVICE_IDS.contains(&d.id)) {
        match threecom::ThreeCom::init(&device, interfaces) {
            Ok(mut nic) => {
                println!("3c905 at {:02x}:{:02x}.{}: MAC {}, link {}.", device.bus, device.device, device.function, Mac(&nic.mac), if nic.link_up() { "up" } else { "down" });
                report(nic.enable_interrupts(&device));
                if nic::install(Nic::ThreeCom(nic)).is_some() {
                    locate(interfaces, &device);
                    interfaces += 1;
                }
            }
            Err(error) => println!("3c905 at {:02x}:{:02x}.{}: initialisation failed ({error:?}).", device.bus, device.device, device.function),
        }
    }
    if interfaces == 0 {
        println!("No supported network interface found (e1000, e1000e, I350, RTL8139, RTL8168 and 3c905 are driven).");
    }
}

// ---- the NIC lock ----

/// Serialises every access to the NIC table and the tapes' receive FIFOs (statics in `kernel-core`): the service thread's polling
/// and the tape calls run on any CPU. Re-entrant on one CPU, because the frame handler sends replies while the poll holds it.
static NICS: SpinLock<()> = SpinLock::new(());
/// The CPU (index + 1) that holds `NICS`, 0 for none.
static NICS_OWNER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub struct NicGuard {
    // The ticket lock's guard (None for a nested call); dropped after the owner mark is cleared.
    guard: Option<crate::sync::Guard<'static, ()>>,
}

pub fn lock_nics() -> NicGuard {
    let me = crate::percpu::this().index + 1;
    if NICS_OWNER.load(Ordering::Acquire) == me {
        return NicGuard { guard: None };
    }
    let guard = NICS.lock();
    NICS_OWNER.store(me, Ordering::Release);
    NicGuard { guard: Some(guard) }
}

impl Drop for NicGuard {
    fn drop(&mut self) {
        if self.guard.is_some() {
            NICS_OWNER.store(0, Ordering::Release);
        }
    }
}

// ---- frames out ----

pub fn mac(index: usize) -> Option<[u8; 6]> {
    nic::at(index).map(|nic| nic.mac())
}

pub fn link_up(index: usize) -> bool {
    nic::at(index).is_some_and(|nic| nic.link_up())
}

/// Builds and sends one fos frame (ethertype 0x88b6, tag, payload) out of interface `index`.
fn send(index: usize, destination: &[u8; 6], tag: u8, payload: &[u8]) -> bool {
    let _nics = lock_nics();
    let mut frame = [0u8; 1514];
    let Some(nic) = nic::at(index) else { return false };
    if payload.len() > 1514 - 15 {
        return false;
    }
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(&nic.mac());
    frame[12..14].copy_from_slice(&ETHERTYPE);
    frame[14] = tag;
    frame[15..15 + payload.len()].copy_from_slice(payload);
    nic.send(&frame[..15 + payload.len()])
}

/// `send` for the NETFS module.
pub fn send_frame(index: usize, destination: &[u8; 6], tag: u8, payload: &[u8]) -> bool {
    send(index, destination, tag, payload)
}

/// True while a kernel update is arriving (NETFS then refuses a transfer: both want the disk and the NIC).
pub fn kup_busy() -> bool {
    let kup = kup();
    kup.active && apic::ticks().wrapping_sub(kup.last_activity) < KUP_STALE_TICKS
}

/// The name announced for interface `index`: `NAME<n>` from the `hostname` file (n is the one-based interface number), or
/// without that file `fos-` and the last three bytes of the interface's MAC address.
pub fn hostname(index: usize, out: &mut [u8; 20]) -> usize {
    use core::fmt::Write;
    struct Cursor<'a>(&'a mut [u8; 20], usize);
    impl Write for Cursor<'_> {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let take = text.len().min(20 - self.1);
            self.0[self.1..self.1 + take].copy_from_slice(&text.as_bytes()[..take]);
            self.1 += take;
            Ok(())
        }
    }
    let mut cursor = Cursor(out, 0);
    let _ = match (config::hostname(), mac(index)) {
        (Some(base), _) => write!(cursor, "{base}{}", index + 1),
        (None, Some(mac)) => write!(cursor, "fos-{:02x}{:02x}{:02x}", mac[3], mac[4], mac[5]),
        (None, None) => write!(cursor, "fos"),
    };
    cursor.1
}

static mut ANNOUNCE_COUNT: u32 = 0;
static mut NEXT_ANNOUNCE: u64 = 0;

/// Broadcasts this machine's name on every interface that has a link: four times at one second, then every 10 s.
fn announce() {
    unsafe {
        if apic::ticks() < NEXT_ANNOUNCE {
            return;
        }
        ANNOUNCE_COUNT = ANNOUNCE_COUNT.saturating_add(1);
        NEXT_ANNOUNCE = apic::ticks() + if ANNOUNCE_COUNT <= 4 { 1 } else { 10 } * TICK_HZ;
    }
    for index in 0..nic::count() {
        // virtual interfaces do not announce: their host is already known through its physical NIC
        if link_up(index) && !nic::is_virtual(index) {
            let mut name = [0u8; 20];
            let length = hostname(index, &mut name);
            send(index, &[0xff; 6], TAG_DISCOVER_REPLY, &name[..length]);
        }
    }
}

// ---- the terminal sessions ----

/// A `term net` client. Slot `n` of `TERMINALS` is shell session `n + 1` (`session.rs`; session 0 is the console).
#[derive(Clone, Copy)]
struct Terminal {
    peer: Option<[u8; 6]>,
    /// The interface the client's HELLO arrived on: its output goes out of it.
    nic: usize,
    activity: u64,
    /// Non-zero: a tape (this process) holds the session instead of a client on the wire (`session_open`, ABI 38).
    owner: u64,
    /// The shell's `exit` ended it: the tape reads `SESSION_CLOSED` once the output is drained.
    exited: bool,
}
impl Terminal {
    const FREE: Terminal = Terminal { peer: None, nic: 0, activity: 0, owner: 0, exited: false };
    fn open(&self) -> bool {
        self.peer.is_some() || self.owner != 0
    }
}
const TERMINAL_SLOTS: usize = crate::session::MAX - 1;
static TERMINALS: SpinLock<[Terminal; TERMINAL_SLOTS]> = SpinLock::new([Terminal::FREE; TERMINAL_SLOTS]);
/// Bytes a tape typed into its session, fed to the shell by the service loop (the shell runs on that thread only).
struct Input {
    bytes: [u8; 1024],
    length: usize,
}
static INPUTS: [SpinLock<Input>; TERMINAL_SLOTS] = [const { SpinLock::new(Input { bytes: [0; 1024], length: 0 }) }; TERMINAL_SLOTS];

/// Console output waiting for a client; `println!` fills it from any CPU, `flush` empties it on the boot CPU.
struct Relay {
    bytes: [u8; 16384],
    length: usize,
}
static RELAYS: [SpinLock<Relay>; TERMINAL_SLOTS] = [const { SpinLock::new(Relay { bytes: [0; 16384], length: 0 }) }; TERMINAL_SLOTS];
/// How many sessions are open (a cheap test for the console's output path).
static OPEN: AtomicUsize = AtomicUsize::new(0);

/// True while a remote terminal session is open.
pub fn session_open() -> bool {
    OPEN.load(Ordering::Relaxed) != 0
}

fn queue(slot: usize, text: &str) {
    let mut ring = RELAYS[slot].lock();
    let take = text.len().min(ring.bytes.len() - ring.length);
    let start = ring.length;
    ring.bytes[start..start + take].copy_from_slice(&text.as_bytes()[..take]);
    ring.length += take;
}

/// Called by the console for every piece of text; queues it for the clients. Text that belongs to one session goes to that
/// session only (the console still shows it); text that belongs to none goes to every open one. Never transmits: output that
/// does not fit is dropped (a terminal can tolerate a torn screen; a stuck NIC must not stall the kernel).
pub fn relay(text: &str) {
    if !session_open() {
        return;
    }
    match crate::session::current() {
        Some(0) => {}
        Some(session) => queue(session - 1, text),
        None => {
            let open = TERMINALS.lock().map(|terminal| terminal.peer.is_some() && terminal.owner == 0);
            for (slot, _) in open.iter().enumerate().filter(|(_, open)| **open) {
                queue(slot, text);
            }
        }
    }
}

/// Sends everything queued as DATA frames.
fn flush() {
    for slot in 0..TERMINAL_SLOTS {
        let (peer, nic) = {
            let terminals = TERMINALS.lock();
            match terminals[slot].peer {
                Some(peer) => (peer, terminals[slot].nic),
                None => continue,
            }
        };
        loop {
            let mut chunk = [0u8; DATA_CAPACITY];
            let take = {
                let mut ring = RELAYS[slot].lock();
                let take = ring.length.min(DATA_CAPACITY);
                chunk[..take].copy_from_slice(&ring.bytes[..take]);
                let length = ring.length;
                ring.bytes.copy_within(take..length, 0);
                ring.length -= take;
                take
            };
            if take == 0 {
                break;
            }
            send(nic, &peer, TAG_DATA, &chunk[..take]);
        }
    }
}

fn close_terminal(slot: usize) {
    {
        let mut terminals = TERMINALS.lock();
        if terminals[slot].open() {
            OPEN.fetch_sub(1, Ordering::Relaxed);
        }
        terminals[slot] = Terminal::FREE;
    }
    RELAYS[slot].lock().length = 0;
    INPUTS[slot].lock().length = 0;
    shell::detach(slot + 1);
}

fn find_terminal(index: usize, source: [u8; 6]) -> Option<usize> {
    TERMINALS.lock().iter().position(|terminal| terminal.owner == 0 && terminal.peer == Some(source) && terminal.nic == index)
}

/// The slot a new HELLO gets: the client's own (it reconnects), else a free one, else the one silent longest if it has been
/// silent for five minutes. None when every slot is in use.
fn claim_terminal(index: usize, source: [u8; 6]) -> Option<usize> {
    if let Some(slot) = find_terminal(index, source) {
        return Some(slot);
    }
    let now = apic::ticks();
    let mut terminals = TERMINALS.lock();
    let slot = terminals.iter().position(|terminal| !terminal.open()).or_else(|| {
        let (slot, oldest) = terminals.iter().enumerate().filter(|(_, terminal)| terminal.owner == 0).min_by_key(|(_, terminal)| terminal.activity)?;
        (now.wrapping_sub(oldest.activity) >= STALE_TICKS).then_some(slot)
    })?;
    if !terminals[slot].open() {
        OPEN.fetch_add(1, Ordering::Relaxed);
    }
    terminals[slot] = Terminal { peer: Some(source), nic: index, activity: now, owner: 0, exited: false };
    drop(terminals);
    RELAYS[slot].lock().length = 0;
    shell::detach(slot + 1);
    Some(slot)
}

fn handle_terminal(index: usize, source: [u8; 6], tag: u8, payload: &[u8]) -> bool {
    match tag {
        TAG_HELLO if config::deny_term_net() || !config::term_net_allowed(index) => {}
        TAG_HELLO => {
            let Some(slot) = claim_terminal(index, source) else { return true };
            let session = slot + 1;
            crate::session::with(session, || {
                println!("Remote terminal connected from {} on interface {}.", Mac(&source), index + 1);
                println!("Type `help` for the commands.");
            });
            crate::shellhost::reset_folder(session);
            shell::reset(session);
            crate::session::with(session, shell::prompt);
        }
        TAG_DATA => {
            let Some(slot) = find_terminal(index, source) else { return false };
            // The frame has no length field and Ethernet pads a short frame with zeros: drop the trailing zero bytes.
            let end = payload.iter().rposition(|&byte| byte != 0).map_or(0, |last| last + 1);
            TERMINALS.lock()[slot].activity = apic::ticks();
            for &byte in &payload[..end] {
                shell::feed_session(slot + 1, byte);
            }
        }
        TAG_BYE => {
            let Some(slot) = find_terminal(index, source) else { return false };
            flush();
            close_terminal(slot);
            println!("Remote terminal {} disconnected.", slot + 1);
        }
        _ => return false,
    }
    true
}

/// `session_open` for process `pid`: a free slot becomes a shell session with a prompt waiting. Session number or `SESSION_LIMIT`.
pub fn tape_session_open(pid: u64) -> isize {
    let slot = {
        let mut terminals = TERMINALS.lock();
        let Some(slot) = terminals.iter().position(|terminal| !terminal.open()) else { return -2 };
        terminals[slot] = Terminal { peer: None, nic: 0, activity: apic::ticks(), owner: pid, exited: false };
        OPEN.fetch_add(1, Ordering::Relaxed);
        slot
    };
    RELAYS[slot].lock().length = 0;
    INPUTS[slot].lock().length = 0;
    shell::detach(slot + 1);
    // The shell runs on the service thread only; the greeting is queued there with the next input pass.
    TERMINALS.lock()[slot].activity = 0;
    (slot + 1) as isize
}

fn owned_slot(pid: u64, session: u64) -> Option<usize> {
    let slot = (session as usize).checked_sub(1).filter(|&slot| slot < TERMINAL_SLOTS)?;
    (TERMINALS.lock()[slot].owner == pid).then_some(slot)
}

/// `session_write`: queues typed bytes; how many were taken.
pub fn tape_session_write(pid: u64, session: u64, bytes: &[u8]) -> isize {
    let Some(slot) = owned_slot(pid, session) else { return -4 };
    let mut input = INPUTS[slot].lock();
    let take = bytes.len().min(input.bytes.len() - input.length);
    let start = input.length;
    input.bytes[start..start + take].copy_from_slice(&bytes[..take]);
    input.length += take;
    take as isize
}

/// `session_read`: output waiting, `SESSION_CLOSED` (-3) after `exit` once it is drained.
pub fn tape_session_read(pid: u64, session: u64, buffer: &mut [u8]) -> isize {
    let Some(slot) = owned_slot(pid, session) else { return -4 };
    let take = {
        let mut ring = RELAYS[slot].lock();
        let take = ring.length.min(buffer.len());
        buffer[..take].copy_from_slice(&ring.bytes[..take]);
        let length = ring.length;
        ring.bytes.copy_within(take..length, 0);
        ring.length -= take;
        take
    };
    if take == 0 && TERMINALS.lock()[slot].exited {
        return -3;
    }
    take as isize
}

pub fn tape_session_close(pid: u64, session: u64) -> isize {
    match owned_slot(pid, session) {
        Some(slot) => {
            close_terminal(slot);
            0
        }
        None => -4,
    }
}

/// A process ended: its sessions end with it.
pub fn tape_sessions_ended(pid: u64) {
    for slot in 0..TERMINAL_SLOTS {
        if TERMINALS.lock()[slot].owner == pid {
            close_terminal(slot);
        }
    }
}

/// The shell's `exit`: ends a tape's session (the tape sees it on its next read). False for the console and network clients.
pub fn exit_session(session: usize) -> bool {
    let Some(slot) = session.checked_sub(1).filter(|&slot| slot < TERMINAL_SLOTS) else { return false };
    let mut terminals = TERMINALS.lock();
    if terminals[slot].owner == 0 {
        return false;
    }
    terminals[slot].exited = true;
    true
}

/// Service loop: feeds what tapes typed into their sessions to the shell, and greets a session just opened.
fn feed_tape_sessions() {
    for slot in 0..TERMINAL_SLOTS {
        let (owner, fresh, exited) = {
            let terminals = TERMINALS.lock();
            (terminals[slot].owner, terminals[slot].activity == 0, terminals[slot].exited)
        };
        if owner == 0 || exited {
            continue;
        }
        let session = slot + 1;
        if fresh {
            TERMINALS.lock()[slot].activity = apic::ticks().max(1);
            crate::shellhost::reset_folder(session);
            shell::reset(session);
            crate::session::with(session, || {
                println!("Type `help` for the commands.");
                shell::prompt();
            });
        }
        loop {
            let (byte, more) = {
                let mut input = INPUTS[slot].lock();
                if input.length == 0 {
                    break;
                }
                let byte = input.bytes[0];
                let length = input.length;
                input.bytes.copy_within(1..length, 0);
                input.length -= 1;
                (byte, input.length != 0)
            };
            shell::feed_session(session, byte);
            if !more || TERMINALS.lock()[slot].exited {
                break;
            }
        }
    }
}

// ---- KUP ----

struct Kup {
    active: bool,
    source: [u8; 6],
    expected: usize,
    checksum: u16,
    received: usize,
    next_sequence: u32,
    last_activity: u64,
    /// The staging area: (physical address, frames), allocated for the declared length.
    stage: Option<(u64, u64)>,
}

static mut KUP: Kup = Kup { active: false, source: [0; 6], expected: 0, checksum: 0, received: 0, next_sequence: 0, last_activity: 0, stage: None };
/// Tick at which to reset the machine after an accepted update (0 = none pending).
static mut RESET_AT: u64 = 0;

fn kup() -> &'static mut Kup {
    unsafe { &mut *core::ptr::addr_of_mut!(KUP) }
}

fn release_stage(kup: &mut Kup) {
    if let Some((address, frames)) = kup.stage.take() {
        pmm::free(address, frames);
    }
}

fn stage_bytes(kup: &Kup) -> Option<&'static mut [u8]> {
    let (address, frames) = kup.stage?;
    Some(unsafe { core::slice::from_raw_parts_mut(vm::direct(address) as *mut u8, (frames * pmm::FRAME) as usize) })
}

fn kup_reply(index: usize, destination: &[u8; 6], tag: u8, payload: &[u8]) {
    send(index, destination, tag, payload);
}

fn kup_request(index: usize, source: [u8; 6], payload: &[u8]) {
    let kup = kup();
    if kup.active && kup.source != source && apic::ticks().wrapping_sub(kup.last_activity) < KUP_STALE_TICKS {
        return kup_reply(index, &source, TAG_KUP_ERROR, &[KUP_BUSY]);
    }
    if payload.len() < 6 {
        return;
    }
    release_stage(kup);
    let expected = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    let checksum = u16::from_le_bytes(payload[4..6].try_into().unwrap());
    let sectors = expected.div_ceil(512) as u64;
    let frames = (sectors * 512).div_ceil(pmm::FRAME);
    let stage = if expected == 0 || sectors > fosfor_abi::SLOT_SECTORS { None } else { pmm::allocate(frames, None) };
    let Some(stage) = stage else {
        kup.active = false;
        return kup_reply(index, &source, TAG_KUP_ERROR, &[KUP_TOO_LARGE]);
    };
    *kup = Kup { active: true, source, expected, checksum, received: 0, next_sequence: 0, last_activity: apic::ticks(), stage: Some((stage, frames)) };
    println!("KUP: receiving a kernel image of {expected} bytes from {}.", Mac(&source));
}

fn kup_data(source: [u8; 6], payload: &[u8]) {
    let kup = kup();
    if !kup.active || kup.source != source || payload.len() < 4 {
        return;
    }
    let sequence = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    if sequence != kup.next_sequence {
        // A gap or a reorder abandons the transfer; there is no retransmission.
        kup.active = false;
        release_stage(kup);
        return println!("KUP: chunk {sequence} arrived where {} was expected; transfer abandoned.", kup.next_sequence);
    }
    let Some(staging) = stage_bytes(kup) else { return };
    // The chunk's size comes from the declared length, never from the (padded) frame.
    let take = (payload.len() - 4).min(1024).min(kup.expected - kup.received);
    staging[kup.received..kup.received + take].copy_from_slice(&payload[4..4 + take]);
    kup.received += take;
    kup.next_sequence += 1;
    kup.last_activity = apic::ticks();
}

fn kup_stop(index: usize, source: [u8; 6]) {
    let kup = kup();
    if !kup.active || kup.source != source {
        return;
    }
    kup.active = false;
    let Some(staging) = stage_bytes(kup) else { return };
    let result = kup_finish(kup, staging);
    release_stage(kup);
    match result {
        Ok(slot) => {
            println!("KUP: image verified and written to slot {slot} (pending); restarting in about a second to try it.");
            kup_reply(index, &source, TAG_KUP_ACK, &[]);
            flush();
            unsafe { RESET_AT = apic::ticks() + TICK_HZ };
        }
        Err(code) => kup_reply(index, &source, TAG_KUP_ERROR, &[code]),
    }
}

/// Verifies the staged image and writes it to the inactive slot; returns the slot, or the KUP error code.
fn kup_finish(kup: &Kup, staging: &mut [u8]) -> Result<u8, u8> {
    if kup.received != kup.expected {
        println!("KUP: transfer ended after {} of {} bytes; abandoned.", kup.received, kup.expected);
        return Err(KUP_DISK_ERROR);
    }
    // Verify before writing anything: a mismatch leaves the running kernel and the slot metadata completely untouched.
    if fosfor_abi::internet_checksum(&staging[..kup.expected]) != kup.checksum {
        println!("KUP: checksum mismatch; image rejected.");
        return Err(KUP_CHECKSUM_MISMATCH);
    }
    // The receive path cannot sleep, so a busy disk (a tape or NETFS transfer) fails this attempt; the host retries.
    let Some(_disk_lock) = crate::tapefs::try_lock() else {
        println!("KUP: the disk is busy; try again.");
        return Err(KUP_DISK_ERROR);
    };
    let (Some(metadata), Some((disk, payload))) = (slots::read_metadata(), slots::payload()) else { return Err(KUP_DISK_ERROR) };
    let slot = 1 - (metadata.active_slot & 1);
    let sectors = kup.expected.div_ceil(512);
    staging[kup.expected..sectors * 512].fill(0);
    if crate::block::write(disk, payload + fosfor_abi::slot_start(u64::from(slot)), &staging[..sectors * 512]).is_err() {
        println!("KUP: writing slot {slot} failed.");
        return Err(KUP_DISK_ERROR);
    }
    let updated = slots::update_metadata(|m| {
        m.pending_slot = Some(slot);
        m.attempted = false;
        m.confirmed = false;
        m.slot_sectors[usize::from(slot)] = sectors as u16;
    });
    updated.map(|_| slot).ok_or(KUP_DISK_ERROR)
}

// ---- the receive loop ----

/// Handles one received frame if it is the terminal's, a discovery probe's or KUP's business.
/// The frame handler `nicctl` calls for every received frame: true for the kernel's own (ethertype 0x88b6), which never reach a tape.
fn frame_hook(index: usize, frame: &[u8]) -> bool {
    if frame.len() < 15 || frame[12..14] != ETHERTYPE {
        return false;
    }
    handle_frame(index, frame);
    true
}

fn handle_frame(index: usize, frame: &[u8]) {
    let source: [u8; 6] = frame[6..12].try_into().unwrap();
    let (tag, payload) = (frame[14], &frame[15..]);
    if crate::netclient::capture(index, source, tag, payload) {
        return;
    }
    match tag {
        TAG_DISCOVER_REQUEST => {
            let mut name = [0u8; 20];
            let length = hostname(index, &mut name);
            send(index, &source, TAG_DISCOVER_REPLY, &name[..length]);
        }
        TAG_KUP_REQUEST if config::kernel_update() => kup_request(index, source, payload),
        TAG_KUP_DATA if config::kernel_update() => kup_data(source, payload),
        TAG_KUP_STOP if config::kernel_update() => kup_stop(index, source),
        tag if netfs::owns(tag) => netfs::handle(index, source, tag, payload),
        _ => {
            handle_terminal(index, source, tag, payload);
        }
    }
}

/// Handles whatever the NICs have received, announces the name and sends queued console output. Call from the service loop.
pub fn poll() {
    {
        let _nics = lock_nics();
        nicctl::drain_nics_to_fifo();
    }
    netfs::serve();
    announce();
    if session_open() {
        feed_tape_sessions();
        flush();
    }
}

/// Service-loop work: restart after an accepted kernel update.
pub fn housekeeping() {
    unsafe {
        if RESET_AT != 0 && apic::ticks() >= RESET_AT {
            RESET_AT = 0;
            slots::mark_clean_stop();
            power::reset();
        }
    }
}
