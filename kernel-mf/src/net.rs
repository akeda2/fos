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
//! Not served yet: NETFS (file GET/PUT), tape sync and GPIO (`DEFERRED.md`). Everything runs on the boot CPU.

use core::sync::atomic::{AtomicBool, Ordering};

use kernel_core::nic::{self, Nic};
use kernel_core::{e1000, netdump::Mac, pci, rtl8139, rtl8168, threecom};

use crate::apic::{self, TICK_HZ};
use crate::sync::SpinLock;
use crate::{config, pmm, power, shell, slots, vm};

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

/// Starts every NIC the drivers know and puts it in the table. Needs interrupts on (the drivers prove their interrupt path).
pub fn init() {
    let mut interfaces = 0;
    for device in pci::devices().filter(|d| d.vendor == e1000::VENDOR_INTEL && e1000::DEVICE_IDS.contains(&d.id)) {
        match e1000::E1000::init(&device, interfaces) {
            Ok(mut nic) => {
                println!("e1000 ({:?}) at {:02x}:{:02x}.{}: MAC {}, link {}.", nic.variant, device.bus, device.device, device.function, Mac(&nic.mac), if nic.link_up() { "up" } else { "down" });
                report(nic.enable_interrupts(&device, false));
                if nic::install(Nic::E1000(nic)).is_some() {
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

// ---- frames out ----

pub fn mac(index: usize) -> Option<[u8; 6]> {
    nic::at(index).map(|nic| nic.mac())
}

pub fn link_up(index: usize) -> bool {
    nic::at(index).is_some_and(|nic| nic.link_up())
}

/// Builds and sends one fos frame (ethertype 0x88b6, tag, payload) out of interface `index`.
fn send(index: usize, destination: &[u8; 6], tag: u8, payload: &[u8]) -> bool {
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

/// The name announced for interface `index`: `NAME<n>` from the `hostname` file (n is the one-based interface number), or
/// without that file `fos-` and the last three bytes of the interface's MAC address.
fn hostname(index: usize, out: &mut [u8; 20]) -> usize {
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
        if link_up(index) {
            let mut name = [0u8; 20];
            let length = hostname(index, &mut name);
            send(index, &[0xff; 6], TAG_DISCOVER_REPLY, &name[..length]);
        }
    }
}

// ---- the terminal session ----

static mut PEER: Option<[u8; 6]> = None;
/// The interface the open session's HELLO arrived on: console output goes out of it.
static mut SESSION_NIC: usize = 0;
static mut PEER_ACTIVITY: u64 = 0;
/// Console output waiting for the client; `println!` fills it from any CPU, `flush` empties it on the boot CPU.
struct Relay {
    bytes: [u8; 16384],
    length: usize,
}
static RELAY: SpinLock<Relay> = SpinLock::new(Relay { bytes: [0; 16384], length: 0 });
static SESSION_OPEN: AtomicBool = AtomicBool::new(false);

/// True while a remote terminal session is open.
pub fn session_open() -> bool {
    SESSION_OPEN.load(Ordering::Relaxed)
}

/// Called by the console for every piece of text; queues it for the client. Never transmits: output that does not fit is
/// dropped (a terminal can tolerate a torn screen; a stuck NIC must not stall the kernel).
pub fn relay(text: &str) {
    if !session_open() {
        return;
    }
    let mut ring = RELAY.lock();
    let take = text.len().min(ring.bytes.len() - ring.length);
    let start = ring.length;
    ring.bytes[start..start + take].copy_from_slice(&text.as_bytes()[..take]);
    ring.length += take;
}

/// Sends everything queued as DATA frames.
fn flush() {
    let Some(peer) = (unsafe { *core::ptr::addr_of!(PEER) }) else { return };
    loop {
        let mut chunk = [0u8; DATA_CAPACITY];
        let take = {
            let mut ring = RELAY.lock();
            let take = ring.length.min(DATA_CAPACITY);
            chunk[..take].copy_from_slice(&ring.bytes[..take]);
            let length = ring.length;
            ring.bytes.copy_within(take..length, 0);
            ring.length -= take;
            take
        };
        if take == 0 {
            return;
        }
        send(unsafe { SESSION_NIC }, &peer, TAG_DATA, &chunk[..take]);
    }
}

fn close_session() {
    unsafe { *core::ptr::addr_of_mut!(PEER) = None };
    SESSION_OPEN.store(false, Ordering::Relaxed);
    RELAY.lock().length = 0;
}

fn handle_terminal(index: usize, source: [u8; 6], tag: u8, payload: &[u8]) -> bool {
    let peer = unsafe { *core::ptr::addr_of!(PEER) };
    match tag {
        TAG_HELLO if config::deny_term_net() => {}
        // A session is open: another client's HELLO does not steal it, unless the owner has been silent for five minutes.
        TAG_HELLO if peer.is_some() && peer != Some(source) && apic::ticks().wrapping_sub(unsafe { PEER_ACTIVITY }) < STALE_TICKS => {}
        TAG_HELLO => {
            unsafe {
                *core::ptr::addr_of_mut!(PEER) = Some(source);
                SESSION_NIC = index;
                PEER_ACTIVITY = apic::ticks();
            }
            RELAY.lock().length = 0;
            SESSION_OPEN.store(true, Ordering::Relaxed);
            println!("Remote terminal connected from {} on interface {}.", Mac(&source), index + 1);
            println!("Type `help` for the commands.");
            shell::reset();
            shell::prompt();
        }
        TAG_DATA if peer == Some(source) && unsafe { SESSION_NIC } == index => {
            // The frame has no length field and Ethernet pads a short frame with zeros: drop the trailing zero bytes.
            let end = payload.iter().rposition(|&byte| byte != 0).map_or(0, |last| last + 1);
            unsafe { PEER_ACTIVITY = apic::ticks() };
            for &byte in &payload[..end] {
                shell::feed(byte);
            }
        }
        TAG_BYE if peer == Some(source) => {
            flush();
            close_session();
            println!("Remote terminal disconnected.");
        }
        _ => return false,
    }
    true
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
fn handle_frame(index: usize, frame: &[u8]) {
    if frame.len() < 15 || frame[12..14] != ETHERTYPE {
        return;
    }
    let source: [u8; 6] = frame[6..12].try_into().unwrap();
    let (tag, payload) = (frame[14], &frame[15..]);
    match tag {
        TAG_DISCOVER_REQUEST => {
            let mut name = [0u8; 20];
            let length = hostname(index, &mut name);
            send(index, &source, TAG_DISCOVER_REPLY, &name[..length]);
        }
        TAG_KUP_REQUEST if config::kernel_update() => kup_request(index, source, payload),
        TAG_KUP_DATA if config::kernel_update() => kup_data(source, payload),
        TAG_KUP_STOP if config::kernel_update() => kup_stop(index, source),
        _ => {
            handle_terminal(index, source, tag, payload);
        }
    }
}

/// Handles whatever the NICs have received, announces the name and sends queued console output. Call from the service loop.
pub fn poll() {
    let mut buffer = [0u8; 1600];
    for index in 0..nic::count() {
        for _ in 0..256 {
            let Some(length) = nic::at(index).and_then(|nic| nic.receive(&mut buffer)) else { break };
            handle_frame(index, &buffer[..length]);
        }
    }
    announce();
    if session_open() {
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
