#![no_std]
#![no_main]

//! A dedicated raw-Ethernet audio-streaming tape for the ES1370 playback
//! facade (`kernel/src/audio.rs`).
//!
//! `tape run` (no arguments) is **listen mode**: it prints the locally
//! attached audio device's real format (queried through
//! `audio_sample_rate`/`audio_channels_and_bits`, never hardcoded), then
//! waits for any peer's `AUDIO_TAG_START` frame declaring a matching format,
//! locks onto that peer's MAC (mirroring `tape_load_net`'s "first sender
//! wins" convention), and forwards every subsequent `AUDIO_TAG_DATA`
//! payload straight into `audio_submit` until `AUDIO_TAG_STOP` arrives or
//! the operator gives up (F12/F4/F6/F7/F8/F10, local or remote via
//! `should_abort`).
//!
//! `tape run send NAME [AA:BB:CC:DD:EE:FF]` is **send mode**: it replays a
//! raw, headerless PCM file already sitting on FOSFS (fetched via
//! `fs_file_length`/`fs_read`, `NAME` accepting the usual bare-name or
//! `diskN/NAME` reference) to one peer or, if no MAC is given, broadcast.
//! This remains a useful, deterministic way to test the wire protocol and
//! the other side's playback path without needing a real microphone
//! attached to the sending machine.
//!
//! `tape run capture [AA:BB:CC:DD:EE:FF]` is **live capture mode**, the
//! real ADC-to-network counterpart to send mode: it reads whatever
//! `kernel/src/es1370.rs`'s ISR-driven ADC pipeline has actually captured
//! (via the `audio_capture` tape callback, `kernel/src/shell.rs`'s
//! `tape_audio_capture`) and streams it out live, one `AUDIO_TAG_DATA`
//! frame per non-empty poll, until the operator gives up. Paired with
//! another fos machine running plain `tape run` (listen mode), this is a
//! genuine, real-time microphone-to-speaker path over raw Ethernet --
//! ADC on one machine, DAC on the other -- with no file or pre-recording
//! involved on either side.
//!
//! The wire format reuses the existing shared `CHAT_ETHERTYPE` (0x88b6),
//! the same ethertype `chat`/`nic discover`/`tape`/`term net`/NETFS/KUP all
//! already share, with three new tag bytes continuing that numbering past
//! KUP's own highest (`0x3e`): `AUDIO_TAG_START` (0x40, sample rate + channel
//! count + bit depth), `AUDIO_TAG_DATA` (0x41, an explicit 16-bit length
//! prefix followed by raw PCM bytes -- unlike `CHAT_TAG_TEXT`/`TERM_TAG_DATA`,
//! trailing zero bytes can never be trimmed here, since 16-bit signed silence
//! is a legitimate, common PCM value), and `AUDIO_TAG_STOP` (0x42, no
//! payload). Like `TERM_TAG_DATA`, `AUDIO_TAG_DATA` carries no sequence
//! number: an occasional dropped frame is meant to read as a brief audio
//! glitch, not an aborted transfer, matching this protocol's real-time,
//! best-effort delivery model rather than `tape`'s own strict transfer
//! semantics.
//!
//! No resampling or format conversion happens anywhere in this tape: a
//! stream whose declared format does not match the local device's actual
//! format is rejected outright (with a clear message) rather than played
//! back at the wrong pitch/speed.

use core::slice;
use tape_rt::{print_bytes, print_u32};

const MAX_FRAME_LEN: usize = 1514;
const AUDIO_ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const AUDIO_TAG_START: u8 = 0x40;
const AUDIO_TAG_DATA: u8 = 0x41;
const AUDIO_TAG_STOP: u8 = 0x42;
const HEADER_LEN: usize = 15; // 6 (dst) + 6 (src) + 2 (ethertype) + 1 (tag)
const START_PAYLOAD_LEN: usize = 6; // sample_rate (u32 LE) + channels (u8) + bits (u8)
const DATA_LENGTH_PREFIX: usize = 2; // u16 LE byte count, since PCM silence is zero
const DATA_CHUNK_CAPACITY: usize = 1024; // comfortably under the Ethernet MTU with headers
const BROADCAST_MAC: [u8; 6] = [0xff; 6];

// Send mode's declared format: fixed at ES1370's own playback ceiling (see
// `kernel/src/audio.rs`), since this tape does not parse WAV headers or
// negotiate a format -- a raw PCM file uploaded for send mode must already
// be in this exact layout.
const SEND_SAMPLE_RATE: u32 = 44100;
const SEND_CHANNELS: u8 = 2;
const SEND_BITS: u8 = 16;
const SEND_BYTES_PER_SEC: u64 = SEND_SAMPLE_RATE as u64 * SEND_CHANNELS as u64 * (SEND_BITS as u64 / 8);
const PIT_HZ: u64 = 100;

// A raw PCM file for send mode is read whole into one allocator-backed
// buffer (the FOSFS tape callback ABI has no partial/offset read), capped
// well below what even a several-minute stereo recording needs.
const SEND_MAX_CAPACITY: u32 = 16 * 1024 * 1024;

const FS_OK: u32 = 0;
const FS_NOT_FOUND: u32 = 1;
const FS_INVALID_ARGUMENT: u32 = 2;
const FS_BUFFER_TOO_SMALL: u32 = 3;
const FS_DISK_ERROR: u32 = 4;

type Putc = extern "C" fn(u32);
type ShouldAbort = extern "C" fn() -> u32;
type Alloc = extern "C" fn(u32) -> u32;
type Free = extern "C" fn(u32, u32);
type NicMac = extern "C" fn(u32) -> u32;
type NicSend = extern "C" fn(u32, u32) -> u32;
type NicReceive = extern "C" fn(u32, u32) -> u32;
type Ticks = extern "C" fn() -> u64;
type FsFileLength = extern "C" fn(u32, u32, u32) -> u32;
type FsRead = extern "C" fn(u32, u32, u32, u32, u32) -> u32;
type AudioSampleRate = extern "C" fn() -> u32;
type AudioChannelsAndBits = extern "C" fn() -> u32;
type AudioSubmit = extern "C" fn(u32, u32) -> u32;
type AudioCapture = extern "C" fn(u32, u32) -> u32;

// One capture poll's worth of PCM bytes, matching send mode's own
// per-frame chunk size so both directions produce comparably sized
// AUDIO_TAG_DATA frames.
const CAPTURE_CHUNK_CAPACITY: usize = DATA_CHUNK_CAPACITY;

fn print_mac(putc: Putc, mac: &[u8; 6]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (index, &byte) in mac.iter().enumerate() {
        if index != 0 {
            putc(u32::from(b':'));
        }
        putc(u32::from(HEX[(byte >> 4) as usize]));
        putc(u32::from(HEX[(byte & 0x0f) as usize]));
    }
}

/// Parses the same six-octet form every other tape's `parse_mac` accepts.
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

fn print_format(putc: Putc, sample_rate: u32, channels: u8, bits: u8) {
    print_u32(putc, sample_rate);
    print_bytes(putc, b" Hz / ");
    print_u32(putc, u32::from(channels));
    print_bytes(putc, b"ch / ");
    print_u32(putc, u32::from(bits));
    print_bytes(putc, b"-bit");
}

fn send_frame(nic_send: NicSend, destination: &[u8; 6], source: &[u8; 6], tag: u8, payload: &[u8]) {
    let mut frame = [0u8; HEADER_LEN + DATA_LENGTH_PREFIX + DATA_CHUNK_CAPACITY];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&AUDIO_ETHERTYPE);
    frame[14] = tag;
    frame[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
    let _ = nic_send(frame.as_ptr() as u32, (HEADER_LEN + payload.len()) as u32);
}

fn split_words(arguments: &[u8]) -> impl Iterator<Item = &[u8]> {
    arguments
        .split(|&byte| byte == b' ')
        .filter(|word| !word.is_empty())
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: Putc,
    should_abort: ShouldAbort,
    alloc: Alloc,
    free: Free,
    _read_key: extern "C" fn() -> u32,
    args_ptr: u32,
    args_len: u32,
    nic_mac: NicMac,
    nic_send: NicSend,
    nic_receive: NicReceive,
    _nic_max_frame_len: extern "C" fn() -> u32,
    ticks: Ticks,
    _nic_count: extern "C" fn() -> u32,
    _nic_mac_at: extern "C" fn(u32, u32) -> u32,
    _nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
    _nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
    _nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
    _terminal_position: extern "C" fn() -> u32,
    fs_file_length: FsFileLength,
    fs_read: FsRead,
    _fs_write: extern "C" fn(u32, u32, u32, u32) -> u32,
    _fs_remove: extern "C" fn(u32, u32) -> u32,
    audio_sample_rate: AudioSampleRate,
    audio_channels_and_bits: AudioChannelsAndBits,
    audio_submit: AudioSubmit,
    audio_capture: AudioCapture,
) {
    let mut own_mac = [0u8; 6];
    if nic_mac(own_mac.as_mut_ptr() as u32) == 0 {
        print_bytes(putc, b"audio-net cannot start: no supported NIC is available.\n");
        return;
    }

    let arguments = unsafe { slice::from_raw_parts(args_ptr as *const u8, args_len as usize) };
    let mut words = split_words(arguments);
    match words.next() {
        None => listen_mode(putc, should_abort, nic_receive, audio_sample_rate, audio_channels_and_bits, audio_submit),
        Some(b"send") => {
            let Some(name) = words.next() else {
                print_bytes(putc, b"Usage: tape run send NAME [AA:BB:CC:DD:EE:FF]\n");
                return;
            };
            let destination = match words.next() {
                None => BROADCAST_MAC,
                Some(mac_text) => match parse_mac(mac_text) {
                    Some(mac) => mac,
                    None => {
                        print_bytes(putc, b"Usage: tape run send NAME [AA:BB:CC:DD:EE:FF]\n");
                        return;
                    }
                },
            };
            if words.next().is_some() {
                print_bytes(putc, b"Usage: tape run send NAME [AA:BB:CC:DD:EE:FF]\n");
                return;
            }
            send_mode(putc, should_abort, alloc, free, ticks, fs_file_length, fs_read, name, &own_mac, &destination, nic_send);
        }
        Some(b"capture") => {
            let destination = match words.next() {
                None => BROADCAST_MAC,
                Some(mac_text) => match parse_mac(mac_text) {
                    Some(mac) => mac,
                    None => {
                        print_bytes(putc, b"Usage: tape run capture [AA:BB:CC:DD:EE:FF]\n");
                        return;
                    }
                },
            };
            if words.next().is_some() {
                print_bytes(putc, b"Usage: tape run capture [AA:BB:CC:DD:EE:FF]\n");
                return;
            }
            capture_mode(
                putc,
                should_abort,
                audio_sample_rate,
                audio_channels_and_bits,
                audio_capture,
                &own_mac,
                &destination,
                nic_send,
            );
        }
        Some(_) => {
            print_bytes(
                putc,
                b"Usage: tape run [send NAME [MAC]|capture [MAC]]\n",
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn listen_mode(
    putc: Putc,
    should_abort: ShouldAbort,
    nic_receive: NicReceive,
    audio_sample_rate: AudioSampleRate,
    audio_channels_and_bits: AudioChannelsAndBits,
    audio_submit: AudioSubmit,
) {
    let sample_rate = audio_sample_rate();
    if sample_rate == 0 {
        print_bytes(
            putc,
            b"audio-net: no audio device found; is an ES1370 (or compatible) card attached?\n",
        );
        return;
    }
    let packed = audio_channels_and_bits();
    let channels = (packed & 0xff) as u8;
    let bits = ((packed >> 8) & 0xff) as u8;

    print_bytes(putc, b"Local audio device: ");
    print_format(putc, sample_rate, channels, bits);
    print_bytes(putc, b".\n");
    print_bytes(
        putc,
        b"Waiting for a matching audio stream from any peer; F12 (or\n",
    );
    print_bytes(putc, b"F4/F6/F7/F8/F10) stops before a stream connects.\n");

    let mut frame = [0u8; MAX_FRAME_LEN];
    let mut source: Option<[u8; 6]> = None;
    let mut bytes_received = 0u64;
    let mut bytes_played = 0u64;
    let mut bytes_dropped = 0u64;

    loop {
        if source.is_none() && should_abort() != 0 {
            print_bytes(putc, b"audio-net: stopped locally; no stream ever connected.\n");
            return;
        }

        let received = nic_receive(frame.as_mut_ptr() as u32, MAX_FRAME_LEN as u32) as usize;
        if received < HEADER_LEN || frame[12..14] != AUDIO_ETHERTYPE {
            if source.is_some() && should_abort() != 0 {
                break;
            }
            continue;
        }

        let mut frame_source = [0u8; 6];
        frame_source.copy_from_slice(&frame[6..12]);
        let tag = frame[14];
        let payload = &frame[HEADER_LEN..received];

        match tag {
            AUDIO_TAG_START if source.is_none() => {
                if payload.len() < START_PAYLOAD_LEN {
                    continue;
                }
                let remote_rate = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let remote_channels = payload[4];
                let remote_bits = payload[5];
                if remote_rate != sample_rate || remote_channels != channels || remote_bits != bits {
                    print_bytes(putc, b"audio-net: rejected a stream declaring ");
                    print_format(putc, remote_rate, remote_channels, remote_bits);
                    print_bytes(putc, b"\n(local device is ");
                    print_format(putc, sample_rate, channels, bits);
                    print_bytes(putc, b"); no resampling is supported. Still waiting.\n");
                    continue;
                }
                print_bytes(putc, b"audio-net: stream started from ");
                print_mac(putc, &frame_source);
                print_bytes(putc, b".\n");
                source = Some(frame_source);
                bytes_received = 0;
                bytes_played = 0;
                bytes_dropped = 0;
            }
            AUDIO_TAG_DATA if source == Some(frame_source) => {
                if payload.len() < DATA_LENGTH_PREFIX {
                    continue;
                }
                let declared = u16::from_le_bytes([payload[0], payload[1]]) as usize;
                let available = payload.len() - DATA_LENGTH_PREFIX;
                let length = declared.min(available);
                let pcm = &payload[DATA_LENGTH_PREFIX..DATA_LENGTH_PREFIX + length];
                bytes_received += length as u64;

                let mut offset = 0usize;
                let mut spins = 0u32;
                while offset < pcm.len() && spins < 64 {
                    let accepted =
                        audio_submit(pcm[offset..].as_ptr() as u32, (pcm.len() - offset) as u32)
                            as usize;
                    if accepted == 0 {
                        spins += 1;
                        continue;
                    }
                    offset += accepted;
                    bytes_played += accepted as u64;
                }
                bytes_dropped += (pcm.len() - offset) as u64;
            }
            AUDIO_TAG_STOP if source == Some(frame_source) => {
                print_bytes(putc, b"audio-net: stream stopped by sender.\n");
                break;
            }
            _ => {}
        }
    }

    print_bytes(putc, b"Bytes received: ");
    print_u32(putc, bytes_received as u32);
    print_bytes(putc, b", played: ");
    print_u32(putc, bytes_played as u32);
    print_bytes(putc, b", dropped: ");
    print_u32(putc, bytes_dropped as u32);
    print_bytes(putc, b".\nLeft audio-net; back at the fos shell.\n");
}

#[allow(clippy::too_many_arguments)]
fn send_mode(
    putc: Putc,
    should_abort: ShouldAbort,
    alloc: Alloc,
    free: Free,
    ticks: Ticks,
    fs_file_length: FsFileLength,
    fs_read: FsRead,
    name: &[u8],
    own_mac: &[u8; 6],
    destination: &[u8; 6],
    nic_send: NicSend,
) {
    let mut file_length = 0u64;
    let status = fs_file_length(
        name.as_ptr() as u32,
        name.len() as u32,
        &mut file_length as *mut u64 as u32,
    );
    match status {
        FS_OK => {}
        FS_NOT_FOUND => {
            print_bytes(putc, b"audio-net: file not found.\n");
            return;
        }
        FS_INVALID_ARGUMENT => {
            print_bytes(putc, b"audio-net: invalid file reference.\n");
            return;
        }
        _ => {
            print_bytes(putc, b"audio-net: cannot inspect file (status ");
            print_u32(putc, status);
            print_bytes(putc, b").\n");
            return;
        }
    }
    if file_length == 0 || file_length > u64::from(SEND_MAX_CAPACITY) {
        print_bytes(putc, b"audio-net: file is empty or exceeds the 16 MiB send limit.\n");
        return;
    }

    let capacity = file_length as u32;
    let address = alloc(capacity);
    if address == 0 {
        print_bytes(putc, b"audio-net: unable to allocate a buffer for the file.\n");
        return;
    }

    let mut read_length = 0u32;
    let read_status = fs_read(
        name.as_ptr() as u32,
        name.len() as u32,
        address,
        capacity,
        &mut read_length as *mut u32 as u32,
    );
    if read_status != FS_OK {
        print_bytes(putc, b"audio-net: cannot read file (status ");
        print_u32(putc, read_status);
        print_bytes(putc, b").\n");
        free(address, capacity);
        return;
    }
    if read_status == FS_BUFFER_TOO_SMALL || read_status == FS_DISK_ERROR {
        free(address, capacity);
        return;
    }

    print_bytes(putc, b"audio-net: sending ");
    print_u32(putc, read_length);
    print_bytes(putc, b" byte(s) of raw PCM (");
    print_format(putc, SEND_SAMPLE_RATE, SEND_CHANNELS, SEND_BITS);
    print_bytes(putc, b") to ");
    print_mac(putc, destination);
    print_bytes(putc, b"...\n");

    let mut start_payload = [0u8; START_PAYLOAD_LEN];
    start_payload[0..4].copy_from_slice(&SEND_SAMPLE_RATE.to_le_bytes());
    start_payload[4] = SEND_CHANNELS;
    start_payload[5] = SEND_BITS;
    send_frame(nic_send, destination, own_mac, AUDIO_TAG_START, &start_payload);

    let buffer = unsafe { slice::from_raw_parts(address as *const u8, read_length as usize) };
    let start_ticks = ticks();
    let mut offset = 0usize;
    let mut aborted = false;

    while offset < buffer.len() {
        if should_abort() != 0 {
            aborted = true;
            break;
        }
        let target_ticks = start_ticks + (offset as u64 * PIT_HZ) / SEND_BYTES_PER_SEC;
        while ticks() < target_ticks {
            if should_abort() != 0 {
                aborted = true;
                break;
            }
        }
        if aborted {
            break;
        }

        let chunk_end = (offset + DATA_CHUNK_CAPACITY).min(buffer.len());
        let chunk = &buffer[offset..chunk_end];
        let mut payload = [0u8; DATA_LENGTH_PREFIX + DATA_CHUNK_CAPACITY];
        payload[0..2].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
        payload[DATA_LENGTH_PREFIX..DATA_LENGTH_PREFIX + chunk.len()].copy_from_slice(chunk);
        send_frame(
            nic_send,
            destination,
            own_mac,
            AUDIO_TAG_DATA,
            &payload[..DATA_LENGTH_PREFIX + chunk.len()],
        );
        offset = chunk_end;
    }

    send_frame(nic_send, destination, own_mac, AUDIO_TAG_STOP, &[]);
    free(address, capacity);

    if aborted {
        print_bytes(putc, b"audio-net: send aborted locally after ");
        print_u32(putc, offset as u32);
        print_bytes(putc, b" byte(s).\n");
    } else {
        print_bytes(putc, b"audio-net: send complete (");
        print_u32(putc, offset as u32);
        print_bytes(putc, b" byte(s)).\n");
    }
}

/// Live microphone-to-network capture: the real ADC counterpart to
/// `send_mode`'s file replay. Each iteration polls `audio_capture` for
/// however much the kernel's ISR-driven ADC ring currently has ready
/// (mirroring `listen_mode`'s own `nic_receive` polling shape -- "copy
/// however much is ready," never an all-or-nothing read), and immediately
/// forwards a non-empty result as one `AUDIO_TAG_DATA` frame. There is no
/// pacing/target-tick schedule here unlike `send_mode`: `audio_capture`
/// itself only ever returns bytes once the hardware ISR has actually
/// produced them, so the real ADC period rate is the pacing, and an
/// unconditional tight poll loop (bounded only by `should_abort`) is
/// exactly right, the same reasoning `nic_receive`-based loops elsewhere
/// in this project already rely on.
#[allow(clippy::too_many_arguments)]
fn capture_mode(
    putc: Putc,
    should_abort: ShouldAbort,
    audio_sample_rate: AudioSampleRate,
    audio_channels_and_bits: AudioChannelsAndBits,
    audio_capture: AudioCapture,
    own_mac: &[u8; 6],
    destination: &[u8; 6],
    nic_send: NicSend,
) {
    let sample_rate = audio_sample_rate();
    if sample_rate == 0 {
        print_bytes(
            putc,
            b"audio-net: no audio device found; is an ES1370 (or compatible) card attached?\n",
        );
        return;
    }
    let packed = audio_channels_and_bits();
    let channels = (packed & 0xff) as u8;
    let bits = ((packed >> 8) & 0xff) as u8;

    print_bytes(putc, b"Local audio device (capture): ");
    print_format(putc, sample_rate, channels, bits);
    print_bytes(putc, b".\nStreaming live ADC input to ");
    print_mac(putc, destination);
    print_bytes(putc, b"; F12 (or F4/F6/F7/F8/F10) stops.\n");

    let mut start_payload = [0u8; START_PAYLOAD_LEN];
    start_payload[0..4].copy_from_slice(&sample_rate.to_le_bytes());
    start_payload[4] = channels;
    start_payload[5] = bits;
    send_frame(nic_send, destination, own_mac, AUDIO_TAG_START, &start_payload);

    let mut chunk = [0u8; CAPTURE_CHUNK_CAPACITY];
    let mut bytes_sent = 0u64;

    loop {
        if should_abort() != 0 {
            break;
        }
        let captured = audio_capture(chunk.as_mut_ptr() as u32, CAPTURE_CHUNK_CAPACITY as u32) as usize;
        if captured == 0 {
            continue;
        }
        let mut payload = [0u8; DATA_LENGTH_PREFIX + DATA_CHUNK_CAPACITY];
        payload[0..2].copy_from_slice(&(captured as u16).to_le_bytes());
        payload[DATA_LENGTH_PREFIX..DATA_LENGTH_PREFIX + captured].copy_from_slice(&chunk[..captured]);
        send_frame(
            nic_send,
            destination,
            own_mac,
            AUDIO_TAG_DATA,
            &payload[..DATA_LENGTH_PREFIX + captured],
        );
        bytes_sent += captured as u64;
    }

    send_frame(nic_send, destination, own_mac, AUDIO_TAG_STOP, &[]);
    print_bytes(putc, b"audio-net: capture stopped locally after ");
    print_u32(putc, bytes_sent as u32);
    print_bytes(putc, b" byte(s) sent.\n");
}
