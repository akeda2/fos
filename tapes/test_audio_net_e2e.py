#!/usr/bin/env python3
"""End-to-end test for the raw-Ethernet `audio-net` PCM streaming tape.

One QEMU guest carries both an RTL8139 (for the multicast Ethernet segment
every other e2e test already uses) and a QEMU-emulated ES1370, captured to a
host-side WAV file via `-audiodev wav`. The test drives the guest entirely
over one continuous `term net` session (matching every other tape e2e test's
convention): it loads and runs `audio-net.bin` in listen mode, then acts as
the *sender* peer itself -- sending `AUDIO_TAG_START`/`AUDIO_TAG_DATA`/
`AUDIO_TAG_STOP` frames directly over the shared multicast socket, exactly
the wire protocol `tools/send_audio.py`/the tape's own `send` mode use.

This proves the whole path end to end: the tape's listen mode accepts a
matching-format stream, forwards every payload into `audio_submit`, and the
ES1370 DMA/ISR pipeline actually plays it -- verified by reading the
captured WAV file back and confirming it contains real, non-silent audio
matching the sent tone, not just that the tape's own byte counters look
right.

Not part of `make test`; this is the focused manual regression test for this
tape and the network-fed side of `kernel/src/audio.rs`, mirroring every
other `tapes/test_*_e2e.py` harness. Actual audible verification (does it
sound right on a real Sound Blaster PCI card) remains a separate, real-
hardware-only step -- see `AGENTS.md`'s "Real hardware notes" and the
`audio-real-hw` project item.
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

MCAST_GROUP = "230.42.42.57"
MCAST_PORT = 4257
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "audio-net.bin"
MONITOR_SOCK = Path("/tmp/fos-test-audio-net-monitor.sock")
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef31")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef32")
AUDIO_SENDER_MAC = bytes.fromhex("02deadbeef33")
GUEST_MAC = "52:54:00:12:34:60"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22
AUDIO_TAG_START = 0x40
AUDIO_TAG_DATA = 0x41
AUDIO_TAG_STOP = 0x42

SAMPLE_RATE = 44100
CHANNELS = 2
BITS_PER_SAMPLE = 16
DATA_CHUNK_CAPACITY = 1024
TONE_SECONDS = 0.2
TONE_HZ = 440


def eth_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def synth_tone() -> bytes:
    """A short 440 Hz stereo sine wave, raw 16-bit signed PCM, no header."""
    frame_count = int(SAMPLE_RATE * TONE_SECONDS)
    samples = bytearray()
    for index in range(frame_count):
        value = int(20000 * math.sin(2 * math.pi * TONE_HZ * index / SAMPLE_RATE))
        packed = struct.pack("<h", value)
        samples += packed * CHANNELS
    return bytes(samples)


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-audio-net` first.")

    pcm = synth_tone()
    wav_capture = Path(tempfile.mkstemp(suffix=".wav", prefix="fos-audio-net-")[1])
    if MONITOR_SOCK.exists():
        MONITOR_SOCK.unlink()

    selector = selectors.DefaultSelector()
    output = bytearray()

    guest = subprocess.Popen(
        [
            "qemu-system-i386",
            "-drive",
            f"format=raw,file={IMAGE}",
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
            f"rtl8139,netdev=net0,mac={GUEST_MAC}",
            "-audiodev",
            f"wav,id=snd0,path={wav_capture}",
            "-device",
            "ES1370,audiodev=snd0",
            "-display",
            "none",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
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
                    raise SystemExit(
                        f"serial stream ended early:\n{output.decode('utf-8', 'replace')}"
                    )
                output.extend(chunk)
        return bytes(output[start:])

    def run_command(command: bytes, timeout: float = 10) -> bytes:
        start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, command + b"\n"))
        return read_until(b"fos> ", timeout, start)

    guest_mac = send_tape.parse_mac(GUEST_MAC)

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

        read_until(b"Waiting for a remote terminal client to connect", 30)
        mcast.send(eth_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        import re

        audio_output = run_command(b"audio")
        if b"[0] ES1370" not in audio_output:
            raise SystemExit(f"ES1370 not found:\n{audio_output.decode('utf-8', 'replace')}")
        if b"44100 Hz / 2ch / 16-bit" not in audio_output:
            raise SystemExit(
                f"unexpected local audio format:\n{audio_output.decode('utf-8', 'replace')}"
            )
        print("PASS: ES1370 discovered with the expected fixed format.")

        load_start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        send_tape.send_tape(mcast, guest_mac, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until(b"Tape loaded", 30, load_start)
        read_until(b"fos> ", 5, load_start)

        run_start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until(b"[0] 44100 Hz / 2ch / 16-bit", 10, run_start)
        read_until(b"Waiting for a matching audio stream", 10, run_start)
        print("PASS: audio-net tape started listen mode with the correct local format.")

        # Act as the sending peer ourselves: this is exactly the wire
        # protocol `tools/send_audio.py`/the tape's own `send` mode use.
        # DEVICE_INDEX (0) targets the guest's one local device.
        DEVICE_INDEX = 0
        stream_start = len(output)
        start_payload = struct.pack("<IBBB", SAMPLE_RATE, CHANNELS, BITS_PER_SAMPLE, DEVICE_INDEX)
        mcast.send(eth_frame(guest_mac, AUDIO_SENDER_MAC, AUDIO_TAG_START, start_payload))
        read_until(b"stream started from", 10, stream_start)
        print("PASS: tape accepted the matching-format stream and locked onto the sender.")

        bytes_per_sec = SAMPLE_RATE * CHANNELS * (BITS_PER_SAMPLE // 8)
        offset = 0
        while offset < len(pcm):
            chunk = pcm[offset : offset + DATA_CHUNK_CAPACITY]
            payload = struct.pack("<BH", DEVICE_INDEX, len(chunk)) + chunk
            mcast.send(eth_frame(guest_mac, AUDIO_SENDER_MAC, AUDIO_TAG_DATA, payload))
            offset += len(chunk)
            time.sleep(len(chunk) / bytes_per_sec)
        mcast.send(eth_frame(guest_mac, AUDIO_SENDER_MAC, AUDIO_TAG_STOP))

        stop_start = len(output)
        read_until(b"stream stopped by sender", 15, stop_start)
        summary = read_until(b"Left audio-net; back at the fos shell.", 10, stop_start)
        read_until(b"Tape returned; back at the fos shell.", 10, stop_start)

        received_match = re.search(
            rb"Bytes received: (\d+), played: (\d+), dropped: (\d+)\.",
            summary,
        )
        if not received_match:
            raise SystemExit(f"could not find byte-count summary:\n{summary.decode('utf-8', 'replace')}")
        bytes_received = int(received_match.group(1))
        bytes_played = int(received_match.group(2))
        bytes_dropped = int(received_match.group(3))
        if bytes_received != len(pcm):
            raise SystemExit(
                f"expected {len(pcm)} bytes received, tape reported {bytes_received}"
            )
        if bytes_dropped != 0:
            raise SystemExit(f"tape reported {bytes_dropped} dropped byte(s), expected 0")
        print(
            f"PASS: tape received all {bytes_received} byte(s), "
            f"played {bytes_played}, dropped {bytes_dropped}."
        )

        recovery_start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(
            b"fos is a small educational 32-bit x86 operating system.",
            10,
            recovery_start,
        )
        print("PASS: the same remote shell session remained usable after the tape returned.")

        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()

    # The real proof: read the captured WAV file back and confirm the DMA/ISR
    # pipeline actually produced non-silent audio, not just that the tape's
    # own counters looked right.
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
    samples = struct.unpack(f"<{sample_count}h", captured_bytes[: sample_count * 2])
    peak = max(abs(sample) for sample in samples)
    if peak < 1000:
        raise SystemExit(
            f"captured WAV peak amplitude is only {peak}; expected clearly audible, "
            "non-silent output"
        )
    print(f"PASS: captured WAV contains real, non-silent audio (peak amplitude {peak}).")

    wav_capture.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
