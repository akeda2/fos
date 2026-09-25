#!/usr/bin/env python3
"""End-to-end test for isochronous USB Audio playback in
`kernel/src/uhci.rs` (`IsocOutStream`) and the `usb play` shell command
(`kernel/src/shell.rs`).

This test proves:

- `usb play` finds a real USB Audio Class 1.0 playback endpoint on
  QEMU's `usb-audio` device (48 kHz/16-bit/stereo, 192 bytes/packet --
  the real values `usb play`'s own descriptor walk reports, matching
  what `tapes/test_uhci_e2e.py` already captured directly from this same
  device's configuration descriptor).
- Playback actually blocks for close to the tone's real duration
  (~1000 ms for a 1 second tone), proving genuine ~1 kHz-paced
  isochronous streaming happened rather than an instant no-op.
- No isochronous packet reported an error (`usb play`'s own
  `IsocOutStream::error_count` warning line is absent) -- the exact
  class of silent failure a wrong TD-token endpoint-field bit position
  produced during development (packets silently reached a nonexistent
  device address instead of the real one).
- The captured WAV file has real energy and a single-bin Goertzel DFT
  check (the same dependency-free technique
  `tapes/test_audio_rtp_opus_e2e.py` already uses) finds the seeded
  tone's frequency as the dominant one.
- A second, independent `usb play` run in the same session succeeds
  identically -- proving `IsocOutStream::stop`'s Frame List restoration
  actually leaves the schedule clean for a following control transfer
  or stream.

Not part of `make test`; mirrors every other `tapes/test_*_e2e.py`
focused manual regression harness.
"""

import math
import os
import selectors
import struct
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
sys.path.insert(0, str(ROOT / "tapes"))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.64"
MCAST_PORT = 4264
IMAGE = ROOT / "build" / "fos.img"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef64")
GUEST_MAC = "52:54:00:12:34:73"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

SAMPLE_RATE = 48000
CHANNELS = 2
TONE_SECONDS = 1.0
TONE_HZ = 660
TONE_AMPLITUDE = 20000
TONE_NAME = "usbtone.pcm"
EXPECTED_PACKET_BYTES = 192  # 48 samples/ms * 2ch * 2 bytes -- verified against the real device


def synth_tone() -> bytes:
    """A stereo sine wave, raw 16-bit signed PCM, no header -- exactly
    the format `usb play` streams unmodified to the device."""
    frame_count = int(SAMPLE_RATE * TONE_SECONDS)
    samples = bytearray()
    for index in range(frame_count):
        value = int(TONE_AMPLITUDE * math.sin(2 * math.pi * TONE_HZ * index / SAMPLE_RATE))
        packed = struct.pack("<h", value)
        samples += packed * CHANNELS
    return bytes(samples)


def eth_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def rebuild_image_with_extra_file(name: str, path: Path) -> None:
    subprocess.run(
        ["make", "image", f"EXTRA_FOSFS_FILES=--file {name} {path}"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.DEVNULL,
    )


def restore_default_image() -> None:
    subprocess.run(["make", "image"], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)


def goertzel_power(samples, sample_rate: int, freq: float) -> float:
    n = len(samples)
    k = int(0.5 + (n * freq) / sample_rate)
    w = 2 * math.pi * k / n
    coeff = 2 * math.cos(w)
    s_prev, s_prev2 = 0.0, 0.0
    for sample in samples:
        s0 = sample + coeff * s_prev - s_prev2
        s_prev2 = s_prev
        s_prev = s0
    return s_prev2**2 + s_prev**2 - coeff * s_prev * s_prev2


def check_playback_output(text: str) -> float:
    """Verifies one `usb play` run's printed output; returns elapsed ms."""
    if "No USB Audio Class playback device found" in text:
        raise SystemExit(f"no playback device found:\n{text}")
    if f"{SAMPLE_RATE} Hz/16-bit/{CHANNELS}ch, {EXPECTED_PACKET_BYTES} bytes/packet" not in text:
        raise SystemExit(f"unexpected discovered format:\n{text}")
    if "isochronous packet(s) failed" in text:
        raise SystemExit(f"playback reported failed isochronous packets:\n{text}")

    import re

    match = re.search(r"Playback complete \((\d+) ms\)\.", text)
    if not match:
        raise SystemExit(f"no successful playback-complete line found:\n{text}")
    return float(match.group(1))


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"missing {IMAGE}; run `make image` first")

    tone_bytes = synth_tone()
    tone_path = ROOT / "build" / "uhci_play_test_tone.pcm"
    tone_path.write_bytes(tone_bytes)
    print(f"Seeding {len(tone_bytes)} byte(s) as '{TONE_NAME}'...")
    rebuild_image_with_extra_file(TONE_NAME, tone_path)
    tone_path.unlink(missing_ok=True)

    selector = selectors.DefaultSelector()
    output = bytearray()
    wav_path = ROOT / "build" / "uhci_play_test_capture.wav"

    guest = subprocess.Popen(
        [
            "qemu-system-i386",
            "-drive",
            f"format=raw,file={IMAGE}",
            "-snapshot",
            "-serial",
            "stdio",
            "-no-reboot",
            "-no-shutdown",
            "-netdev",
            f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
            "-device",
            f"rtl8139,netdev=net0,mac={GUEST_MAC}",
            "-usb",
            "-device",
            "piix3-usb-uhci,id=uhci",
            "-device",
            "usb-audio,bus=uhci.0,audiodev=snd0",
            "-audiodev",
            f"wav,id=snd0,path={wav_path}",
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

    def run_command(command: bytes, timeout: float = 20) -> bytes:
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

        first_text = run_command(f"usb play {TONE_NAME}".encode()).decode("utf-8", "replace")
        elapsed_ms = check_playback_output(first_text)
        print(f"PASS: 'usb play' found the real endpoint and streamed with no packet errors.")

        expected_ms = TONE_SECONDS * 1000
        if abs(elapsed_ms - expected_ms) > expected_ms * 0.2:
            raise SystemExit(
                f"playback duration {elapsed_ms} ms is not close to the expected "
                f"{expected_ms} ms -- suspiciously fast/slow for real 1 kHz-paced "
                f"streaming:\n{first_text}"
            )
        print(f"PASS: playback took {elapsed_ms:.0f} ms, close to the tone's real {expected_ms:.0f} ms duration.")

        second_text = run_command(f"usb play {TONE_NAME}".encode()).decode("utf-8", "replace")
        check_playback_output(second_text)
        print("PASS: a second, independent 'usb play' run succeeds identically.")

        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()
        restore_default_image()

    with __import__("wave").open(str(wav_path), "rb") as captured:
        n_channels = captured.getnchannels()
        sample_rate = captured.getframerate()
        n_frames = captured.getnframes()
        raw = captured.readframes(n_frames)

    if n_frames == 0:
        raise SystemExit("captured WAV file has zero frames -- no audio was actually recorded")

    sample_count = n_frames * n_channels
    samples = struct.unpack(f"<{sample_count}h", raw[: struct.calcsize(f'<{sample_count}h')])
    left_channel = samples[0::n_channels]

    energy = sum(sample * sample for sample in left_channel) / len(left_channel)
    if energy < 1000:
        raise SystemExit(f"captured audio has essentially no energy ({energy:.1f}) -- looks silent")
    print(f"PASS: captured WAV has real energy ({energy:.1f}).")

    target_power = goertzel_power(left_channel, sample_rate, TONE_HZ)
    control_power = goertzel_power(left_channel, sample_rate, TONE_HZ + 340)
    if target_power < control_power * 10:
        raise SystemExit(
            f"{TONE_HZ} Hz is not clearly dominant (power {target_power:.3e} vs "
            f"control {control_power:.3e} at an off-target frequency)"
        )
    print(f"PASS: captured audio's dominant frequency matches the seeded {TONE_HZ} Hz tone.")

    wav_path.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
