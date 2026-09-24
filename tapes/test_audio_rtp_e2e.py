#!/usr/bin/env python3
"""End-to-end test for the RTP/UDP real-time audio tape (`audio-rtp`).

Unlike `audio-net` (a bespoke raw-Ethernet protocol), this tape speaks
genuine RTP (RFC 3550) over UDP/IPv4 -- so this test hand-builds real,
spec-compliant RTP/UDP/IPv4/Ethernet frames in Python (never importing
anything from the kernel/tape side) rather than reusing any fos-specific
framing, mirroring `tapes/test_telnet_e2e.py`'s own from-scratch
Ethernet/ARP/IPv4 helpers.

One QEMU guest carries an ES1370 (44100 Hz/2ch/16-bit -- RFC 3551's one
real static payload-type match, PT 10) captured to a host-side WAV file.
The test drives the guest over the usual `term net` multicast session:

- Phase A (listen): loads/runs `audio-rtp.bin` in listen mode, answers
  the guest's own ARP request for the host's address is not needed here
  (the *host* sends the guest an ARP request instead, proving the
  tape's ARP-reply mechanism -- required for any real sender to ever
  resolve this tape's MAC at all), then streams a synthesized tone as
  real RTP packets (correct big-endian samples, incrementing sequence
  numbers, constant SSRC, correctly-incrementing timestamp), including a
  deliberate sequence gap and a deliberate stale/duplicate packet to
  prove the tape's loss/reorder accounting. Verifies the captured WAV is
  non-silent *and* has the right dominant frequency -- not just peak
  amplitude, which a byte-swap bug could still pass while producing the
  wrong pitch.
- Phase B (send): seeds a raw-PCM tone file into FOSFS (via a throwaway
  image rebuilt with `EXTRA_FOSFS_FILES`, restored to the default image
  afterward), runs `audio-rtp.bin send NAME HOST_IP PORT`, and verifies
  the host receives a real RTP stream (correct payload type, monotonic
  sequence, constant SSRC, and payload bytes that byte-swap back to
  exactly the known source samples) after answering the guest's own ARP
  request for the host's address.

Capture mode (live ADC) is not separately exercised here: it shares
`send_mode`'s exact packetization/pacing code path, and QEMU's `none`
audiodev produces no real captured content to verify anyway -- the same
reasoning `tapes/test_audio_capture_e2e.py` already accepts for ES1370.

Not part of `make test`; run it after `make tape-audio-rtp` and `make
image`. See `AGENTS.md`'s `audio-rtp` section for the payload-type table
and the documented reasons behind the packetization size/byte-swap.
"""

import math
import os
import selectors
import socket as socketlib
import struct
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.77"
MCAST_PORT = 4277
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "audio-rtp.bin"
MONITOR_SOCK = Path("/tmp/fos-test-audio-rtp-monitor.sock")

TERM_CLIENT_MAC = bytes.fromhex("02deadbeef71")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef72")
GUEST_MAC = bytes.fromhex("525400123480")
GUEST_MAC_STR = "52:54:00:12:34:80"
HOST_MAC = bytes.fromhex("02deadbeef73")

GUEST_IP = bytes((192, 0, 2, 10))
HOST_IP = bytes((192, 0, 2, 20))
GATEWAY_IP = bytes((0, 0, 0, 0))
RTP_PORT = 5004

CHAT_ETHERTYPE = bytes((0x88, 0xB6))
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

SAMPLE_RATE = 44100
CHANNELS = 2
BITS_PER_SAMPLE = 16
PAYLOAD_TYPE = 10  # RFC 3551 static L16/44100/stereo
# FOSFS names are capped at 16 lowercase letters/digits/./_/- (see
# AGENTS.md's `fosfs::valid_name`); a `tempfile`-generated name is far
# too long, so the on-FOSFS name is fixed and short regardless of the
# host-side temp file's own path.
FOSFS_TONE_NAME = "tone.pcm"
TONE_SECONDS = 0.2
TONE_HZ = 880  # deliberately far from ES1370's ~441 Hz built-in test tone
RTP_PAYLOAD_BYTES = 1440


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + CHAT_ETHERTYPE + bytes((tag,)) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def checksum(data: bytes) -> int:
    if len(data) & 1:
        data += b"\0"
    total = 0
    for offset in range(0, len(data), 2):
        total += int.from_bytes(data[offset : offset + 2], "big")
        total = (total & 0xFFFF) + (total >> 16)
    return (~total) & 0xFFFF


def parse_arp(frame: bytes) -> tuple[bytes, bytes, bytes, bytes, int] | None:
    if len(frame) < 42 or frame[12:14] != b"\x08\x06":
        return None
    payload = frame[14:42]
    if payload[0:2] != b"\x00\x01" or payload[2:4] != b"\x08\x00":
        return None
    if payload[4] != 6 or payload[5] != 4:
        return None
    operation = int.from_bytes(payload[6:8], "big")
    return payload[8:14], payload[14:18], payload[18:24], payload[24:28], operation


def arp_request_frame(source_mac: bytes, source_ip: bytes, target_ip: bytes) -> bytes:
    payload = (
        b"\x00\x01\x08\x00\x06\x04\x00\x01"
        + source_mac
        + source_ip
        + b"\x00" * 6
        + target_ip
    )
    frame = b"\xff" * 6 + source_mac + b"\x08\x06" + payload
    return frame.ljust(60, b"\0")


def ip_header(source: bytes, destination: bytes, protocol: int, payload_len: int) -> bytes:
    header = bytearray(20)
    header[0] = 0x45
    header[2:4] = (20 + payload_len).to_bytes(2, "big")
    header[8] = 64
    header[9] = protocol
    header[12:16] = source
    header[16:20] = destination
    header[10:12] = checksum(bytes(header)).to_bytes(2, "big")
    return bytes(header)


def udp_datagram(source_ip: bytes, dest_ip: bytes, source_port: int, dest_port: int, payload: bytes) -> bytes:
    length = 8 + len(payload)
    udp = bytearray(struct.pack("!HHHH", source_port, dest_port, length, 0) + payload)
    pseudo = source_ip + dest_ip + b"\x00\x11" + length.to_bytes(2, "big")
    value = checksum(pseudo + bytes(udp))
    udp[6:8] = (value if value != 0 else 0xFFFF).to_bytes(2, "big")
    return bytes(udp)


def rtp_packet(payload_type: int, marker: bool, sequence: int, timestamp: int, ssrc: int, payload: bytes) -> bytes:
    byte0 = 0x80
    byte1 = (0x80 if marker else 0) | (payload_type & 0x7F)
    header = (
        bytes((byte0, byte1))
        + (sequence & 0xFFFF).to_bytes(2, "big")
        + (timestamp & 0xFFFFFFFF).to_bytes(4, "big")
        + (ssrc & 0xFFFFFFFF).to_bytes(4, "big")
    )
    return header + payload


def eth_ip_frame(destination_mac: bytes, source_mac: bytes, ip_payload: bytes) -> bytes:
    frame = bytearray(destination_mac + source_mac + b"\x08\x00" + ip_payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def rtp_frame(
    destination_mac: bytes,
    source_mac: bytes,
    source_ip: bytes,
    dest_ip: bytes,
    source_port: int,
    dest_port: int,
    rtp_bytes: bytes,
) -> bytes:
    udp = udp_datagram(source_ip, dest_ip, source_port, dest_port, rtp_bytes)
    ip = ip_header(source_ip, dest_ip, 17, len(udp)) + udp
    return eth_ip_frame(destination_mac, source_mac, ip)


def synth_tone() -> bytes:
    """A short 440 Hz stereo sine wave, raw 16-bit signed PCM, no header."""
    frame_count = int(SAMPLE_RATE * TONE_SECONDS)
    samples = bytearray()
    for index in range(frame_count):
        value = int(20000 * math.sin(2 * math.pi * TONE_HZ * index / SAMPLE_RATE))
        packed = struct.pack("<h", value)
        samples += packed * CHANNELS
    return bytes(samples)


def dominant_frequency(samples: list[int], sample_rate: int) -> float:
    """A coarse zero-crossing-rate frequency estimate -- enough to catch a
    byte-swap bug (which would still produce loud, non-silent, but wrong-
    pitch/aliased audio a peak-amplitude-only check would wrongly pass).
    """
    crossings = 0
    for previous, current in zip(samples, samples[1:]):
        if previous < 0 <= current or current < 0 <= previous:
            crossings += 1
    seconds = len(samples) / sample_rate
    return (crossings / 2) / seconds if seconds > 0 else 0.0


def contains_frequency(
    samples: list[int],
    sample_rate: int,
    target_hz: float,
    tolerance_hz: float,
    window_frames: int = 2205,
    silence_threshold: int = 1000,
) -> bool:
    """Scans in fixed windows (default 50 ms) rather than averaging over
    the whole capture, for two reasons specific to this device: ES1370
    plays its own built-in ~441 Hz *square*-wave test tone continuously
    from boot until the first `audio_submit_at` call engages network
    mode (see `kernel/src/es1370.rs`'s `fill_period_tone`), so the WAV's
    early portion is *not* our signal at all; and a whole-file average
    would also get diluted by any trailing silence after the stream
    ends. Skips near-silent windows (`silence_threshold`) so trailing
    silence can't drag a window's own estimate towards zero either.
    """
    for start in range(0, len(samples) - window_frames, window_frames):
        window = samples[start : start + window_frames]
        if max(abs(sample) for sample in window) < silence_threshold:
            continue
        if abs(dominant_frequency(window, sample_rate) - target_hz) <= tolerance_hz:
            return True
    return False


def recv_or_none(mcast: McastSocket, timeout: float) -> bytes | None:
    """`McastSocket.recv(size)` takes a buffer size and blocks, unlike a
    timeout-based recv -- this wraps it with `settimeout` so callers can
    poll in a bounded loop the same way every other e2e test's raw
    `AF_PACKET`-shaped receive loop does."""
    mcast.settimeout(timeout)
    try:
        return mcast.recv(65536)
    except socketlib.timeout:
        return None


def monitor_command(command: str, wait: float = 0.5) -> None:
    sock = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
    for _ in range(50):
        try:
            sock.connect(str(MONITOR_SOCK))
            break
        except (FileNotFoundError, ConnectionRefusedError):
            time.sleep(0.1)
    else:
        raise SystemExit("could not connect to the QEMU monitor socket")
    sock.recv(65536)
    sock.sendall((command + "\n").encode())
    time.sleep(wait)
    sock.close()


def rebuild_image_with_extra_file(name: str, path: Path) -> None:
    subprocess.run(
        ["make", "image", f"EXTRA_FOSFS_FILES=--file {name} {path}"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.DEVNULL,
    )


def restore_default_image() -> None:
    subprocess.run(["make", "image"], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)


def run_guest(image: Path, wav_capture: Path):
    if MONITOR_SOCK.exists():
        MONITOR_SOCK.unlink()
    return subprocess.Popen(
        [
            "qemu-system-i386",
            # `send_mode`'s `alloc()` call is served entirely from the
            # "big pool" above `pmem::TRACKED_BYTES` (128 MiB) -- see
            # `tapes/test_bigsieve_e2e.py`'s own identical `-m 256`
            # requirement for the same reason -- so QEMU's default memory
            # size (at or below 128 MiB) would leave zero big-pool bytes
            # to serve even this tiny file, regardless of how small it is.
            "-m",
            "256",
            "-drive",
            f"format=raw,file={image}",
            "-snapshot",
            "-serial",
            "stdio",
            "-monitor",
            f"unix:{MONITOR_SOCK},server,nowait",
            "-no-reboot",
            "-no-shutdown",
            "-netdev",
            f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
            "-device",
            f"rtl8139,netdev=net0,mac={GUEST_MAC_STR}",
            "-audiodev",
            f"wav,id=snd0,path={wav_capture},out.frequency={SAMPLE_RATE}",
            "-device",
            "ES1370,audiodev=snd0",
            "-display",
            "none",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )


def phase_a_listen(wav_capture: Path) -> None:
    """Listen mode: real RTP send from the host, real ARP-reply proof,
    deliberate loss/duplicate injection, and pitch-verified WAV output."""
    guest = run_guest(IMAGE, wav_capture)
    selector = selectors.DefaultSelector()
    output = bytearray()
    selector.register(guest.stdout, selectors.EVENT_READ, "guest")

    def read_until(marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in output[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r}; output since checkpoint:\n"
                    f"{output[start:].decode('utf-8', 'replace')}"
                )
            for _ in selector.select(remaining):
                chunk = os.read(guest.stdout.fileno(), 4096)
                if not chunk:
                    raise SystemExit(f"serial stream ended early:\n{output.decode('utf-8', 'replace')}")
                output.extend(chunk)
        return bytes(output[start:])

    def run_command(command: bytes, timeout: float = 10) -> bytes:
        start = len(output)
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, command + b"\n"))
        return read_until(b"fos> ", timeout, start)

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

        read_until(b"Waiting for a remote terminal client to connect", 30)
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        audio_output = run_command(b"audio")
        if b"[0] ES1370" not in audio_output or b"44100 Hz / 2ch / 16-bit" not in audio_output:
            raise SystemExit(f"ES1370 not found or unexpected format:\n{audio_output.decode('utf-8', 'replace')}")
        print("PASS: ES1370 discovered with the expected fixed format.")

        load_start = len(output)
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        send_tape.send_tape(mcast, GUEST_MAC, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until(b"Tape loaded", 30, load_start)
        read_until(b"fos> ", 5, load_start)

        run_start = len(output)
        command = f"tape run static {'.'.join(map(str, GUEST_IP))}/24 0.0.0.0\n".encode()
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, command))
        read_until(b"RTP payload type 10", 10, run_start)
        read_until(b"Listening on UDP port 5004", 10, run_start)
        print("PASS: audio-rtp tape started listen mode with the correct payload type.")

        # Prove the tape's ARP-reply mechanism: a genuine ARP request for
        # the tape's own configured address must get a genuine unicast
        # reply naming the guest's real MAC -- with no reply, no external
        # RTP sender could ever deliver a unicast packet to it at all.
        arp_start = len(output)
        mcast.send(arp_request_frame(HOST_MAC, HOST_IP, GUEST_IP))
        deadline = time.monotonic() + 5
        replied_mac = None
        while time.monotonic() < deadline and replied_mac is None:
            frame = recv_or_none(mcast, 0.2)
            if frame is None:
                continue
            parsed = parse_arp(frame)
            if parsed is None:
                continue
            sender_mac, sender_ip, target_mac, target_ip, operation = parsed
            if operation == 2 and sender_ip == GUEST_IP and target_mac == HOST_MAC:
                replied_mac = sender_mac
        if replied_mac != GUEST_MAC:
            raise SystemExit(f"expected an ARP reply from {GUEST_MAC.hex()}, got {replied_mac}")
        print("PASS: the tape answered a real ARP request for its own address.")

        pcm = synth_tone()
        ssrc = 0xC0FFEE01
        sequence = 1000
        timestamp = 0
        offset = 0
        stream_start = len(output)
        bytes_per_frame = CHANNELS * (BITS_PER_SAMPLE // 8)
        frames_per_packet = RTP_PAYLOAD_BYTES // bytes_per_frame
        gap_injected = False
        duplicate_sent = False
        first_packet_bytes = None

        while offset < len(pcm):
            chunk = pcm[offset : offset + RTP_PAYLOAD_BYTES]
            wire = bytearray(len(chunk))
            for i in range(0, len(chunk), 2):
                wire[i : i + 2] = struct.pack(">h", struct.unpack("<h", chunk[i : i + 2])[0])
            if first_packet_bytes is None:
                first_packet_bytes = (sequence, bytes(wire))
            rtp = rtp_packet(PAYLOAD_TYPE, False, sequence, timestamp, ssrc, bytes(wire))
            mcast.send(rtp_frame(GUEST_MAC, HOST_MAC, HOST_IP, GUEST_IP, RTP_PORT, RTP_PORT, rtp))

            # Deliberately skip one sequence number partway through, to
            # exercise the tape's gap accounting.
            if not gap_injected and offset > len(pcm) // 3:
                sequence += 2
                gap_injected = True
            else:
                sequence += 1
            timestamp += len(chunk) // bytes_per_frame
            offset += len(chunk)
            time.sleep(len(chunk) / (SAMPLE_RATE * bytes_per_frame))

        # Deliberately resend the very first packet (now stale/duplicate)
        # to exercise the tape's late/duplicate accounting.
        assert first_packet_bytes is not None
        stale_sequence, stale_wire = first_packet_bytes
        stale_rtp = rtp_packet(PAYLOAD_TYPE, False, stale_sequence, 0, ssrc, stale_wire)
        mcast.send(rtp_frame(GUEST_MAC, HOST_MAC, HOST_IP, GUEST_IP, RTP_PORT, RTP_PORT, stale_rtp))
        duplicate_sent = True
        time.sleep(0.2)

        stream_summary_start = len(output)
        read_until(b"stream started from", 15, stream_start)
        print("PASS: tape locked onto the RTP stream's source.")

        print("sending a simulated local F12 keypress via the QEMU monitor...", file=sys.stderr)
        monitor_command("sendkey f12", wait=0.5)
        summary = read_until(b"gap(s),", 10, stream_summary_start)
        read_until(b"late/duplicate.", 5, stream_summary_start)

        import re

        match = re.search(
            rb"(\d+) packet\(s\) accepted, (\d+) byte\(s\) played, (\d+) dropped, "
            rb"(\d+) gap\(s\), (\d+) late/duplicate\.",
            summary,
        )
        if not match:
            raise SystemExit(f"could not find summary line:\n{summary.decode('utf-8', 'replace')}")
        packets_accepted, bytes_played, bytes_dropped, gap_count, late_duplicate = (
            int(match.group(index)) for index in range(1, 6)
        )
        if gap_count < 1:
            raise SystemExit(f"expected at least one gap to be counted, got {gap_count}")
        if late_duplicate < 1:
            raise SystemExit(f"expected at least one late/duplicate packet, got {late_duplicate}")
        if bytes_dropped != 0:
            raise SystemExit(f"expected no dropped bytes, got {bytes_dropped}")
        print(
            f"PASS: {packets_accepted} packet(s) accepted, {bytes_played} byte(s) "
            f"played, {gap_count} gap(s), {late_duplicate} late/duplicate -- as expected."
        )
        assert duplicate_sent

        recovery_start = len(output)
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(b"fos is a small educational 32-bit x86 operating system.", 10, recovery_start)
        print("PASS: the same remote shell session remained usable after the tape returned.")

        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()

    with wave.open(str(wav_capture), "rb") as captured:
        if captured.getframerate() != SAMPLE_RATE or captured.getnchannels() != CHANNELS:
            raise SystemExit(
                f"captured WAV has unexpected format: {captured.getframerate()} Hz / "
                f"{captured.getnchannels()}ch"
            )
        captured_bytes = captured.readframes(captured.getnframes())
    if len(captured_bytes) == 0:
        raise SystemExit("captured WAV file is empty; no audio was ever played")
    sample_count = len(captured_bytes) // 2
    samples = list(struct.unpack(f"<{sample_count}h", captured_bytes[: sample_count * 2]))
    peak = max(abs(sample) for sample in samples)
    if peak < 1000:
        raise SystemExit(f"captured WAV peak amplitude is only {peak}; expected clearly audible output")
    # The capture also contains ES1370's own built-in ~441 Hz test tone
    # (played continuously until the tape's first `audio_submit_at` call
    # engages network mode) and possibly trailing silence afterward --
    # see `contains_frequency`'s own doc comment for why this looks for
    # our tone in a window, rather than averaging over the whole file.
    if not contains_frequency(samples[::CHANNELS], SAMPLE_RATE, TONE_HZ, TONE_HZ * 0.1):
        raise SystemExit(
            f"no 50 ms window of the captured WAV matched ~{TONE_HZ} Hz -- a byte-swap bug "
            "would produce loud, non-silent, but wrong-pitch/aliased audio that a "
            "peak-amplitude-only check would wrongly pass"
        )
    print(f"PASS: captured WAV contains real audio at the correct pitch (~{TONE_HZ} Hz, peak {peak}).")


def phase_b_send(tone_path: Path, tone_bytes: bytes) -> None:
    """Send mode: the tape replays a FOSFS file as a real outgoing RTP
    stream; the host verifies it end to end with a plain UDP socket."""
    wav_capture = Path(tempfile.mkstemp(suffix=".wav", prefix="fos-audio-rtp-send-")[1])
    guest = run_guest(IMAGE, wav_capture)
    selector = selectors.DefaultSelector()
    output = bytearray()
    selector.register(guest.stdout, selectors.EVENT_READ, "guest")

    def read_until(marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in output[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r}; output since checkpoint:\n"
                    f"{output[start:].decode('utf-8', 'replace')}"
                )
            for _ in selector.select(remaining):
                chunk = os.read(guest.stdout.fileno(), 4096)
                if not chunk:
                    raise SystemExit(f"serial stream ended early:\n{output.decode('utf-8', 'replace')}")
                output.extend(chunk)
        return bytes(output[start:])

    received_sequences: list[int] = []
    received_ssrcs: set[int] = set()
    received_payload = bytearray()

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

        read_until(b"Waiting for a remote terminal client to connect", 30)
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        # `audio_count()` only reflects discovery the shell's own `audio`/
        # `hda` commands already triggered -- nothing in the tape ABI
        # discovers hardware lazily (see `kernel/src/audio.rs::count`'s own
        # doc comment). Every audio-consuming tape's test already works
        # around this the same way (see `test_audio_net_e2e.py`).
        audio_start = len(output)
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"audio\n"))
        read_until(b"fos> ", 10, audio_start)

        load_start = len(output)
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        send_tape.send_tape(mcast, GUEST_MAC, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until(b"Tape loaded", 30, load_start)
        read_until(b"fos> ", 5, load_start)

        run_start = len(output)
        command = (
            f"tape run static {'.'.join(map(str, GUEST_IP))}/24 0.0.0.0 send "
            f"{FOSFS_TONE_NAME} {'.'.join(map(str, HOST_IP))} {RTP_PORT}\n"
        ).encode()
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, command))

        def drain_guest_output() -> None:
            for _ in selector.select(0):
                chunk = os.read(guest.stdout.fileno(), 4096)
                if chunk:
                    output.extend(chunk)

        # Answer the guest's own ARP request for the host's address --
        # required before it can deliver any unicast frame to us.
        deadline = time.monotonic() + 10
        replied = False
        while time.monotonic() < deadline and not replied:
            drain_guest_output()
            frame = recv_or_none(mcast, 0.2)
            if frame is None:
                continue
            parsed = parse_arp(frame)
            if parsed is None:
                continue
            sender_mac, sender_ip, target_mac, target_ip, operation = parsed
            if operation == 1 and target_ip == HOST_IP:
                arp_reply = (
                    b"\x00\x01\x08\x00\x06\x04\x00\x02"
                    + HOST_MAC
                    + HOST_IP
                    + sender_mac
                    + sender_ip
                )
                mcast.send((sender_mac + HOST_MAC + b"\x08\x06" + arp_reply).ljust(60, b"\0"))
                replied = True
        if not replied:
            raise SystemExit(
                "never saw the guest's ARP request for the host address; guest output:\n"
                + output[run_start:].decode("utf-8", "replace")
            )
        print("PASS: answered the guest's ARP request for the host's address.")

        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            drain_guest_output()
            frame = recv_or_none(mcast, 0.2)
            if frame is None:
                continue
            if len(frame) < 42 or frame[12:14] != b"\x08\x00":
                continue
            ip = frame[14:]
            ihl = (ip[0] & 0x0F) * 4
            if ip[9] != 17:
                continue
            total_length = int.from_bytes(ip[2:4], "big")
            udp = ip[ihl:total_length]
            source_port, dest_port, udp_length = struct.unpack("!HHH", udp[:6])
            if dest_port != RTP_PORT:
                continue
            rtp = udp[8:udp_length]
            byte0, byte1 = rtp[0], rtp[1]
            payload_type = byte1 & 0x7F
            if payload_type != PAYLOAD_TYPE:
                continue
            sequence = int.from_bytes(rtp[2:4], "big")
            ssrc = int.from_bytes(rtp[8:12], "big")
            received_sequences.append(sequence)
            received_ssrcs.add(ssrc)
            received_payload += rtp[12:]
            if len(received_payload) >= len(tone_bytes):
                break

        read_until(b"send complete", 10, run_start)
        print("PASS: 'tape run send' reported completion.")
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()
        wav_capture.unlink(missing_ok=True)

    if len(received_ssrcs) != 1:
        raise SystemExit(f"expected exactly one SSRC across the whole stream, got {received_ssrcs}")
    if received_sequences != sorted(received_sequences) or len(received_sequences) != len(set(received_sequences)):
        raise SystemExit(f"expected strictly monotonic, gap-free sequence numbers, got {received_sequences}")
    print(f"PASS: received {len(received_sequences)} RTP packet(s), one SSRC, no gaps.")

    wire_bytes = bytes(received_payload[: len(tone_bytes)])
    recovered = bytearray(len(wire_bytes))
    for i in range(0, len(wire_bytes), 2):
        recovered[i : i + 2] = struct.pack("<h", struct.unpack(">h", wire_bytes[i : i + 2])[0])
    if bytes(recovered) != tone_bytes:
        raise SystemExit("recovered PCM does not match the original source file byte-for-byte")
    print("PASS: recovered PCM matches the original source file exactly.")


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-audio-rtp` first.")

    wav_capture = Path(tempfile.mkstemp(suffix=".wav", prefix="fos-audio-rtp-")[1])
    try:
        phase_a_listen(wav_capture)
    finally:
        wav_capture.unlink(missing_ok=True)

    tone_bytes = synth_tone()
    tone_path = Path(tempfile.mkstemp(suffix=".pcm", prefix="fos-audio-rtp-tone-")[1])
    tone_path.write_bytes(tone_bytes)
    try:
        print("rebuilding the image with the tone file seeded into FOSFS...", file=sys.stderr)
        rebuild_image_with_extra_file(FOSFS_TONE_NAME, tone_path)
        phase_b_send(tone_path, tone_bytes)
    finally:
        tone_path.unlink(missing_ok=True)
        print("restoring the default image...", file=sys.stderr)
        restore_default_image()

    print("PASS: RTP/UDP real-time audio streaming works end to end.")


if __name__ == "__main__":
    main()
