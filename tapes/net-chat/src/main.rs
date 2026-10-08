#![no_std]
#![no_main]

//! A 32-bit Rust reimplementation of fos's raw-Ethernet `chat` command.
//!
//! This tape exercises the kernel-managed network callbacks appended to the
//! tape ABI: the kernel keeps ownership of PCI discovery, NIC DMA
//! buffers, and receive-ring recovery, while the tape builds and consumes
//! ordinary Ethernet frames. It deliberately uses the shell's existing
//! `CHAT_ETHERTYPE`/tag wire format, so it can exchange text and discovery
//! probes with `chat`, `nic discover`, and another copy of this tape.
//!
//! `tape run` starts broadcast chat. `tape run AA:BB:CC:DD:EE:FF` selects a
//! unicast peer. `/discover` broadcasts a discovery probe, `/peers` prints
//! the small in-tape address book, `/to MAC` changes the unicast target, and
//! `/broadcast` returns to broadcast mode. Other completed lines are sent as
//! chat text; `/quit` or the usual `read_key` abort event returns to fos.
//!
//! Like every `read_key` tape, input works from a local PS/2 keyboard or an
//! active `term net` session. The callback and raw `nic_receive` cooperate
//! in the kernel: terminal-control frames remain keyboard events, while chat
//! frames remain visible here.

use tape_rt::print_bytes;

const MAX_FRAME_LEN: usize = 1514;
const CHAT_TEXT_CAPACITY: usize = 200;
const MAX_PEERS: usize = 16;
const BROADCAST_MAC: [u8; 6] = [0xff; 6];
const CHAT_ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const CHAT_TAG_TEXT: u8 = 0x01;
const CHAT_TAG_DISCOVER_REQUEST: u8 = 0x02;
const CHAT_TAG_DISCOVER_REPLY: u8 = 0x03;

const TAPE_EVENT_NONE: u32 = 0;
const TAPE_EVENT_ABORT: u32 = 1;
const TAPE_EVENT_CHAR_BASE: u32 = 0x100;

type Putc = extern "C" fn(u32);
type NicMac = fn(usize) -> u32;
type NicSend = fn(usize, u32) -> u32;
type NicReceive = fn(usize, u32) -> u32;

fn print_hex_byte(putc: Putc, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    putc(u32::from(HEX[(byte >> 4) as usize]));
    putc(u32::from(HEX[(byte & 0x0f) as usize]));
}

fn print_mac(putc: Putc, mac: &[u8; 6]) {
    for (index, &byte) in mac.iter().enumerate() {
        if index != 0 {
            putc(u32::from(b':'));
        }
        print_hex_byte(putc, byte);
    }
}

/// Parses the exact six-octet form the shell's own `chat` command accepts.
/// The tape accepts it as a startup argument or as `/to MAC`, avoiding a
/// hidden dependency on the shell's private peer-number table.
fn parse_mac(text: &[u8]) -> Option<[u8; 6]> {
    if text.len() != 17 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (index, slot) in mac.iter_mut().enumerate() {
        let high = hex_value(text[index * 3])?;
        let low = hex_value(text[index * 3 + 1])?;
        if index < 5 && text[index * 3 + 2] != b':' {
            return None;
        }
        *slot = high << 4 | low;
    }
    Some(mac)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn remember_peer(peers: &mut [[u8; 6]; MAX_PEERS], count: &mut usize, mac: [u8; 6]) -> bool {
    if peers[..*count].contains(&mac) || *count == MAX_PEERS {
        return false;
    }
    peers[*count] = mac;
    *count += 1;
    true
}

fn print_peers(putc: Putc, peers: &[[u8; 6]; MAX_PEERS], count: usize) {
    if count == 0 {
        print_bytes(putc, b"No tape-chat peers known yet; type /discover.\n");
        return;
    }
    print_bytes(putc, b"Tape-chat peers:\n");
    for (index, peer) in peers[..count].iter().enumerate() {
        putc(u32::from(b' '));
        putc(u32::from(b'1' + index as u8));
        print_bytes(putc, b": ");
        print_mac(putc, peer);
        putc(u32::from(b'\n'));
    }
}

/// Emits one complete frame through the kernel callback. The tape never
/// touches PCI registers or DMA buffers itself; it only owns this small
/// stack-built Ethernet payload.
fn send_frame(
    nic_send: NicSend,
    destination: &[u8; 6],
    source: &[u8; 6],
    tag: u8,
    payload: &[u8],
) -> bool {
    let mut frame = [0u8; 15 + CHAT_TEXT_CAPACITY];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = tag;
    frame[15..15 + payload.len()].copy_from_slice(payload);
    nic_send(frame.as_ptr() as usize, (15 + payload.len()) as u32) != 0
}

fn send_discovery(nic_send: NicSend, source: &[u8; 6]) {
    let _ = send_frame(
        nic_send,
        &BROADCAST_MAC,
        source,
        CHAT_TAG_DISCOVER_REQUEST,
        b"",
    );
}

fn print_destination(putc: Putc, destination: Option<&[u8; 6]>) {
    match destination {
        Some(mac) => {
            print_bytes(putc, b"Chatting with ");
            print_mac(putc, mac);
            print_bytes(putc, b" only.\n");
        }
        None => print_bytes(putc, b"Broadcasting to everyone listening.\n"),
    }
}

/// Handles one received shell-compatible chat/discovery frame. The padded
/// zero bytes the active NIC adds to short Ethernet frames are removed before
/// display, matching `shell::handle_incoming_chat_frame`.
fn handle_frame(
    putc: Putc,
    nic_send: NicSend,
    frame: &[u8],
    own_mac: &[u8; 6],
    destination: Option<&[u8; 6]>,
    peers: &mut [[u8; 6]; MAX_PEERS],
    peer_count: &mut usize,
) {
    if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
        return;
    }
    let source: [u8; 6] = frame[6..12].try_into().unwrap();
    match frame[14] {
        CHAT_TAG_TEXT => {
            if let Some(destination) = destination {
                if &source != destination {
                    return;
                }
            }
            let payload = &frame[15..];
            let end = payload
                .iter()
                .rposition(|&byte| byte != 0)
                .map_or(0, |index| index + 1);
            print_bytes(putc, b"\n");
            print_mac(putc, &source);
            print_bytes(putc, b": ");
            print_bytes(putc, &payload[..end]);
            putc(u32::from(b'\n'));
        }
        CHAT_TAG_DISCOVER_REQUEST => {
            let new_peer = remember_peer(peers, peer_count, source);
            let _ = send_frame(nic_send, &source, own_mac, CHAT_TAG_DISCOVER_REPLY, b"");
            if new_peer {
                print_bytes(putc, b"\nDiscovered ");
                print_mac(putc, &source);
                print_bytes(putc, b" and sent its reply.\n");
            }
        }
        CHAT_TAG_DISCOVER_REPLY => {
            if remember_peer(peers, peer_count, source) {
                print_bytes(putc, b"\nFound peer ");
                print_mac(putc, &source);
                putc(u32::from(b'\n'));
            }
        }
        _ => {}
    }
}

fn command(
    putc: Putc,
    nic_send: NicSend,
    line: &[u8],
    own_mac: &[u8; 6],
    destination: &mut Option<[u8; 6]>,
    peers: &[[u8; 6]; MAX_PEERS],
    peer_count: usize,
) -> bool {
    if line == b"/quit" {
        return true;
    }
    if line == b"/discover" {
        send_discovery(nic_send, own_mac);
        print_bytes(putc, b"Sent a discovery probe.\n");
        return false;
    }
    if line == b"/peers" {
        print_peers(putc, peers, peer_count);
        return false;
    }
    if line == b"/broadcast" {
        *destination = None;
        print_destination(putc, None);
        return false;
    }
    if let Some(address) = line.strip_prefix(b"/to ") {
        match parse_mac(address) {
            Some(mac) => {
                *destination = Some(mac);
                print_destination(putc, destination.as_ref());
            }
            None => print_bytes(putc, b"Usage: /to AA:BB:CC:DD:EE:FF\n"),
        }
        return false;
    }

    let target = destination.as_ref().unwrap_or(&BROADCAST_MAC);
    if !send_frame(nic_send, target, own_mac, CHAT_TAG_TEXT, line) {
        print_bytes(
            putc,
            b"(message not sent: NIC unavailable, rejected, or timed out)\n",
        );
    }
    false
}

/// Declares every preceding ABI slot to reach the final NIC callbacks.
/// `read_key` is deliberately the sole input callback: it includes abort
/// events and must not race `should_abort` for the same keyboard/term-net
/// sources. `nic_mac` is checked first so a missing supported NIC is reported
/// clearly instead of becoming a silent stream of zero-length receives.
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static tape_rt::TapeApi) {
    tape_rt::init(api);
    use tape_rt::compat::*;
    let putc = api.putc;
    let args = tape_rt::args();
    let (args_ptr, args_len) = (args.as_ptr() as usize, args.len() as u32);

    let mut own_mac = [0u8; 6];
    if nic_mac(own_mac.as_mut_ptr() as usize) == 0 {
        print_bytes(
            putc,
            b"Network chat cannot start: no supported NIC is available.\n",
        );
        return;
    }

    let arguments =
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) };
    let mut destination = if arguments.is_empty() {
        None
    } else {
        match parse_mac(arguments) {
            Some(mac) => Some(mac),
            None => {
                print_bytes(putc, b"Usage: tape run [AA:BB:CC:DD:EE:FF]\n");
                return;
            }
        }
    };

    print_bytes(
        putc,
        b"fos tape chat: /discover, /peers, /to MAC, /broadcast, /quit.\n",
    );
    print_destination(putc, destination.as_ref());
    print_bytes(
        putc,
        b"Type a line and press Enter; F12 (or F4/F6/F7/F8/F10) exits.\n",
    );

    let mut line = [0u8; CHAT_TEXT_CAPACITY];
    let mut line_len = 0usize;
    let mut saw_carriage_return = false;
    let mut frame = [0u8; MAX_FRAME_LEN];
    let mut peers = [[0u8; 6]; MAX_PEERS];
    let mut peer_count = 0usize;

    loop {
        let event = read_key();
        // No key this pass: if no frame arrives either, sleep (below).
        let key_idle = event == TAPE_EVENT_NONE;
        match event {
            TAPE_EVENT_NONE => {}
            TAPE_EVENT_ABORT => break,
            event if event >= TAPE_EVENT_CHAR_BASE => {
                let byte = (event & 0xff) as u8;
                match byte {
                    b'\n' if saw_carriage_return => saw_carriage_return = false,
                    b'\r' | b'\n' => {
                        putc(u32::from(b'\n'));
                        if line_len > 0
                            && command(
                                putc,
                                nic_send,
                                &line[..line_len],
                                &own_mac,
                                &mut destination,
                                &peers,
                                peer_count,
                            )
                        {
                            break;
                        }
                        line_len = 0;
                        saw_carriage_return = byte == b'\r';
                    }
                    8 | 127 if line_len > 0 => {
                        line_len -= 1;
                        saw_carriage_return = false;
                    }
                    printable
                        if (0x20..=0x7e).contains(&printable) && line_len < CHAT_TEXT_CAPACITY =>
                    {
                        line[line_len] = printable;
                        line_len += 1;
                        saw_carriage_return = false;
                        putc(u32::from(printable));
                    }
                    printable if (0x20..=0x7e).contains(&printable) => {
                        print_bytes(putc, b"\nChat lines are limited to 200 characters.\n");
                        saw_carriage_return = false;
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        let received = nic_receive(frame.as_mut_ptr() as usize, MAX_FRAME_LEN as u32) as usize;
        if received != 0 {
            handle_frame(
                putc,
                nic_send,
                &frame[..received],
                &own_mac,
                destination.as_ref(),
                &mut peers,
                &mut peer_count,
            );
        } else if key_idle {
            tape_rt::idle();
        }
    }

    print_bytes(putc, b"Left tape chat; back at the fos shell.\n");
}
