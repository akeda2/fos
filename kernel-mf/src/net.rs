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

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kernel_core::nic::{self, Nic};
use kernel_core::nicctl;
use kernel_core::{e1000, netdump::Mac, pci, rtl8139, rtl8168, threecom, virtio_net};

use crate::time::{self, TICK_HZ};
use crate::sync::SpinLock;
use crate::{config, netfs, pmm, power, shell, slots, vm};

const ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const TAG_DISCOVER_REQUEST: u8 = 0x02;
const TAG_DISCOVER_REPLY: u8 = 0x03;
const TAG_HELLO: u8 = 0x20;
const TAG_DATA: u8 = 0x21;
const TAG_BYE: u8 = 0x22;
/// Keep-alive from a client (no payload): it says the client is still there while a tape waits for input.
const TAG_PING: u8 = 0x23;
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
/// A client that has sent PINGs and then stays silent this long is gone: its session ends and its foreground tape is killed.
/// `term_net.py` pings every 10 s; a client that never pings (older ones) is never expired this way.
const PING_TIMEOUT_TICKS: u64 = 30 * TICK_HZ;
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
                if let Some(ip) = crate::tapenet::address(index) {
                    println!("   address {}.{}.{}.{} (seen in frames the tape sent)", ip[0], ip[1], ip[2], ip[3]);
                }
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
        if let Some(ip) = crate::tapenet::address(index) {
            println!("   address {}.{}.{}.{} (seen in frames the tape sent)", ip[0], ip[1], ip[2], ip[3]);
        }
        let (received, sent, dropped) = nic.counters();
        println!("   frames: {received} received, {sent} sent{}", if dropped != 0 { " (some dropped: no link)" } else { "" });
        nic.describe();
    }
    if nic::count() == 0 {
        println!("No network interface.");
    }
}

/// `nic regs N`: the chip's registers and ring state, for a link or interrupt that misbehaves.
pub fn print_registers(number: usize) {
    let _nics = lock_nics();
    match number.checked_sub(1).and_then(nic::at) {
        Some(nic) if !nic::is_virtual(number - 1) => nic.dump_registers(),
        _ => println!("nic regs: no physical interface {number}."),
    }
}

/// Starts every NIC the drivers know and puts it in the table. Needs interrupts on (the drivers prove their interrupt path).
pub fn init() {
    nicctl::set_frame_handler(frame_hook);
    nicctl::set_frame_allocator(|bytes| match pmm::allocate((bytes as u64).div_ceil(pmm::FRAME), None) {
        Some(address) => vm::direct(address) as *mut u8,
        None => core::ptr::null_mut(),
    });
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
    for device in pci::devices().filter(|d| d.vendor == virtio_net::VENDOR_VIRTIO && virtio_net::DEVICE_IDS.contains(&d.id)) {
        match virtio_net::VirtioNet::init(&device, interfaces) {
            Ok(nic) => {
                println!("virtio-net at {:02x}:{:02x}.{}: MAC {}, link {}.", device.bus, device.device, device.function, Mac(&nic.mac), if nic.link_up() { "up" } else { "down" });
                if nic::install(Nic::Virtio(nic)).is_some() {
                    locate(interfaces, &device);
                    interfaces += 1;
                }
            }
            Err(error) => println!("virtio-net at {:02x}:{:02x}.{}: initialisation failed ({error:?}).", device.bus, device.device, device.function),
        }
    }
    #[cfg(target_arch = "aarch64")]
    if crate::arch::genet::init() {
        if nic::install(Nic::Platform(&crate::arch::GenetNic)).is_some() {
            interfaces += 1;
        }
    }
    if interfaces == 0 {
        println!("No supported network interface found (e1000, e1000e, I350, RTL8139, RTL8168, 3c905 and virtio-net are driven).");
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
    kup.active && time::ticks().wrapping_sub(kup.last_activity) < KUP_STALE_TICKS
}

/// The name announced for interface `index`: `NAMEvnic` on a virtual interface, else `NAME<n>` from the `hostname` file (n is the one-based interface number), or
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
    // A virtual interface is named after its vnic (`rack` + `web` = `rackweb`): its NIC number can change between boots.
    let own = if nic::virtual_parent(index).is_some() { crate::vnic::name_of(index) } else { None };
    let _ = match (config::hostname(), own, mac(index)) {
        (base, Some(own), _) => write!(cursor, "{}{own}", base.as_deref().unwrap_or("fos-")),
        (Some(base), None, _) => write!(cursor, "{base}{}", index + 1),
        (None, None, Some(mac)) => write!(cursor, "fos-{:02x}{:02x}{:02x}", mac[3], mac[4], mac[5]),
        (None, None, None) => write!(cursor, "fos"),
    };
    cursor.1
}

static mut ANNOUNCE_COUNT: u32 = 0;
static mut NEXT_ANNOUNCE: u64 = 0;

/// Broadcasts this machine's name on every interface that has a link: four times at one second, then every 10 s.
fn announce() {
    unsafe {
        if time::ticks() < NEXT_ANNOUNCE {
            return;
        }
        ANNOUNCE_COUNT = ANNOUNCE_COUNT.saturating_add(1);
        NEXT_ANNOUNCE = time::ticks() + if ANNOUNCE_COUNT <= 4 { 1 } else { 10 } * TICK_HZ;
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
    /// The client has sent a PING, so silence means it is gone.
    pings: bool,
    /// A client's HELLO or BYE waiting for the service loop (`feed_sessions`): the frame handler runs under the NIC lock with
    /// interrupts off, too long a time to print a greeting or flush the last output there (12 ms and 4.5 ms measured).
    greet: bool,
    bye: bool,
}
impl Terminal {
    const FREE: Terminal = Terminal { peer: None, nic: 0, activity: 0, owner: 0, exited: false, pings: false, greet: false, bye: false };
    fn open(&self) -> bool {
        self.peer.is_some() || self.owner != 0
    }
}
const TERMINAL_SLOTS: usize = crate::session::MAX - 1;
static TERMINALS: SpinLock<[Terminal; TERMINAL_SLOTS]> = SpinLock::new([Terminal::FREE; TERMINAL_SLOTS]);
/// Bytes typed into a session, by a tape or a network terminal, fed to the shell by the service loop (the shell runs on that
/// thread only).
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
    end_foreground_tape(slot);
}

/// The session in `slot` is over: its foreground tape (an `ed` waiting for keys, say) would never get another key, so it is
/// asked to stop (and killed if it does not, `shell::request_stop`) rather than left running with its files.
fn end_foreground_tape(slot: usize) {
    let pid = shell::foreground(slot + 1);
    shell::detach(slot + 1);
    if pid != 0 {
        println!("Session {} ended; stopping its tape (process {pid}).", slot + 1);
        shell::request_stop(pid);
    }
}

/// Service loop: closes the sessions of clients that pinged and then went silent.
fn expire_silent_clients() {
    let now = time::ticks();
    for slot in 0..TERMINAL_SLOTS {
        let gone = {
            let terminals = TERMINALS.lock();
            let terminal = &terminals[slot];
            terminal.owner == 0 && terminal.peer.is_some() && terminal.pings && now.wrapping_sub(terminal.activity) >= PING_TIMEOUT_TICKS
        };
        if gone {
            close_terminal(slot);
            println!("Remote terminal {} timed out.", slot + 1);
        }
    }
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
    let now = time::ticks();
    let mut terminals = TERMINALS.lock();
    let slot = terminals.iter().position(|terminal| !terminal.open()).or_else(|| {
        let (slot, oldest) = terminals.iter().enumerate().filter(|(_, terminal)| terminal.owner == 0).min_by_key(|(_, terminal)| terminal.activity)?;
        (now.wrapping_sub(oldest.activity) >= STALE_TICKS).then_some(slot)
    })?;
    if !terminals[slot].open() {
        OPEN.fetch_add(1, Ordering::Relaxed);
    }
    terminals[slot] = Terminal { peer: Some(source), nic: index, activity: now, owner: 0, exited: false, pings: false, greet: false,
        bye: false };
    drop(terminals);
    RELAYS[slot].lock().length = 0;
    end_foreground_tape(slot);
    Some(slot)
}

fn handle_terminal(index: usize, source: [u8; 6], tag: u8, payload: &[u8]) -> bool {
    match tag {
        TAG_HELLO if config::deny_term_net() || !config::term_net_allowed(index) => {}
        TAG_HELLO => {
            let Some(slot) = claim_terminal(index, source) else { return true };
            INPUTS[slot].lock().length = 0;
            let mut terminals = TERMINALS.lock();
            terminals[slot].greet = true;
            terminals[slot].bye = false;
        }
        TAG_DATA => {
            let Some(slot) = find_terminal(index, source) else { return false };
            // The frame has no length field and Ethernet pads a short frame with zeros: drop the trailing zero bytes.
            let end = payload.iter().rposition(|&byte| byte != 0).map_or(0, |last| last + 1);
            TERMINALS.lock()[slot].activity = time::ticks();
            // Queued, not fed: this runs under the NIC lock with interrupts off, and a typed command would run inside it
            // (an `audio` listing held off the xHCI interrupt for 0.2 s). `feed_sessions` hands the bytes to the shell.
            let mut input = INPUTS[slot].lock();
            let take = end.min(input.bytes.len() - input.length);
            let start = input.length;
            input.bytes[start..start + take].copy_from_slice(&payload[..take]);
            input.length += take;
        }
        TAG_PING => {
            let Some(slot) = find_terminal(index, source) else { return false };
            let mut terminals = TERMINALS.lock();
            terminals[slot].activity = time::ticks();
            terminals[slot].pings = true;
        }
        TAG_BYE => {
            let Some(slot) = find_terminal(index, source) else { return false };
            TERMINALS.lock()[slot].bye = true;
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
        terminals[slot] = Terminal { peer: None, nic: 0, activity: time::ticks(), owner: pid, exited: false, pings: false, greet: false,
            bye: false };
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

/// Service loop: greets a session just opened, feeds what tapes and network terminals typed into their sessions to the shell,
/// and closes a terminal whose client said goodbye.
fn feed_sessions() {
    for slot in 0..TERMINAL_SLOTS {
        let (owner, peer, nic, fresh, exited, greet) = {
            let mut terminals = TERMINALS.lock();
            let terminal = &mut terminals[slot];
            let greet = core::mem::take(&mut terminal.greet);
            (terminal.owner, terminal.peer, terminal.nic, terminal.activity == 0, terminal.exited, greet)
        };
        if (owner == 0 && peer.is_none()) || exited {
            continue;
        }
        let session = slot + 1;
        if let (true, Some(source)) = (greet, peer) {
            crate::session::with(session, || {
                println!("Remote terminal connected from {} on interface {}.", Mac(&source), nic + 1);
                println!("Type `help` for the commands.");
            });
            crate::shellhost::reset_folder(session);
            shell::reset(session);
            crate::session::with(session, shell::prompt);
        }
        if owner != 0 && fresh {
            TERMINALS.lock()[slot].activity = time::ticks().max(1);
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
        if owner == 0 && TERMINALS.lock()[slot].bye {
            flush();
            close_terminal(slot);
            println!("Remote terminal {session} disconnected.");
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
    if kup.active && kup.source != source && time::ticks().wrapping_sub(kup.last_activity) < KUP_STALE_TICKS {
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
    *kup = Kup { active: true, source, expected, checksum, received: 0, next_sequence: 0, last_activity: time::ticks(), stage: Some((stage, frames)) };
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
    kup.last_activity = time::ticks();
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
            unsafe { RESET_AT = time::ticks() + TICK_HZ };
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
    // ATA over Ethernet, while a disk is exported: queued for the `aoe` thread (never a disk read here, the NIC lock is held).
    if frame.len() >= 24 && frame[12..14] == aoe::ETHERTYPE.to_be_bytes() && crate::aoetarget::wanted() {
        crate::aoetarget::enqueue(index, frame, RESCUER.load(Ordering::Acquire) == 0);
        return true;
    }
    if frame.len() < 15 || frame[12..14] != ETHERTYPE {
        return false;
    }
    if RESCUER.load(Ordering::Acquire) == crate::percpu::this().index + 1 {
        // The SysRq rescue drains from an interrupt: only a SysRq is answered there, the rest waits for the service thread.
        if frame[14] == crate::sysrq::TAG_SYSRQ {
            let mut request = RESCUE_REQUEST.lock();
            *request = Some((index, frame[6..12].try_into().unwrap()));
        } else if let Some(mut deferred) = DEFERRED.try_lock() {
            deferred.push(index, frame);
        }
        return true;
    }
    handle_frame(index, frame);
    true
}

// ---- the SysRq rescue ----

/// The CPU (index + 1) draining the NICs for the SysRq rescue (`sysrq::tick`), 0 for none.
static RESCUER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static RESCUE_REQUEST: SpinLock<Option<(usize, [u8; 6])>> = SpinLock::new(None);

/// The kernel's own frames the rescue took off the NICs, kept for the service thread (`poll` replays them first).
struct Deferred {
    frames: [[u8; 1514]; 8],
    lengths: [usize; 8],
    nics: [usize; 8],
    count: usize,
}

impl Deferred {
    fn push(&mut self, index: usize, frame: &[u8]) {
        if self.count < self.frames.len() && frame.len() <= 1514 {
            self.frames[self.count][..frame.len()].copy_from_slice(frame);
            self.lengths[self.count] = frame.len();
            self.nics[self.count] = index;
            self.count += 1;
        }
    }
}

static DEFERRED: SpinLock<Deferred> = SpinLock::new(Deferred { frames: [[0; 1514]; 8], lengths: [0; 8], nics: [0; 8], count: 0 });

/// For `sysrq::tick` (a timer interrupt, the service thread silent): drains every NIC as the service thread would, but only
/// a SysRq request is acted on; `answer(nic, requester)` runs with the NIC lock held, so it may send. Waits for no lock:
/// false when another CPU holds the NICs.
pub fn rescue_drain(answer: impl FnOnce(usize, [u8; 6])) -> bool {
    let Some(guard) = NICS.try_lock() else { return false };
    let me = crate::percpu::this().index + 1;
    NICS_OWNER.store(me, Ordering::Release);
    RESCUER.store(me, Ordering::Release);
    nicctl::drain_nics_to_fifo();
    RESCUER.store(0, Ordering::Release);
    let request = RESCUE_REQUEST.lock().take();
    if let Some((index, source)) = request {
        answer(index, source);
    }
    NICS_OWNER.store(0, Ordering::Release);
    drop(guard);
    true
}

/// Sends `text` to `destination` as `term net` DATA frames (the SysRq answer). The NIC lock is re-entrant, so this works
/// inside `rescue_drain` too.
pub fn send_text(index: usize, destination: &[u8; 6], text: &[u8]) {
    for chunk in text.chunks(DATA_CAPACITY) {
        send(index, destination, TAG_DATA, chunk);
    }
}

fn replay_deferred() {
    loop {
        let mut frame = [0u8; 1514];
        let (index, length) = {
            let mut deferred = DEFERRED.lock();
            if deferred.count == 0 {
                return;
            }
            let (index, length) = (deferred.nics[0], deferred.lengths[0]);
            frame[..length].copy_from_slice(&deferred.frames[0][..length]);
            let count = deferred.count;
            deferred.frames.copy_within(1..count, 0);
            deferred.lengths.copy_within(1..count, 0);
            deferred.nics.copy_within(1..count, 0);
            deferred.count -= 1;
            (index, length)
        };
        handle_frame(index, &frame[..length]);
    }
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
        crate::sysrq::TAG_SYSRQ => crate::sysrq::request(index, source),
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
    replay_deferred();
    {
        let _nics = lock_nics();
        nicctl::drain_nics_to_fifo();
    }
    netfs::serve();
    announce();
    expire_silent_clients();
    if session_open() {
        feed_sessions();
        flush();
    }
}

// ---- the receive thread ----

/// Set by a NIC's interrupt handler (`rx_event`), cleared by `rx_thread` before it drains.
static RX_PENDING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
const RX_KEY: usize = 0x6e65_7478;

/// What the receive thread did, for `aoe` (and anyone measuring the receive path): interrupts seen, times it woke, and the
/// time from the first interrupt of a round to the thread running (summed, and the largest), in TSC cycles (`aoe` converts).
pub struct RxStats {
    pub events: AtomicU64,
    pub wakes: AtomicU64,
    pub latency_us: AtomicU64,
    pub latency_max_us: AtomicU64,
}

pub static RX_STATS: RxStats = RxStats { events: AtomicU64::new(0), wakes: AtomicU64::new(0), latency_us: AtomicU64::new(0), latency_max_us: AtomicU64::new(0) };
/// When the first interrupt since the thread last woke arrived (TSC cycles), 0 for none.
static RX_EVENT_AT: AtomicU64 = AtomicU64::new(0);
/// Set while `rx_thread` drains (under the NIC lock, so whoever else drains sees it clear).
static RX_DRAINING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether the frame being handled was drained by the receive thread (true) or the service thread. Valid under the NIC lock.
pub fn draining_from_rx_thread() -> bool {
    RX_DRAINING.load(Ordering::Relaxed)
}

pub fn reset_rx_stats() {
    for counter in [&RX_STATS.events, &RX_STATS.wakes, &RX_STATS.latency_us, &RX_STATS.latency_max_us] {
        counter.store(0, Ordering::Relaxed);
    }
}

/// A NIC interrupted (`Platform::net_event`, from its interrupt handler): wake the receive thread. `wake_all` takes only the
/// scheduler's lock, which every holder takes with interrupts off, so an interrupt handler may call it.
pub fn rx_event() {
    RX_STATS.events.fetch_add(1, Ordering::Relaxed);
    let _ = RX_EVENT_AT.compare_exchange(0, crate::cpu::cycles().max(1), Ordering::AcqRel, Ordering::Relaxed);
    RX_PENDING.store(true, Ordering::Release);
    crate::sched::wake_all(RX_KEY);
}

/// Drains the NICs when one of them interrupts, so frames are handled while the service thread is busy with a shell command
/// (a long `fsck`, a screenful of output) or anything else that keeps it away from `poll`: KUP, the network terminal, the
/// tapes' receive queues and the AoE target would all stand still behind it. The service thread keeps draining too: a NIC
/// without interrupts (polled) is still only served there. The NIC lock is held only while draining, never while waiting,
/// because the SysRq rescue drains with a `try_lock` and must find it free (a thread that spun on the NICs starved it).
pub extern "C" fn rx_thread(_: usize) {
    // NIC interrupts are on from `net::init`, a second or more before this thread is spawned (the self-test runs in
    // between): an event stamped then is no wake-up latency, so it is dropped. `RX_PENDING` stays set, so frames that
    // arrived meanwhile are still drained in the first round.
    RX_EVENT_AT.store(0, Ordering::Release);
    loop {
        crate::sched::wait_while(RX_KEY, || !RX_PENDING.load(Ordering::Acquire));
        // Cleared first: an interrupt that arrives while draining sets it again and the next round picks that frame up.
        RX_PENDING.store(false, Ordering::Release);
        RX_STATS.wakes.fetch_add(1, Ordering::Relaxed);
        let at = RX_EVENT_AT.swap(0, Ordering::AcqRel);
        if at != 0 {
            let waited = crate::cpu::cycles().saturating_sub(at);
            RX_STATS.latency_us.fetch_add(waited, Ordering::Relaxed);
            RX_STATS.latency_max_us.fetch_max(waited, Ordering::Relaxed);
        }
        let _nics = lock_nics();
        RX_DRAINING.store(true, Ordering::Relaxed);
        nicctl::drain_nics_to_fifo();
        RX_DRAINING.store(false, Ordering::Relaxed);
    }
}

/// Service-loop work: restart after an accepted kernel update.
pub fn housekeeping() {
    unsafe {
        if RESET_AT != 0 && time::ticks() >= RESET_AT {
            RESET_AT = 0;
            slots::mark_clean_stop();
            power::reset();
        }
    }
}

/// The debug dump's session list; never waits for a lock.
pub fn write_sessions(out: &mut dyn core::fmt::Write) {
    let Some(terminals) = TERMINALS.try_lock() else {
        let _ = writeln!(out, "sessions: their lock is held");
        return;
    };
    let mut any = false;
    for (slot, terminal) in terminals.iter().enumerate().filter(|(_, t)| t.open()) {
        any = true;
        let _ = write!(out, "  session {}", slot + 1);
        match terminal.peer {
            Some(peer) => {
                let _ = write!(out, " nic{} {}", terminal.nic, Mac(&peer));
            }
            None => {
                let _ = write!(out, " tape pid {}", terminal.owner);
            }
        }
        let _ = writeln!(out, ", idle {} ms", time::ticks().saturating_sub(terminal.activity) * 1000 / TICK_HZ);
    }
    if !any {
        let _ = writeln!(out, "  no terminal sessions");
    }
}
