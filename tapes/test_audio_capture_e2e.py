#!/usr/bin/env python3
"""End-to-end test for ES1370 ADC (microphone/capture) support in
`kernel/src/es1370.rs`.

This does not (and, in this sandboxed environment, cannot) verify actual
recorded *content* -- see the note below on why -- but it does prove the
whole ADC hardware/interrupt/ring pipeline genuinely runs, independently
of and simultaneously with DAC1 playback:

- QEMU's ES1370 model raises `STAT_ADC` and this driver's
  `drain_capture_period`/`acknowledge_adc` actually run (periods captured
  increases over time, matching DAC1's own period count almost exactly,
  since both channels are programmed at the identical 44100 Hz rate).
- Once nothing drains `CAPTURE_RING` (no tape has called `tape_audio_
  capture` in this test), it fills to capacity and then reports a
  correctly incrementing overrun count instead of silently wedging or
  corrupting state -- proving `drain_capture_period`'s "drop the newest
  period, count it, keep going" design actually works under sustained
  backpressure.
- DAC1 keeps refilling correctly the entire time ADC is also running,
  proving the read-modify-write fix to `SERIAL_CONTROL`/`CONTROL`
  (`start_dac1`/`start_adc`/`acknowledge_dac1`/`acknowledge_adc`) lets
  both channels coexist without either clobbering the other's bits --
  the specific bug this session's implementation had to guard against.

**Why this cannot verify captured audio content**: QEMU's `wav` audiodev
backend (`audio/wavaudio.c` in QEMU's own source, confirmed by reading it
directly for this test) implements only `HWVoiceOut` -- there is no
`in.path` input option in this QEMU packaging at all, despite that
combination appearing in some online examples for other QEMU versions/
forks. Every audiodev driver that *can* provide fake, file-sourced
microphone input (that same source's own comments) needs a real host
audio subsystem (ALSA/PulseAudio/PipeWire) generally unavailable in a
sandboxed CI/dev container. This test therefore uses the `none` audiodev
driver instead, which is bidirectional (it opens both an out and an in
voice) but only ever produces/consumes silence -- sufficient to prove the
mechanism works, not to prove a captured sample's actual value is
correct. Verifying real, non-silent captured content is deferred to real
hardware (see `AGENTS.md`'s "Real hardware notes" and the `audio-real-hw`
project item) or a future host environment with a usable audio backend.

Not part of `make test`; mirrors every other `tapes/test_*_e2e.py`
focused manual regression harness.
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

MCAST_GROUP = "230.42.42.58"
MCAST_PORT = 4258
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
MONITOR_SOCK = Path("/tmp/fos-test-audio-capture-monitor.sock")
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef41")
GUEST_MAC = "52:54:00:12:34:61"

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

    def parse_audio_status(text: bytes):
        periods = int(re.search(rb"DAC1: (\d+) periods refilled", text).group(1))
        captured = int(re.search(rb"ADC: (\d+) periods captured; overruns \(ring full\): (\d+)", text).group(1))
        overruns = int(re.search(rb"overruns \(ring full\): (\d+)", text).group(1))
        backlog = int(re.search(rb"Capture ring backlog: (\d+) of (\d+) byte", text).group(1))
        capacity = int(re.search(rb"Capture ring backlog: (\d+) of (\d+) byte", text).group(2))
        return periods, captured, overruns, backlog, capacity

    guest_mac = send_tape.parse_mac(GUEST_MAC)

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

        read_until(b"Waiting for a remote terminal client to connect", 30)
        mcast.send(eth_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        first = run_command(b"audio")
        if b"[0] ES1370" not in first:
            raise SystemExit(f"ES1370 not found:\n{first.decode('utf-8', 'replace')}")
        periods_1, captured_1, overruns_1, backlog_1, capacity = parse_audio_status(first)
        print("PASS: ES1370 discovered; ADC and DAC1 diagnostics present.")

        time.sleep(1.5)

        second = run_command(b"audio")
        periods_2, captured_2, overruns_2, backlog_2, _ = parse_audio_status(second)

        if periods_2 <= periods_1:
            raise SystemExit(
                f"DAC1 periods did not advance: {periods_1} -> {periods_2}"
            )
        print(f"PASS: DAC1 kept servicing periods ({periods_1} -> {periods_2}).")

        if captured_2 <= captured_1:
            raise SystemExit(
                f"ADC periods did not advance: {captured_1} -> {captured_2}; "
                "STAT_ADC never fired or drain_capture_period never ran"
            )
        print(f"PASS: ADC kept servicing periods ({captured_1} -> {captured_2}).")

        # Nothing in this test ever drains CAPTURE_RING (no tape called
        # tape_audio_capture), so with enough elapsed time it must fill to
        # capacity and start reporting overruns instead of silently
        # wedging -- the expected, harmless "nobody is listening" case.
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
