//! The server kernel as a NETFS client: the peer table (`nic peers`), discovery, the GET receive loop and the `tapesync` command.
//! The command itself is `kernel_core::tapesync_client`, shared with the embedded kernel; `Mf` below is what it needs from here.
//!
//! The receive side runs on whatever CPU typed the command, usually inside the NIC drain of a `term net` session, with the NIC
//! lock held and interrupts possibly off. So it polls: it drains the NICs itself and times out on the TSC clock
//! (`apic::micros`), never on timer ticks. `net::handle_frame` offers every frame to `capture` first; while a transfer is
//! running, the GET answers of the target machine end up here instead of in the NETFS request queue, and terminal frames are
//! dropped (a keystroke arriving mid-transfer must not reach the shell; ESC aborts).

use kernel_core::fosfs::Volume;
use kernel_core::nic;
use kernel_core::nicctl;
use kernel_core::tapesync_client::{self as client, Env};
use kernel_core::volumes;

use crate::netfs::{self, TAG_GPIO_REPLY, TAG_GPIO_REQUEST, TAG_GET_DATA, TAG_GET_ERROR, TAG_GET_REQUEST, TAG_GET_START, TAG_GET_STOP};
use crate::{apic, net, tapefs};

const TAG_DISCOVER_REQUEST: u8 = 0x02;
const TAG_DISCOVER_REPLY: u8 = 0x03;
const TAG_HELLO: u8 = 0x20;
const TAG_DATA: u8 = 0x21;
const MAX_PEERS: usize = 32;
const CHUNK: usize = 1024;
/// The transfer buffer: the largest file a transfer carries.
const STAGING: usize = 8 << 20;
const SILENCE_US: u64 = 3_000_000;

// ---- the peer table ----

/// Who has announced itself or answered a probe: (MAC, name, name length, interface).
static mut PEERS: [([u8; 6], [u8; 20], usize, usize); MAX_PEERS] = [([0; 6], [0; 20], 0, 0); MAX_PEERS];
static mut PEER_COUNT: usize = 0;

/// Files a peer's name (a discovery reply or the periodic announce). This machine is not its own peer: some virtual
/// networks echo our frames back.
fn remember(index: usize, mac: [u8; 6], name: &[u8]) {
    if (0..nic::count()).any(|own| net::mac(own) == Some(mac)) {
        return;
    }
    let name = &name[..name.iter().position(|&byte| byte == 0).unwrap_or(name.len())];
    let length = name.len().min(20);
    unsafe {
        let peers = &mut *core::ptr::addr_of_mut!(PEERS);
        let count = PEER_COUNT;
        if let Some(entry) = peers[..count].iter_mut().find(|entry| entry.0 == mac) {
            if length != 0 {
                entry.1 = [0; 20];
                entry.1[..length].copy_from_slice(&name[..length]);
                entry.2 = length;
            }
            entry.3 = index;
        } else if count < MAX_PEERS {
            peers[count] = (mac, [0; 20], length, index);
            peers[count].1[..length].copy_from_slice(&name[..length]);
            PEER_COUNT = count + 1;
        }
    }
}

fn peer_at(number: usize) -> Option<([u8; 6], [u8; 20], usize, usize)> {
    (number >= 1 && number <= unsafe { PEER_COUNT }).then(|| unsafe { (*core::ptr::addr_of!(PEERS))[number - 1] })
}

/// `nic peers`.
pub fn print_peers() {
    let count = unsafe { PEER_COUNT };
    if count == 0 {
        return println!("No peers known yet (`nic discover` looks for them).");
    }
    for number in 1..=count {
        let (mac, name, length, index) = peer_at(number).unwrap();
        println!("{:>2}: {}  {}  (via NIC {})", number, kernel_core::netdump::Mac(&mac), core::str::from_utf8(&name[..length]).unwrap_or("?"), index + 1);
    }
}

fn lookup(host: &str) -> Option<([u8; 6], usize)> {
    let count = unsafe { PEER_COUNT };
    if let Ok(number) = host.parse::<usize>() {
        return peer_at(number).map(|peer| (peer.0, peer.3));
    }
    (1..=count).filter_map(peer_at).find(|peer| core::str::from_utf8(&peer.1[..peer.2]).is_ok_and(|name| name.eq_ignore_ascii_case(host))).map(|peer| (peer.0, peer.3))
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split(':');
    for byte in mac.iter_mut() {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

// ---- the capture hook and the GET receive loop ----

struct Rx {
    active: bool,
    mac: [u8; 6],
    out: *mut u8,
    capacity: usize,
    total: Option<usize>,
    checksum: u16,
    received: usize,
    sequence: u32,
    outcome: Option<Result<(), &'static str>>,
    aborted: bool,
    deadline: u64,
}

/// Touched under the NIC lock only (`capture` runs inside a drain, `receive` takes the lock around each drain).
static mut RX: Rx = Rx { active: false, mac: [0; 6], out: core::ptr::null_mut(), capacity: 0, total: None, checksum: 0, received: 0, sequence: 0, outcome: None, aborted: false, deadline: 0 };

fn rx() -> &'static mut Rx {
    unsafe { &mut *core::ptr::addr_of_mut!(RX) }
}

fn error_text(code: u8) -> &'static str {
    match code {
        1 => "no such file there",
        2 => "that disk is not available on the machine",
        3 => "file too large for the machine's transfer buffer, or empty",
        4 => "invalid file name",
        5 => "its FOSFS directory is full",
        6 => "its FOSFS data area is full",
        7 => "disk error on the machine",
        8 => "checksum mismatch (data was damaged in transit)",
        9 => "busy: another transfer is in progress there",
        10 => "tape sync is not available there (no synckey file, or asked again within 2 s)",
        _ => "unknown error",
    }
}

/// Called by `net::handle_frame` before anything else; true when the frame has been dealt with.
pub fn capture(index: usize, source: [u8; 6], tag: u8, payload: &[u8]) -> bool {
    if tag == TAG_DISCOVER_REPLY {
        remember(index, source, payload);
        return false;
    }
    if tag == TAG_GPIO_REPLY {
        return gpio_frame(source, payload);
    }
    let rx = rx();
    if !rx.active {
        return false;
    }
    match tag {
        TAG_HELLO => true,
        TAG_DATA => {
            rx.aborted |= payload.contains(&0x1b);
            true
        }
        TAG_GET_ERROR | TAG_GET_START | TAG_GET_DATA | TAG_GET_STOP if source == rx.mac => {
            receive_frame(rx, tag, payload);
            true
        }
        _ => false,
    }
}

/// One request frame out, one reply frame back (`gpio @HOST`); touched under the NIC lock like `RX`.
struct Single {
    active: bool,
    mac: [u8; 6],
    out: *mut u8,
    capacity: usize,
    length: Option<usize>,
}

static mut SINGLE: Single = Single { active: false, mac: [0; 6], out: core::ptr::null_mut(), capacity: 0, length: None };

fn gpio_frame(source: [u8; 6], payload: &[u8]) -> bool {
    let single = unsafe { &mut *core::ptr::addr_of_mut!(SINGLE) };
    if !single.active || single.length.is_some() || source != single.mac {
        return false;
    }
    let take = payload.len().min(single.capacity);
    unsafe { core::ptr::copy_nonoverlapping(payload.as_ptr(), single.out, take) };
    single.length = Some(take);
    true
}

/// Sends `request` with the GPIO request tag and waits up to 2 s for the same machine's reply frame.
fn exchange(mac: [u8; 6], nic_index: usize, request: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
    let single = unsafe { &mut *core::ptr::addr_of_mut!(SINGLE) };
    {
        let _nics = net::lock_nics();
        *single = Single { active: true, mac, out: out.as_mut_ptr(), capacity: out.len(), length: None };
    }
    net::send_frame(nic_index, &mac, TAG_GPIO_REQUEST, request);
    let deadline = apic::micros() + 2_000_000;
    let result = loop {
        {
            let _nics = net::lock_nics();
            nicctl::drain_nics_to_fifo();
            if let Some(length) = single.length {
                break Ok(length);
            }
            if apic::micros() > deadline {
                break Err("no answer");
            }
        }
        crate::debug::pet();
        core::hint::spin_loop();
    };
    let _nics = net::lock_nics();
    single.active = false;
    result
}

fn receive_frame(rx: &mut Rx, tag: u8, payload: &[u8]) {
    if rx.outcome.is_some() {
        return;
    }
    match tag {
        TAG_GET_ERROR if !payload.is_empty() => rx.outcome = Some(Err(error_text(payload[0]))),
        TAG_GET_START if rx.total.is_none() && payload.len() >= 6 => {
            let length = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
            if length == 0 || length > rx.capacity {
                rx.outcome = Some(Err("the file does not fit the transfer buffer"));
            } else {
                rx.total = Some(length);
                rx.checksum = u16::from_le_bytes(payload[4..6].try_into().unwrap());
            }
            rx.deadline = apic::micros() + SILENCE_US;
        }
        TAG_GET_DATA if payload.len() >= 4 => {
            if let Some(length) = rx.total {
                if u32::from_le_bytes(payload[0..4].try_into().unwrap()) != rx.sequence {
                    rx.outcome = Some(Err("lost a frame (there is no retransmission); try again"));
                } else {
                    let take = (payload.len() - 4).min(CHUNK).min(length - rx.received);
                    unsafe { core::ptr::copy_nonoverlapping(payload[4..].as_ptr(), rx.out.add(rx.received), take) };
                    rx.received += take;
                    rx.sequence += 1;
                    rx.deadline = apic::micros() + SILENCE_US;
                }
            }
        }
        TAG_GET_STOP => {
            let data = |length: usize| unsafe { core::slice::from_raw_parts(rx.out, length) };
            rx.outcome = Some(match rx.total {
                Some(length) if rx.received == length && fosfor_abi::internet_checksum(data(length)) == rx.checksum => Ok(()),
                Some(length) if rx.received == length => Err("checksum mismatch; the data was damaged in transit"),
                _ => Err("the transfer ended early"),
            });
        }
        _ => {}
    }
}

fn receive(mac: [u8; 6], nic_index: usize, tag: u8, request: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
    if netfs::busy(&mac) {
        return Err("a transfer into this machine is in progress");
    }
    {
        let _nics = net::lock_nics();
        let rx = rx();
        *rx = Rx { active: true, mac, out: out.as_mut_ptr(), capacity: out.len(), total: None, checksum: 0, received: 0, sequence: 0, outcome: None, aborted: false, deadline: apic::micros() + SILENCE_US };
    }
    net::send_frame(nic_index, &mac, tag, request);
    let result = loop {
        {
            let _nics = net::lock_nics();
            nicctl::drain_nics_to_fifo();
            let rx = rx();
            if let Some(outcome) = rx.outcome {
                break outcome.map(|()| rx.total.unwrap_or(0));
            }
            if rx.aborted {
                break Err("interrupted");
            }
            if apic::micros() > rx.deadline {
                break Err("no answer (is the machine at its prompt or in term net, and is the name right?)");
            }
        }
        crate::debug::pet();
        core::hint::spin_loop();
    };
    let _nics = net::lock_nics();
    rx().active = false;
    result
}

/// `nic discover`: probes every interface once a second for three seconds and files the answers.
fn discover() {
    if nic::count() == 0 {
        return println!("No network interface.");
    }
    println!("Looking for other fos machines (3 s)...");
    unsafe { PEER_COUNT = 0 };
    let started = apic::micros();
    let mut next_probe = started;
    while apic::micros() < started + 3_000_000 {
        if apic::micros() >= next_probe {
            next_probe += 1_000_000;
            for index in 0..nic::count() {
                if net::link_up(index) {
                    let mut name = [0u8; 20];
                    let length = net::hostname(index, &mut name);
                    net::send_frame(index, &[0xff; 6], TAG_DISCOVER_REQUEST, &name[..length]);
                }
            }
        }
        {
            let _nics = net::lock_nics();
            nicctl::drain_nics_to_fifo();
        }
        crate::debug::pet();
        core::hint::spin_loop();
    }
    print_peers();
}

/// `nic discover`.
pub fn nic_discover() {
    discover();
}

// ---- the tapesync client's environment ----

pub struct Mf;

static mut STAGING_PTR: *mut u8 = core::ptr::null_mut();

impl Env for Mf {
    fn print(&self, line: core::fmt::Arguments) {
        println!("{}", line);
    }
    fn volume(&self) -> Option<Volume> {
        volumes::boot_volume()
    }
    fn staging(&self) -> Option<&'static mut [u8]> {
        unsafe {
            if STAGING_PTR.is_null() {
                let mut buffer: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
                buffer.try_reserve_exact(STAGING).ok()?;
                buffer.resize(STAGING, 0);
                STAGING_PTR = buffer.leak().as_mut_ptr();
            }
            Some(core::slice::from_raw_parts_mut(STAGING_PTR, STAGING))
        }
    }
    fn pet(&self) {
        crate::debug::pet();
    }
    fn disk<R>(&self, work: impl FnOnce() -> R) -> R {
        let _disk = tapefs::lock();
        work()
    }
    fn seed(&self) -> [u8; 16] {
        let mut seed = [0u8; 16];
        seed[0..8].copy_from_slice(&unsafe { core::arch::x86_64::_rdtsc() }.to_le_bytes());
        seed[8..16].copy_from_slice(&apic::micros().to_le_bytes());
        seed
    }
    fn peer(&self, host: &str) -> Option<([u8; 6], usize)> {
        if let Some(mac) = parse_mac(host) {
            return Some((mac, (0..nic::count()).find(|&index| net::link_up(index)).unwrap_or(0)));
        }
        if lookup(host).is_none() && host.parse::<usize>().is_err() {
            discover();
        }
        lookup(host)
    }
    fn peer_count(&self) -> usize {
        unsafe { PEER_COUNT }
    }
    fn peer_at(&self, number: usize) -> Option<([u8; 6], [u8; 20], usize, usize)> {
        peer_at(number)
    }
    fn discover(&self) {
        discover();
    }
    fn receive(&self, mac: [u8; 6], nic: usize, tag: u8, request: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
        receive(mac, nic, tag, request, out)
    }
    fn fetch(&self, mac: [u8; 6], nic: usize, name: &str) -> Result<usize, &'static str> {
        let staging = self.staging().ok_or("no memory for the transfer buffer")?;
        let mut request = [0u8; 1 + 48];
        let name = name.as_bytes();
        if name.len() > 48 {
            return Err("name too long");
        }
        request[0] = 1;
        request[1..1 + name.len()].copy_from_slice(name);
        receive(mac, nic, TAG_GET_REQUEST, &request[..1 + name.len()], staging)
    }
}

/// `tapesync [HOST] [apply]`.
pub fn tapesync(words: core::str::SplitWhitespace) {
    client::command(&Mf, words);
}

impl kernel_core::gpio_client::Env for Mf {
    fn print(&self, line: core::fmt::Arguments) {
        println!("{}", line);
    }
    fn key(&self, out: &mut [u8; kernel_core::gpio_client::KEY_MAX]) -> Option<usize> {
        let volume = volumes::boot_volume()?;
        client::read_key(self, &volume, out)
    }
    fn peer(&self, host: &str) -> Option<([u8; 6], usize)> {
        Env::peer(self, host)
    }
    fn exchange(&self, mac: [u8; 6], nic: usize, request: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
        exchange(mac, nic, request, out)
    }
}

/// `gpio [@HOST] ...`: this kernel has no pins of its own.
pub fn gpio(rest: &str) {
    kernel_core::gpio_client::command(&Mf, rest, |_| println!("gpio: this machine has no GPIO pins; `gpio @HOST ...` works on one that has."));
}
