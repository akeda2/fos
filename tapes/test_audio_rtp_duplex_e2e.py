#!/usr/bin/env python3
"""End-to-end test for `audio-rtp`'s `duplex` mode: simultaneous
receive-and-play plus capture-and-send in one invocation.

Proves the two directions genuinely run concurrently, not just that
`duplex_mode` compiles: while the host streams a synthesized tone to the
guest (exactly `test_audio_rtp_e2e.py`'s own Phase A tone/gap/duplicate
generator, imported rather than duplicated), it simultaneously drains and
verifies the guest's own outgoing RTP stream (structural correctness --
payload type, monotonically increasing sequence number, one constant
SSRC, and that at least one outgoing packet arrives *before* the host
finishes sending its tone, ruling out an accidental "receive fully, then
send" serialization bug).

Uses the `none` audiodev (matching `tapes/test_audio_capture_e2e.py`'s
own precedent), not `wav`: ES1370's ADC (feeding the send side) still
produces genuine periodic period-fill activity under `none` -- just
all-zero/silent samples -- but QEMU's `wav` audiodev backend has no
`in.path` option in this QEMU packaging at all (see that other test's
docstring), so a single guest cannot get *both* real captured-WAV output
verification and a live ADC feed at once. This test therefore verifies
the receive/playback side structurally too (the tape's own printed
packet/byte/gap counters), not via a captured-WAV pitch check --
`test_audio_rtp_e2e.py`'s Phase A already covers that permutation on its
own, unmodified `listen` mode, which `duplex_mode` reuses verbatim.

Raw PCM only (no Opus): duplex's send/receive code paths are reused
unchanged from `send_mode`/`capture_mode`/`listen_mode`, which are each
already verified with Opus by the other `test_audio_rtp_*_e2e.py`
scripts; this test's own job is proving concurrency, not re-verifying
codec correctness.

Not part of `make test`; run it after `make tape-audio-rtp` and `make
image`.
"""

import os
import re
import selectors
import struct
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402
from test_audio_rtp_e2e import (  # noqa: E402
    CHANNELS,
    BITS_PER_SAMPLE,
    GUEST_MAC,
    GUEST_MAC_STR,
    HOST_MAC,
    GUEST_IP,
    HOST_IP,
    MONITOR_SOCK,
    PAYLOAD_TYPE,
    RTP_PAYLOAD_BYTES,
    RTP_PORT,
    SAMPLE_RATE,
    TAPE_SENDER_MAC,
    TERM_CLIENT_MAC,
    TERM_TAG_BYE,
    TERM_TAG_DATA,
    TERM_TAG_HELLO,
    monitor_command,
    parse_arp,
    recv_or_none,
    rtp_frame,
    rtp_packet,
    synth_tone,
    term_frame,
)

MCAST_GROUP = "230.42.42.78"
MCAST_PORT = 4278
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "audio-rtp.bin"


def run_guest(image: Path):
    if MONITOR_SOCK.exists():
        MONITOR_SOCK.unlink()
    return subprocess.Popen(
        [
            "qemu-system-i386",
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
            "none,id=snd0",
            "-device",
            "ES1370,audiodev=snd0",
            "-display",
            "none",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )


def parse_incoming_rtp(frame: bytes):
    """Returns (payload_type, sequence, ssrc) for a frame that is a real
    Ethernet/IPv4/UDP/RTP packet from the guest to the host on RTP_PORT,
    or None otherwise -- mirrors `test_audio_rtp_e2e.py`'s own inline
    parse in `phase_b_send`. The Ethernet source/destination MAC check is
    required (unlike `phase_b_send`, which never sends and receives RTP
    over the same socket at once): this test's own outgoing tone packets
    share the exact same destination UDP port as the guest's replies (a
    deliberately symmetric duplex session), and the multicast test
    fixture loops a sender's own packets back to itself, so port alone
    cannot distinguish "the guest's reply" from "our own tone packet,
    echoed back."""
    if len(frame) < 42 or frame[0:6] != HOST_MAC or frame[6:12] != GUEST_MAC or frame[12:14] != b"\x08\x00":
        return None
    ip = frame[14:]
    ihl = (ip[0] & 0x0F) * 4
    if ip[9] != 17:
        return None
    total_length = int.from_bytes(ip[2:4], "big")
    udp = ip[ihl:total_length]
    if len(udp) < 8:
        return None
    _source_port, dest_port, udp_length = struct.unpack("!HHH", udp[:6])
    if dest_port != RTP_PORT:
        return None
    rtp = udp[8:udp_length]
    if len(rtp) < 12:
        return None
    payload_type = rtp[1] & 0x7F
    sequence = int.from_bytes(rtp[2:4], "big")
    ssrc = int.from_bytes(rtp[8:12], "big")
    return payload_type, sequence, ssrc


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-audio-rtp` first.")
    if not IMAGE.exists():
        raise SystemExit(f"missing {IMAGE}; run `make image` first")

    guest = run_guest(IMAGE)
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

    def drain_guest_output() -> None:
        for _ in selector.select(0):
            chunk = os.read(guest.stdout.fileno(), 4096)
            if chunk:
                output.extend(chunk)

    outgoing_payload_types: list[int] = []
    outgoing_sequences: list[int] = []
    outgoing_ssrcs: set[int] = set()
    first_outgoing_seen_at: float | None = None

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

        read_until(b"Waiting for a remote terminal client to connect", 30)
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        audio_output_start = len(output)
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"audio\n"))
        audio_output = read_until(b"fos> ", 10, audio_output_start)
        if b"[0] ES1370" not in audio_output or f"{SAMPLE_RATE} Hz / {CHANNELS}ch / {BITS_PER_SAMPLE}-bit".encode() not in audio_output:
            raise SystemExit(f"ES1370 not found or unexpected format:\n{audio_output.decode('utf-8', 'replace')}")
        print("PASS: ES1370 discovered with the expected fixed format.")

        load_start = len(output)
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        send_tape.send_tape(mcast, GUEST_MAC, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until(b"Tape loaded", 30, load_start)
        read_until(b"fos> ", 5, load_start)

        run_start = len(output)
        command = (
            f"tape run static {'.'.join(map(str, GUEST_IP))}/24 0.0.0.0 duplex "
            f"{'.'.join(map(str, HOST_IP))} {RTP_PORT}\n"
        ).encode()
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, command))
        read_until(f"Duplex: listening on UDP port {RTP_PORT}".encode(), 10, run_start)
        read_until(b"streaming live capture to", 10, run_start)
        print("PASS: audio-rtp tape started duplex mode with the correct format/ports.")

        # Answer the guest's own ARP request for the host's address --
        # required before it can deliver any unicast frame to us (the send
        # side resolves this up front, before its combined loop starts).
        deadline = time.monotonic() + 10
        replied = False
        while time.monotonic() < deadline and not replied:
            drain_guest_output()
            frame = recv_or_none(mcast, 0.2)
            if frame is None:
                continue
            parsed = parse_arp(frame)
            if parsed is not None:
                sender_mac, sender_ip, target_mac, target_ip, operation = parsed
                if operation == 1 and target_ip == HOST_IP:
                    arp_reply = (
                        b"\x00\x01\x08\x00\x06\x04\x00\x02" + HOST_MAC + HOST_IP + sender_mac + sender_ip
                    )
                    mcast.send((sender_mac + HOST_MAC + b"\x08\x06" + arp_reply).ljust(60, b"\0"))
                    replied = True
                    continue
            payload = parse_incoming_rtp(frame)
            if payload is not None:
                payload_type, sequence, ssrc = payload
                outgoing_payload_types.append(payload_type)
                outgoing_sequences.append(sequence)
                outgoing_ssrcs.add(ssrc)
        if not replied:
            raise SystemExit(
                "never saw the guest's ARP request for the host address; guest output:\n"
                + output[run_start:].decode("utf-8", "replace")
            )
        print("PASS: answered the guest's ARP request for the host's address.")

        # Stream the tone to the guest's receive side while continuing to
        # drain its outgoing send-side stream, interleaved in one loop --
        # the whole point of this test is that both happen at once.
        pcm = synth_tone()
        ssrc = 0xC0FFEE02
        sequence = 2000
        timestamp = 0
        offset = 0
        bytes_per_frame = CHANNELS * (BITS_PER_SAMPLE // 8)
        stream_start = len(output)

        while offset < len(pcm):
            chunk = pcm[offset : offset + RTP_PAYLOAD_BYTES]
            wire = bytearray(len(chunk))
            for i in range(0, len(chunk), 2):
                wire[i : i + 2] = struct.pack(">h", struct.unpack("<h", chunk[i : i + 2])[0])
            rtp = rtp_packet(PAYLOAD_TYPE, False, sequence, timestamp, ssrc, bytes(wire))
            mcast.send(rtp_frame(GUEST_MAC, HOST_MAC, HOST_IP, GUEST_IP, RTP_PORT, RTP_PORT, rtp))
            sequence += 1
            timestamp += len(chunk) // bytes_per_frame
            offset += len(chunk)

            pacing_deadline = time.monotonic() + (len(chunk) / (SAMPLE_RATE * bytes_per_frame))
            while time.monotonic() < pacing_deadline:
                drain_guest_output()
                frame = recv_or_none(mcast, max(pacing_deadline - time.monotonic(), 0))
                if frame is None:
                    continue
                payload = parse_incoming_rtp(frame)
                if payload is not None:
                    if first_outgoing_seen_at is None:
                        first_outgoing_seen_at = time.monotonic()
                    payload_type, seq, seen_ssrc = payload
                    outgoing_payload_types.append(payload_type)
                    outgoing_sequences.append(seq)
                    outgoing_ssrcs.add(seen_ssrc)

        read_until(b"receive stream started from", 15, stream_start)
        print("PASS: guest locked onto the host's RTP stream (receive side).")

        # Keep draining for a little longer so the send side has more
        # chances to produce packets even after the tone finished sending.
        drain_deadline = time.monotonic() + 2.0
        while time.monotonic() < drain_deadline:
            drain_guest_output()
            frame = recv_or_none(mcast, max(drain_deadline - time.monotonic(), 0))
            if frame is None:
                continue
            payload = parse_incoming_rtp(frame)
            if payload is not None:
                if first_outgoing_seen_at is None:
                    first_outgoing_seen_at = time.monotonic()
                payload_type, seq, seen_ssrc = payload
                outgoing_payload_types.append(payload_type)
                outgoing_sequences.append(seq)
                outgoing_ssrcs.add(seen_ssrc)
        tone_send_finished_at = drain_deadline - 2.0

        summary_start = len(output)
        print("sending a simulated local F12 keypress via the QEMU monitor...", file=sys.stderr)
        monitor_command("sendkey f12", wait=0.5)
        summary = read_until(b"byte(s) sent.", 10, summary_start)

        receive_match = re.search(
            rb"(\d+) packet\(s\) accepted, (\d+) byte\(s\) played, (\d+) dropped, "
            rb"(\d+) gap\(s\), (\d+) late/duplicate\.",
            summary,
        )
        if not receive_match:
            raise SystemExit(f"could not find receive-side summary line:\n{summary.decode('utf-8', 'replace')}")
        packets_accepted, bytes_played, bytes_dropped, _gap_count, _late_duplicate = (
            int(receive_match.group(index)) for index in range(1, 6)
        )
        if packets_accepted == 0 or bytes_played == 0:
            raise SystemExit("receive side accepted no packets/played no bytes -- duplex receive is not working")
        if bytes_dropped != 0:
            raise SystemExit(f"expected no dropped bytes on the receive side, got {bytes_dropped}")
        print(f"PASS: receive side: {packets_accepted} packet(s) accepted, {bytes_played} byte(s) played.")

        send_match = re.search(rb"send: (\d+) byte\(s\) sent\.", summary)
        if not send_match:
            raise SystemExit(f"could not find send-side summary line:\n{summary.decode('utf-8', 'replace')}")
        send_bytes_reported = int(send_match.group(1))
        if send_bytes_reported == 0:
            raise SystemExit("send side reported zero bytes sent -- duplex send is not working")
        print(f"PASS: send side: {send_bytes_reported} byte(s) sent (guest's own report).")

        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()

    if len(outgoing_sequences) == 0:
        raise SystemExit("never observed a single outgoing RTP packet from the guest's send side")
    if set(outgoing_payload_types) != {PAYLOAD_TYPE}:
        raise SystemExit(f"expected only payload type {PAYLOAD_TYPE}, saw {set(outgoing_payload_types)}")
    if len(outgoing_ssrcs) != 1:
        raise SystemExit(f"expected exactly one SSRC across the outgoing stream, got {outgoing_ssrcs}")
    if outgoing_sequences != sorted(outgoing_sequences) or len(outgoing_sequences) != len(set(outgoing_sequences)):
        raise SystemExit(f"expected strictly monotonic, gap-free outgoing sequence numbers, got {outgoing_sequences}")
    print(
        f"PASS: observed {len(outgoing_sequences)} structurally correct outgoing RTP "
        "packet(s) from the guest's send side (payload type, monotonic sequence, one SSRC)."
    )

    if first_outgoing_seen_at is None or tone_send_finished_at is None:
        raise SystemExit("could not determine timing of the first outgoing packet vs. tone completion")
    if first_outgoing_seen_at >= tone_send_finished_at:
        raise SystemExit(
            "the guest's first outgoing packet was only observed after the host finished "
            "sending its tone -- suggests receive and send are not actually concurrent"
        )
    print(
        "PASS: the guest's send side produced output while the receive side was still "
        "active -- both directions genuinely ran concurrently, not sequentially."
    )

    print("PASS: audio-rtp duplex mode works end to end.")


if __name__ == "__main__":
    main()
