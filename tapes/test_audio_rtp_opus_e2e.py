#!/usr/bin/env python3
"""End-to-end test for Opus decode support in the RTP/UDP audio tape
(`audio-rtp`), added on top of `tapes/opus-codec`'s vendored, fixed-point
libopus decoder (see AGENTS.md's "Real-time audio streaming over RTP/UDP"
section).

Unlike `tapes/test_audio_rtp_e2e.py` (which hand-builds every RTP packet
byte-for-byte to test the raw-PCM path in isolation), this test sources
its RTP packets from a *real* `ffmpeg`/`libopus` encoder: a short tone is
encoded straight to genuine RTP/UDP packets on localhost (`ffmpeg -f rtp
rtp://127.0.0.1:PORT`), captured with a plain UDP socket, and replayed to
the guest with only the RTP payload-type byte patched from whatever
ffmpeg's own SDP negotiation picked to this project's fixed
`PAYLOAD_TYPE_OPUS` (98) -- sequence numbers, timestamps, SSRC, and every
Opus-encoded payload byte are exactly what a real, independent Opus
sender produced, not anything this test constructs itself. This is the
same technique used to validate the vendored decoder during development
(see the AGENTS.md history for that investigation); this test makes it a
permanent, repeatable regression check.

Opus decode is only offered by `listen_mode` when the local device's
native format is 16-bit (this vendored build has no float API, so
`opus_decode` only ever produces 16-bit PCM) *and* its sample rate is one
of the five libopus accepts (8/12/16/24/48 kHz) -- ES1370's fixed 44100 Hz
satisfies neither the *documented* accepted-Fs constraint, so this test
uses an Intel HDA guest instead (`intel-hda` + `hda-output`), which this
project's own `negotiate_format` already negotiates to 48000 Hz/16-bit
against QEMU's synthetic codec -- mirroring `tapes/test_hda_playback_e2e.py`'s
device flags exactly, just with `audio-rtp.bin` in place of `audio-net.bin`.

Not part of `make test`; run it after `make tape-audio-rtp` and `make
image`. Requires a real `ffmpeg` built with `libopus` on the host. Uses
`-accel kvm` when `/dev/kvm` is accessible (see `kvm_available()`'s doc
comment), falling back to plain TCG with a printed warning otherwise --
mirroring `tapes/test_audio_net_real_mic_e2e.py`'s own precedent for the
same underlying QEMU/TCG timing artifact.
"""

import math
import os
import selectors
import socket as socketlib
import struct
import subprocess
import sys
import time
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402
from test_audio_rtp_e2e import (  # noqa: E402
    GUEST_IP,
    GUEST_MAC,
    GUEST_MAC_STR,
    HOST_IP,
    HOST_MAC,
    IMAGE,
    MCAST_GROUP,
    MCAST_PORT,
    RTP_PORT,
    TAPE,
    TAPE_SENDER_MAC,
    TERM_CLIENT_MAC,
    TERM_TAG_BYE,
    TERM_TAG_DATA,
    TERM_TAG_HELLO,
    arp_request_frame,
    parse_arp,
    recv_or_none,
    rtp_frame,
    term_frame,
)

MONITOR_SOCK = Path("/tmp/fos-test-audio-rtp-opus-monitor.sock")

# The HDA-negotiated format `negotiate_format` (kernel/src/hda.rs) picks
# against QEMU's synthetic `hda-output` codec -- one of libopus's five
# accepted decode rates, unlike ES1370's fixed 44100 Hz.
SAMPLE_RATE = 48000
CHANNELS = 2
BITS_PER_SAMPLE = 16

PAYLOAD_TYPE_RAW = 96  # audio_rtp_tape::PAYLOAD_TYPE_L16_48000_STEREO
PAYLOAD_TYPE_OPUS = 98  # audio_rtp_tape::PAYLOAD_TYPE_OPUS

TONE_HZ = 660  # HDA's fill_period plays silence (not a built-in test
# tone) before network mode engages, so no particular frequency needs
# avoiding here -- chosen only to differ from the raw-PCM test's 880 Hz.
TONE_DURATION_SECONDS = 1.0
OPUS_CAPTURE_PORT = 15006


def capture_opus_rtp_packets(tone_hz: float, duration: float, port: int) -> list[bytes]:
    """Encodes a tone with a real `ffmpeg`+`libopus` straight to genuine
    RTP/UDP packets on localhost, and returns each packet's raw bytes
    (12-byte RTP header + Opus payload) exactly as a real sender produced
    them."""
    sock = socketlib.socket(socketlib.AF_INET, socketlib.SOCK_DGRAM)
    sock.bind(("127.0.0.1", port))
    sock.settimeout(5.0)

    proc = subprocess.Popen(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "warning",
            "-f",
            "lavfi",
            "-i",
            f"sine=frequency={tone_hz}:duration={duration}:sample_rate={SAMPLE_RATE}",
            "-ac",
            str(CHANNELS),
            # ffmpeg's lavfi `sine` source defaults to a quiet ~2900/32767
            # peak; QEMU's synthetic hda-output codec/audiodev chain then
            # attenuates whatever reaches it by roughly another 13x on top
            # of that (confirmed empirically against
            # tapes/test_hda_playback_e2e.py's own already-passing, much
            # louder 20000-amplitude source and its resulting ~1491 peak)
            # -- so an unboosted sine here would clear neither this test's
            # amplitude assertion nor any real audibility, independent of
            # whether Opus decode itself is correct.
            "-af",
            "volume=10",
            "-c:a",
            "libopus",
            "-ar",
            str(SAMPLE_RATE),
            "-ac",
            str(CHANNELS),
            "-application",
            "audio",
            "-f",
            "rtp",
            f"rtp://127.0.0.1:{port}",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    packets: list[bytes] = []
    try:
        while True:
            packets.append(sock.recv(2048))
    except socketlib.timeout:
        pass
    finally:
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        sock.close()

    if not packets:
        raise SystemExit(
            "ffmpeg produced no RTP packets; is this host's ffmpeg built with libopus?"
        )
    return packets


def with_payload_type(packet: bytes, payload_type: int) -> bytes:
    patched = bytearray(packet)
    patched[1] = (patched[1] & 0x80) | (payload_type & 0x7F)
    return bytes(patched)


def goertzel_magnitude(samples: list[int], sample_rate: int, target_hz: float) -> float:
    """A single-bin DFT magnitude (the Goertzel algorithm) -- avoids a
    numpy dependency (this project's host tools stay standard-library
    only, e.g. tools/send_frame.py's own stated convention) while being
    far more robust than a zero-crossing count against a signal that
    isn't a perfectly clean, gap-free sine. `test_audio_rtp_e2e.py`'s own
    `dominant_frequency`/`contains_frequency` (zero-crossing counting in
    small fixed windows) is accurate for that test's hand-built,
    byte-for-byte raw PCM, but proved unreliable here: this test's audio
    passes through a real Opus encode/decode round trip plus whatever
    brief HDA DMA-underrun micro-gaps QEMU's software-emulated guest CPU
    introduces under real-time pacing, and a handful of discontinuities
    within a 50 ms window can throw off a simple crossing count even
    though the underlying decoded audio is genuinely correct (confirmed
    against an independent, full-signal FFT during development)."""
    n = len(samples)
    if n == 0:
        return 0.0
    k = round(n * target_hz / sample_rate)
    omega = 2 * math.pi * k / n
    coeff = 2 * math.cos(omega)
    s_prev = 0.0
    s_prev2 = 0.0
    for sample in samples:
        s = sample + coeff * s_prev - s_prev2
        s_prev2 = s_prev
        s_prev = s
    real = s_prev - s_prev2 * math.cos(omega)
    imag = s_prev2 * math.sin(omega)
    return math.sqrt(real * real + imag * imag) / n


def monitor_command(command: str, wait: float = 0.5) -> None:
    # A local copy of test_audio_rtp_e2e's helper of the same name: that
    # one closes over *its own* module's MONITOR_SOCK global, not this
    # file's, so importing it directly would silently try to connect to
    # the wrong (and in this test, nonexistent) socket path.
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


def kvm_available() -> bool:
    """See tapes/test_audio_net_real_mic_e2e.py's own doc comment for the
    full story: under plain TCG software emulation, a guest's tight,
    uninterrupted busy loop starves QEMU's own timer/interrupt delivery
    for the loop's entire duration (TCG holds one Big QEMU Lock almost
    continuously in that mode) -- confirmed there down to an unrelated
    CPU-bound tape showing the identical symptom, and fixed immediately
    by switching to `-accel kvm` (real hardware-virtualized execution),
    no other change. SILK's fixed-point encoder/decoder analysis is
    exactly this kind of sustained CPU-bound loop, so this test uses the
    same acceleration when available, with the same graceful TCG
    fallback otherwise -- this is purely a QEMU dev-loop timing fidelity
    concern, not a real-hardware one: this project targets Pentium 4 and
    later, and Opus decode is comfortably real-time on hardware far
    slower than that."""
    try:
        fd = os.open("/dev/kvm", os.O_RDWR)
    except OSError:
        return False
    os.close(fd)
    return True


def run_guest(image: Path, wav_capture: Path):
    if MONITOR_SOCK.exists():
        MONITOR_SOCK.unlink()
    accel_args = ["-accel", "kvm", "-cpu", "host"] if kvm_available() else []
    if not accel_args:
        print(
            "WARNING: /dev/kvm is not accessible; falling back to plain TCG "
            "(see kvm_available()'s doc comment -- a QEMU timing artifact, "
            "not a real regression, but underrun-driven gaps become more "
            "likely).",
            file=sys.stderr,
        )
    return subprocess.Popen(
        [
            "qemu-system-i386",
            # OpusDecoder::new allocates its decoder-state buffer via the
            # tape ABI's alloc callback, served from pmem's "big pool"
            # above TRACKED_BYTES (128 MiB) -- same reasoning as
            # test_audio_rtp_e2e.py's send-mode `-m 256`.
            "-m",
            "256",
            *accel_args,
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
            "intel-hda",
            "-device",
            "hda-output,audiodev=snd0",
            "-display",
            "none",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-audio-rtp` first")
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} does not exist; run `make image` first")

    opus_packets = [
        with_payload_type(packet, PAYLOAD_TYPE_OPUS)
        for packet in capture_opus_rtp_packets(TONE_HZ, TONE_DURATION_SECONDS, OPUS_CAPTURE_PORT)
    ]
    print(f"captured {len(opus_packets)} real Opus/RTP packet(s) from ffmpeg.")

    wav_capture = Path("/tmp/fos-test-audio-rtp-opus-capture.wav")
    if wav_capture.exists():
        wav_capture.unlink()

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

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

        read_until(b"Waiting for a remote terminal client to connect", 30)
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        audio_start = len(output)
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"audio\n"))
        audio_output = read_until(b"fos> ", 10, audio_start)
        if b"48000 Hz / 2ch / 16-bit" not in audio_output:
            raise SystemExit(f"HDA did not negotiate 48000 Hz/2ch/16-bit:\n{audio_output.decode('utf-8', 'replace')}")
        print("PASS: HDA negotiated the expected 48000 Hz / 2ch / 16-bit format.")

        load_start = len(output)
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        send_tape.send_tape(mcast, GUEST_MAC, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until(b"Tape loaded", 30, load_start)
        read_until(b"fos> ", 5, load_start)

        run_start = len(output)
        command = f"tape run static {'.'.join(map(str, GUEST_IP))}/24 0.0.0.0\n".encode()
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, command))
        read_until(f"RTP payload type {PAYLOAD_TYPE_RAW}".encode(), 10, run_start)
        read_until(b"Listening on UDP port 5004", 10, run_start)
        print("PASS: audio-rtp tape started listen mode with the raw-PCM payload type.")
        read_until(f"Opus decode also accepted (RTP payload type {PAYLOAD_TYPE_OPUS})".encode(), 5, run_start)
        print("PASS: Opus decoder started (48000 Hz/2ch is a libopus-accepted rate).")

        # Prove ARP still works on this path too, exactly as
        # test_audio_rtp_e2e.py does for the raw-PCM device.
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

        stream_start = len(output)
        for packet in opus_packets:
            mcast.send(rtp_frame(GUEST_MAC, HOST_MAC, HOST_IP, GUEST_IP, RTP_PORT, RTP_PORT, packet))
            # Real 20 ms Opus-frame pacing, matching ffmpeg's own send
            # rate: each packet only carries ~20 ms of audio, so pacing
            # any slower than that (an earlier version of this test used
            # 160 ms, based on a mistaken TCG-starvation theory) just
            # inserts real silence gaps of its own between packets --
            # not a decode/DMA bug, just this test sending slower than
            # the stream it claims to be. `run_guest`'s `-accel kvm` (see
            # `kvm_available()`) is the actual fix for genuine QEMU/TCG
            # timing starvation; it does not need help from artificial
            # pacing on top of it.
            time.sleep(0.02)

        read_until(b"stream started from", 15, stream_start)
        print("PASS: tape locked onto the real ffmpeg-sourced Opus RTP stream.")

        monitor_command("sendkey f12", wait=0.5)
        summary = read_until(b"late/duplicate.", 10, stream_start)

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
        if packets_accepted == 0:
            raise SystemExit("no Opus packets were accepted at all")
        if bytes_played == 0:
            raise SystemExit("Opus packets were accepted but nothing was ever submitted for playback")
        print(
            f"PASS: {packets_accepted} Opus packet(s) accepted, {bytes_played} decoded "
            f"byte(s) played, {bytes_dropped} dropped, {gap_count} gap(s), "
            f"{late_duplicate} late/duplicate."
        )

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
    left = samples[::CHANNELS]
    peak = max(abs(sample) for sample in left)
    if peak < 1000:
        raise SystemExit(f"captured WAV peak amplitude is only {peak}; expected clearly audible output")

    # The active region: HDA's `fill_period` plays plain silence (not a
    # built-in test tone, unlike ES1370) before network mode engages, so
    # trimming to where real signal actually starts/ends keeps the
    # Goertzel bin from being diluted by leading/trailing silence.
    active_indices = [i for i, sample in enumerate(left) if abs(sample) > 1000]
    active = left[active_indices[0] : active_indices[-1] + 1]

    magnitude_at_tone = goertzel_magnitude(active, SAMPLE_RATE, TONE_HZ)
    # A clearly different, comfortably-in-band frequency, as a sanity
    # check that the tone bin isn't just picking up broadband energy.
    magnitude_off_target = goertzel_magnitude(active, SAMPLE_RATE, TONE_HZ * 2)
    if magnitude_at_tone < magnitude_off_target * 3:
        raise SystemExit(
            f"Goertzel magnitude at ~{TONE_HZ} Hz ({magnitude_at_tone:.0f}) is not clearly "
            f"dominant over {TONE_HZ * 2} Hz ({magnitude_off_target:.0f}) -- decoded audio "
            "was present but at the wrong pitch, suggesting a real decode bug rather than "
            "just silence"
        )
    print(
        f"PASS: captured WAV contains real, correctly-pitched decoded Opus audio "
        f"(~{TONE_HZ} Hz, peak {peak})."
    )
    print("PASS: Opus decode through the vendored fixed-point libopus works end to end.")


if __name__ == "__main__":
    main()
