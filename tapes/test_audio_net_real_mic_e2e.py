#!/usr/bin/env python3
"""Manual, real-hardware-dependent demonstration: real microphone ->
ES1370 ADC -> `tapes/audio-net`'s `capture` mode -> raw Ethernet -> a
second guest's `listen` mode -> ES1370 DAC1 -> a captured host-side WAV
file.

This exists to answer "can a real mic on this host feed `tape run
capture`?" with actual evidence, using two QEMU guests on one multicast
Ethernet segment (mirroring every other two-guest tapes/test_*_e2e.py
harness): the first guest's ES1370 is backed by QEMU's `pipewire`
audiodev pointed at the host's real default audio capture device; the
second guest's ES1370 is backed by `wav` (output only) so its actual
played-back audio can be inspected afterward.

**Not part of `make test`/`make test-audio-net`, and not runnable in
every environment** -- it depends on physical audio hardware being
present, usable, and actually reachable by whichever session runs it
(mirroring `tools/test_realnet_e2e.py`'s own documented "depends on host
machine state" precedent). In particular, on a machine normally accessed
remotely (e.g. over SSH), the *local* graphical desktop session must
actually be logged into on that seat for logind to grant the running
user's PipeWire instance `uaccess` on the real capture device --
`wpctl status`'s `Audio -> Sources` list must show a real device, not
just a dummy/virtual sink, before running this script (see AGENTS.md's
"Real-microphone testing on a dev machine" section for the full
diagnosis of this exact failure mode). The committed, environment-
independent regression coverage for this feature is
`tapes/test_audio_capture_e2e.py` and `tapes/test_audio_net_e2e.py`,
both using the `none`/`wav` audiodev backends, which are available
everywhere but only prove the mechanism, not real audio content.

Run manually with: `python3 tapes/test_audio_net_real_mic_e2e.py`
(builds nothing itself -- run `make image tape-audio-net` first).
"""

import os
import re
import selectors
import struct
import subprocess
import sys
import time
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "tools"))
sys.path.insert(0, str(Path(__file__).resolve().parent / "tapes"))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.59"
MCAST_PORT = 4259
ROOT = Path(__file__).resolve()
IMAGE = ROOT.parent / "build" / "fos.img"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22
REMOTE_ABORT_BYTE = 0x1B

MIC_CLIENT_MAC = bytes.fromhex("02deadbeef51")
LISTEN_CLIENT_MAC = bytes.fromhex("02deadbeef52")
MIC_GUEST_MAC = "52:54:00:12:34:71"
LISTEN_GUEST_MAC = "52:54:00:12:34:72"


def eth_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"missing {IMAGE}; run `make image` first")

    wav_capture = Path("/tmp/fos-real-mic-capture.wav")
    if wav_capture.exists():
        wav_capture.unlink()

    selector = selectors.DefaultSelector()
    output: dict[str, bytearray] = {"mic": bytearray(), "listen": bytearray()}
    guests: dict[str, subprocess.Popen] = {}

    def start_guest(name: str, mac: str, audiodev_args: list[str]) -> None:
        guest = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive",
                f"format=raw,file={IMAGE}",
                "-snapshot",
                "-serial",
                "stdio",
                "-monitor",
                "none",
                "-no-reboot",
                "-no-shutdown",
                "-netdev",
                f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
                "-device",
                f"rtl8139,netdev=net0,mac={mac}",
                *audiodev_args,
                "-device",
                "ES1370,audiodev=snd0",
                "-display",
                "none",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        guests[name] = guest
        selector.register(guest.stdout, selectors.EVENT_READ, name)

    def read_until(name: str, marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        buf = output[name]
        while marker not in buf[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r} from {name}; output since checkpoint:\n"
                    f"{buf[start:].decode('utf-8', 'replace')}"
                )
            for key, _ in selector.select(remaining):
                gname = key.data
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(f"{gname} serial stream ended early")
                output[gname].extend(chunk)
        return bytes(buf[start:])

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        mic_mac = send_tape.parse_mac(MIC_GUEST_MAC)
        listen_mac = send_tape.parse_mac(LISTEN_GUEST_MAC)

        print("Starting listen guest (ES1370 -> wav capture file)...")
        start_guest(
            "listen",
            LISTEN_GUEST_MAC,
            ["-audiodev", f"wav,id=snd0,path={wav_capture}"],
        )
        read_until("listen", b"Waiting for a remote terminal client to connect", 30)
        mcast.send(eth_frame(b"\xff" * 6, LISTEN_CLIENT_MAC, TERM_TAG_HELLO))
        read_until("listen", b"Remote terminal connected", 10)
        read_until("listen", b"fos> ", 5)

        def run_listen(command: bytes, marker: bytes, timeout: float = 10) -> bytes:
            start = len(output["listen"])
            mcast.send(eth_frame(listen_mac, LISTEN_CLIENT_MAC, TERM_TAG_DATA, command + b"\n"))
            return read_until("listen", marker, timeout, start)

        run_listen(b"tape load disk audio-net.bin", b"Loaded ")
        run_start = len(output["listen"])
        mcast.send(eth_frame(listen_mac, LISTEN_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until("listen", b"Waiting for a matching audio stream", 10, run_start)
        print("PASS: listen guest is waiting for a stream.")

        print("Starting mic guest (ES1370 <- real host ALSA microphone)...")
        start_guest(
            "mic",
            MIC_GUEST_MAC,
            ["-audiodev", "pipewire,id=snd0"],
        )
        read_until("mic", b"Waiting for a remote terminal client to connect", 30)
        mcast.send(eth_frame(b"\xff" * 6, MIC_CLIENT_MAC, TERM_TAG_HELLO))
        read_until("mic", b"Remote terminal connected", 10)
        read_until("mic", b"fos> ", 5)

        def run_mic(command: bytes, marker: bytes, timeout: float = 10) -> bytes:
            start = len(output["mic"])
            mcast.send(eth_frame(mic_mac, MIC_CLIENT_MAC, TERM_TAG_DATA, command + b"\n"))
            return read_until("mic", marker, timeout, start)

        audio_output = run_mic(b"audio", b"Raw STATUS register")
        overrun_before = int(
            re.search(rb"overruns \(ring full\): (\d+)", audio_output).group(1)
        )
        print(f"Mic guest ADC overruns before capture: {overrun_before}")

        run_mic(b"tape load disk audio-net.bin", b"Loaded ")

        capture_start = len(output["mic"])
        capture_cmd = b"tape run capture " + LISTEN_GUEST_MAC.encode() + b"\n"
        mcast.send(eth_frame(mic_mac, MIC_CLIENT_MAC, TERM_TAG_DATA, capture_cmd))
        read_until("mic", b"Streaming live ADC input", 10, capture_start)
        print("PASS: mic guest is streaming live captured audio to the listen guest.")

        # Let real ambient sound (and whatever noise the room/mic picks up)
        # stream for a few seconds.
        time.sleep(4)

        # Stop the mic guest's capture loop remotely (the documented
        # REMOTE_ABORT_BYTE convention), then let the listen guest's own
        # stream-stopped-by-sender path end its tape naturally. The
        # `listen` checkpoint MUST be captured before sending the abort
        # byte, not after waiting for the mic guest's own response: the
        # listen guest keeps draining/relaying its own output the whole
        # time this script is busy waiting on the mic guest, so its
        # "stream stopped"/"Left audio-net" messages can already be past
        # a checkpoint taken only afterward, making read_until() search
        # from a start position later than where they actually landed --
        # this is what an earlier run of this script misreported as the
        # mic guest "hanging" (it was this checkpoint racing ahead, not a
        # real hang; see AGENTS.md's audio-net real-hardware notes).
        stop_start = len(output["listen"])
        mcast.send(eth_frame(mic_mac, MIC_CLIENT_MAC, TERM_TAG_DATA, bytes([REMOTE_ABORT_BYTE])))
        # The marker must be the tail of the line, not its start:
        # `print_bytes`/`print_u32` relay byte-at-a-time over `term_net`,
        # so a prefix like "capture stopped locally" can arrive (and match)
        # well before " after N byte(s) sent." has actually landed --
        # exactly the same class of bug already documented for the
        # "audio" command's own marker above.
        read_until("mic", b"byte(s) sent.", 10, capture_start)
        mic_summary = output["mic"][capture_start:]
        sent_match = re.search(rb"capture stopped locally after (\d+) byte", mic_summary)
        bytes_sent = int(sent_match.group(1))
        # bytes_sent == 0 is a hard failure, not just "the room was quiet":
        # the ADC channel produces a steady stream of samples at the
        # configured rate regardless of amplitude (digital silence is still
        # data), so zero bytes sent means the capture ring never received
        # any samples at all -- e.g. because the host has no functioning
        # audio *input* device for QEMU's audiodev backend to attach to.
        # See AGENTS.md's "Real-microphone testing on a dev machine" section
        # for how to check this (`wpctl status`'s Sources list) before
        # assuming a real hang or driver bug.
        if bytes_sent == 0:
            raise SystemExit(
                "mic guest sent 0 captured byte(s): the ADC channel produced "
                "no samples at all. This almost certainly means the host has "
                "no working audio input device for QEMU's audiodev backend "
                "to attach to (check `wpctl status`'s Audio -> Sources list), "
                "not a real fos/kernel/driver bug."
            )
        print(f"PASS: mic guest sent {bytes_sent} captured byte(s) over the network.")

        read_until("listen", b"stream stopped by sender", 15, stop_start)
        read_until("listen", b"Left audio-net; back at the fos shell.", 10, stop_start)
        listen_summary = output["listen"][stop_start:]
        received_match = re.search(
            rb"Bytes received: (\d+), played: (\d+), dropped: (\d+)\.", listen_summary
        )
        bytes_received, bytes_played, bytes_dropped = (
            int(x) for x in received_match.groups()
        )
        print(
            f"PASS: listen guest received {bytes_received}, played {bytes_played}, "
            f"dropped {bytes_dropped}."
        )
        # bytes_received == 0 means no real network audio ever reached the
        # listen guest's DAC -- it kept playing ES1370's own built-in idle
        # test tone (see AGENTS.md: "DAC1 runs unconditionally ... silence
        # plays continuously even with nothing feeding audio_submit") the
        # entire time. That default tone is itself non-silent, so the WAV
        # amplitude check below cannot tell the two cases apart on its own;
        # this check is what actually proves the captured WAV reflects real
        # relayed audio rather than the idle tone.
        if bytes_received == 0:
            raise SystemExit(
                "listen guest received 0 bytes: the captured WAV file (if "
                "non-silent) is just ES1370's own default idle DAC1 test "
                "tone, not real relayed microphone audio."
            )

        mcast.send(eth_frame(mic_mac, MIC_CLIENT_MAC, TERM_TAG_BYE))
        mcast.send(eth_frame(listen_mac, LISTEN_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        for guest in guests.values():
            guest.terminate()
        for guest in guests.values():
            try:
                guest.wait(timeout=5)
            except subprocess.TimeoutExpired:
                guest.kill()

    with wave.open(str(wav_capture), "rb") as captured:
        rate = captured.getframerate()
        channels = captured.getnchannels()
        frames = captured.readframes(captured.getnframes())
    print(f"Captured WAV: {rate} Hz / {channels}ch, {len(frames)} byte(s).")
    if len(frames) == 0:
        raise SystemExit("captured WAV file is empty; nothing was ever played back")
    sample_count = len(frames) // 2
    samples = struct.unpack(f"<{sample_count}h", frames[: sample_count * 2])
    peak = max(abs(s) for s in samples)
    rms = (sum(s * s for s in samples) / len(samples)) ** 0.5
    print(f"Captured audio peak amplitude: {peak} (of 32767 max), RMS: {rms:.1f}")
    if peak < 50:
        raise SystemExit(
            "captured audio is essentially silent (peak < 50); the real "
            "microphone path may not actually be carrying audio"
        )
    print("PASS: real, non-silent audio flowed mic -> ADC -> network -> DAC -> WAV file.")


if __name__ == "__main__":
    main()
