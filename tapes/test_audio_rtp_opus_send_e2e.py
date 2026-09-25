#!/usr/bin/env python3
"""End-to-end test for Opus *encode* support in the RTP/UDP audio tape
(`audio-rtp`'s `send opus` mode), the reverse direction of
`tapes/test_audio_rtp_opus_e2e.py` (which proves decode).

A known tone is seeded into FOSFS as raw 48 kHz/2ch/16-bit PCM (the one
format Opus send/capture can use -- see `AGENTS.md`'s "Real-time audio
streaming over RTP/UDP" section for why this is HDA-only, same reasoning
as decode). The guest tape (`send opus NAME HOST_IP HOST_PORT`) encodes
it for real, through the actual vendored fixed-point libopus encoder,
and sends it as genuine RTP packets; this test answers the guest's ARP
request for the host address (mirroring `test_audio_rtp_e2e.py`'s own
Phase B), captures every packet, and verifies:

- exactly one SSRC and a strictly monotonic, gap-free sequence, same as
  the raw-PCM send test;
- the captured Opus payloads decode cleanly through a small *host-side*
  build of this project's own vendored decoder (`tapes/opus-codec/
  vendor/opus/`, compiled fresh here rather than checked in as a
  binary), reusing the exact decoder already proven correct by
  `test_audio_rtp_opus_e2e.py` rather than depending on a second
  implementation (e.g. ffmpeg) to cross-check against;
- the decoded audio has the source tone's pitch and roughly its expected
  duration.

Not part of `make test`; run it after `make tape-audio-rtp` and `make
image`. Requires a host C toolchain (plain `gcc`, no special flags --
this compiles the vendored sources natively, not for the freestanding
i686-fos target). Uses `-accel kvm` when available (see
`tapes/test_audio_rtp_opus_e2e.py`'s `kvm_available()`), for the same
QEMU/TCG timing reason encode needs it even more than decode did.
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
    ROOT,
    TAPE,
    TAPE_SENDER_MAC,
    TERM_CLIENT_MAC,
    TERM_TAG_DATA,
    TERM_TAG_HELLO,
    parse_arp,
    rebuild_image_with_extra_file,
    restore_default_image,
    term_frame,
)
from test_audio_rtp_opus_e2e import goertzel_magnitude, kvm_available  # noqa: E402

SAMPLE_RATE = 48000
CHANNELS = 2
BITS_PER_SAMPLE = 16
RTP_PORT = 5004
PAYLOAD_TYPE_OPUS = 98  # audio_rtp_tape::PAYLOAD_TYPE_OPUS

FOSFS_TONE_NAME = "opustone.pcm"
TONE_SECONDS = 1.0
TONE_HZ = 660
TONE_AMPLITUDE = 20000


def synth_tone() -> bytes:
    """A stereo sine wave, raw 16-bit signed PCM, no header -- the exact
    kernel-format bytes `send_mode`'s Opus path reinterprets directly as
    `i16` samples (no wire repacking, unlike the raw path)."""
    frame_count = int(SAMPLE_RATE * TONE_SECONDS)
    samples = bytearray()
    for index in range(frame_count):
        value = int(TONE_AMPLITUDE * math.sin(2 * math.pi * TONE_HZ * index / SAMPLE_RATE))
        packed = struct.pack("<h", value)
        samples += packed * CHANNELS
    return bytes(samples)


def run_guest(image: Path, wav_capture: Path):
    accel_args = ["-accel", "kvm", "-cpu", "host"] if kvm_available() else []
    if not accel_args:
        print(
            "WARNING: /dev/kvm is not accessible; falling back to plain TCG "
            "(see tapes/test_audio_rtp_opus_e2e.py's kvm_available()).",
            file=sys.stderr,
        )
    return subprocess.Popen(
        [
            "qemu-system-i386",
            "-m",
            "256",
            "-drive",
            f"format=raw,file={image}",
            "-snapshot",
            *accel_args,
            "-serial",
            "stdio",
            "-monitor",
            "none",
            "-no-reboot",
            "-no-shutdown",
            "-netdev",
            f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
            "-device",
            f"rtl8139,netdev=net0,mac={GUEST_MAC_STR}",
            # Playback content is irrelevant to this send/encode-focused
            # test (HDA plays silence until network mode engages, and
            # this guest never listens); a real disposable file avoids
            # any edge case in QEMU's wav backend finalizing a header on
            # a special file like /dev/null.
            "-audiodev",
            f"wav,id=snd0,path={wav_capture}",
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


HOST_DECODE_HARNESS_C = r"""
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include "opus.h"

int main(int argc, char **argv) {
    if (argc != 3) { fprintf(stderr, "usage: %s packets.bin out.pcm\n", argv[0]); return 1; }
    FILE *in = fopen(argv[1], "rb");
    FILE *out = fopen(argv[2], "wb");
    if (!in || !out) { fprintf(stderr, "open failed\n"); return 1; }

    int channels = 2;
    int size = opus_decoder_get_size(channels);
    OpusDecoder *dec = malloc((size_t)size);
    if (opus_decoder_init(dec, 48000, channels) != 0) { fprintf(stderr, "init failed\n"); return 1; }

    uint32_t len;
    int16_t pcm[5760 * 2];
    while (fread(&len, sizeof(len), 1, in) == 1) {
        unsigned char buf[2048];
        if (len > sizeof(buf)) { fprintf(stderr, "packet too large\n"); return 1; }
        if (fread(buf, 1, len, in) != len) { fprintf(stderr, "short read\n"); return 1; }
        int n = opus_decode(dec, buf, (int)len, pcm, 5760, 0);
        if (n < 0) { fprintf(stderr, "opus_decode error %d\n", n); return 1; }
        fwrite(pcm, sizeof(int16_t), (size_t)n * channels, out);
    }
    return 0;
}
"""


def build_host_opus_decoder() -> Path:
    """Compiles a small native decode harness directly from this
    project's own vendored libopus sources (never checked in as a
    binary), so verifying what the kernel's encoder produced reuses the
    exact fixed-point decoder tapes/test_audio_rtp_opus_e2e.py already
    proved correct, rather than depending on a second implementation."""
    vendor = ROOT / "tapes" / "opus-codec" / "vendor" / "opus"
    workdir = Path(tempfile.mkdtemp(prefix="fos-opus-host-decoder-"))
    (workdir / "config.h").write_text(
        "#ifndef CONFIG_H\n#define CONFIG_H\n"
        "#define OPUS_BUILD 1\n#define FIXED_POINT 1\n"
        "#define DISABLE_FLOAT_API 1\n#define VAR_ARRAYS 1\n#endif\n"
    )
    harness = workdir / "decode_harness.c"
    harness.write_text(HOST_DECODE_HARNESS_C)
    sources = (
        sorted((vendor / "celt").glob("*.c"))
        + sorted((vendor / "silk").glob("*.c"))
        + sorted((vendor / "silk" / "fixed").glob("*.c"))
        + sorted((vendor / "src").glob("*.c"))
    )
    binary = workdir / "decode_harness"
    subprocess.run(
        [
            "gcc",
            "-O1",
            "-o",
            str(binary),
            str(harness),
            *(str(s) for s in sources),
            f"-I{workdir}",
            f"-I{vendor / 'include'}",
            f"-I{vendor / 'celt'}",
            f"-I{vendor / 'silk'}",
            f"-I{vendor / 'silk' / 'fixed'}",
            "-DHAVE_CONFIG_H",
            "-lm",
        ],
        check=True,
    )
    return binary


def recv_or_none(mcast: McastSocket, timeout: float) -> bytes | None:
    mcast.settimeout(timeout)
    try:
        return mcast.recv(65536)
    except socketlib.timeout:
        return None


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-audio-rtp` first")
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} does not exist; run `make image` first")

    decoder = build_host_opus_decoder()
    print(f"built host decode harness at {decoder}")

    tone_bytes = synth_tone()
    tone_path = Path(tempfile.mkstemp(suffix=".pcm", prefix="fos-opus-tone-")[1])
    tone_path.write_bytes(tone_bytes)
    rebuild_image_with_extra_file(FOSFS_TONE_NAME, tone_path)

    wav_capture = Path(tempfile.mkstemp(suffix=".wav", prefix="fos-audio-rtp-opus-send-")[1])
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

    def drain_guest_output() -> None:
        for _ in selector.select(0):
            chunk = os.read(guest.stdout.fileno(), 4096)
            if chunk:
                output.extend(chunk)

    received_sequences: list[int] = []
    received_ssrcs: set[int] = set()
    received_packets: list[bytes] = []

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
        command = (
            f"tape run static {'.'.join(map(str, GUEST_IP))}/24 0.0.0.0 send opus "
            f"{FOSFS_TONE_NAME} {'.'.join(map(str, HOST_IP))} {RTP_PORT}\n"
        ).encode()
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, command))

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
                arp_reply = b"\x00\x01\x08\x00\x06\x04\x00\x02" + HOST_MAC + HOST_IP + sender_mac + sender_ip
                mcast.send((sender_mac + HOST_MAC + b"\x08\x06" + arp_reply).ljust(60, b"\0"))
                replied = True
        if not replied:
            raise SystemExit(
                "never saw the guest's ARP request for the host address; guest output:\n"
                + output[run_start:].decode("utf-8", "replace")
            )
        print("PASS: answered the guest's ARP request for the host's address.")

        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            drain_guest_output()
            if b"send complete" in output[run_start:]:
                break
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
            payload_type = rtp[1] & 0x7F
            if payload_type != PAYLOAD_TYPE_OPUS:
                continue
            sequence = int.from_bytes(rtp[2:4], "big")
            ssrc = int.from_bytes(rtp[8:12], "big")
            received_sequences.append(sequence)
            received_ssrcs.add(ssrc)
            received_packets.append(bytes(rtp[12:]))

        read_until(b"send complete", 10, run_start)
        print(f"PASS: 'tape run send opus' reported completion ({len(received_packets)} packet(s) captured).")
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()
        restore_default_image()
        tone_path.unlink(missing_ok=True)
        wav_capture.unlink(missing_ok=True)

    if not received_packets:
        raise SystemExit("no Opus/RTP packets were captured at all")
    if len(received_ssrcs) != 1:
        raise SystemExit(f"expected exactly one SSRC across the whole stream, got {received_ssrcs}")
    if received_sequences != sorted(received_sequences) or len(received_sequences) != len(set(received_sequences)):
        raise SystemExit(f"expected strictly monotonic, gap-free sequence numbers, got {received_sequences}")
    print(f"PASS: received {len(received_sequences)} real Opus/RTP packet(s), one SSRC, no gaps.")

    packets_path = Path(tempfile.mkstemp(suffix=".bin", prefix="fos-opus-packets-")[1])
    with open(packets_path, "wb") as f:
        for packet in received_packets:
            f.write(struct.pack("<I", len(packet)))
            f.write(packet)
    decoded_path = Path(tempfile.mkstemp(suffix=".pcm", prefix="fos-opus-decoded-")[1])
    subprocess.run([str(decoder), str(packets_path), str(decoded_path)], check=True)

    decoded_bytes = decoded_path.read_bytes()
    packets_path.unlink(missing_ok=True)
    decoded_path.unlink(missing_ok=True)
    if len(decoded_bytes) == 0:
        raise SystemExit("host decoder produced no PCM at all")
    sample_count = len(decoded_bytes) // 2
    samples = list(struct.unpack(f"<{sample_count}h", decoded_bytes[: sample_count * 2]))
    left = samples[::CHANNELS]
    duration = len(left) / SAMPLE_RATE
    if abs(duration - TONE_SECONDS) > 0.1:
        raise SystemExit(f"decoded duration {duration:.3f}s does not match the ~{TONE_SECONDS}s source")

    peak = max(abs(sample) for sample in left)
    if peak < 1000:
        raise SystemExit(f"decoded PCM peak amplitude is only {peak}; expected clearly audible output")

    magnitude_at_tone = goertzel_magnitude(left, SAMPLE_RATE, TONE_HZ)
    magnitude_off_target = goertzel_magnitude(left, SAMPLE_RATE, TONE_HZ * 2)
    if magnitude_at_tone < magnitude_off_target * 3:
        raise SystemExit(
            f"Goertzel magnitude at ~{TONE_HZ} Hz ({magnitude_at_tone:.0f}) is not clearly "
            f"dominant over {TONE_HZ * 2} Hz ({magnitude_off_target:.0f}) -- the kernel's Opus "
            "encoder produced audio at the wrong pitch"
        )
    print(
        f"PASS: the kernel-encoded Opus stream decodes to correctly-pitched audio "
        f"(~{TONE_HZ} Hz, peak {peak}, {duration:.3f}s)."
    )
    print("PASS: Opus encode through the vendored fixed-point libopus works end to end.")


if __name__ == "__main__":
    main()
