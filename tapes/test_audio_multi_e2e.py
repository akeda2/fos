#!/usr/bin/env python3
"""End-to-end test for multi-instance ES1370 support in
`kernel/src/es1370.rs`/`kernel/src/audio.rs`.

Proves the driver/facade conversion from a singleton `Es1370`/`Audio` to a
fixed-size `[Option<...>; MAX_INSTANCES]` array (mirroring `nic.rs`'s own
multi-NIC pattern) actually works end to end, by attaching *two*
QEMU-emulated ES1370 cards -- each with its own `audiodev` backend -- to
one guest:

- `audio` (the shell command, `audio_status()` in `kernel/src/shell.rs`)
  reports two independent devices, `[0]` and `[1]`, each with its own I/O
  base and its own IRQ/period/backlog diagnostics -- not one device
  reported twice, and not one device's state bleeding into the other's.
- Both devices' DAC1 channel keeps refilling independently and
  simultaneously (each device's own period count advances on its own,
  the same "both channels don't clobber each other's bits" proof
  `test_audio_capture_e2e.py` already established for one card's DAC1
  and ADC together, extended here across two entirely separate cards
  sharing the same driver code and, per this project's shared-legacy-
  PCI-IRQ-line fix, potentially the same INTx line).
- The new index-based tape ABI callbacks
  (`tape_audio_count`/`tape_audio_sample_rate_at`/
  `tape_audio_channels_and_bits_at`/`tape_audio_submit_at`/
  `tape_audio_capture_at`) are exercised separately by
  `tapes/audio-net`'s own e2e test once that tape is updated to use them
  (see `AGENTS.md`); this test focuses on proving the underlying driver/
  facade/shell-command plumbing they depend on -- `es1370::CARDS`,
  `audio::DEVICES`, and `audio_status()`'s per-device reporting -- is
  correct first, independently of any tape.

This is not part of `make test`; mirrors every other
`tapes/test_*_e2e.py` focused manual regression harness. Uses the `none`
audiodev backend for both devices (available everywhere, unlike `wav`/
`pipewire`), matching `test_audio_capture_e2e.py`'s own environment-
portability reasoning.
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
MONITOR_SOCK = Path("/tmp/fos-test-audio-multi-monitor.sock")
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef42")
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


def parse_audio_status(text: bytes):
    """Returns a dict keyed by device index, each a dict of the fields
    this test cares about: io_base, periods (DAC1), backlog (network-fed
    ring)."""
    devices = {}
    for match in re.finditer(
        rb"\[(\d+)\] ES1370 at base 0x([0-9a-f]+): (\d+) Hz / (\d+)ch / (\d+)-bit\.",
        text,
    ):
        index = int(match.group(1))
        devices[index] = {
            "io_base": int(match.group(2), 16),
            "sample_rate": int(match.group(3)),
            "channels": int(match.group(4)),
            "bits": int(match.group(5)),
        }
    # DAC1/backlog lines follow each device's header line in order, so
    # walk the text a second time pairing each header's position with the
    # next "DAC1: N periods" / "Network-fed ring backlog: N of M" lines
    # that follow it, before the next device header (if any).
    headers = list(
        re.finditer(
            rb"\[(\d+)\] ES1370 at base 0x[0-9a-f]+: \d+ Hz / \d+ch / \d+-bit\.",
            text,
        )
    )
    for position, header in enumerate(headers):
        index = int(header.group(1))
        start = header.end()
        end = headers[position + 1].start() if position + 1 < len(headers) else len(text)
        block = text[start:end]
        periods_match = re.search(rb"DAC1: (\d+) periods refilled", block)
        backlog_match = re.search(rb"Network-fed ring backlog: (\d+) of (\d+) byte", block)
        devices[index]["periods"] = int(periods_match.group(1))
        devices[index]["backlog"] = int(backlog_match.group(1))
    return devices


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
            "-audiodev",
            "none,id=snd1",
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

        first = run_command(b"audio")
        if b"2 audio device(s) found" not in first:
            raise SystemExit(
                f"expected 2 devices reported:\n{first.decode('utf-8', 'replace')}"
            )
        devices_1 = parse_audio_status(first)
        if set(devices_1.keys()) != {0, 1}:
            raise SystemExit(f"expected devices [0] and [1], got {devices_1}")
        if devices_1[0]["io_base"] == devices_1[1]["io_base"]:
            raise SystemExit(
                "both devices reported the identical I/O base -- they are "
                "not actually independent PCI functions"
            )
        print(
            "PASS: two independent ES1370 devices discovered at distinct "
            f"I/O bases (0x{devices_1[0]['io_base']:04x}, "
            f"0x{devices_1[1]['io_base']:04x})."
        )

        for index in (0, 1):
            fmt = devices_1[index]
            if (fmt["sample_rate"], fmt["channels"], fmt["bits"]) != (44100, 2, 16):
                raise SystemExit(f"unexpected format for device {index}: {fmt}")
        print("PASS: both devices report the expected 44100 Hz / 2ch / 16-bit format.")

        time.sleep(1.5)
        second = run_command(b"audio")
        devices_2 = parse_audio_status(second)

        for index in (0, 1):
            if devices_2[index]["periods"] <= devices_1[index]["periods"]:
                raise SystemExit(
                    f"device {index}'s DAC1 periods did not advance: "
                    f"{devices_1[index]['periods']} -> {devices_2[index]['periods']}"
                )
        print(
            "PASS: both devices' DAC1 channels kept refilling "
            f"independently (device 0: {devices_1[0]['periods']} -> "
            f"{devices_2[0]['periods']}; device 1: "
            f"{devices_1[1]['periods']} -> {devices_2[1]['periods']})."
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
