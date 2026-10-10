//! Announcements and the peer list. Every `audio-net` that streams over RTP announces itself on the SAP group
//! (239.255.255.255:9875, RFC 2974): a sender with an SDP of what it sends (`a=sendonly`), a receiver with an SDP of
//! what it accepts (`a=recvonly`), both under a name (`name=`, default `fos-` and the last three bytes of the MAC).
//! `audio-net list` shows what has been heard; a receiver can say `source=NAME` instead of describing the stream
//! (the SDP supplies codec, channels, port and multicast group), and a sender `to=NAME` instead of an address.
//!
//! **Layer 2.** A machine on raw Ethernet or AVTP has no IP address, so the same announcements travel in plain Ethernet
//! frames (ethertype `ANNOUNCE_ETHERTYPE`, broadcast): a SAP packet whose SDP carries `a=x-fos-l2:raw|avtp`. A name then
//! stands for the MAC address the frame came from, and `to=NAME`, `source=NAME` and the panel's peer picker work there too.
//!
//! The announcement is repeated every 5 s, and a withdrawal (SAP's deletion flag) is sent when the tape stops. A peer
//! that is switched off without saying so is simply not heard again; `list` shows only what it heard during its run.

use crate::igmp::Igmp;
use crate::net::{self, Inbound, NetConfig};
use crate::{MacText, Options, Proto, Transport};
use audio_wire::sap::{self, Codec, Session};
use audio_wire::{Sdp, SdpOpus, multicast_mac, write_sdp, write_sdp_opus};
use tape_net::{Ipv4Address, MacAddress};
use tape_rt::{TapeApi, print, println};

const ANNOUNCE_MICROS: u64 = 5_000_000;
const MAX_ENTRIES: usize = 24;

/// Sends one session's SAP announcement now and then.
pub struct Announcer {
    sdp: [u8; 1000],
    length: usize,
    id: u16,
    next: u64,
    active: bool,
}

impl Announcer {
    pub const fn idle() -> Announcer {
        Announcer { sdp: [0; 1000], length: 0, id: 0, next: 0, active: false }
    }

    pub fn start(&mut self, sdp: &[u8]) {
        let length = sdp.len().min(self.sdp.len());
        self.sdp[..length].copy_from_slice(&sdp[..length]);
        self.length = length;
        // The message id only has to change when the description does: a hash of its text.
        self.id = sdp.iter().fold(0x811cu32, |hash, &byte| hash.wrapping_mul(31).wrapping_add(u32::from(byte))) as u16;
        self.next = 0;
        self.active = true;
    }

    fn send(&self, api: &TapeApi, nic: usize, config: &NetConfig, delete: bool) {
        let mut packet = [0u8; 1100];
        let length = sap::encode(config.ip.0, delete, self.id, &self.sdp[..self.length], &mut packet);
        let mut frame = [0u8; 1514];
        if config.ip.0 == [0; 4] {
            // No address: a layer 2 network. The SAP packet goes straight into a broadcast Ethernet frame.
            frame[..6].fill(0xff);
            frame[6..12].copy_from_slice(&config.mac.0);
            frame[12..14].copy_from_slice(&audio_wire::ANNOUNCE_ETHERTYPE.to_be_bytes());
            frame[14..14 + length].copy_from_slice(&packet[..length]);
            (api.nic_send)(nic, frame.as_ptr(), (14 + length).max(60));
            return;
        }
        if let Some(total) = net::build_udp_frame(&mut frame, config, MacAddress(multicast_mac(sap::GROUP)), Ipv4Address(sap::GROUP), sap::PORT, sap::PORT, 16, &packet[..length]) {
            (api.nic_send)(nic, frame.as_ptr(), total);
        }
    }

    pub fn poll(&mut self, api: &TapeApi, nic: usize, config: &NetConfig) {
        if self.active && tape_rt::micros() >= self.next {
            self.next = tape_rt::micros() + ANNOUNCE_MICROS;
            self.send(api, nic, config, false);
        }
    }

    pub fn withdraw(&mut self, api: &TapeApi, nic: usize, config: &NetConfig) {
        if self.active {
            self.active = false;
            self.send(api, nic, config, true);
        }
    }
}

/// The other machines the kernel has heard announce themselves (`peer_info`, ABI 5); none on an older kernel.
pub fn kernel_peers(api: &TapeApi) -> impl Iterator<Item = tape_rt::PeerInfo> + '_ {
    let usable = api.version >= 5 && (api.size as usize) >= core::mem::size_of::<TapeApi>();
    let count = if usable { (api.peer_info)(0, core::ptr::null_mut()) } else { 0 };
    (1..=count).filter_map(move |number| {
        let mut info = tape_rt::PeerInfo::default();
        ((api.peer_info)(number, &mut info) >= number).then_some(info)
    })
}

/// The MAC of the machine the kernel knows as `name` (case ignored), if it has announced itself.
pub fn kernel_peer(api: &TapeApi, name: &str) -> Option<tape_rt::PeerInfo> {
    kernel_peers(api).find(|peer| peer.name().eq_ignore_ascii_case(name))
}

/// The name this machine announces under: `name=`, else the kernel's name, else `fos-` and the last three MAC bytes.
pub fn session_name<'a>(configured: &str, mac: [u8; 6], buffer: &'a mut [u8; sap::MAX_NAME]) -> &'a str {
    let mut length = 0;
    let mut push = |byte: u8, length: &mut usize| {
        if *length < buffer.len() {
            buffer[*length] = byte;
            *length += 1;
        }
    };
    // The kernel's name for the interface that has this MAC (the `hostname` file's, or `fos-` and the end of the MAC).
    let mut kernel_name = [0u8; 20];
    let kernel_length = match crate::nic_by_mac(mac) {
        Some(nic) => (tape_rt::api().hostname)(nic, kernel_name.as_mut_ptr(), kernel_name.len()),
        None => 0,
    };
    let default = &kernel_name[..kernel_length];
    if configured.is_empty() && !default.is_empty() {
        for &byte in default {
            push(byte, &mut length);
        }
    } else if configured.is_empty() {
        for &byte in b"fos-" {
            push(byte, &mut length);
        }
        for &byte in &mac[3..] {
            for nibble in [byte >> 4, byte & 15] {
                push(b"0123456789abcdef"[usize::from(nibble)], &mut length);
            }
        }
    } else {
        for &byte in configured.as_bytes() {
            push(byte, &mut length);
        }
    }
    core::str::from_utf8(&buffer[..length]).unwrap_or("fos")
}

/// A receiver's announcement: where and how it listens. `destination` is the multicast group when it joined one,
/// else its own address.
pub fn receiver_sdp(options: &Options, config: &NetConfig, name: &str, idle: bool, out: &mut [u8]) -> usize {
    let destination = net::parse_ip(options.group).unwrap_or(config.ip).0;
    let session = (tape_rt::micros() >> 20) as u32;
    if options.opus {
        write_sdp_opus(&SdpOpus { name, receive_only: true, idle, origin: config.ip.0, destination, port: options.port, payload_type: options.payload_type, channels: options.chan, packet_ms_x10: options.frame_x10, session }, out)
    } else {
        write_sdp(&Sdp { name, receive_only: true, idle, origin: config.ip.0, destination, port: options.port, payload_type: options.payload_type, channels: options.chan, rate: options.rate, bits: options.bits, packet_us: 1000, mac: config.mac.0, ptp: None, session }, out)
    }
}

/// A layer 2 announcement: what this machine sends (`receive_only` false) or accepts, as plain PCM (raw Ethernet and AVTP
/// carry nothing else), with the marker that says which kind of network it is on.
pub fn l2_sdp(options: &Options, mac: [u8; 6], name: &str, receive_only: bool, idle: bool, channels: usize, out: &mut [u8]) -> usize {
    let session = (tape_rt::micros() >> 20) as u32;
    let mut length = write_sdp(&Sdp { name, receive_only, idle, origin: [0; 4], destination: [0; 4], port: 0, payload_type: options.payload_type, channels, rate: options.rate, bits: options.bits, packet_us: 1000, mac, ptp: None, session }, out);
    let marker: &[u8] = if options.proto == Proto::Avtp { b"a=x-fos-l2:avtp\r\n" } else { b"a=x-fos-l2:raw\r\n" };
    out[length..length + marker.len()].copy_from_slice(marker);
    length += marker.len();
    length
}

#[derive(Clone, Copy)]
pub struct Entry {
    pub session: Session,
    /// Who sent the announcement, and from which MAC (a router's, if it came across one).
    pub from_ip: [u8; 4],
    pub from_mac: [u8; 6],
    /// When (micros) the announcement was last heard, so a machine that went away drops out of a long-lived list.
    pub seen: u64,
}

pub struct Directory {
    entries: [Option<Entry>; MAX_ENTRIES],
}

impl Directory {
    pub fn new() -> Directory {
        Directory { entries: [None; MAX_ENTRIES] }
    }

    pub fn update(&mut self, packet: &sap::Packet, from_mac: [u8; 6]) {
        let Some(session) = sap::parse_sdp(packet.sdp) else { return };
        let same = |entry: &Entry| entry.from_ip == packet.origin && entry.session.name() == session.name() && entry.session.receive_only == session.receive_only;
        if packet.delete {
            for slot in self.entries.iter_mut() {
                if slot.as_ref().is_some_and(same) {
                    *slot = None;
                }
            }
            return;
        }
        let entry = Entry { session, from_ip: packet.origin, from_mac, seen: tape_rt::micros() };
        if let Some(slot) = self.entries.iter_mut().find(|slot| slot.as_ref().is_some_and(same)) {
            *slot = Some(entry);
        } else if let Some(slot) = self.entries.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(entry);
        }
    }

    /// Forgets announcements not heard for `micros` (they are repeated every 5 s).
    pub fn expire(&mut self, micros: u64) {
        let now = tape_rt::micros();
        for slot in self.entries.iter_mut() {
            if slot.as_ref().is_some_and(|entry| now.saturating_sub(entry.seen) > micros) {
                *slot = None;
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().flatten()
    }

    pub fn find(&self, name: &str, receivers: bool) -> Option<Entry> {
        self.iter().find(|entry| entry.session.name() == name && entry.session.receive_only == receivers).copied()
    }
}

/// Listens for announcements for up to `seconds` (or until `done` says the directory has what it needs). Escape ends
/// it early. The SAP group is joined with IGMP, so a switch that snoops forwards the announcements to this port.
pub fn listen(api: &TapeApi, options: &Options, transport: &mut Transport, seconds: u64, mut done: impl FnMut(&Directory) -> bool) -> Directory {
    let mut directory = Directory::new();
    if transport.config.ip.0 == [0; 4] {
        return listen_l2(api, options, transport, seconds, directory, done);
    }
    let mut igmp = Igmp::new(sap::GROUP);
    igmp.join(api, options.nic, &transport.config);
    let started = tape_rt::micros();
    let mut frame = [0u8; 1514];
    while tape_rt::micros() < started + seconds * 1_000_000 {
        if (api.should_abort)() != 0 {
            break;
        }
        igmp.poll(api, options.nic, &transport.config, tape_rt::micros());
        transport.announce_service(api, options.nic); // a peer that is being looked up for must stay findable meanwhile
        let length = (api.nic_receive)(options.nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            tape_rt::idle();
            continue;
        }
        if igmp.handle(&frame[..length], tape_rt::micros()) {
            continue;
        }
        let now = (api.ticks)() as u32;
        if let Inbound::Udp { source, payload, .. } = net::inbound(api, options.nic, &transport.config, &mut transport.arp, &frame[..length], sap::PORT..=sap::PORT, now) {
            let own = source == transport.config.ip;
            if let Some(packet) = sap::parse(payload).filter(|_| !own) {
                directory.update(&packet, frame[6..12].try_into().unwrap_or([0; 6]));
                if done(&directory) {
                    break;
                }
            }
        }
    }
    igmp.leave(api, options.nic, &transport.config);
    directory
}

/// `listen` on a layer 2 network: the announcements arrive as Ethernet frames of `ANNOUNCE_ETHERTYPE`.
fn listen_l2(api: &TapeApi, options: &Options, transport: &mut Transport, seconds: u64, mut directory: Directory, mut done: impl FnMut(&Directory) -> bool) -> Directory {
    let started = tape_rt::micros();
    let mut frame = [0u8; 1514];
    while tape_rt::micros() < started + seconds * 1_000_000 {
        if (api.should_abort)() != 0 {
            break;
        }
        transport.announce_service(api, options.nic); // a peer that is being looked up for must stay findable meanwhile
        let length = (api.nic_receive)(options.nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            tape_rt::idle();
            continue;
        }
        if let Some(entry_mac) = l2_announcement(&frame[..length], transport.mac, &mut directory) {
            let _ = entry_mac;
            if done(&directory) {
                break;
            }
        }
    }
    directory
}

/// Reads one received frame as a layer 2 announcement into `directory`; the sender's MAC if it was one (and not our own).
pub fn l2_announcement(frame: &[u8], own_mac: [u8; 6], directory: &mut Directory) -> Option<[u8; 6]> {
    if frame.len() < 15 || frame[12..14] != audio_wire::ANNOUNCE_ETHERTYPE.to_be_bytes() {
        return None;
    }
    let mac: [u8; 6] = frame[6..12].try_into().ok()?;
    if mac == own_mac {
        return None;
    }
    let packet = sap::parse(&frame[14..])?;
    directory.update(&packet, mac);
    Some(mac)
}

fn codec_text(session: &Session) -> &'static str {
    match session.codec {
        Codec::L16 => "L16",
        Codec::L24 => "L24",
        Codec::Opus => "Opus",
    }
}

/// `audio-net list`: what has been announced, as a table.
pub fn list(api: &'static TapeApi, options: &Options) {
    let Some(mut transport) = Transport::open(api, options) else { return };
    let layer_2 = transport.config.ip.0 == [0; 4];
    let seconds = if options.seconds != 0 { options.seconds } else { 12 };
    println!("Listening for announcements for {seconds} s (Escape stops)...");
    let directory = listen(api, options, &mut transport, seconds, |_| false);
    let mut count = 0;
    println!(" #  Name                     Role      From (IP, MAC)                    Codec  Ch  Rate   Address:port");
    for entry in directory.iter() {
        count += 1;
        let (s, ip, d) = (&entry.session, entry.from_ip, entry.session.destination);
        if layer_2 {
            println!("{count:>2}  {:<24} {:<9} {}  {:<5} {:>2}  {:>5}  ({} network)", s.name(), if s.receive_only { "listens" } else { "sends" }, MacText(entry.from_mac), codec_text(s), s.channels, s.rate, if s.l2 == 2 { "AVTP" } else { "raw Ethernet" });
            continue;
        }
        let you = if d == transport.config.ip.0 { " (this machine)" } else { "" };
        println!(
            "{count:>2}  {:<24} {:<9} {}.{}.{}.{} {}  {:<5} {:>2}  {:>5}  {}.{}.{}.{}:{}{}",
            s.name(),
            if s.receive_only { "listens" } else { "sends" },
            ip[0], ip[1], ip[2], ip[3],
            MacText(entry.from_mac),
            codec_text(s),
            s.channels,
            s.rate,
            d[0], d[1], d[2], d[3],
            s.port,
            you
        );
    }
    let mut machines = 0;
    for peer in kernel_peers(api) {
        if machines == 0 {
            println!("Machines the kernel has heard announce their names (usable as `to=NAME` on raw Ethernet / AVTP):");
        }
        machines += 1;
        println!("{machines:>2}  {:<24} {}  (NIC {})", peer.name(), MacText(peer.mac), usize::from(peer.nic) + 1);
    }
    if count == 0 {
        println!("No announcements heard. A peer announces while it streams (or has its panel open) on the same kind of network ({}).", if layer_2 { "raw Ethernet or AVTP, same switch" } else { "RTP" });
    }
}

/// Waits (up to 15 s) for the announcement of `name` as a sender (`receivers` false) or a listener (true).
pub fn find(api: &'static TapeApi, options: &Options, transport: &mut Transport, name: &str, receivers: bool) -> Option<Entry> {
    println!("Looking for the announcement of {} '{name}'...", if receivers { "the listener" } else { "the sender" });
    let directory = listen(api, options, transport, 15, |directory| directory.find(name, receivers).is_some());
    let found = directory.find(name, receivers);
    match found {
        Some(entry) if entry.session.l2 != 0 => {
            let s = &entry.session;
            println!("Found '{name}': MAC {}, {} {} ch, {} Hz.", MacText(entry.from_mac), codec_text(s), s.channels, s.rate);
        }
        Some(entry) => {
            let (ip, s) = (entry.from_ip, &entry.session);
            println!("Found '{name}': {}.{}.{}.{} (MAC {}), {} {} ch, {} Hz, port {}.", ip[0], ip[1], ip[2], ip[3], MacText(entry.from_mac), codec_text(s), s.channels, s.rate, s.port);
        }
        None => println!("audio-net: no announcement from '{name}' within 15 s."),
    }
    found
}

static mut TEXT_BUFFERS: [[u8; 16]; 3] = [[0; 16]; 3];

/// A static text of an address, for an `Options` field that borrows its text.
fn address_text(slot: usize, address: [u8; 4]) -> &'static str {
    use core::fmt::Write;
    let buffers: &'static mut [[u8; 16]; 3] = unsafe { &mut *(&raw mut TEXT_BUFFERS) };
    let buffer: &'static mut [u8; 16] = &mut buffers[slot];
    let mut cursor = audio_wire::Cursor { bytes: &mut buffer[..], length: 0 };
    let _ = write!(cursor, "{}.{}.{}.{}", address[0], address[1], address[2], address[3]);
    let length = cursor.length;
    core::str::from_utf8(&buffer[..length]).unwrap_or("")
}

static mut MAC_BUFFER: [u8; 17] = [0; 17];

/// A static text of a MAC address, for `options.to` (which borrows its text).
fn mac_text(mac: [u8; 6]) -> &'static str {
    use core::fmt::Write;
    let buffer: &'static mut [u8; 17] = unsafe { &mut *(&raw mut MAC_BUFFER) };
    let mut cursor = audio_wire::Cursor { bytes: &mut buffer[..], length: 0 };
    let _ = write!(cursor, "{}", MacText(mac));
    let length = cursor.length;
    core::str::from_utf8(&buffer[..length]).unwrap_or("")
}

/// `source=NAME` on a layer 2 network: receive from the MAC that announced the stream, in the format it announced.
pub fn adopt_sender_l2(options: &mut Options<'static>, entry: &Entry) {
    let s = &entry.session;
    options.from = Some(entry.from_mac);
    options.chan = s.channels;
    options.bits = if s.codec == Codec::L16 { 16 } else { 24 };
    options.rate = s.rate;
    options.adopted_receive = true;
}

/// `to=NAME` on a layer 2 network: send to the MAC that announced it listens.
pub fn adopt_listener_l2(options: &mut Options<'static>, entry: &Entry) {
    options.to = mac_text(entry.from_mac);
}

/// `to=NAME` resolved by the kernel's peer table: send to that MAC.
pub fn set_listener_mac(options: &mut Options<'static>, mac: [u8; 6]) {
    options.to = mac_text(mac);
}

/// `source=NAME`: fills in the receive options from a sender's announcement. Err says why it cannot be received.
pub fn adopt_sender(options: &mut Options<'static>, entry: &Entry, my_ip: [u8; 4]) -> Result<(), &'static str> {
    let s = &entry.session;
    options.proto = Proto::Rtp;
    options.opus = s.codec == Codec::Opus;
    options.chan = s.channels;
    options.bits = if s.codec == Codec::L16 { 16 } else { 24 };
    options.rate = s.rate;
    options.port = s.port;
    options.payload_type = s.payload_type;
    options.adopted_receive = true;
    if audio_wire::is_multicast_address(s.destination) {
        options.group = address_text(0, s.destination);
        Ok(())
    } else if s.destination == my_ip {
        Ok(())
    } else {
        Err("the stream is addressed to another machine, not to this one")
    }
}

/// `to=NAME`: where to send, from a listener's announcement (its address or group, and its port). The codec stays what
/// this machine was set to send: the sender decides, and a receiver that picked this sender by name follows it
/// (`adopt_sender`). A listener that is only waiting on a port cannot follow, so a different codec is reported.
pub fn adopt_listener(options: &mut Options<'static>, entry: &Entry) {
    let s = &entry.session;
    options.to = address_text(1, s.destination);
    options.port = s.port;
    options.payload_type = s.payload_type;
    let expects_opus = s.codec == Codec::Opus;
    if expects_opus != options.opus {
        tape_rt::println!(
            "Note: '{}' was set to listen for {}; this machine sends {}. It follows if it picked this sender by name.",
            s.name(),
            if expects_opus { "Opus" } else { "PCM" },
            if options.opus { "Opus" } else { "PCM" }
        );
    }
}

/// A multicast group for a stream, chosen from its name: 239.192.x.y (the organisation-local scope, which routers do
/// not forward). The same name always gives the same group; `taken` says whether another stream already uses one,
/// and the next address is tried then.
pub fn auto_group(name: &str, taken: impl Fn([u8; 4]) -> bool) -> [u8; 4] {
    let hash = name.bytes().fold(0x811c_9dc5u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193));
    let mut n = ((hash >> 16) ^ hash) as u16;
    loop {
        let group = [239, 192, (n >> 8) as u8, n as u8];
        if n != 0 && !taken(group) {
            return group;
        }
        n = n.wrapping_add(1);
    }
}

/// `to=` left empty (or `auto`): the group for the stream called `name`; `group=` is used as it is when set.
/// Otherwise listens 2 s for streams already announced so that the choice avoids their groups.
pub fn choose_group(api: &'static TapeApi, options: &Options<'static>, transport: &mut Transport, name: &str) -> &'static str {
    if !options.group.is_empty() {
        println!("Sending to the multicast group {}.", options.group);
        return options.group;
    }
    println!("Choosing a multicast group for '{name}' (listening 2 s for the groups in use)...");
    let directory = listen(api, options, transport, 2, |_| false);
    let group = auto_group(name, |group| directory.iter().any(|entry| !entry.session.receive_only && entry.session.destination == group && entry.session.name() != name));
    println!("Sending to the multicast group {}.{}.{}.{} (receivers find it from the announcement).", group[0], group[1], group[2], group[3]);
    address_text(2, group)
}

/// `source=auto`: the one RTP stream announced to a multicast group or to this machine. Waits up to 15 s for it.
/// None (after saying why) when there is none yet, or several (then the sender's name must be chosen).
pub fn find_auto(api: &'static TapeApi, options: &Options, transport: &mut Transport) -> Option<Entry> {
    println!("Looking for an announced stream...");
    let my_ip = transport.config.ip.0;
    let wanted = |entry: &&Entry| !entry.session.receive_only && entry.session.l2 == 0 && (audio_wire::is_multicast_address(entry.session.destination) || entry.session.destination == my_ip);
    let started = tape_rt::micros();
    let directory = listen(api, options, transport, 15, |directory| {
        let count = directory.iter().filter(wanted).count();
        count > 1 || (count == 1 && tape_rt::micros() >= started + 2_000_000)
    });
    let mut found = directory.iter().filter(wanted);
    let first = found.next().copied();
    if found.next().is_some() {
        print!("audio-net: several streams are announced: ");
        for entry in directory.iter().filter(wanted) {
            print!("'{}' ", entry.session.name());
        }
        println!("- choose one (Peer, or source=NAME).");
        return None;
    }
    match first {
        Some(entry) => println!("Found '{}'.", entry.session.name()),
        None => println!("No stream announced yet; waiting for a sender."),
    }
    first
}
