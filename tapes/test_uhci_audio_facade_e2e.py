#!/usr/bin/env python3
"""End-to-end test for `audio.rs`'s `Audio::Uhci` variant: a USB Audio
Class playback device reached through the *same* tape-visible facade
ES1370/HDA already use, not the standalone `usb play` shell command.

Mirrors `tapes/test_audio_net_e2e.py`'s exact shape -- one continuous
`term net` session, `audio-net.bin` loaded/run in listen mode, this
script acting as the sending peer over the shared multicast socket --
but the QEMU guest is given only `piix3-usb-uhci` + `usb-audio`, no
ES1370/HDA at all. That makes the USB device `audio::get()`'s device 0
automatically, so `audio-net`'s existing, completely unmodified
`audio_submit`/non-indexed tape ABI path targets it with zero tape-code
changes -- proving the facade wiring itself (`uhci::UsbAudioDevice`,
`uhci::discover_playback_device`, `uhci::service_active_playback`
threaded through `poll_background_work`), not just the standalone
`usb play` command already verified by `tapes/test_uhci_play_e2e.py`.

Verified the same way that test verifies real playback: the QEMU-
captured WAV file's dominant frequency (a single-bin Goertzel DFT) must
match the sent tone, not just that the tape's own byte counters look
right.

Not part of `make test`; mirrors every other `tapes/test_*_e2e.py` focused
manual regression harness.
"""

import math
import os
import re
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

MCAST_GROUP = "230.42.42.65"
MCAST_PORT = 4265
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "audio-net.bin"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef65")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef66")
AUDIO_SENDER_MAC = bytes.fromhex("02deadbeef67")
GUEST_MAC = "52:54:00:12:34:81"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22
AUDIO_TAG_START = 0x40
AUDIO_TAG_DATA = 0x41
AUDIO_TAG_STOP = 0x42

# The real, verified format this project's UHCI driver negotiates
# against QEMU's synthetic usb-audio device (see
# tapes/test_uhci_play_e2e.py, and AGENTS.md's "USB" section).
SAMPLE_RATE = 48000
CHANNELS = 2
BITS_PER_SAMPLE = 16
DATA_CHUNK_CAPACITY = 1024
TONE_SECONDS = 1.0
TONE_HZ = 660
TONE_AMPLITUDE = 20000


def eth_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def synth_tone() -> bytes:
    frame_count = int(SAMPLE_RATE * TONE_SECONDS)
    samples = bytearray()
    for index in range(frame_count):
        value = int(TONE_AMPLITUDE * math.sin(2 * math.pi * TONE_HZ * index / SAMPLE_RATE))
        packed = struct.pack("<h", value)
        samples += packed * CHANNELS
    return bytes(samples)


def kvm_available() -> bool:
    """See tapes/test_audio_net_real_mic_e2e.py's own doc comment for the
    full story: under plain TCG software emulation, a guest's tight,
    uninterrupted busy loop (here: audio-net's own receive loop, now also
    servicing the USB isochronous engine every iteration) starves QEMU's
    own timer/frame-clock delivery for the loop's entire duration. This is
    purely a QEMU dev-loop timing fidelity concern, not a real-hardware
    one (already confirmed working on genuine USB hardware, see
    AGENTS.md's "USB" section) -- so this test uses KVM when available,
    with the same graceful TCG fallback otherwise."""
    try:
        fd = os.open("/dev/kvm", os.O_RDWR)
    except OSError:
        return False
    os.close(fd)
    return True


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


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-audio-net` first.")
    if not IMAGE.exists():
        raise SystemExit(f"missing {IMAGE}; run `make image` first")

    pcm = synth_tone()
    wav_capture = ROOT / "build" / "uhci_audio_facade_capture.wav"

    selector = selectors.DefaultSelector()
    output = bytearray()

    accel_args = ["-accel", "kvm", "-cpu", "host"] if kvm_available() else []
    if not accel_args:
        print(
            "WARNING: /dev/kvm is not accessible; falling back to plain TCG "
            "(see this test's own kvm_available()).",
            file=sys.stderr,
        )
    guest = subprocess.Popen(
        [
            "qemu-system-i386",
            "-drive",
            f"format=raw,file={IMAGE}",
            "-snapshot",
            *accel_args,
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
            f"wav,id=snd0,path={wav_capture}",
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

        audio_output = run_command(b"audio")
        if b"[0] USB Audio" not in audio_output:
            raise SystemExit(f"USB Audio device not found:\n{audio_output.decode('utf-8', 'replace')}")
        if f"{SAMPLE_RATE} Hz / {CHANNELS}ch / {BITS_PER_SAMPLE}-bit".encode() not in audio_output:
            raise SystemExit(
                f"unexpected local audio format:\n{audio_output.decode('utf-8', 'replace')}"
            )
        print("PASS: USB Audio device discovered through audio.rs, with the expected format.")

        load_start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        send_tape.send_tape(mcast, guest_mac, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until(b"Tape loaded", 30, load_start)
        read_until(b"fos> ", 5, load_start)

        run_start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until(f"[0] {SAMPLE_RATE} Hz / {CHANNELS}ch / {BITS_PER_SAMPLE}-bit".encode(), 10, run_start)
        read_until(b"Waiting for a matching audio stream", 10, run_start)
        print("PASS: audio-net tape started listen mode against the USB device's real format.")

        # Act as the sending peer ourselves, exactly like
        # tools/send_audio.py/the tape's own `send` mode. DEVICE_INDEX
        # (0) targets the guest's one local device -- the USB one, since
        # no ES1370/HDA is attached in this test.
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
        bytes_received, bytes_played, bytes_dropped = (int(g) for g in received_match.groups())
        if bytes_received != len(pcm):
            raise SystemExit(f"expected {len(pcm)} bytes received, tape reported {bytes_received}")
        if bytes_dropped != 0:
            raise SystemExit(f"tape reported {bytes_dropped} dropped byte(s), expected 0")
        print(
            f"PASS: tape received all {bytes_received} byte(s), "
            f"played {bytes_played}, dropped {bytes_dropped}."
        )

        recovery_start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(b"fos is a small educational 32-bit x86 operating system.", 10, recovery_start)
        print("PASS: the same remote shell session remained usable after the tape returned.")

        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()

    # The real proof: read the captured WAV file back and confirm the
    # isochronous OUT engine actually produced the right audio, not just
    # that the tape's own counters looked right. QEMU's wav audiodev
    # backend resamples internally, so check the tone's dominant
    # frequency (a Goertzel DFT, the same technique
    # tapes/test_uhci_play_e2e.py already uses) rather than assuming the
    # captured file's own sample rate matches the source.
    with __import__("wave").open(str(wav_capture), "rb") as captured:
        n_channels = captured.getnchannels()
        sample_rate = captured.getframerate()
        n_frames = captured.getnframes()
        raw = captured.readframes(n_frames)
    wav_capture.unlink(missing_ok=True)

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
    print(f"PASS: captured audio's dominant frequency matches the sent {TONE_HZ} Hz tone.")


if __name__ == "__main__":
    main()
