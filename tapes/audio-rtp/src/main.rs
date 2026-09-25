#![no_std]
#![no_main]

//! A real-time audio-streaming tape over standard RTP/UDP (RFC 3550),
//! carrying raw PCM (no codec) -- the standards-based counterpart to
//! `audio-net`'s bespoke raw-Ethernet protocol. Any real RTP tool
//! (`ffmpeg`, `gstreamer`, ...) can send to or receive from this tape
//! directly; nothing about the wire format is fos-specific except the
//! two documented dynamic payload-type conventions `tape-net`'s
//! `RtpPacket`/this crate's `payload_type_for` use for anything beyond
//! RFC 3551's one static `L16`/44100/stereo entry (see `AGENTS.md`).
//!
//! Startup forms (network configuration always first):
//!
//! ```text
//! tape run static LOCAL_IP/PREFIX GATEWAY [PORT]
//! tape run static LOCAL_IP/PREFIX GATEWAY send NAME PEER_IP [PEER_PORT]
//! tape run static LOCAL_IP/PREFIX GATEWAY capture PEER_IP [PEER_PORT]
//! tape run dhcp [PORT]
//! tape run dhcp send NAME PEER_IP [PEER_PORT]
//! tape run dhcp capture PEER_IP [PEER_PORT]
//! ```
//!
//! `tape run` (listen mode, no stream args) queries device 0's real
//! format (`audio_sample_rate_at`/`audio_channels_and_bits_at`, never
//! hardcoded), picks the matching raw-PCM RTP payload type from
//! `payload_type_for`, and waits on the configured UDP port for RTP
//! packets carrying either that payload type or the fixed Opus payload
//! type (`PAYLOAD_TYPE_OPUS`) -- locking onto the first sender's SSRC
//! ("first sender wins," matching `audio-net`/`tape_load_net`'s own
//! convention), tracking its sequence number to detect loss/reorder (a
//! gap just resyncs forward and counts as loss, matching `audio-net`'s
//! accepted best-effort philosophy -- there is no reorder/jitter buffer
//! here beyond the kernel's own `PcmRing`), and forwarding every
//! accepted packet's payload into `audio_submit_at` after converting it
//! from RTP's big-endian wire format to the kernel ABI's own
//! little-endian format.
//!
//! `send NAME PEER_IP [PEER_PORT]` replays a raw, headerless PCM file
//! already on FOSFS (fetched via `fs_file_length`/`fs_read`, matching
//! `audio-net`'s own file-format assumption) as an outgoing RTP stream,
//! resolving the peer's MAC via ARP first. `capture PEER_IP [PEER_PORT]`
//! is the live-ADC counterpart, streaming `audio_capture_at`'s output in
//! real time. Both answer incoming ARP requests for their own configured
//! address while running, exactly like `listen` does -- required for any
//! peer to ever resolve this tape's MAC at all (see `ip-chat`'s identical
//! `send_arp_reply` use, which this tape copies verbatim in shape).
//!
//! Only device 0 is targeted, deliberately: one RTP session models
//! exactly one media stream (one SSRC, one UDP 5-tuple), and this
//! shell's one-foreground-tape-at-a-time model rules out true
//! simultaneous multi-device sessions regardless of protocol -- a second
//! device means invoking this tape again on a second port. There is no
//! RTCP or SDP. `listen_mode` additionally recognizes one Opus payload
//! type (`audio_rtp_tape::PAYLOAD_TYPE_OPUS`), decoded through
//! `tapes/opus-codec`'s vendored fixed-point libopus decoder instead of
//! the raw-PCM repack path -- see AGENTS.md's Opus section. This is
//! decode-only: `send`/`capture` still move raw PCM exclusively, and
//! always will.
//!
//! The tape uses no heap beyond `alloc`/`free`'s one bounded file-replay
//! buffer, and does not own the NIC. IPv4 options, fragmentation, and
//! anything above UDP are intentionally outside this teaching example --
//! see `RTP_PAYLOAD_BYTES`'s own doc comment for why fragmentation
//! specifically had to be designed around, not just left unsupported.

use core::slice;

use audio_rtp_tape::{
    AudioFormat, Codec, DEFAULT_PORT, MAX_FRAME_LEN, NetworkMode, PAYLOAD_TYPE_OPUS,
    RTP_PAYLOAD_BYTES, StartupError, StreamMode, as_u32, build_rtp_udp_frame, derive_stream_seed,
    initial_sequence_from_seed, initial_timestamp_from_seed, parse_startup, payload_type_for,
    same_subnet, ssrc_from_seed,
};
use opus_codec::{Decoder as OpusDecoder, Encoder as OpusEncoder};
use tape_net::{
    ArpCache, ArpOperation, ArpPacket, DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpClient,
    DhcpClientState, DhcpPacket, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetFrame, Ipv4Address,
    Ipv4Packet, MacAddress, RtpPacket, UdpDatagram, sequence16_at_or_after,
};
use tape_rt::{print_bytes, print_u32};

const ARP_TIMEOUT_TICKS: u64 = 300;
const DHCP_TIMEOUT_SECONDS: u32 = 8;
const DHCP_RETRY_SECONDS: u32 = 1;
const DHCP_MAX_RETRIES: u8 = 5;
const PIT_HZ: u64 = 100;
/// How long (in PIT ticks) `listen_mode` waits after the last accepted
/// packet before declaring the stream ended -- bare RTP has no explicit
/// "stop" signal the way `audio-net`'s own `AUDIO_TAG_STOP` does.
const RECEIVE_INACTIVITY_TICKS: u64 = 500;
/// Upper bound on how large the kernel-format PCM scratch buffer needs
/// to be for one RTP packet's payload, across every bit depth this tape
/// supports: 24-bit's 3-byte-wire/4-byte-kernel-container expansion is
/// the largest ratio this tape ever needs to hold at once.
const KERNEL_PCM_SCRATCH_BYTES: usize = 2048;
/// libopus's own largest supported frame duration (120 ms) at the
/// highest rate this tape's devices ever report (48 kHz) -- the upper
/// bound `opus_decode` is ever asked to produce per packet.
const OPUS_MAX_FRAME_SAMPLES_PER_CHANNEL: usize = 5760;
/// Stereo-wide sample capacity for the Opus decode scratch buffer.
const OPUS_PCM_SCRATCH_SAMPLES: usize = OPUS_MAX_FRAME_SAMPLES_PER_CHANNEL * 2;
/// Encode side: a fixed 20 ms frame duration (one of libopus's accepted
/// frame lengths at every rate it supports), not exposed as a choice --
/// matches this tape's existing "don't build untestable generality"
/// convention.
const OPUS_FRAMES_PER_SECOND: usize = 1000 / 20;
/// Stereo-wide sample capacity for one 20 ms encode frame at the highest
/// rate this tape's devices ever report (48 kHz) -- Opus `send`/
/// `capture` are effectively HDA-only for the same reason decode is (see
/// `listen_mode`'s own doc comment), so this is already the worst case.
const OPUS_ENCODE_FRAME_CAPACITY_SAMPLES: usize = (48_000 / OPUS_FRAMES_PER_SECOND) * 2;

const FS_OK: u32 = 0;
const FS_NOT_FOUND: u32 = 1;
const FS_INVALID_ARGUMENT: u32 = 2;
const SEND_MAX_CAPACITY: u32 = 16 * 1024 * 1024;

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
type AudioCount = extern "C" fn() -> u32;
type AudioSampleRateAt = extern "C" fn(u32) -> u32;
type AudioChannelsAndBitsAt = extern "C" fn(u32) -> u32;
type AudioSubmitAt = extern "C" fn(u32, u32, u32) -> u32;
type AudioCaptureAt = extern "C" fn(u32, u32, u32) -> u32;

#[derive(Clone, Copy)]
struct NetworkConfig {
    local: Ipv4Address,
    netmask: Ipv4Address,
    gateway: Ipv4Address,
}

fn print_ip(putc: Putc, address: Ipv4Address) {
    for (index, octet) in address.0.iter().copied().enumerate() {
        if index != 0 {
            putc(u32::from(b'.'));
        }
        print_u32(putc, u32::from(octet));
    }
}

fn print_format(putc: Putc, format: AudioFormat) {
    print_u32(putc, format.sample_rate);
    print_bytes(putc, b" Hz / ");
    print_u32(putc, u32::from(format.channels));
    print_bytes(putc, b"ch / ");
    print_u32(putc, u32::from(format.bits_per_sample));
    print_bytes(putc, b"-bit");
}

fn local_format(
    audio_count: AudioCount,
    audio_sample_rate_at: AudioSampleRateAt,
    audio_channels_and_bits_at: AudioChannelsAndBitsAt,
) -> Option<AudioFormat> {
    if audio_count() == 0 {
        return None;
    }
    let sample_rate = audio_sample_rate_at(0);
    let packed = audio_channels_and_bits_at(0);
    Some(AudioFormat {
        sample_rate,
        channels: (packed & 0xff) as u8,
        bits_per_sample: ((packed >> 8) & 0xff) as u8,
    })
}

fn raw_ticks(ticks: Ticks) -> u64 {
    ticks()
}

fn dhcp_seconds(ticks: Ticks) -> u32 {
    (raw_ticks(ticks) / 100) as u32
}

fn tick_reached(now: u64, deadline: u64) -> bool {
    now.wrapping_sub(deadline) < (1u64 << 63)
}

/// Repacks one buffer of RTP-wire-format PCM (big-endian, tightly
/// packed: 2 bytes/sample for 16-bit `L16`, 3 bytes/sample for 24-bit
/// `L24` per RFC 3190) into the kernel audio ABI's own little-endian
/// format (2 bytes/sample for 16-bit; a 4-byte container per sample for
/// 24-bit, value in the low 3 bytes, high byte zeroed -- see
/// `AGENTS.md`'s HDA notes on why that specific container convention is
/// a documented assumption needing real-hardware confirmation, not a
/// verified fact). Returns the number of kernel-format bytes written.
fn wire_to_kernel_pcm(wire: &[u8], bits: u8, output: &mut [u8]) -> usize {
    match bits {
        16 => {
            let count = (wire.len() / 2).min(output.len() / 2);
            for i in 0..count {
                let value = i16::from_be_bytes([wire[i * 2], wire[i * 2 + 1]]);
                output[i * 2..i * 2 + 2].copy_from_slice(&value.to_le_bytes());
            }
            count * 2
        }
        24 => {
            let count = (wire.len() / 3).min(output.len() / 4);
            for i in 0..count {
                let (w0, w1, w2) = (wire[i * 3], wire[i * 3 + 1], wire[i * 3 + 2]);
                output[i * 4] = w2;
                output[i * 4 + 1] = w1;
                output[i * 4 + 2] = w0;
                output[i * 4 + 3] = 0;
            }
            count * 4
        }
        _ => 0,
    }
}

/// The send/capture-path counterpart to `wire_to_kernel_pcm`.
fn kernel_to_wire_pcm(kernel: &[u8], bits: u8, output: &mut [u8]) -> usize {
    match bits {
        16 => {
            let count = (kernel.len() / 2).min(output.len() / 2);
            for i in 0..count {
                let value = i16::from_le_bytes([kernel[i * 2], kernel[i * 2 + 1]]);
                output[i * 2..i * 2 + 2].copy_from_slice(&value.to_be_bytes());
            }
            count * 2
        }
        24 => {
            let count = (kernel.len() / 4).min(output.len() / 3);
            for i in 0..count {
                let (k0, k1, k2) = (kernel[i * 4], kernel[i * 4 + 1], kernel[i * 4 + 2]);
                output[i * 3] = k2;
                output[i * 3 + 1] = k1;
                output[i * 3 + 2] = k0;
            }
            count * 3
        }
        _ => 0,
    }
}

fn send_frame(nic_send: NicSend, frame: &[u8], max_frame_len: usize) -> bool {
    if frame.len() < EthernetFrame::HEADER_LEN || frame.len() > max_frame_len {
        return false;
    }
    nic_send(frame.as_ptr() as u32, frame.len() as u32) != 0
}

fn send_arp_request(
    nic_send: NicSend,
    own_mac: MacAddress,
    local_ip: Ipv4Address,
    target_ip: Ipv4Address,
) -> bool {
    let packet = ArpPacket::request(own_mac, local_ip, target_ip);
    let mut arp_bytes = [0u8; ArpPacket::LEN];
    let Ok(arp_len) = packet.encode(&mut arp_bytes) else {
        return false;
    };
    let ethernet = EthernetFrame::new(
        MacAddress::BROADCAST,
        own_mac,
        ETHERTYPE_ARP,
        &arp_bytes[..arp_len],
    );
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Ok(length) = ethernet.encode(&mut frame) else {
        return false;
    };
    send_frame(nic_send, &frame[..length], MAX_FRAME_LEN)
}

fn send_arp_reply(
    nic_send: NicSend,
    own_mac: MacAddress,
    local_ip: Ipv4Address,
    target_mac: MacAddress,
    target_ip: Ipv4Address,
) -> bool {
    let packet = ArpPacket::reply(own_mac, local_ip, target_mac, target_ip);
    let mut arp_bytes = [0u8; ArpPacket::LEN];
    let Ok(arp_len) = packet.encode(&mut arp_bytes) else {
        return false;
    };
    let ethernet = EthernetFrame::new(target_mac, own_mac, ETHERTYPE_ARP, &arp_bytes[..arp_len]);
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Ok(length) = ethernet.encode(&mut frame) else {
        return false;
    };
    send_frame(nic_send, &frame[..length], MAX_FRAME_LEN)
}

/// Answers an incoming ARP request for our own configured address --
/// copied in shape from `ip-chat`'s `process_frame`/`send_arp_reply`
/// (also mirrored by `router`): without this, no external sender (a
/// real RTP tool included) could ever resolve this tape's MAC well
/// enough to deliver a unicast UDP packet to it at all. Returns `true`
/// if `frame` was ARP (handled either way), so callers can skip trying
/// to parse it as IPv4/UDP/RTP afterward.
fn handle_arp_frame(
    nic_send: NicSend,
    frame: &[u8],
    own_mac: MacAddress,
    local_ip: Ipv4Address,
    arp: &mut ArpCache<8>,
    now: u32,
) -> bool {
    let Ok(ethernet) = EthernetFrame::parse(frame) else {
        return false;
    };
    if ethernet.ethertype != ETHERTYPE_ARP {
        return false;
    }
    if let Ok(packet) = ArpPacket::parse(ethernet.payload) {
        arp.insert(packet.sender_protocol, packet.sender_hardware, now);
        if packet.operation == ArpOperation::Request
            && packet.target_protocol == local_ip
            && packet.sender_hardware != own_mac
        {
            send_arp_reply(
                nic_send,
                own_mac,
                local_ip,
                packet.sender_hardware,
                packet.sender_protocol,
            );
        }
    }
    true
}

#[derive(Clone, Copy)]
enum ResolveResult {
    Found(MacAddress),
    Failed,
    Aborted,
}

/// Resolves a peer's MAC via ARP, answering any incoming ARP request for
/// our own address in the meantime (mirroring `ip-chat`'s `resolve_mac`,
/// minus the pending-line-editor-event bookkeeping this tape has no
/// need for).
#[allow(clippy::too_many_arguments)]
fn resolve_mac(
    nic_send: NicSend,
    nic_receive: NicReceive,
    should_abort: ShouldAbort,
    ticks: Ticks,
    target_ip: Ipv4Address,
    own_mac: MacAddress,
    local_ip: Ipv4Address,
    arp: &mut ArpCache<8>,
) -> ResolveResult {
    let initial_tick = raw_ticks(ticks);
    if let Some(mac) = arp.lookup(target_ip, initial_tick as u32) {
        return ResolveResult::Found(mac);
    }
    if !send_arp_request(nic_send, own_mac, local_ip, target_ip) {
        return ResolveResult::Failed;
    }

    let deadline = initial_tick.wrapping_add(ARP_TIMEOUT_TICKS);
    let mut frame = [0u8; MAX_FRAME_LEN];
    loop {
        if should_abort() != 0 {
            return ResolveResult::Aborted;
        }
        let length = nic_receive(frame.as_mut_ptr() as u32, MAX_FRAME_LEN as u32) as usize;
        if length != 0 {
            let now = raw_ticks(ticks) as u32;
            handle_arp_frame(nic_send, &frame[..length.min(MAX_FRAME_LEN)], own_mac, local_ip, arp, now);
            if let Some(mac) = arp.lookup(target_ip, now) {
                return ResolveResult::Found(mac);
            }
        }
        if tick_reached(raw_ticks(ticks), deadline) {
            return ResolveResult::Failed;
        }
    }
}

fn receive_dhcp(payload: &[u8], client: &mut DhcpClient<128>, now: u32) {
    let Ok(ethernet) = EthernetFrame::parse(payload) else {
        return;
    };
    if ethernet.ethertype != ETHERTYPE_IPV4 {
        return;
    }
    let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else {
        return;
    };
    if packet.header.protocol != 17 {
        return;
    }
    let Ok(datagram) = UdpDatagram::parse(
        packet.payload,
        true,
        packet.header.source,
        packet.header.destination,
    ) else {
        return;
    };
    if datagram.destination_port != DHCP_CLIENT_PORT {
        return;
    }
    let Ok(dhcp) = DhcpPacket::parse(datagram.payload) else {
        return;
    };
    let _ = client.receive(now, &dhcp);
}

fn send_dhcp_payload(
    nic_send: NicSend,
    own_mac: MacAddress,
    payload: &[u8],
    source: Ipv4Address,
    destination: Ipv4Address,
) -> bool {
    let udp = UdpDatagram::new(DHCP_CLIENT_PORT, DHCP_SERVER_PORT, payload);
    let mut udp_bytes = [0u8; MAX_FRAME_LEN];
    let Ok(udp_len) = udp.encode_with_checksum(&mut udp_bytes, source, destination, false) else {
        return false;
    };
    let mut ip_bytes = [0u8; MAX_FRAME_LEN];
    let packet = tape_net::Ipv4Packet {
        header: tape_net::Ipv4Header::new(source, destination, 17, udp_len),
        payload: &udp_bytes[..udp_len],
    };
    let Ok(ip_len) = packet.encode(&mut ip_bytes) else {
        return false;
    };
    let ethernet = EthernetFrame::new(MacAddress::BROADCAST, own_mac, ETHERTYPE_IPV4, &ip_bytes[..ip_len]);
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Ok(length) = ethernet.encode(&mut frame) else {
        return false;
    };
    send_frame(nic_send, &frame[..length], MAX_FRAME_LEN)
}

/// Runs the standard DISCOVER/OFFER/REQUEST/ACK exchange -- copied in
/// shape from `ip-chat`'s `configure_dhcp`, with an added `should_abort`
/// check this tape's version of the ABI makes available.
fn configure_dhcp(
    putc: Putc,
    nic_send: NicSend,
    nic_receive: NicReceive,
    should_abort: ShouldAbort,
    ticks: Ticks,
    own_mac: MacAddress,
) -> Option<NetworkConfig> {
    print_bytes(putc, b"DHCP: broadcasting DISCOVER and waiting for a server...\n");
    let transaction_id = (raw_ticks(ticks) as u32)
        ^ u32::from_be_bytes(own_mac.0[..4].try_into().unwrap())
        ^ u32::from_be_bytes([own_mac.0[2], own_mac.0[3], own_mac.0[4], own_mac.0[5]]);
    let mut client = DhcpClient::<128>::new(own_mac, transaction_id, DHCP_RETRY_SECONDS, DHCP_MAX_RETRIES);
    let started = dhcp_seconds(ticks);
    let mut payload = [0u8; 600];
    let mut frame = [0u8; MAX_FRAME_LEN];

    loop {
        if should_abort() != 0 {
            print_bytes(putc, b"DHCP aborted locally.\n");
            return None;
        }
        let now = dhcp_seconds(ticks);
        if now.wrapping_sub(started) > DHCP_TIMEOUT_SECONDS {
            print_bytes(putc, b"DHCP failed: no server responded before the timeout.\n");
            return None;
        }
        match client.poll(now, &mut payload) {
            Ok(Some(length)) => {
                let transmit = client.transmit();
                if !send_dhcp_payload(nic_send, own_mac, &payload[..length], transmit.source, transmit.destination) {
                    print_bytes(putc, b"DHCP failed: NIC rejected the request frame.\n");
                    return None;
                }
            }
            Ok(None) => {}
            Err(_) => {
                print_bytes(putc, b"DHCP failed: packet construction error.\n");
                return None;
            }
        }
        if client.state() == DhcpClientState::Bound {
            let lease = client.lease().unwrap();
            if lease.subnet_mask == Ipv4Address::UNSPECIFIED {
                print_bytes(putc, b"DHCP failed: server supplied no subnet mask.\n");
                return None;
            }
            print_bytes(putc, b"DHCP lease acquired for ");
            print_ip(putc, lease.address);
            print_bytes(putc, b".\n");
            return Some(NetworkConfig {
                local: lease.address,
                netmask: lease.subnet_mask,
                gateway: lease.router,
            });
        }
        if client.state() == DhcpClientState::Failed {
            print_bytes(putc, b"DHCP failed: no usable offer/acknowledgement arrived.\n");
            return None;
        }
        let length = nic_receive(frame.as_mut_ptr() as u32, MAX_FRAME_LEN as u32) as usize;
        if length != 0 {
            receive_dhcp(&frame[..length.min(MAX_FRAME_LEN)], &mut client, now);
        }
    }
}

fn print_startup_error(putc: Putc, error: StartupError) {
    let detail: &[u8] = match error {
        StartupError::MissingMode => b"missing startup mode",
        StartupError::UnknownMode => b"mode must be static or dhcp",
        StartupError::WrongArgumentCount => b"wrong number of arguments",
        StartupError::BadAddress => b"invalid dotted IPv4 address",
        StartupError::BadNetmask => b"invalid netmask/prefix",
        StartupError::BadPort => b"invalid port number",
        StartupError::BadBitrate => b"invalid bitrate (bits/sec)",
        StartupError::MissingName => b"missing file name",
    };
    print_bytes(putc, b"audio-rtp startup error: ");
    print_bytes(putc, detail);
    // Split across several print_bytes calls (rather than one long line)
    // to stay within AGENTS.md's 79-column-per-line convention -- this
    // grew too long for one line once `opus`/`BITRATE` joined `PORT`.
    print_bytes(putc, b".\nUsage: tape run static LOCAL_IP/PREFIX GATEWAY [MODE]\n");
    print_bytes(putc, b"   or  tape run dhcp [MODE]\n");
    print_bytes(putc, b"MODE: PORT\n");
    print_bytes(putc, b"    | send [opus] NAME PEER_IP [PORT] [BITRATE]\n");
    print_bytes(putc, b"    | capture [opus] PEER_IP [PORT] [BITRATE]\n");
}

/// Listen mode: waits for RTP packets carrying the local device's own
/// raw-PCM payload type, or the fixed Opus payload type
/// (`PAYLOAD_TYPE_OPUS`) when the local device's format allows it,
/// locking onto the first sender's SSRC and tracking its sequence
/// number for loss/reorder accounting. See this file's own module doc
/// comment for the full policy.
#[allow(clippy::too_many_arguments)]
fn listen_mode(
    putc: Putc,
    should_abort: ShouldAbort,
    alloc: Alloc,
    free: Free,
    nic_send: NicSend,
    nic_receive: NicReceive,
    ticks: Ticks,
    own_mac: MacAddress,
    local: Ipv4Address,
    port: u16,
    audio_count: AudioCount,
    audio_sample_rate_at: AudioSampleRateAt,
    audio_channels_and_bits_at: AudioChannelsAndBitsAt,
    audio_submit_at: AudioSubmitAt,
) {
    let Some(format) = local_format(audio_count, audio_sample_rate_at, audio_channels_and_bits_at) else {
        print_bytes(putc, b"audio-rtp: no audio device found.\n");
        return;
    };
    let Some(payload_type) = payload_type_for(format) else {
        print_bytes(putc, b"audio-rtp: local device format (");
        print_format(putc, format);
        print_bytes(putc, b") has no documented RTP payload type; cannot stream.\n");
        return;
    };
    print_bytes(putc, b"Local audio device: ");
    print_format(putc, format);
    print_bytes(putc, b" (RTP payload type ");
    print_u32(putc, u32::from(payload_type));
    print_bytes(putc, b").\nListening on UDP port ");
    print_u32(putc, u32::from(port));
    print_bytes(putc, b"; F12 (or F4/F6/F7/F8/F10) stops.\n");

    // Opus decode always produces 16-bit PCM (this build has no float API);
    // only offer it when the local device's own native format is 16-bit,
    // so decoded output never needs a bit-depth conversion this tape does
    // not implement. A device negotiated to 24-bit (HDA) keeps working on
    // its own raw-PCM payload type above; it just cannot also accept Opus.
    let mut opus_decoder = if format.bits_per_sample == 16 {
        match OpusDecoder::new(format.sample_rate as i32, i32::from(format.channels), alloc, free) {
            Ok(decoder) => {
                print_bytes(putc, b"Opus decode also accepted (RTP payload type ");
                print_u32(putc, u32::from(PAYLOAD_TYPE_OPUS));
                print_bytes(putc, b").\n");
                Some(decoder)
            }
            Err(_) => {
                print_bytes(putc, b"audio-rtp: could not start Opus decoder; Opus packets will be ignored.\n");
                None
            }
        }
    } else {
        None
    };

    let mut arp = ArpCache::<8>::new();
    let mut frame = [0u8; MAX_FRAME_LEN];
    let mut kernel_pcm = [0u8; KERNEL_PCM_SCRATCH_BYTES];
    let mut opus_pcm = [0i16; OPUS_PCM_SCRATCH_SAMPLES];
    let mut locked: Option<(u32, u32)> = None; // (ssrc, source_ip as u32)
    let mut expected_sequence: u16 = 0;
    let mut packets_accepted = 0u64;
    let mut bytes_submitted = 0u64;
    let mut bytes_dropped = 0u64;
    let mut gap_count = 0u64;
    let mut late_or_duplicate_count = 0u64;
    let mut last_activity = raw_ticks(ticks);

    loop {
        if should_abort() != 0 {
            break;
        }
        if locked.is_some() && tick_reached(raw_ticks(ticks), last_activity.wrapping_add(RECEIVE_INACTIVITY_TICKS)) {
            print_bytes(putc, b"audio-rtp: stream ended (no packets for a while).\n");
            break;
        }

        let received = nic_receive(frame.as_mut_ptr() as u32, MAX_FRAME_LEN as u32) as usize;
        if received == 0 {
            continue;
        }
        let now32 = raw_ticks(ticks) as u32;
        if handle_arp_frame(nic_send, &frame[..received.min(MAX_FRAME_LEN)], own_mac, local, &mut arp, now32) {
            continue;
        }
        let Ok(ethernet) = EthernetFrame::parse(&frame[..received.min(MAX_FRAME_LEN)]) else {
            continue;
        };
        if ethernet.ethertype != ETHERTYPE_IPV4 {
            continue;
        }
        let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else {
            continue;
        };
        if packet.header.protocol != 17 {
            continue;
        }
        let Ok(datagram) =
            UdpDatagram::parse(packet.payload, true, packet.header.source, packet.header.destination)
        else {
            continue;
        };
        if datagram.destination_port != port {
            continue;
        }
        let Ok(rtp) = RtpPacket::parse(datagram.payload) else {
            continue;
        };
        let is_opus = rtp.payload_type == PAYLOAD_TYPE_OPUS;
        if rtp.payload_type != payload_type && !(is_opus && opus_decoder.is_some()) {
            continue;
        }

        let source_ip = as_u32(packet.header.source);
        match locked {
            None => {
                locked = Some((rtp.ssrc, source_ip));
                expected_sequence = rtp.sequence_number.wrapping_add(1);
                print_bytes(putc, b"audio-rtp: stream started from ");
                print_ip(putc, packet.header.source);
                print_bytes(putc, b".\n");
            }
            Some((ssrc, ip)) if ssrc == rtp.ssrc && ip == source_ip => {
                if rtp.sequence_number == expected_sequence {
                    expected_sequence = expected_sequence.wrapping_add(1);
                } else if sequence16_at_or_after(rtp.sequence_number, expected_sequence) {
                    gap_count += 1;
                    expected_sequence = rtp.sequence_number.wrapping_add(1);
                } else {
                    late_or_duplicate_count += 1;
                    continue;
                }
            }
            Some(_) => continue, // a different, unrelated sender; ignore
        }

        last_activity = raw_ticks(ticks);
        packets_accepted += 1;

        let submit_len = if is_opus {
            let Some(decoder) = opus_decoder.as_mut() else {
                continue;
            };
            match decoder.decode(rtp.payload, &mut opus_pcm, OPUS_MAX_FRAME_SAMPLES_PER_CHANNEL) {
                Ok(samples_per_channel) => {
                    samples_per_channel * usize::from(format.channels) * size_of::<i16>()
                }
                Err(_) => {
                    // A bad Opus packet is dropped, same "keep going, count
                    // it as loss" policy as a sequence gap above -- there
                    // is no error signalling back to the sender in RTP.
                    bytes_dropped += rtp.payload.len() as u64;
                    continue;
                }
            }
        } else {
            wire_to_kernel_pcm(rtp.payload, format.bits_per_sample, &mut kernel_pcm)
        };
        let pcm_bytes: &[u8] = if is_opus {
            unsafe { slice::from_raw_parts(opus_pcm.as_ptr().cast::<u8>(), submit_len) }
        } else {
            &kernel_pcm[..submit_len]
        };

        let mut offset = 0usize;
        let mut spins = 0u32;
        while offset < submit_len && spins < 64 {
            let accepted = audio_submit_at(
                0,
                pcm_bytes[offset..submit_len].as_ptr() as u32,
                (submit_len - offset) as u32,
            ) as usize;
            if accepted == 0 {
                spins += 1;
                continue;
            }
            offset += accepted;
            bytes_submitted += accepted as u64;
        }
        bytes_dropped += (submit_len - offset) as u64;
    }

    print_bytes(putc, b"audio-rtp: ");
    print_u32(putc, packets_accepted as u32);
    print_bytes(putc, b" packet(s) accepted, ");
    print_u32(putc, bytes_submitted as u32);
    print_bytes(putc, b" byte(s) played, ");
    print_u32(putc, bytes_dropped as u32);
    print_bytes(putc, b" dropped, ");
    print_u32(putc, gap_count as u32);
    print_bytes(putc, b" gap(s), ");
    print_u32(putc, late_or_duplicate_count as u32);
    print_bytes(putc, b" late/duplicate.\n");
}

#[allow(clippy::too_many_arguments)]
fn send_mode(
    putc: Putc,
    should_abort: ShouldAbort,
    alloc: Alloc,
    free: Free,
    nic_send: NicSend,
    nic_receive: NicReceive,
    ticks: Ticks,
    fs_file_length: FsFileLength,
    fs_read: FsRead,
    own_mac: MacAddress,
    local: Ipv4Address,
    name: &[u8],
    peer_ip: Ipv4Address,
    peer_port: u16,
    codec: Codec,
    audio_count: AudioCount,
    audio_sample_rate_at: AudioSampleRateAt,
    audio_channels_and_bits_at: AudioChannelsAndBitsAt,
) {
    let Some(format) = local_format(audio_count, audio_sample_rate_at, audio_channels_and_bits_at) else {
        print_bytes(putc, b"audio-rtp: no audio device found.\n");
        return;
    };
    let Some(payload_type) = payload_type_for(format) else {
        print_bytes(putc, b"audio-rtp: local device format (");
        print_format(putc, format);
        print_bytes(putc, b") has no documented RTP payload type; cannot stream.\n");
        return;
    };
    let opus_bitrate = match codec {
        Codec::Opus { .. } if format.bits_per_sample != 16 => {
            print_bytes(putc, b"audio-rtp: Opus send needs a 16-bit device format.\n");
            return;
        }
        Codec::Opus { bitrate_bps } => Some(bitrate_bps.unwrap_or(opus_codec::DEFAULT_BITRATE_BPS)),
        Codec::Raw => None,
    };

    let mut file_length = 0u64;
    let status = fs_file_length(name.as_ptr() as u32, name.len() as u32, &mut file_length as *mut u64 as u32);
    match status {
        FS_OK => {}
        FS_NOT_FOUND => {
            print_bytes(putc, b"audio-rtp: file not found.\n");
            return;
        }
        FS_INVALID_ARGUMENT => {
            print_bytes(putc, b"audio-rtp: invalid file reference.\n");
            return;
        }
        _ => {
            print_bytes(putc, b"audio-rtp: cannot inspect file (status ");
            print_u32(putc, status);
            print_bytes(putc, b").\n");
            return;
        }
    }
    if file_length == 0 || file_length > u64::from(SEND_MAX_CAPACITY) {
        print_bytes(putc, b"audio-rtp: file is empty or exceeds the 16 MiB send limit.\n");
        return;
    }
    let capacity = file_length as u32;
    let address = alloc(capacity);
    if address == 0 {
        print_bytes(putc, b"audio-rtp: unable to allocate a buffer for the file.\n");
        return;
    }
    let mut read_length = 0u32;
    let read_status = fs_read(name.as_ptr() as u32, name.len() as u32, address, capacity, &mut read_length as *mut u32 as u32);
    if read_status != FS_OK {
        print_bytes(putc, b"audio-rtp: cannot read file (status ");
        print_u32(putc, read_status);
        print_bytes(putc, b").\n");
        free(address, capacity);
        return;
    }

    let mut arp = ArpCache::<8>::new();
    let peer_mac = match resolve_mac(nic_send, nic_receive, should_abort, ticks, peer_ip, own_mac, local, &mut arp) {
        ResolveResult::Found(mac) => mac,
        ResolveResult::Failed => {
            print_bytes(putc, b"audio-rtp: could not resolve the peer's MAC address (ARP timed out).\n");
            free(address, capacity);
            return;
        }
        ResolveResult::Aborted => {
            free(address, capacity);
            return;
        }
    };

    print_bytes(putc, b"audio-rtp: sending ");
    print_u32(putc, read_length);
    print_bytes(putc, if opus_bitrate.is_some() { b" byte(s) of PCM, Opus-encoded (" } else { b" byte(s) of raw PCM (" });
    print_format(putc, format);
    print_bytes(putc, b") to ");
    print_ip(putc, peer_ip);
    print_bytes(putc, b":");
    print_u32(putc, u32::from(peer_port));
    print_bytes(putc, b"...\n");

    let buffer = unsafe { slice::from_raw_parts(address as *const u8, read_length as usize) };
    let seed = derive_stream_seed(own_mac, raw_ticks(ticks));
    let ssrc = ssrc_from_seed(seed);
    let mut sequence = initial_sequence_from_seed(seed);
    let mut timestamp = initial_timestamp_from_seed(seed);
    let start_ticks = raw_ticks(ticks);
    let mut aborted = false;
    let mut frame = [0u8; MAX_FRAME_LEN];

    let offset = if let Some(bitrate) = opus_bitrate {
        let mut encoder = match OpusEncoder::new(format.sample_rate as i32, i32::from(format.channels), bitrate, alloc, free) {
            Ok(encoder) => encoder,
            Err(_) => {
                print_bytes(putc, b"audio-rtp: could not start the Opus encoder for this device's rate.\n");
                free(address, capacity);
                return;
            }
        };
        // The file's raw PCM is already this device's 16-bit kernel format
        // (native little-endian), so its bytes double directly as `i16`
        // samples -- no repacking step, unlike the raw wire path.
        let pcm: &[i16] = unsafe { slice::from_raw_parts(buffer.as_ptr().cast::<i16>(), buffer.len() / 2) };
        let frame_samples = format.sample_rate as usize / OPUS_FRAMES_PER_SECOND;
        let channels = usize::from(format.channels);
        // `pcm_frame`/`packet` go through the tape ABI's alloc callback
        // rather than the stack: a tape runs nested inside the shell's
        // own call chain on the kernel's shared stack (see AGENTS.md's
        // entry.S/stack-guard notes), and several KB of new locals here
        // was enough to hit the guard page in practice.
        let scratch_bytes = OPUS_ENCODE_FRAME_CAPACITY_SAMPLES as u32 * 2 + RTP_PAYLOAD_BYTES as u32;
        let scratch_addr = alloc(scratch_bytes);
        if scratch_addr == 0 {
            print_bytes(putc, b"audio-rtp: unable to allocate Opus encode scratch space.\n");
            free(address, capacity);
            return;
        }
        let pcm_frame: &mut [i16] =
            unsafe { slice::from_raw_parts_mut(scratch_addr as *mut i16, OPUS_ENCODE_FRAME_CAPACITY_SAMPLES) };
        let packet: &mut [u8] = unsafe {
            slice::from_raw_parts_mut(
                (scratch_addr + OPUS_ENCODE_FRAME_CAPACITY_SAMPLES as u32 * 2) as *mut u8,
                RTP_PAYLOAD_BYTES,
            )
        };
        let mut samples_sent = 0usize; // per channel
        let mut offset_samples = 0usize; // per channel

        while offset_samples * channels < pcm.len() {
            if should_abort() != 0 {
                aborted = true;
                break;
            }
            let target_ticks = start_ticks + (samples_sent as u64 * PIT_HZ) / format.sample_rate.max(1) as u64;
            while raw_ticks(ticks) < target_ticks {
                if should_abort() != 0 {
                    aborted = true;
                    break;
                }
            }
            if aborted {
                break;
            }

            let start = offset_samples * channels;
            let end = (start + frame_samples * channels).min(pcm.len());
            pcm_frame[..end - start].copy_from_slice(&pcm[start..end]);
            pcm_frame[end - start..frame_samples * channels].fill(0); // zero-pad a short final frame

            match encoder.encode(&pcm_frame[..frame_samples * channels], frame_samples, packet) {
                Ok(packet_len) => {
                    let rtp = RtpPacket::new(PAYLOAD_TYPE_OPUS, false, sequence, timestamp, ssrc, &packet[..packet_len]);
                    if let Ok(length) =
                        build_rtp_udp_frame(&mut frame, own_mac, peer_mac, local, peer_ip, DEFAULT_PORT, peer_port, &rtp)
                    {
                        send_frame(nic_send, &frame[..length], MAX_FRAME_LEN);
                    }
                    sequence = sequence.wrapping_add(1);
                    timestamp = timestamp.wrapping_add(frame_samples as u32);
                }
                Err(_) => {
                    // Keep going, same "count it as loss, no retransmission"
                    // policy the raw path implicitly has via dropped bytes.
                }
            }
            samples_sent += frame_samples;
            offset_samples += frame_samples;
        }
        free(scratch_addr, scratch_bytes);
        (offset_samples * channels * 2).min(buffer.len())
    } else {
        let bytes_per_frame = u32::from(format.channels) * (u32::from(format.bits_per_sample) / 8);
        let bytes_per_sec = u64::from(format.sample_rate) * u64::from(bytes_per_frame);
        let wire_bytes_per_sample: usize = if format.bits_per_sample == 24 { 3 } else { 2 };
        let kernel_bytes_per_sample: usize = if format.bits_per_sample == 24 { 4 } else { 2 };
        let samples_per_packet = RTP_PAYLOAD_BYTES / wire_bytes_per_sample;
        let kernel_chunk = samples_per_packet * kernel_bytes_per_sample;

        let mut wire_bytes = [0u8; RTP_PAYLOAD_BYTES];
        let mut offset = 0usize;

        while offset < buffer.len() {
            if should_abort() != 0 {
                aborted = true;
                break;
            }
            let target_ticks = start_ticks + (offset as u64 * PIT_HZ) / bytes_per_sec.max(1);
            while raw_ticks(ticks) < target_ticks {
                if should_abort() != 0 {
                    aborted = true;
                    break;
                }
            }
            if aborted {
                break;
            }

            let chunk_end = (offset + kernel_chunk).min(buffer.len());
            let chunk = &buffer[offset..chunk_end];
            let wire_len = kernel_to_wire_pcm(chunk, format.bits_per_sample, &mut wire_bytes);
            let frames_in_chunk = (wire_len / wire_bytes_per_sample) as u32;

            let rtp = RtpPacket::new(payload_type, false, sequence, timestamp, ssrc, &wire_bytes[..wire_len]);
            if let Ok(length) = build_rtp_udp_frame(
                &mut frame,
                own_mac,
                peer_mac,
                local,
                peer_ip,
                DEFAULT_PORT,
                peer_port,
                &rtp,
            ) {
                send_frame(nic_send, &frame[..length], MAX_FRAME_LEN);
            }
            sequence = sequence.wrapping_add(1);
            timestamp = timestamp.wrapping_add(frames_in_chunk);
            offset = chunk_end;
        }
        offset
    };

    free(address, capacity);
    if aborted {
        print_bytes(putc, b"audio-rtp: send aborted locally after ");
        print_u32(putc, offset as u32);
        print_bytes(putc, b" byte(s).\n");
    } else {
        print_bytes(putc, b"audio-rtp: send complete (");
        print_u32(putc, offset as u32);
        print_bytes(putc, b" byte(s)).\n");
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_mode(
    putc: Putc,
    should_abort: ShouldAbort,
    alloc: Alloc,
    free: Free,
    nic_send: NicSend,
    nic_receive: NicReceive,
    ticks: Ticks,
    own_mac: MacAddress,
    local: Ipv4Address,
    peer_ip: Ipv4Address,
    peer_port: u16,
    codec: Codec,
    audio_count: AudioCount,
    audio_sample_rate_at: AudioSampleRateAt,
    audio_channels_and_bits_at: AudioChannelsAndBitsAt,
    audio_capture_at: AudioCaptureAt,
) {
    let Some(format) = local_format(audio_count, audio_sample_rate_at, audio_channels_and_bits_at) else {
        print_bytes(putc, b"audio-rtp: no audio device found.\n");
        return;
    };
    let Some(payload_type) = payload_type_for(format) else {
        print_bytes(putc, b"audio-rtp: local device format (");
        print_format(putc, format);
        print_bytes(putc, b") has no documented RTP payload type; cannot stream.\n");
        return;
    };
    let opus_bitrate = match codec {
        Codec::Opus { .. } if format.bits_per_sample != 16 => {
            print_bytes(putc, b"audio-rtp: Opus capture needs a 16-bit device format.\n");
            return;
        }
        Codec::Opus { bitrate_bps } => Some(bitrate_bps.unwrap_or(opus_codec::DEFAULT_BITRATE_BPS)),
        Codec::Raw => None,
    };

    let mut arp = ArpCache::<8>::new();
    let peer_mac = match resolve_mac(nic_send, nic_receive, should_abort, ticks, peer_ip, own_mac, local, &mut arp) {
        ResolveResult::Found(mac) => mac,
        ResolveResult::Failed => {
            print_bytes(putc, b"audio-rtp: could not resolve the peer's MAC address (ARP timed out).\n");
            return;
        }
        ResolveResult::Aborted => return,
    };

    print_bytes(putc, b"audio-rtp: streaming live capture (");
    print_format(putc, format);
    print_bytes(putc, b") to ");
    print_ip(putc, peer_ip);
    print_bytes(putc, b":");
    print_u32(putc, u32::from(peer_port));
    print_bytes(putc, b"; F12 (or F4/F6/F7/F8/F10) stops.\n");

    let seed = derive_stream_seed(own_mac, raw_ticks(ticks));
    let ssrc = ssrc_from_seed(seed);
    let mut sequence = initial_sequence_from_seed(seed);
    let mut timestamp = initial_timestamp_from_seed(seed);

    let mut kernel_chunk = [0u8; KERNEL_PCM_SCRATCH_BYTES];
    let mut frame = [0u8; MAX_FRAME_LEN];
    let mut poll_frame = [0u8; MAX_FRAME_LEN];
    let mut bytes_sent = 0u64;

    if let Some(bitrate) = opus_bitrate {
        let mut encoder = match OpusEncoder::new(format.sample_rate as i32, i32::from(format.channels), bitrate, alloc, free) {
            Ok(encoder) => encoder,
            Err(_) => {
                print_bytes(putc, b"audio-rtp: could not start the Opus encoder for this device's rate.\n");
                return;
            }
        };
        let channels = usize::from(format.channels);
        let frame_samples = format.sample_rate as usize / OPUS_FRAMES_PER_SECOND;
        let frame_len = frame_samples * channels;
        // Accumulates captured PCM across loop iterations until a full
        // Opus frame is available: unlike the raw path, `audio_capture_at`
        // drains "whatever's ready" (not frame-aligned), but Opus can only
        // encode exactly one fixed frame duration at a time. Sized for one
        // incoming chunk plus up to one frame's worth of carry-over.
        //
        // Allocated through the tape ABI's alloc callback rather than the
        // stack: a tape runs nested inside the shell's own call chain on
        // the kernel's shared stack (see AGENTS.md's entry.S/stack-guard
        // notes), and this buffer alone is close to 6 KB.
        const ACCUM_CAPACITY: usize = OPUS_ENCODE_FRAME_CAPACITY_SAMPLES + KERNEL_PCM_SCRATCH_BYTES / 2;
        let scratch_bytes = ACCUM_CAPACITY as u32 * 2 + RTP_PAYLOAD_BYTES as u32;
        let scratch_addr = alloc(scratch_bytes);
        if scratch_addr == 0 {
            print_bytes(putc, b"audio-rtp: unable to allocate Opus encode scratch space.\n");
            return;
        }
        let accum: &mut [i16] = unsafe { slice::from_raw_parts_mut(scratch_addr as *mut i16, ACCUM_CAPACITY) };
        let packet: &mut [u8] = unsafe {
            slice::from_raw_parts_mut((scratch_addr + ACCUM_CAPACITY as u32 * 2) as *mut u8, RTP_PAYLOAD_BYTES)
        };
        let mut accum_len = 0usize;

        loop {
            if should_abort() != 0 {
                break;
            }
            let received = nic_receive(poll_frame.as_mut_ptr() as u32, MAX_FRAME_LEN as u32) as usize;
            if received != 0 {
                let now32 = raw_ticks(ticks) as u32;
                handle_arp_frame(nic_send, &poll_frame[..received.min(MAX_FRAME_LEN)], own_mac, local, &mut arp, now32);
            }

            let captured = audio_capture_at(0, kernel_chunk.as_mut_ptr() as u32, KERNEL_PCM_SCRATCH_BYTES as u32) as usize;
            if captured == 0 {
                continue;
            }
            let captured_samples = captured / 2;
            let captured_i16: &[i16] = unsafe { slice::from_raw_parts(kernel_chunk.as_ptr().cast::<i16>(), captured_samples) };
            let copy_len = captured_samples.min(accum.len() - accum_len);
            accum[accum_len..accum_len + copy_len].copy_from_slice(&captured_i16[..copy_len]);
            accum_len += copy_len;

            while accum_len >= frame_len {
                match encoder.encode(&accum[..frame_len], frame_samples, packet) {
                    Ok(packet_len) => {
                        let rtp = RtpPacket::new(PAYLOAD_TYPE_OPUS, false, sequence, timestamp, ssrc, &packet[..packet_len]);
                        if let Ok(length) =
                            build_rtp_udp_frame(&mut frame, own_mac, peer_mac, local, peer_ip, DEFAULT_PORT, peer_port, &rtp)
                        {
                            send_frame(nic_send, &frame[..length], MAX_FRAME_LEN);
                        }
                        sequence = sequence.wrapping_add(1);
                        timestamp = timestamp.wrapping_add(frame_samples as u32);
                        bytes_sent += packet_len as u64;
                    }
                    Err(_) => {}
                }
                accum.copy_within(frame_len..accum_len, 0);
                accum_len -= frame_len;
            }
        }
        free(scratch_addr, scratch_bytes);
    } else {
        let wire_bytes_per_sample: usize = if format.bits_per_sample == 24 { 3 } else { 2 };
        let mut wire_bytes = [0u8; RTP_PAYLOAD_BYTES];

        loop {
            if should_abort() != 0 {
                break;
            }
            // Answer any incoming ARP while we stream, exactly like listen
            // mode does -- a peer may only just now be trying to resolve us.
            let received = nic_receive(poll_frame.as_mut_ptr() as u32, MAX_FRAME_LEN as u32) as usize;
            if received != 0 {
                let now32 = raw_ticks(ticks) as u32;
                handle_arp_frame(nic_send, &poll_frame[..received.min(MAX_FRAME_LEN)], own_mac, local, &mut arp, now32);
            }

            let captured = audio_capture_at(0, kernel_chunk.as_mut_ptr() as u32, KERNEL_PCM_SCRATCH_BYTES as u32) as usize;
            if captured == 0 {
                continue;
            }
            let wire_len = kernel_to_wire_pcm(&kernel_chunk[..captured], format.bits_per_sample, &mut wire_bytes);
            let frames_in_chunk = (wire_len / wire_bytes_per_sample) as u32;

            let rtp = RtpPacket::new(payload_type, false, sequence, timestamp, ssrc, &wire_bytes[..wire_len]);
            if let Ok(length) = build_rtp_udp_frame(&mut frame, own_mac, peer_mac, local, peer_ip, DEFAULT_PORT, peer_port, &rtp) {
                send_frame(nic_send, &frame[..length], MAX_FRAME_LEN);
            }
            sequence = sequence.wrapping_add(1);
            timestamp = timestamp.wrapping_add(frames_in_chunk);
            bytes_sent += wire_len as u64;
        }
    }

    print_bytes(putc, b"audio-rtp: capture stopped locally after ");
    print_u32(putc, bytes_sent as u32);
    print_bytes(putc, b" byte(s) sent.\n");
}

/// Declares the complete current thirty-parameter tape ABI (see
/// `kernel/src/shell.rs`'s `TapeEntry`). `read_key`/`terminal_position`/
/// `fs_write`/`fs_remove`/the non-`_at` `nic_*`/`audio_*` slots are
/// unused (this tape does no line editing, writes no files, and only
/// ever targets device 0 through the `_at` callbacks) but must still be
/// declared, in order, per the append-only cdecl convention.
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
    _audio_sample_rate: extern "C" fn() -> u32,
    _audio_channels_and_bits: extern "C" fn() -> u32,
    _audio_submit: extern "C" fn(u32, u32) -> u32,
    _audio_capture: extern "C" fn(u32, u32) -> u32,
    audio_count: AudioCount,
    audio_sample_rate_at: AudioSampleRateAt,
    audio_channels_and_bits_at: AudioChannelsAndBitsAt,
    audio_submit_at: AudioSubmitAt,
    audio_capture_at: AudioCaptureAt,
) {
    let mut own_mac_bytes = [0u8; 6];
    if nic_mac(own_mac_bytes.as_mut_ptr() as u32) == 0 {
        print_bytes(putc, b"audio-rtp cannot start: no supported NIC is available.\n");
        return;
    }
    let own_mac = MacAddress::new(own_mac_bytes);

    let arguments = unsafe { slice::from_raw_parts(args_ptr as *const u8, args_len as usize) };
    let startup = match parse_startup(arguments) {
        Ok(startup) => startup,
        Err(error) => {
            print_startup_error(putc, error);
            return;
        }
    };

    let network = match startup.network {
        NetworkMode::Static {
            local,
            netmask,
            gateway,
        } => {
            print_bytes(putc, b"IPv4 configured: ");
            print_ip(putc, local);
            print_bytes(putc, b"/");
            print_ip(putc, netmask);
            print_bytes(putc, b", gateway ");
            print_ip(putc, gateway);
            print_bytes(putc, b".\n");
            NetworkConfig {
                local,
                netmask,
                gateway,
            }
        }
        NetworkMode::Dhcp => {
            let Some(config) = configure_dhcp(putc, nic_send, nic_receive, should_abort, ticks, own_mac) else {
                return;
            };
            config
        }
    };
    let _ = same_subnet(network.local, network.netmask, network.gateway);

    match startup.stream {
        StreamMode::Listen { port } => listen_mode(
            putc,
            should_abort,
            alloc,
            free,
            nic_send,
            nic_receive,
            ticks,
            own_mac,
            network.local,
            port,
            audio_count,
            audio_sample_rate_at,
            audio_channels_and_bits_at,
            audio_submit_at,
        ),
        StreamMode::Send {
            name,
            peer_ip,
            peer_port,
            codec,
        } => send_mode(
            putc,
            should_abort,
            alloc,
            free,
            nic_send,
            nic_receive,
            ticks,
            fs_file_length,
            fs_read,
            own_mac,
            network.local,
            name,
            peer_ip,
            peer_port,
            codec,
            audio_count,
            audio_sample_rate_at,
            audio_channels_and_bits_at,
        ),
        StreamMode::Capture { peer_ip, peer_port, codec } => capture_mode(
            putc,
            should_abort,
            alloc,
            free,
            nic_send,
            nic_receive,
            ticks,
            own_mac,
            network.local,
            peer_ip,
            peer_port,
            codec,
            audio_count,
            audio_sample_rate_at,
            audio_channels_and_bits_at,
            audio_capture_at,
        ),
    }
}
