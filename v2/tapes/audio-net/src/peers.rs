//! Announcements and the peer list. Every `audio-net` that streams over RTP announces itself on the SAP group
//! (239.255.255.255:9875, RFC 2974): a sender with an SDP of what it sends (`a=sendonly`), a receiver with an SDP of
//! what it accepts (`a=recvonly`), both under a name (`name=`, default `fos-` and the last three bytes of the MAC).
//! `audio-net list` shows what has been heard; a receiver can say `source=NAME` instead of describing the stream
//! (the SDP supplies codec, channels, port and multicast group), and a sender `to=NAME` instead of an address.
//!
//! The announcement is repeated every 5 s, and a withdrawal (SAP's deletion flag) is sent when the tape stops. A peer
//! that is switched off without saying so is simply not heard again; `list` shows only what it heard during its run.

use crate::igmp::Igmp;
use crate::net::{self, Inbound, NetConfig};
use crate::{MacText, Options, Proto, Transport};
use audio_wire::sap::{self, Codec, Session};
use audio_wire::{Sdp, SdpOpus, multicast_mac, write_sdp, write_sdp_opus};
use tape_net::{Ipv4Address, MacAddress};
use tape_rt::{TapeApi, println};

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

/// The name this machine announces under: `name=` or `fos-` and the last three MAC bytes.
pub fn session_name<'a>(configured: &str, mac: [u8; 6], buffer: &'a mut [u8; sap::MAX_NAME]) -> &'a str {
    let mut length = 0;
    let mut push = |byte: u8, length: &mut usize| {
        if *length < buffer.len() {
            buffer[*length] = byte;
            *length += 1;
        }
    };
    if configured.is_empty() {
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
pub fn receiver_sdp(options: &Options, config: &NetConfig, name: &str, out: &mut [u8]) -> usize {
    let destination = net::parse_ip(options.group).unwrap_or(config.ip).0;
    let session = (tape_rt::micros() >> 20) as u32;
    if options.opus {
        write_sdp_opus(&SdpOpus { name, receive_only: true, origin: config.ip.0, destination, port: options.port, payload_type: options.payload_type, channels: options.chan, packet_ms_x10: options.frame_x10, session }, out)
    } else {
        write_sdp(&Sdp { name, receive_only: true, origin: config.ip.0, destination, port: options.port, payload_type: options.payload_type, channels: options.chan, rate: options.rate, bits: options.bits, packet_us: 1000, mac: config.mac.0, session }, out)
    }
}

#[derive(Clone, Copy)]
pub struct Entry {
    pub session: Session,
    /// Who sent the announcement, and from which MAC (a router's, if it came across one).
    pub from_ip: [u8; 4],
    pub from_mac: [u8; 6],
}

pub struct Directory {
    entries: [Option<Entry>; MAX_ENTRIES],
}

impl Directory {
    pub fn new() -> Directory {
        Directory { entries: [None; MAX_ENTRIES] }
    }

    fn update(&mut self, packet: &sap::Packet, from_mac: [u8; 6]) {
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
        let entry = Entry { session, from_ip: packet.origin, from_mac };
        if let Some(slot) = self.entries.iter_mut().find(|slot| slot.as_ref().is_some_and(same)) {
            *slot = Some(entry);
        } else if let Some(slot) = self.entries.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(entry);
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
    let seconds = if options.seconds != 0 { options.seconds } else { 12 };
    println!("Listening for announcements for {seconds} s (Escape stops)...");
    let directory = listen(api, options, &mut transport, seconds, |_| false);
    let mut count = 0;
    println!(" #  Name                     Role      From (IP, MAC)                    Codec  Ch  Rate   Address:port");
    for entry in directory.iter() {
        count += 1;
        let (s, ip, d) = (&entry.session, entry.from_ip, entry.session.destination);
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
    if count == 0 {
        println!("No announcements heard. A peer announces only while it streams over RTP (proto=rtp) on this network.");
    }
}

/// Waits (up to 15 s) for the announcement of `name` as a sender (`receivers` false) or a listener (true).
pub fn find(api: &'static TapeApi, options: &Options, transport: &mut Transport, name: &str, receivers: bool) -> Option<Entry> {
    println!("Looking for the announcement of {} '{name}'...", if receivers { "the listener" } else { "the sender" });
    let directory = listen(api, options, transport, 15, |directory| directory.find(name, receivers).is_some());
    let found = directory.find(name, receivers);
    match found {
        Some(entry) => {
            let (ip, s) = (entry.from_ip, &entry.session);
            println!("Found '{name}': {}.{}.{}.{} (MAC {}), {} {} ch, {} Hz, port {}.", ip[0], ip[1], ip[2], ip[3], MacText(entry.from_mac), codec_text(s), s.channels, s.rate, s.port);
        }
        None => println!("audio-net: no announcement from '{name}' within 15 s."),
    }
    found
}

static mut TEXT_BUFFERS: [[u8; 16]; 2] = [[0; 16]; 2];

/// A static text of an address, for an `Options` field that borrows its text.
fn address_text(slot: usize, address: [u8; 4]) -> &'static str {
    use core::fmt::Write;
    let buffers: &'static mut [[u8; 16]; 2] = unsafe { &mut *(&raw mut TEXT_BUFFERS) };
    let buffer: &'static mut [u8; 16] = &mut buffers[slot];
    let mut cursor = audio_wire::Cursor { bytes: &mut buffer[..], length: 0 };
    let _ = write!(cursor, "{}.{}.{}.{}", address[0], address[1], address[2], address[3]);
    let length = cursor.length;
    core::str::from_utf8(&buffer[..length]).unwrap_or("")
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
    if audio_wire::is_multicast_address(s.destination) {
        options.group = address_text(0, s.destination);
        Ok(())
    } else if s.destination == my_ip {
        Ok(())
    } else {
        Err("the stream is addressed to another machine, not to this one")
    }
}

/// `to=NAME`: the destination from a listener's announcement (its address or its group, and its port).
pub fn adopt_listener(options: &mut Options<'static>, entry: &Entry) {
    let s = &entry.session;
    options.to = address_text(1, s.destination);
    options.port = s.port;
    options.payload_type = s.payload_type;
    options.opus = s.codec == Codec::Opus;
    if s.codec != Codec::Opus {
        options.bits = if s.codec == Codec::L16 { 16 } else { 24 };
    }
    options.rate = s.rate;
}
