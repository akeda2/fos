//! NETFS, discovery and the peer table, for both kernels (docs/shell.md "Network"): the same wire format `term net`,
//! `tools/foscopy.py` and `tools/peers.py` speak (raw Ethernet, ethertype 0x88b6, a tag byte). Every machine announces its
//! name, files the names it hears (`nic peers`, the tape ABI's `peer_info`), serves GET/PUT of files and the signed tape
//! manifest of `tapesync`, and can fetch from or store on another machine. Only what touches the NICs, the staging buffer
//! and the console's break key is per kernel (`imp`).

use crate::timer;

pub(crate) const ETHERTYPE: [u8; 2] = [0x88, 0xb6];
pub(crate) const TAG_DISCOVER_REQUEST: u8 = 0x02;
pub(crate) const TAG_DISCOVER_REPLY: u8 = 0x03;
pub(crate) const TAG_GET_REQUEST: u8 = 0x30;
pub(crate) const TAG_GET_START: u8 = 0x31;
pub(crate) const TAG_GET_DATA: u8 = 0x32;
pub(crate) const TAG_GET_STOP: u8 = 0x33;
pub(crate) const TAG_GET_ERROR: u8 = 0x34;
pub(crate) const TAG_PUT_REQUEST: u8 = 0x35;
pub(crate) const TAG_PUT_DATA: u8 = 0x36;
pub(crate) const TAG_PUT_STOP: u8 = 0x37;
pub(crate) const TAG_PUT_ACK: u8 = 0x38;
pub(crate) const TAG_PUT_ERROR: u8 = 0x39;
pub(crate) const NETFS_NOT_FOUND: u8 = 1;
pub(crate) const NETFS_NO_DISK: u8 = 2;
pub(crate) const NETFS_TOO_LARGE: u8 = 3;
pub(crate) const NETFS_BAD_NAME: u8 = 4;
pub(crate) const NETFS_DIRECTORY_FULL: u8 = 5;
pub(crate) const NETFS_DATA_FULL: u8 = 6;
pub(crate) const NETFS_DISK_ERROR: u8 = 7;
pub(crate) const NETFS_CHECKSUM: u8 = 8;
pub(crate) const NETFS_BUSY: u8 = 9;
/// `tapesync` is not set up on the machine asked (no `synckey` file, or it asked too soon after the last request).
pub(crate) const NETFS_NO_SYNC: u8 = 10;
/// The tape-sync request (see `tapesync.rs` and crates/tape-sync): a nonce in, a signed manifest out as a GET stream.
pub(crate) const TAG_SYNC_REQUEST: u8 = 0x40;
/// The shared secret of `tapesync`; NETFS never reads or writes it.
pub(crate) const KEY_FILE: &str = "synckey";
pub(crate) const NETFS_CHUNK: usize = 1024;

/// The name announced in discovery replies for interface `index`: `NAME<n>` from the
/// `hostname` file (n is the one-based interface number, as v1 does), or without that file
/// `fos-` and the last three bytes of the interface's MAC address (so that machines that were
/// never named still tell each other apart).
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
    let _ = match (imp::hostname_file(), imp::mac(index)) {
        (Some(base), _) => write!(cursor, "{base}{}", index + 1),
        (None, Some(mac)) => write!(cursor, "fos-{:02x}{:02x}{:02x}", mac[3], mac[4], mac[5]),
        (None, None) => write!(cursor, "fos"),
    };
    cursor.1
}


pub(crate) fn netfs_enabled() -> bool {
    imp::served()
}

/// The name in a request: up to the first zero byte (frames are padded to the Ethernet minimum).
fn netfs_name(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len())]
}

struct NetfsPut {
    active: bool,
    source: [u8; 6],
    disk: usize,
    name: [u8; 16],
    name_length: usize,
    expected: usize,
    checksum: u16,
    received: usize,
    next_sequence: u32,
    last_activity: u64,
}

static mut NETFS_PUT: NetfsPut = NetfsPut { active: false, source: [0; 6], disk: 0, name: [0; 16], name_length: 0, expected: 0, checksum: 0, received: 0, next_sequence: 0, last_activity: 0 };

pub(crate) fn netfs_reply(index: usize, destination: &[u8; 6], tag: u8, payload: &[u8]) {
    send(index, destination, tag, payload);
}

/// True if the staging buffer is in use by a transfer that is still alive (a PUT in progress, or a kernel update).
pub(crate) fn netfs_busy(source: &[u8; 6]) -> bool {
    let put = unsafe { &*core::ptr::addr_of!(NETFS_PUT) };
    (put.active && put.source != *source && timer::ticks().wrapping_sub(put.last_activity) < 500) || imp::kup_busy()
}

/// A request to read a file: answers with START, numbered DATA chunks and STOP, or an error code.
fn netfs_get_request(index: usize, source: [u8; 6], payload: &[u8]) {
    if payload.len() < 2 {
        return;
    }
    let name = netfs_name(&payload[1..]);
    let reply_error = |code: u8| netfs_reply(index, &source, TAG_GET_ERROR, &[code]);
    if netfs_busy(&source) {
        return reply_error(NETFS_BUSY);
    }
    if !crate::fosfs::valid_name(name) {
        return reply_error(NETFS_BAD_NAME);
    }
    // The shared secret of `tapesync` never leaves the machine, whoever asks.
    if name == KEY_FILE.as_bytes() {
        return reply_error(NETFS_NOT_FOUND);
    }
    let Ok(volume) = crate::storage::volume_on(usize::from(payload[0])) else { return reply_error(NETFS_NO_DISK) };
    let Some(staging) = imp::staging() else { return reply_error(NETFS_TOO_LARGE) };
    let length = match volume.read(name, staging) {
        Ok(Some(0)) => return reply_error(NETFS_TOO_LARGE),
        Ok(Some(length)) => length,
        Ok(None) => return reply_error(NETFS_NOT_FOUND),
        Err(crate::fosfs::Error::TooBig) => return reply_error(NETFS_TOO_LARGE),
        Err(_) => return reply_error(NETFS_DISK_ERROR),
    };
    netfs_stream(index, &source, &staging[..length], core::str::from_utf8(name).unwrap_or("?"));
}

/// Sends `data` to `source` as a GET answer: START with the length and checksum, numbered DATA chunks, STOP.
pub(crate) fn netfs_stream(index: usize, source: &[u8; 6], data: &[u8], what: &str) {
    let length = data.len();
    let checksum = fosfor_abi::internet_checksum(data);
    let mut start = [0u8; 6];
    start[0..4].copy_from_slice(&(length as u32).to_le_bytes());
    start[4..6].copy_from_slice(&checksum.to_le_bytes());
    netfs_reply(index, source, TAG_GET_START, &start);
    crate::println!("NETFS: sending {what} ({length} bytes) to {}.", crate::netdump::Mac(source));
    // The receiver reads from a finite ring: one chunk a millisecond is a pace real hardware keeps up with.
    let mut next = timer::micros();
    for (sequence, chunk) in data.chunks(NETFS_CHUNK).enumerate() {
        let mut frame = [0u8; 4 + NETFS_CHUNK];
        frame[0..4].copy_from_slice(&(sequence as u32).to_le_bytes());
        frame[4..4 + chunk.len()].copy_from_slice(chunk);
        while timer::micros() < next {
            core::hint::spin_loop();
        }
        next = timer::micros() + 1000;
        netfs_reply(index, source, TAG_GET_DATA, &frame[..4 + chunk.len()]);
        imp::pet();
    }
    netfs_reply(index, source, TAG_GET_STOP, &[]);
}

/// A request to store a file. Acceptance is silent; a refusal is an error frame.
fn netfs_put_request(index: usize, source: [u8; 6], payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }
    let reply_error = |code: u8| netfs_reply(index, &source, TAG_PUT_ERROR, &[code]);
    if netfs_busy(&source) {
        return reply_error(NETFS_BUSY);
    }
    let expected = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
    let checksum = u16::from_le_bytes(payload[5..7].try_into().unwrap());
    let name = netfs_name(&payload[7..]);
    if !crate::fosfs::valid_name(name) || name == KEY_FILE.as_bytes() {
        return reply_error(NETFS_BAD_NAME);
    }
    if crate::storage::volume_on(usize::from(payload[0])).is_err() {
        return reply_error(NETFS_NO_DISK);
    }
    if expected == 0 || expected > imp::STAGE_BYTES || imp::staging().is_none() {
        return reply_error(NETFS_TOO_LARGE);
    }
    let put = unsafe { &mut *core::ptr::addr_of_mut!(NETFS_PUT) };
    *put = NetfsPut { active: true, source, disk: usize::from(payload[0]), name: [0; 16], name_length: name.len(), expected, checksum, received: 0, next_sequence: 0, last_activity: timer::ticks() };
    put.name[..name.len()].copy_from_slice(name);
    crate::println!("NETFS: receiving {} ({expected} bytes) from {}.", core::str::from_utf8(name).unwrap_or("?"), crate::netdump::Mac(&source));
}

fn netfs_put_data(source: [u8; 6], payload: &[u8]) {
    let put = unsafe { &mut *core::ptr::addr_of_mut!(NETFS_PUT) };
    if !put.active || put.source != source || payload.len() < 4 {
        return;
    }
    let sequence = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    if sequence != put.next_sequence {
        put.active = false;
        return crate::println!("NETFS: chunk {sequence} arrived where {} was expected; transfer abandoned.", put.next_sequence);
    }
    let Some(staging) = imp::staging() else { return };
    // The chunk's size comes from the declared length, never from the (padded) frame.
    let take = (payload.len() - 4).min(NETFS_CHUNK).min(put.expected - put.received);
    staging[put.received..put.received + take].copy_from_slice(&payload[4..4 + take]);
    put.received += take;
    put.next_sequence += 1;
    put.last_activity = timer::ticks();
}

fn netfs_put_stop(index: usize, source: [u8; 6]) {
    let put = unsafe { &mut *core::ptr::addr_of_mut!(NETFS_PUT) };
    if !put.active || put.source != source {
        return;
    }
    put.active = false;
    let Some(staging) = imp::staging() else { return };
    let reply_error = |code: u8| netfs_reply(index, &source, TAG_PUT_ERROR, &[code]);
    if put.received != put.expected {
        crate::println!("NETFS: transfer ended after {} of {} bytes; abandoned.", put.received, put.expected);
        return reply_error(NETFS_CHECKSUM);
    }
    if fosfor_abi::internet_checksum(&staging[..put.expected]) != put.checksum {
        crate::println!("NETFS: checksum mismatch; file rejected.");
        return reply_error(NETFS_CHECKSUM);
    }
    let Ok(volume) = crate::storage::volume_on(put.disk) else { return reply_error(NETFS_NO_DISK) };
    match volume.write(&put.name[..put.name_length], &staging[..put.expected]) {
        Ok(()) => {
            crate::println!("NETFS: stored {} ({} bytes).", core::str::from_utf8(&put.name[..put.name_length]).unwrap_or("?"), put.expected);
            netfs_reply(index, &source, TAG_PUT_ACK, &[]);
        }
        Err(crate::fosfs::Error::DirectoryFull) => reply_error(NETFS_DIRECTORY_FULL),
        Err(crate::fosfs::Error::DataFull) => reply_error(NETFS_DATA_FULL),
        Err(crate::fosfs::Error::InvalidName) => reply_error(NETFS_BAD_NAME),
        Err(_) => reply_error(NETFS_DISK_ERROR),
    }
}

fn netfs_error_text(code: u8) -> &'static str {
    match code {
        NETFS_NOT_FOUND => "no such file there",
        NETFS_NO_DISK => "that disk is not available on the machine",
        NETFS_TOO_LARGE => "file too large for the machine's transfer buffer (4 MiB), or empty",
        NETFS_BAD_NAME => "invalid file name",
        NETFS_DIRECTORY_FULL => "its FOSFS directory is full",
        NETFS_DATA_FULL => "its FOSFS data area is full",
        NETFS_DISK_ERROR => "disk error on the machine",
        NETFS_CHECKSUM => "checksum mismatch (data was damaged in transit)",
        NETFS_BUSY => "busy: another transfer is in progress there",
        NETFS_NO_SYNC => "tape sync is not available there (no synckey file, or asked again within 2 s)",
        _ => "unknown error",
    }
}

/// `HOST[:diskN]` to the machine's MAC, the interface to reach it on and the disk number. HOST is a name from
/// `nic peers` (looked up with a discovery if the table has no such name yet), a MAC address, or a peer number.
pub(crate) fn netfs_peer(spec: &str) -> Option<([u8; 6], usize, usize)> {
    let (host, disk) = match spec.split_once(":disk") {
        Some((host, number)) => (host, number.parse::<usize>().ok()?),
        None => (spec, 1),
    };
    let parse_mac = |text: &str| -> Option<[u8; 6]> {
        let mut mac = [0u8; 6];
        let mut parts = text.split(':');
        for byte in mac.iter_mut() {
            *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
        }
        parts.next().is_none().then_some(mac)
    };
    if let Some(mac) = parse_mac(host) {
        return Some((mac, imp::active_nic(), disk));
    }
    let lookup = |host: &str| -> Option<([u8; 6], usize)> {
        let count = unsafe { PEER_COUNT };
        let peers = unsafe { &*core::ptr::addr_of!(PEERS) };
        if let Ok(number) = host.parse::<usize>() {
            return (number >= 1 && number <= count).then(|| (peers[number - 1].0, peers[number - 1].3));
        }
        peers[..count].iter().find(|peer| core::str::from_utf8(&peer.1[..peer.2]).is_ok_and(|name| name.eq_ignore_ascii_case(host))).map(|peer| (peer.0, peer.3))
    };
    if lookup(host).is_none() && host.parse::<usize>().is_err() {
        nic_discover();
    }
    lookup(host).map(|(mac, nic)| (mac, nic, disk))
}

/// Fetches a file from a peer into the staging buffer; returns its length.
pub(crate) fn netfs_fetch(mac: [u8; 6], nic_index: usize, disk: usize, name: &str) -> Result<usize, &'static str> {
    let staging = imp::staging().ok_or("no memory for the transfer buffer")?;
    let mut request = [0u8; 1 + 16];
    request[0] = disk as u8;
    request[1..1 + name.len()].copy_from_slice(name.as_bytes());
    netfs_receive(mac, nic_index, TAG_GET_REQUEST, &request[..1 + name.len()], staging)
}

/// Sends `request` (a GET-style request with tag `tag`) to a peer and collects its START/DATA/STOP answer into `staging`.
pub(crate) fn netfs_receive(mac: [u8; 6], nic_index: usize, tag: u8, request: &[u8], staging: &mut [u8]) -> Result<usize, &'static str> {
    if netfs_busy(&mac) {
        return Err("a transfer into this machine is in progress");
    }
    netfs_reply(nic_index, &mac, tag, request);
    let (mut total, mut checksum, mut received, mut sequence) = (None::<usize>, 0u16, 0usize, 0u32);
    let mut outcome: Option<Result<(), &'static str>> = None;
    let mut deadline = timer::ticks() + 3 * timer::TICK_HZ;
    imp::blocking(true);
    while outcome.is_none() && timer::ticks() < deadline && !imp::break_requested() {
        imp::drain(|_, frame| {
            if frame.len() < 15 || frame[12..14] != ETHERTYPE || frame[6..12] != mac {
                return;
            }
            let payload = &frame[15..];
            match frame[14] {
                TAG_GET_ERROR if !payload.is_empty() => outcome = Some(Err(netfs_error_text(payload[0]))),
                TAG_GET_START if total.is_none() && payload.len() >= 6 => {
                    let length = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
                    if length == 0 || length > staging.len() {
                        outcome = Some(Err("the file does not fit the transfer buffer"));
                    } else {
                        total = Some(length);
                        checksum = u16::from_le_bytes(payload[4..6].try_into().unwrap());
                    }
                    deadline = timer::ticks() + 3 * timer::TICK_HZ;
                }
                TAG_GET_DATA if payload.len() >= 4 => {
                    if let Some(length) = total {
                        if u32::from_le_bytes(payload[0..4].try_into().unwrap()) != sequence {
                            outcome = Some(Err("lost a frame (there is no retransmission); try again"));
                        } else {
                            let take = (payload.len() - 4).min(NETFS_CHUNK).min(length - received);
                            staging[received..received + take].copy_from_slice(&payload[4..4 + take]);
                            received += take;
                            sequence += 1;
                            deadline = timer::ticks() + 3 * timer::TICK_HZ;
                        }
                    }
                }
                TAG_GET_STOP => {
                    outcome = Some(match total {
                        Some(length) if received == length && fosfor_abi::internet_checksum(&staging[..length]) == checksum => Ok(()),
                        Some(length) if received == length => Err("checksum mismatch; the data was damaged in transit"),
                        _ => Err("the transfer ended early"),
                    });
                }
                _ => {}
            }
        });
        imp::pet();
        crate::cpu::sleep();
    }
    imp::blocking(false);
    match outcome {
        Some(Ok(())) => Ok(total.unwrap_or(0)),
        Some(Err(reason)) => Err(reason),
        None => Err("no answer (is the machine at its prompt or in term net, and is the name right?)"),
    }
}

/// Sends the first `length` bytes of the staging buffer to a peer as a file.
pub(crate) fn netfs_store(mac: [u8; 6], nic_index: usize, disk: usize, name: &str, length: usize) -> Result<(), &'static str> {
    let staging = imp::staging().ok_or("no memory for the transfer buffer")?;
    let checksum = fosfor_abi::internet_checksum(&staging[..length]);
    let mut request = [0u8; 7 + 16];
    request[0] = disk as u8;
    request[1..5].copy_from_slice(&(length as u32).to_le_bytes());
    request[5..7].copy_from_slice(&checksum.to_le_bytes());
    request[7..7 + name.len()].copy_from_slice(name.as_bytes());
    netfs_reply(nic_index, &mac, TAG_PUT_REQUEST, &request[..7 + name.len()]);
    imp::blocking(true);
    let mut refusal: Option<&'static str> = None;
    // A refusal arrives at once; acceptance is silent.
    let until = timer::ticks() + timer::TICK_HZ / 2;
    while timer::ticks() < until && refusal.is_none() {
        imp::drain(|_, frame| {
            if frame.len() >= 16 && frame[12..14] == ETHERTYPE && frame[6..12] == mac && frame[14] == TAG_PUT_ERROR {
                refusal = Some(netfs_error_text(frame[15]));
            }
        });
        crate::cpu::sleep();
    }
    if let Some(reason) = refusal {
        imp::blocking(false);
        return Err(reason);
    }
    let mut next = timer::micros();
    for (sequence, chunk) in staging[..length].chunks(NETFS_CHUNK).enumerate() {
        let mut frame = [0u8; 4 + NETFS_CHUNK];
        frame[0..4].copy_from_slice(&(sequence as u32).to_le_bytes());
        frame[4..4 + chunk.len()].copy_from_slice(chunk);
        while timer::micros() < next {
            core::hint::spin_loop();
        }
        next = timer::micros() + 1000;
        netfs_reply(nic_index, &mac, TAG_PUT_DATA, &frame[..4 + chunk.len()]);
        imp::pet();
        if imp::break_requested() {
            imp::blocking(false);
            return Err("interrupted");
        }
    }
    netfs_reply(nic_index, &mac, TAG_PUT_STOP, &[]);
    let mut result: Option<Result<(), &'static str>> = None;
    let deadline = timer::ticks() + 10 * timer::TICK_HZ;
    while result.is_none() && timer::ticks() < deadline && !imp::break_requested() {
        imp::drain(|_, frame| {
            if frame.len() >= 15 && frame[12..14] == ETHERTYPE && frame[6..12] == mac {
                match frame[14] {
                    TAG_PUT_ACK => result = Some(Ok(())),
                    TAG_PUT_ERROR if frame.len() >= 16 => result = Some(Err(netfs_error_text(frame[15]))),
                    _ => {}
                }
            }
        });
        crate::cpu::sleep();
    }
    imp::blocking(false);
    result.unwrap_or(Err("no reply (a frame may have been lost: there is no retransmission)"))
}


const MAX_PEERS: usize = 32;
/// Who has announced itself or answered a probe: (MAC, name, name length, interface).
static mut PEERS: [([u8; 6], [u8; 20], usize, usize); MAX_PEERS] = [([0; 6], [0; 20], 0, 0); MAX_PEERS];
static mut PEER_COUNT: usize = 0;

pub(crate) fn peer_count() -> usize {
    unsafe { PEER_COUNT }
}

/// Peer `number` (1-based, as `nic peers` lists them): MAC, name, name length, interface.
pub(crate) fn peer_at(number: usize) -> Option<([u8; 6], [u8; 20], usize, usize)> {
    (number >= 1 && number <= peer_count()).then(|| unsafe { (*core::ptr::addr_of!(PEERS))[number - 1] })
}

/// Files a peer's name; true if the peer is new. A known peer's name is replaced (its `hostname` file may have changed).
pub(crate) fn remember_peer(index: usize, mac: [u8; 6], name: &[u8]) -> bool {
    // Some virtual networks echo our own frames back; this machine is not its own peer.
    if (0..imp::count()).any(|own| imp::mac(own) == Some(mac)) {
        return false;
    }
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
            return false;
        }
        if count < MAX_PEERS {
            peers[count] = (mac, [0; 20], length, index);
            peers[count].1[..length].copy_from_slice(&name[..length]);
            PEER_COUNT = count + 1;
            return true;
        }
    }
    false
}

/// A tape's `peer_info`: peer `number` (1-based; 0 only counts) into `out`.
pub fn peer_info(number: usize, out: &mut fosfor_abi::tape::PeerInfo) -> usize {
    let count = unsafe { PEER_COUNT };
    if number >= 1 && number <= count {
        let (mac, name, length, nic) = unsafe { (*core::ptr::addr_of!(PEERS))[number - 1] };
        out.mac = mac;
        out.nic = nic as u8;
        out.name_length = length as u8;
        out.name = name;
    }
    count
}

pub(crate) fn print_peers() {
    let count = unsafe { PEER_COUNT };
    if count == 0 {
        return crate::println!("No peers known yet (`nic discover` looks for them).");
    }
    for number in 0..count {
        let (mac, name, length, nic) = unsafe { (*core::ptr::addr_of!(PEERS))[number] };
        crate::println!("{:>2}: {}  {}  (via NIC {})", number + 1, crate::netdump::Mac(&mac), core::str::from_utf8(&name[..length]).unwrap_or("?"), nic + 1);
    }
}

/// `nic discover`: asks every interface who else runs fos (a broadcast probe, repeated each second for three seconds)
/// and lists the answers with their host names. Machines also answer on their own, whenever they are running.
pub(crate) fn nic_discover() {
    if imp::count() == 0 {
        return crate::println!("No network interface.");
    }
    crate::println!("Looking for other fos machines (3 s)...");
    unsafe { PEER_COUNT = 0 };
    imp::blocking(true);
    let started = timer::ticks();
    let mut next_probe = started;
    while timer::ticks() < started + 3 * timer::TICK_HZ && !imp::break_requested() {
        if timer::ticks() >= next_probe {
            next_probe += timer::TICK_HZ;
            for index in 0..imp::count() {
                let mut name = [0u8; 20];
                let length = hostname(index, &mut name);
                send(index, &[0xff; 6], TAG_DISCOVER_REQUEST, &name[..length]);
            }
        }
        imp::drain(|index, frame| {
            if frame.len() >= 15 && frame[12..14] == ETHERTYPE && frame[14] == TAG_DISCOVER_REPLY {
                let name = netfs_name(&frame[15..]); // the frame is padded with zeros
                remember_peer(index, frame[6..12].try_into().unwrap(), name);
            }
        });
        imp::pet();
        crate::cpu::sleep();
    }
    imp::blocking(false);
    print_peers();
}


/// Seconds between a machine's unsolicited name announcements; the first few come faster so that a machine
/// that has just booted is known to the others (and knows them) within seconds.
const ANNOUNCE_SECONDS: u64 = 10;
static mut NEXT_ANNOUNCE: u64 = 0;
static mut ANNOUNCE_COUNT: u32 = 0;

/// Broadcasts this machine's name on every interface that has a link, the same frame a discovery probe is answered
/// with, so every fos machine on the segment learns it without asking (`handle` files it in the peer table, which
/// tapes read with `peer_info`). Called from the main loops and from a running tape's service loop.
pub(crate) fn announce() {
    unsafe {
        if timer::ticks() < NEXT_ANNOUNCE {
            return;
        }
        ANNOUNCE_COUNT = ANNOUNCE_COUNT.saturating_add(1);
        NEXT_ANNOUNCE = timer::ticks() + if ANNOUNCE_COUNT <= 4 { 1 } else { ANNOUNCE_SECONDS } * timer::TICK_HZ;
    }
    for index in 0..imp::count() {
        if imp::link_up(index) {
            let mut name = [0u8; 20];
            let length = hostname(index, &mut name);
            send(index, &[0xff; 6], TAG_DISCOVER_REPLY, &name[..length]);
        }
    }
}

/// The discovery and NETFS frames, for both kernels' receive loops. `Some(consumed)` if the tag is one of ours (false: a
/// tape with its own address book may still want it), `None` for any other tag.
pub(crate) fn handle(index: usize, source: [u8; 6], tag: u8, payload: &[u8]) -> Option<bool> {
    match tag {
        TAG_DISCOVER_REQUEST => {
            let mut name = [0u8; 20];
            let length = hostname(index, &mut name);
            send(index, &source, TAG_DISCOVER_REPLY, &name[..length]);
        }
        // A name announcement (or the answer to a probe): remember who it is. A machine we had not heard of may
        // have booted after our last broadcast, so it is told our name straight away (it does the same for us once).
        TAG_DISCOVER_REPLY => {
            if remember_peer(index, source, netfs_name(payload)) {
                let mut name = [0u8; 20];
                let length = hostname(index, &mut name);
                send(index, &source, TAG_DISCOVER_REPLY, &name[..length]);
            }
            return Some(false);
        }
        // NETFS is served at the prompt and in `term net`, never while a tape runs (the staging buffer is the tape's) or
        // when the settings file closed the network terminal.
        TAG_GET_REQUEST if netfs_enabled() => netfs_get_request(index, source, payload),
        TAG_PUT_REQUEST if netfs_enabled() => netfs_put_request(index, source, payload),
        TAG_PUT_DATA if netfs_enabled() => netfs_put_data(source, payload),
        TAG_PUT_STOP if netfs_enabled() => netfs_put_stop(index, source),
        TAG_SYNC_REQUEST if netfs_enabled() => crate::tapesync::serve(index, source, payload),
        _ => return None,
    }
    Some(true)
}

/// Builds and sends one fos frame (ethertype 0x88b6, tag, payload) out of interface `index`.
pub(crate) fn send(index: usize, destination: &[u8; 6], tag: u8, payload: &[u8]) -> bool {
    // Up to a full Ethernet frame (NETFS data frames carry 4 + 1024 bytes after the tag).
    let mut frame = [0u8; 1514];
    let Some(mac) = imp::mac(index) else { return false };
    if payload.len() > 1514 - 15 {
        return false;
    }
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(&mac);
    frame[12..14].copy_from_slice(&ETHERTYPE);
    frame[14] = tag;
    frame[15..15 + payload.len()].copy_from_slice(payload);
    imp::send_frame(index, &frame[..15 + payload.len()])
}

// ---- `copy` between machines ---------------------------------------------------------------------------------------

pub(crate) fn split_volume(word: &str) -> Option<(&str, &str)> {
    let (volume, name) = word.split_once('/')?;
    let is_volume = volume == "esp" || volume.strip_prefix("disk").is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())) || volume.strip_prefix('@').is_some_and(|label| !label.is_empty());
    (is_volume && !name.is_empty()).then_some((volume, name))
}

/// A volume named by a command word: `diskN` or `@LABEL`. `None` if the word is neither.
pub(crate) fn volume_word(word: &str) -> Option<Result<(usize, crate::fosfs::Volume), String8Error>> {
    if let Some(label) = word.strip_prefix('@') {
        return Some(crate::storage::volume_labelled(label.as_bytes()).ok_or(String8Error::NoLabel));
    }
    let number = word.strip_prefix("disk")?.parse::<usize>().ok()?;
    Some(crate::storage::volume_on(number).map(|volume| (number, volume)).map_err(String8Error::Volume))
}

pub(crate) enum String8Error {
    NoLabel,
    Volume(crate::storage::VolumeError),
}


fn write_error(error: crate::fosfs::Error) -> &'static str {
    use crate::fosfs::Error;
    match error {
        Error::DirectoryFull => "the destination directory is full",
        Error::DataFull => "the destination volume is full",
        Error::InvalidName => "the destination name is invalid",
        _ => "writing the destination failed",
    }
}

/// The `copy` forms that name another machine: `copy HOST[:diskN]/NAME DST [NEWNAME]` fetches, `copy SRC NAME HOST[:diskN]
/// [NEWNAME]` stores. True if `rest` was one of them (and is done); false leaves it to the local copy.
pub(crate) fn copy_peer(rest: &str) -> bool {
    let mut words = rest.split_whitespace();
    let Some(source) = words.next() else { return false };
    if split_volume(source).is_some() {
        return false;
    }
    if volume_word(source).is_none() && source.contains('/') {
        let Some(destination) = words.next() else {
            crate::println!("copy: where to? (copy HOST[:diskN]/NAME DST [NEWNAME])");
            return true;
        };
        copy_from_peer(source, destination, words.next());
        return true;
    }
    let (Some(name), Some(destination)) = (words.next(), words.next()) else { return false };
    if split_volume(destination).is_some() || volume_word(destination).is_some() || destination == "esp" {
        return false;
    }
    copy_to_peer(source, name, destination, words.next().unwrap_or(name));
    true
}

/// `copy HOST[:diskN]/NAME DST [NEWNAME]`: the first word is the remote file, the second the local volume (and the third,
/// the new name, arrives as `destination` here because the parser shifted the words).
fn copy_from_peer(remote: &str, local_volume: &str, new_name: Option<&str>) {
    let Some((spec, name)) = remote.rsplit_once('/') else { return };
    let new_name = new_name.unwrap_or(name);
    let Some(Ok((_, volume))) = volume_word(local_volume) else {
        return crate::println!("copy: {local_volume} is not diskN or @LABEL.");
    };
    if !crate::fosfs::valid_name(name.as_bytes()) || !crate::fosfs::valid_name(new_name.as_bytes()) {
        return crate::println!("copy: invalid file name.");
    }
    let Some((mac, nic, disk)) = netfs_peer(spec) else { return crate::println!("copy: no machine called {spec} answered (see `nic peers`).") };
    crate::println!("Fetching {name} from {} disk{disk}...", crate::netdump::Mac(&mac));
    match netfs_fetch(mac, nic, disk, name) {
        Ok(length) => {
            let staging = staging().unwrap();
            match volume.write(new_name.as_bytes(), &staging[..length]) {
                Ok(()) => crate::println!("Copied {name} ({length} bytes) from {spec} to {local_volume} as {new_name}."),
                Err(error) => crate::println!("copy: {}.", write_error(error)),
            }
        }
        Err(reason) => crate::println!("copy: {reason}."),
    }
}

/// `copy SRC NAME HOST[:diskN] [NEWNAME]`: a local file to another machine.
fn copy_to_peer(source: &str, name: &str, host: &str, new_name: &str) {
    let Some(Ok((_, volume))) = volume_word(source) else {
        return crate::println!("copy: {source} is not diskN or @LABEL.");
    };
    if !crate::fosfs::valid_name(name.as_bytes()) || !crate::fosfs::valid_name(new_name.as_bytes()) {
        return crate::println!("copy: invalid file name.");
    }
    let Some(staging) = staging() else { return crate::println!("copy: no memory for the transfer buffer.") };
    let length = match volume.read(name.as_bytes(), staging) {
        Ok(Some(0)) => return crate::println!("copy: {name} is empty."),
        Ok(Some(length)) => length,
        Ok(None) => return crate::println!("copy: {name}: no such file on {source}."),
        Err(_) => return crate::println!("copy: {name} cannot be read (over 4 MiB?)."),
    };
    let Some((mac, nic, disk)) = netfs_peer(host) else { return crate::println!("copy: no machine called {host} answered (see `nic peers`).") };
    crate::println!("Sending {name} ({length} bytes) to {} disk{disk}...", crate::netdump::Mac(&mac));
    match netfs_store(mac, nic, disk, new_name, length) {
        Ok(()) => crate::println!("Copied {name} to {host} as {new_name}."),
        Err(reason) => crate::println!("copy: {reason}."),
    }
}


pub(crate) fn count() -> usize {
    imp::count()
}

pub(crate) fn mac(index: usize) -> Option<[u8; 6]> {
    imp::mac(index)
}

pub(crate) fn link_up(index: usize) -> bool {
    imp::link_up(index)
}

pub(crate) fn send_frame(index: usize, frame: &[u8]) -> bool {
    imp::send_frame(index, frame)
}

/// Receives what is waiting on every interface; our own protocols are served, the rest goes to `visit(interface, frame)`.
pub(crate) fn drain(visit: impl FnMut(usize, &[u8])) {
    imp::drain(visit);
}

/// A shell loop that runs until Escape: `blocking(true)` first (a `term net` client's keys must not nest commands).
pub(crate) fn blocking(on: bool) {
    imp::blocking(on);
}

pub(crate) fn break_requested() -> bool {
    imp::break_requested()
}

pub(crate) fn staging() -> Option<&'static mut [u8]> {
    imp::staging()
}

pub(crate) fn pet() {
    imp::pet();
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use crate::term;

    pub const STAGE_BYTES: usize = crate::tape::STAGE_BYTES;

    pub fn count() -> usize {
        term::nic_count()
    }

    pub fn mac(index: usize) -> Option<[u8; 6]> {
        term::nic_at(index).map(|nic| nic.mac())
    }

    pub fn link_up(index: usize) -> bool {
        term::nic_at(index).is_some_and(|nic| nic.link_up())
    }

    pub fn send_frame(index: usize, frame: &[u8]) -> bool {
        term::nic_at(index).is_some_and(|nic| nic.send(frame))
    }

    /// Receives what is waiting; the terminal's and discovery's own frames are handled, the rest goes to `visit`.
    pub fn drain(visit: impl FnMut(usize, &[u8])) {
        term::drain_nics(visit);
    }

    pub fn blocking(on: bool) {
        term::blocking(on);
    }

    pub fn break_requested() -> bool {
        term::break_requested()
    }

    pub fn pet() {
        crate::debug::pet();
    }

    pub fn staging() -> Option<&'static mut [u8]> {
        crate::tape::staging()
    }

    /// NETFS is not served while a tape runs (the staging buffer is the tape's) or when `settings` closed the network terminal.
    pub fn served() -> bool {
        !term::tape_mode() && !crate::config::deny_term_net()
    }

    pub fn kup_busy() -> bool {
        term::kup_transfer_alive()
    }

    pub fn active_nic() -> usize {
        term::active_nic()
    }

    pub fn hostname_file() -> Option<&'static str> {
        crate::config::hostname()
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use crate::genet;

    pub const STAGE_BYTES: usize = 4 * 1024 * 1024;

    pub fn count() -> usize {
        usize::from(genet::present())
    }

    pub fn mac(index: usize) -> Option<[u8; 6]> {
        genet::mac().filter(|_| index == 0)
    }

    pub fn link_up(index: usize) -> bool {
        index == 0 && genet::info().is_some_and(|info| info.link_up)
    }

    pub fn send_frame(index: usize, frame: &[u8]) -> bool {
        // The Genet does not pad: a short frame goes out as the 60-byte minimum.
        let mut padded = [0u8; 60];
        let frame = if frame.len() < 60 {
            padded[..frame.len()].copy_from_slice(frame);
            &padded[..]
        } else {
            frame
        };
        index == 0 && genet::send(frame)
    }

    /// Receives what is waiting; the discovery, NETFS and KUP frames are served, the rest goes to `visit`.
    pub fn drain(mut visit: impl FnMut(usize, &[u8])) {
        let mut buffer = [0u8; 1600];
        loop {
            let length = genet::receive(&mut buffer);
            if length == 0 {
                break;
            }
            if !crate::netservice::serve_frame(&buffer[..length]) {
                visit(0, &buffer[..length]);
            }
        }
    }

    pub fn blocking(_on: bool) {}

    /// Escape on the serial line or the USB keyboard.
    pub fn break_requested() -> bool {
        crate::usb_kbd::poll();
        let mut escape = false;
        while let Some(key) = crate::arch::aarch64::pl011::read_byte().or_else(crate::usb_kbd::pop) {
            escape |= key == 0x1b;
        }
        escape
    }

    pub fn pet() {}

    /// One 4 MiB transfer buffer, allocated on first use and kept.
    pub fn staging() -> Option<&'static mut [u8]> {
        static mut STAGING: u64 = 0;
        unsafe {
            if STAGING == 0 {
                STAGING = crate::memory::allocate_contiguous(STAGE_BYTES as u64 / crate::memory::FRAME, None)?;
            }
            Some(core::slice::from_raw_parts_mut(STAGING as *mut u8, STAGE_BYTES))
        }
    }

    pub fn served() -> bool {
        true
    }

    pub fn kup_busy() -> bool {
        crate::netservice::kup_transfer_alive()
    }

    pub fn active_nic() -> usize {
        0
    }

    /// The `hostname` file of the boot volume, read once.
    pub fn hostname_file() -> Option<&'static str> {
        static mut TEXT: [u8; 18] = [0; 18];
        static mut LENGTH: usize = usize::MAX;
        unsafe {
            if LENGTH == usize::MAX {
                LENGTH = 0;
                if let Some(volume) = crate::storage::boot_volume() {
                    if let Ok(Some(read)) = volume.read(b"hostname", &mut *core::ptr::addr_of_mut!(TEXT)) {
                        LENGTH = read;
                    }
                }
            }
            let text = &*core::ptr::addr_of!(TEXT);
            let text = core::str::from_utf8(&text[..LENGTH]).ok()?.trim();
            (!text.is_empty()).then_some(text)
        }
    }
}
