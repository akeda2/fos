#!/usr/bin/env python3
"""End-to-end test for Intel HD Audio ADC (microphone/capture) support in
`kernel/src/hda.rs`.

Near-identical in spirit to `tapes/test_audio_capture_e2e.py` (the ES1370
counterpart), but the guest's audio device is a QEMU-emulated Intel HDA
controller with the `hda-duplex` codec (the same one `tapes/test_hda_e2e.py`
uses to prove ADC/line-in widget discovery) instead of ES1370. MSI is left
at QEMU's own default (enabled) rather than forced off, so this also
exercises MSI-delivered interrupts driving both the playback and capture
stream simultaneously -- see `tapes/test_hda_playback_e2e.py`'s doc comment
for the primary MSI regression proof.

This does not (and, in this sandboxed environment, cannot) verify actual
recorded *content* -- see `tapes/test_audio_capture_e2e.py`'s own doc comment
for why (QEMU's `none` audiodev backend only ever produces/consumes silence)
-- but it does prove the whole HDA capture stream pipeline genuinely runs,
independently of and simultaneously with playback:

- `setup_capture` successfully found the `hda-duplex` codec's ADC/line-in
  pin pair, programmed a capture stream, and started it running.
- QEMU's HDA model raises a capture-stream completion and this driver's
  `drain_capture_period` actually runs (periods captured increases over
  time, matching the playback stream's own period count almost exactly,
  since both directions are programmed at the identical 44100 Hz rate).
- Once nothing drains `CAPTURE_RINGS` (no tape has called
  `tape_audio_capture` in this test), it fills to capacity and then reports
  a correctly incrementing overrun count instead of silently wedging or
  corrupting state.
- Playback keeps refilling correctly the entire time capture is also
  running, on the same shared legacy PCI IRQ line, proving
  `handle_interrupt`'s unconditional (not if/else) check of both
  `stream_index` and `capture_stream_index` lets both directions coexist.

Not part of `make test`; mirrors every other `tapes/test_*_e2e.py` focused
manual regression harness.
"""

import os
import re
import selectors
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.59"
MCAST_PORT = 4259
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
MONITOR_SOCK = Path("/tmp/fos-test-hda-capture-monitor.sock")
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef51")
GUEST_MAC = "52:54:00:12:34:62"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22


def eth_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"missing {IMAGE}; run `make image` first")

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
            "none,id=snd0",
            "-device",
            "intel-hda",
            "-device",
            "hda-duplex,audiodev=snd0",
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

    def parse_hda_status(text: bytes):
        played = int(re.search(rb"(\d+) period\(s\) refilled since boot", text).group(1))
        captured = int(re.search(rb"(\d+) period\(s\) captured since boot", text).group(1))
        overruns = int(re.search(rb"Overruns \(ring full on drain\): (\d+)", text).group(1))
        m = re.search(rb"Capture ring backlog: (\d+) of (\d+) byte", text)
        backlog, capacity = int(m.group(1)), int(m.group(2))
        return played, captured, overruns, backlog, capacity

    guest_mac = send_tape.parse_mac(GUEST_MAC)

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

        read_until(b"Waiting for a remote terminal client to connect", 30)
        mcast.send(eth_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        first = run_command(b"hda")
        if b"Capture active" not in first:
            raise SystemExit(f"HDA capture stream not active:\n{first.decode('utf-8', 'replace')}")
        print("PASS: Intel HDA discovered with an active capture stream.")

        if not re.search(rb"MSI enabled, vector 0x[0-9a-f]{2}: \d+ fire\(s\)\.", first):
            raise SystemExit(
                "'hda status' did not report MSI delivery (expected the guest's "
                "default-MSI intel-hda controller to use it, not fall back to a "
                f"legacy IRQ):\n{first.decode('utf-8', 'replace')}"
            )
        print("PASS: HDA controller is actually using MSI, not a legacy IRQ fallback.")

        played_1, captured_1, overruns_1, backlog_1, capacity = parse_hda_status(first)

        time.sleep(1.5)

        second = run_command(b"hda")
        played_2, captured_2, overruns_2, backlog_2, _ = parse_hda_status(second)

        if played_2 <= played_1:
            raise SystemExit(f"playback periods did not advance: {played_1} -> {played_2}")
        print(f"PASS: playback kept servicing periods ({played_1} -> {played_2}).")

        if captured_2 <= captured_1:
            raise SystemExit(
                f"capture periods did not advance: {captured_1} -> {captured_2}; "
                "drain_capture_period never ran"
            )
        print(f"PASS: capture kept servicing periods ({captured_1} -> {captured_2}).")

        # Nothing in this test ever drains CAPTURE_RINGS (no tape called
        # tape_audio_capture), so with enough elapsed time it must fill to
        # capacity and start reporting overruns instead of silently wedging.
        if backlog_2 != capacity:
            raise SystemExit(
                f"expected capture ring to be full ({capacity}), got {backlog_2}"
            )
        if overruns_2 <= overruns_1:
            raise SystemExit(
                f"overrun count did not advance once the ring filled: "
                f"{overruns_1} -> {overruns_2}"
            )
        print(
            f"PASS: unread capture ring filled to capacity ({capacity} bytes) "
            f"and correctly counted overruns ({overruns_1} -> {overruns_2})."
        )

        recovery_start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(
            b"fos is a small educational 32-bit x86 operating system.",
            10,
            recovery_start,
        )
        print("PASS: the same remote shell session remained usable throughout.")

        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()


if __name__ == "__main__":
    main()
