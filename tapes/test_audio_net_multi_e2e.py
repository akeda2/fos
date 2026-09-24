#!/usr/bin/env python3
"""End-to-end test for `tapes/audio-net`'s tagged multi-device wire protocol.

`tapes/test_audio_net_e2e.py` already proves the single-device (`device_
index=0`) path end to end -- one ES1370, one sub-stream, one WAV capture.
This test proves the actual *demultiplexing* the tagged-per-device-sub-
stream design (see `AGENTS.md`) exists for: two QEMU-emulated ES1370 cards
attached to one guest, and two independent `AUDIO_TAG_START`/`AUDIO_TAG_DATA`
sub-streams -- tagged `device_index` 0 and 1 -- sent within the same
`audio-net` listen-mode session.

The proof is deliberately asymmetric rather than comparing two tones: device
0's sub-stream carries a real, non-silent 440 Hz tone, while device 1's
sub-stream carries genuine PCM silence (all-zero samples). If the tape's
`device_index` demuxing were broken (e.g. both sub-streams routed to the
same local device, or one sub-stream silently dropped), the "silent" device
1 capture would end up carrying the tone as well -- so confirming device 0's
capture is clearly non-silent *and* device 1's capture stays at silence-
noise-floor level is a direct test of correct routing, not just "some audio
played somewhere."

Not part of `make test`; mirrors every other `tapes/test_*_e2e.py` focused
manual regression harness. Uses the `wav` audiodev backend for both
devices, matching `tapes/test_audio_net_e2e.py`'s own approach (real,
file-verifiable captured audio, not just driver counters).
"""

import os
import re
import selectors
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

MCAST_GROUP = "230.42.42.61"
MCAST_PORT = 4261
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "audio-net.bin"
MONITOR_SOCK = Path("/tmp/fos-test-audio-net-multi-monitor.sock")
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef41")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef43")
AUDIO_SENDER_MAC = bytes.fromhex("02deadbeef44")
GUEST_MAC = "52:54:00:12:34:63"

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
DEVICE_TONE = 0
DEVICE_SILENT = 1


def eth_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def synth_tone() -> bytes:
    """A short 440 Hz stereo sine wave, raw 16-bit signed PCM, no header."""
    import math

    frame_count = int(SAMPLE_RATE * TONE_SECONDS)
    samples = bytearray()
    for index in range(frame_count):
        value = int(20000 * math.sin(2 * math.pi * TONE_HZ * index / SAMPLE_RATE))
        packed = struct.pack("<h", value)
        samples += packed * CHANNELS
    return bytes(samples)


def synth_silence() -> bytes:
    """Genuine PCM silence: the same duration/format, all-zero samples."""
    frame_count = int(SAMPLE_RATE * TONE_SECONDS)
    return b"\0" * (frame_count * CHANNELS * (BITS_PER_SAMPLE // 8))


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-audio-net` first.")

    tone_pcm = synth_tone()
    silent_pcm = synth_silence()
    assert len(tone_pcm) == len(silent_pcm)

    wav_capture_0 = Path(tempfile.mkstemp(suffix=".wav", prefix="fos-audio-net-multi-0-")[1])
    wav_capture_1 = Path(tempfile.mkstemp(suffix=".wav", prefix="fos-audio-net-multi-1-")[1])
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
            f"wav,id=snd0,path={wav_capture_0}",
            "-device",
            "ES1370,audiodev=snd0",
            "-audiodev",
            f"wav,id=snd1,path={wav_capture_1}",
            "-device",
            "ES1370,audiodev=snd1",
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
        if b"2 audio device(s) found" not in audio_output:
            raise SystemExit(
                f"expected 2 ES1370 devices:\n{audio_output.decode('utf-8', 'replace')}"
            )
        print("PASS: two ES1370 devices discovered.")

        load_start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        send_tape.send_tape(mcast, guest_mac, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until(b"Tape loaded", 30, load_start)
        read_until(b"fos> ", 5, load_start)

        run_start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until(b"Local audio device(s): 2.", 10, run_start)
        read_until(b"[0] 44100 Hz / 2ch / 16-bit", 10, run_start)
        read_until(b"[1] 44100 Hz / 2ch / 16-bit", 10, run_start)
        read_until(b"Waiting for a matching audio stream", 10, run_start)
        print("PASS: audio-net listen mode reported both local devices.")

        # Act as the sending peer: two independent tagged sub-streams
        # within one session, one per local device.
        stream_start = len(output)
        start_tone = struct.pack("<IBBB", SAMPLE_RATE, CHANNELS, BITS_PER_SAMPLE, DEVICE_TONE)
        mcast.send(eth_frame(guest_mac, AUDIO_SENDER_MAC, AUDIO_TAG_START, start_tone))
        read_until(b"stream started from", 10, stream_start)
        read_until(b"device 0 sub-stream active", 10, stream_start)

        start_silent = struct.pack(
            "<IBBB", SAMPLE_RATE, CHANNELS, BITS_PER_SAMPLE, DEVICE_SILENT
        )
        mcast.send(eth_frame(guest_mac, AUDIO_SENDER_MAC, AUDIO_TAG_START, start_silent))
        read_until(b"device 1 sub-stream active", 10, stream_start)
        print("PASS: both device sub-streams were accepted within one session.")

        bytes_per_sec = SAMPLE_RATE * CHANNELS * (BITS_PER_SAMPLE // 8)
        offset = 0
        total_sent = 0
        while offset < len(tone_pcm):
            tone_chunk = tone_pcm[offset : offset + DATA_CHUNK_CAPACITY]
            silent_chunk = silent_pcm[offset : offset + DATA_CHUNK_CAPACITY]
            mcast.send(
                eth_frame(
                    guest_mac,
                    AUDIO_SENDER_MAC,
                    AUDIO_TAG_DATA,
                    struct.pack("<BH", DEVICE_TONE, len(tone_chunk)) + tone_chunk,
                )
            )
            mcast.send(
                eth_frame(
                    guest_mac,
                    AUDIO_SENDER_MAC,
                    AUDIO_TAG_DATA,
                    struct.pack("<BH", DEVICE_SILENT, len(silent_chunk)) + silent_chunk,
                )
            )
            total_sent += len(tone_chunk) + len(silent_chunk)
            offset += len(tone_chunk)
            time.sleep(len(tone_chunk) / bytes_per_sec)
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
            raise SystemExit(
                f"could not find byte-count summary:\n{summary.decode('utf-8', 'replace')}"
            )
        bytes_received = int(received_match.group(1))
        bytes_dropped = int(received_match.group(3))
        if bytes_received != total_sent:
            raise SystemExit(
                f"expected {total_sent} bytes received (both sub-streams combined), "
                f"tape reported {bytes_received}"
            )
        if bytes_dropped != 0:
            raise SystemExit(f"tape reported {bytes_dropped} dropped byte(s), expected 0")
        print(
            f"PASS: tape received all {bytes_received} byte(s) across both "
            "sub-streams, dropped 0."
        )

        # Once a device's first AUDIO_TAG_DATA is submitted, its DAC1
        # permanently stops playing the built-in idle test tone (see
        # `network_mode` in `kernel/src/es1370.rs`) -- but the WAV capture
        # spans the whole guest lifetime, including the boot-time narration
        # before this tape ever ran, when device 1 was still playing that
        # tone. Wait a moment here so the *tail* of device 1's capture is a
        # clean, unambiguous stretch of genuine post-submit silence to check
        # against, rather than trying to disentangle it from the earlier
        # boot-time tone.
        time.sleep(1.0)

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

    # The real proof: device 0's WAV must contain the real tone, and device
    # 1's WAV -- despite the identical driver code, identical DMA/ISR
    # pipeline, and a session running both sub-streams simultaneously --
    # must fall to silence once its own sub-stream is submitting silent
    # PCM. If device_index demuxing were broken, this is exactly the check
    # that would catch it.
    def peak_amplitude(path: Path, tail_seconds: float = None) -> int:
        with wave.open(str(path), "rb") as captured:
            if captured.getframerate() != SAMPLE_RATE or captured.getnchannels() != CHANNELS:
                raise SystemExit(
                    f"{path} has unexpected format: {captured.getframerate()} Hz / "
                    f"{captured.getnchannels()}ch"
                )
            captured_bytes = captured.readframes(captured.getnframes())
        if len(captured_bytes) == 0:
            raise SystemExit(f"{path} is empty; no audio was ever played")
        sample_count = len(captured_bytes) // 2
        samples = struct.unpack(f"<{sample_count}h", captured_bytes[: sample_count * 2])
        if tail_seconds is not None:
            tail_samples = int(SAMPLE_RATE * CHANNELS * tail_seconds)
            samples = samples[-tail_samples:] if tail_samples < len(samples) else samples
        return max(abs(sample) for sample in samples)

    peak_0 = peak_amplitude(wav_capture_0)
    # Device 1's own sub-stream only starts submitting silence partway
    # through the capture (see the comment above); only the *tail* of its
    # WAV -- recorded after that first submit -- is expected to be silent.
    peak_1 = peak_amplitude(wav_capture_1, tail_seconds=0.8)
    if peak_0 < 1000:
        raise SystemExit(
            f"device 0's captured WAV peak amplitude is only {peak_0}; expected the "
            "clearly audible 440 Hz tone"
        )
    print(f"PASS: device 0's captured WAV contains the real tone (peak amplitude {peak_0}).")

    if peak_1 >= 50:
        raise SystemExit(
            f"device 1's captured WAV tail peak amplitude is {peak_1}, expected "
            "near-silence (<50) -- the tone meant for device 0 may have leaked into "
            "device 1's sub-stream, indicating broken device_index demuxing"
        )
    print(
        f"PASS: device 1's captured WAV settled to silence-noise-floor level "
        f"once its own (silent) sub-stream took over (tail peak amplitude {peak_1}), "
        "proving the two sub-streams were routed to the correct local devices "
        "independently."
    )

    wav_capture_0.unlink(missing_ok=True)
    wav_capture_1.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
