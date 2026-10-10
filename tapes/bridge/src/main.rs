//! A small layer-2 learning bridge/switch.
//!
//! The kernel owns PCI discovery, DMA buffers, and every NIC driver. This
//! tape only uses the appended per-interface callbacks from the tape ABI:
//! it polls selected Ethernet interfaces, learns source-MAC locations, and
//! forwards frames to the correct egress interface or floods them when the
//! destination is broadcast, multicast, or not yet known.
//!
//! Run it with `tape run bridge` to bridge every initialized interface, or
//! `tape run bridge 1 2` to select specific one-based entries from `nic list`.
//! This is deliberately Ethernet-only: it does not inspect ARP, IPv4, UDP,
//! TCP, or any other protocol, and it does not route between IP networks.
//! Press F12 (or F4/F6/F7/F8/F10) to stop it.
#![no_std]
#![no_main]

use tape_rt::{print_bytes, print_u32};

const MAX_FRAME_LEN: usize = 1514;
const MAX_PORTS: usize = 8;
const MAX_LEARNED: usize = 32;
const ETHERNET_HEADER_LEN: usize = 14;
const BROADCAST_MAC: [u8; 6] = [0xff; 6];

type Putc = extern "C" fn(u32);
type NicSendAt = fn(u32, usize, u32) -> u32;

#[derive(Clone, Copy)]
struct LearnedMac {
    mac: [u8; 6],
    port: usize,
    valid: bool,
}

const ECHO_SLOTS: usize = 16;

/// Hashes of frames this bridge recently sent on one port. A segment that loops a sender's own frames back
/// to it (QEMU's multicast netdev does) would otherwise be learned from and re-flooded forever.
#[derive(Clone, Copy)]
struct Echoes {
    hashes: [u32; ECHO_SLOTS],
    valid: [bool; ECHO_SLOTS],
    next: usize,
}

const EMPTY_ECHOES: Echoes = Echoes {
    hashes: [0; ECHO_SLOTS],
    valid: [false; ECHO_SLOTS],
    next: 0,
};

fn frame_hash(frame: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for &byte in frame {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    hash
}

impl Echoes {
    fn remember(&mut self, frame: &[u8]) {
        self.hashes[self.next] = frame_hash(frame);
        self.valid[self.next] = true;
        self.next = (self.next + 1) % ECHO_SLOTS;
    }

    /// True (once per remembered send) if `frame` is a copy of one this port just sent.
    fn take(&mut self, frame: &[u8]) -> bool {
        let hash = frame_hash(frame);
        for slot in 0..ECHO_SLOTS {
            if self.valid[slot] && self.hashes[slot] == hash {
                self.valid[slot] = false;
                return true;
            }
        }
        false
    }
}

const EMPTY_LEARNED: LearnedMac = LearnedMac {
    mac: [0; 6],
    port: 0,
    valid: false,
};

fn print_mac(putc: Putc, mac: &[u8; 6]) {
    for (index, byte) in mac.iter().enumerate() {
        if index != 0 {
            putc(u32::from(b':'));
        }
        const HEX: &[u8; 16] = b"0123456789abcdef";
        putc(u32::from(HEX[usize::from(byte >> 4)]));
        putc(u32::from(HEX[usize::from(byte & 0x0f)]));
    }
}

fn print_port_list(putc: Putc, ports: &[u32], macs: &[[u8; 6]], count: usize) {
    print_bytes(putc, b"fos bridge: ports");
    for index in 0..count {
        print_bytes(putc, if index == 0 { b" " } else { b", " });
        print_u32(putc, ports[index] + 1);
        print_bytes(putc, b" (");
        print_mac(putc, &macs[index]);
        print_bytes(putc, b")");
    }
    print_bytes(
        putc,
        b"; learning Ethernet source MACs and forwarding raw frames. F12 (or F4/F6/F7/F8/F10) stops.\n",
    );
}

fn skip_spaces(input: &[u8], mut position: usize) -> usize {
    while position < input.len() && input[position] == b' ' {
        position += 1;
    }
    position
}

fn parse_number(input: &[u8], mut position: usize) -> Option<(u32, usize)> {
    position = skip_spaces(input, position);
    let start = position;
    let mut value = 0u32;
    while position < input.len() {
        let byte = input[position];
        if !(b'0'..=b'9').contains(&byte) {
            break;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
        position += 1;
    }
    if position == start {
        None
    } else {
        Some((value, position))
    }
}

/// Strips an optional leading `bridge` word (a whole word, not just a
/// prefix -- `bridged`/`bridge2` must not match), matching this ABI's
/// established "the tape's own name is an optional, ignored leading
/// word" convention (`router`'s `tape_argument_source` does the same for
/// `tape run router ...`). Without this, `tape run bridge 1 2` -- the
/// exact form this file's own module doc comment and usage string both
/// document -- would fail to parse at all (`parse_number` sees `'b'`
/// first and returns `None`), a real doc/parser mismatch found while
/// building AGENTS.md's cross-tape command-syntax reference.
fn strip_leading_bridge_word(arguments: &[u8]) -> &[u8] {
    match arguments.strip_prefix(b"bridge") {
        Some(rest) if rest.is_empty() || rest[0] == b' ' => rest,
        _ => arguments,
    }
}

fn parse_ports(
    arguments: &[u8],
    nic_count: usize,
    ports: &mut [u32; MAX_PORTS],
) -> Option<usize> {
    let arguments = strip_leading_bridge_word(arguments);
    let arguments = &arguments[skip_spaces(arguments, 0)..];
    if arguments.is_empty() {
        if nic_count > MAX_PORTS {
            return None;
        }
        let count = nic_count;
        for (index, port) in ports.iter_mut().take(count).enumerate() {
            *port = index as u32;
        }
        return (count >= 2).then_some(count);
    }

    let mut position = 0;
    let mut count = 0;
    while position < arguments.len() {
        let (number, next) = parse_number(arguments, position)?;
        if number == 0 || number as usize > nic_count || count == MAX_PORTS {
            return None;
        }
        let port = number - 1;
        if ports[..count].contains(&port) {
            return None;
        }
        ports[count] = port;
        count += 1;
        position = skip_spaces(arguments, next);
    }
    (count >= 2).then_some(count)
}

fn is_multicast_or_broadcast(mac: &[u8; 6]) -> bool {
    mac == &BROADCAST_MAC || mac[0] & 1 != 0
}

fn is_zero(mac: &[u8; 6]) -> bool {
    mac.iter().all(|&byte| byte == 0)
}

fn local_port(macs: &[[u8; 6]], count: usize, destination: &[u8; 6]) -> bool {
    macs[..count].iter().any(|mac| mac == destination)
}

fn learn(table: &mut [LearnedMac; MAX_LEARNED], source: [u8; 6], port: usize) {
    if is_zero(&source) || is_multicast_or_broadcast(&source) {
        return;
    }
    if let Some(entry) = table.iter_mut().find(|entry| entry.valid && entry.mac == source) {
        entry.port = port;
        return;
    }
    if let Some(entry) = table.iter_mut().find(|entry| !entry.valid) {
        *entry = LearnedMac {
            mac: source,
            port,
            valid: true,
        };
        return;
    }
    let slot = (usize::from(source[5])) % MAX_LEARNED;
    table[slot] = LearnedMac {
        mac: source,
        port,
        valid: true,
    };
}

fn learned_port(table: &[LearnedMac; MAX_LEARNED], destination: &[u8; 6]) -> Option<usize> {
    table
        .iter()
        .find(|entry| entry.valid && entry.mac == *destination)
        .map(|entry| entry.port)
}

fn forward_frame(
    nic_send_at: NicSendAt,
    ports: &[u32; MAX_PORTS],
    port_count: usize,
    ingress: usize,
    frame: &[u8],
    echoes: &mut [Echoes; MAX_PORTS],
    learned: Option<usize>,
    forwarded: &mut u32,
    dropped: &mut u32,
) {
    for egress in 0..port_count {
        if egress == ingress || learned.is_some_and(|port| port != egress) {
            continue;
        }
        if nic_send_at(ports[egress], frame.as_ptr() as usize, frame.len() as u32) != 0 {
            echoes[egress].remember(frame);
            *forwarded += 1;
        } else {
            *dropped += 1;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static tape_rt::TapeApi) {
    tape_rt::init(api);
    use tape_rt::compat::*;
    let putc = api.putc;
    let args = tape_rt::args();
    let (args_ptr, args_len) = (args.as_ptr() as usize, args.len() as u32);
    let nic_total = nic_count() as usize;
    if nic_total < 2 {
        print_bytes(putc, b"Bridge needs at least two initialized NICs.\n");
        return;
    }

    let arguments = if args_len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) }
    };
    let mut ports = [0u32; MAX_PORTS];
    let Some(port_count) = parse_ports(arguments, nic_total, &mut ports) else {
        print_bytes(
            putc,
            b"Usage: tape run bridge [NIC_NUMBER NIC_NUMBER ...] (use 'nic list'; at least two distinct NICs)\n",
        );
        return;
    };

    let max_frame_len = usize::try_from(nic_max_frame_len())
        .unwrap_or(MAX_FRAME_LEN)
        .min(MAX_FRAME_LEN);
    if max_frame_len < ETHERNET_HEADER_LEN {
        print_bytes(putc, b"Bridge cannot start: NIC frame limit is too small.\n");
        return;
    }

    let mut macs = [[0u8; 6]; MAX_PORTS];
    for index in 0..port_count {
        if nic_mac_at(ports[index], macs[index].as_mut_ptr() as usize) == 0 {
            print_bytes(putc, b"Bridge cannot start: failed to read a selected NIC MAC.\n");
            return;
        }
    }
    let mut default_mac = [0u8; 6];
    if nic_mac(default_mac.as_mut_ptr() as usize) == 0 {
        print_bytes(putc, b"Bridge cannot start: no active NIC is available.\n");
        return;
    }
    let _ = default_mac;

    for index in 0..port_count {
        if nic_set_promiscuous_at(ports[index], 1) == 0 {
            for previous in 0..index {
                let _ = nic_set_promiscuous_at(ports[previous], 0);
            }
            print_bytes(putc, b"Bridge cannot start: failed to enable promiscuous mode.\n");
            return;
        }
    }

    print_port_list(putc, &ports, &macs, port_count);
    let mut learned = [EMPTY_LEARNED; MAX_LEARNED];
    let mut echoes = [EMPTY_ECHOES; MAX_PORTS];
    let mut frame = [0u8; MAX_FRAME_LEN];
    let mut forwarded = 0u32;
    let mut dropped = 0u32;

    'bridge: loop {
        let mut received_any = false;
        for ingress in 0..port_count {
            if should_abort() != 0 {
                break 'bridge;
            }
            let received = nic_receive_at(
                ports[ingress],
                frame.as_mut_ptr() as usize,
                max_frame_len as u32,
            ) as usize;
            if received < ETHERNET_HEADER_LEN {
                continue;
            }
            received_any = true;
            if echoes[ingress].take(&frame[..received]) {
                continue;
            }
            let source: [u8; 6] = frame[6..12].try_into().unwrap();
            let destination: [u8; 6] = frame[0..6].try_into().unwrap();
            if local_port(&macs, port_count, &destination)
                || macs[..port_count].iter().any(|mac| mac == &source)
            {
                continue;
            }
            learn(&mut learned, source, ingress);
            let learned_destination = if is_multicast_or_broadcast(&destination) {
                None
            } else {
                learned_port(&learned, &destination)
            };
            forward_frame(
                nic_send_at,
                &ports,
                port_count,
                ingress,
                &frame[..received],
                &mut echoes,
                learned_destination,
                &mut forwarded,
                &mut dropped,
            );
        }
        if should_abort() != 0 {
            break;
        }
        // Idle pass: sleep until a frame arrives on any port (every NIC's
        // receive interrupt wakes the CPU) or the next timer tick.
        if !received_any {
            wait_for_event();
        }
    }

    for index in 0..port_count {
        let _ = nic_set_promiscuous_at(ports[index], 0);
    }
    print_bytes(putc, b"Bridge stopped; forwarded ");
    print_u32(putc, forwarded);
    print_bytes(putc, b" frame(s), dropped ");
    print_u32(putc, dropped);
    print_bytes(putc, b".\n");
}

tape_rt::tape_help!(br#"bridge: layer-2 learning bridge between network interfaces.

  tape run bridge          bridge every interface
  tape run bridge 1 2      only the one-based entries of `nic list`

  Forwards Ethernet frames unchanged; no IP routing. Escape stops.
"#);
