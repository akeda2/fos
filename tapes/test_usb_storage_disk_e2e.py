#!/usr/bin/env python3
"""End-to-end test for `disk.rs`'s USB Mass Storage integration
(`disk::attach_usb_disk`, the `usb storage attach` shell command, and the
`SecondaryBackend::Usb` dispatch in `transfer`/`transfer_chunk`).

Unlike `tapes/test_usb_storage_e2e.py` (which only proves the standalone,
read-only `usb storage` diagnostic command), this test proves the
attached USB disk is usable as an ordinary secondary disk through the
ordinary FOSFS command surface: `usb storage attach` reports a new
`diskN`, `format diskN` succeeds, and a `write`/`cat` round trip through
that disk returns byte-exact content -- the same read+write, byte-exact
methodology `make test-second-disk` already applies to a BIOS-backed
secondary disk, now exercised against the USB-backed one instead.

Note: a single command in this flow has occasionally been observed to
fail with a transient `FirmwareFailure` in a busy host-scheduling
environment (retried successfully in every case so far, never a
byte-mismatch) -- consistent with this project's other UHCI-adjacent
QEMU tests' documented host-timing jitter, not a logic bug reproduced by
this test's own fixed command sequence. This test does not retry:
a failure here should be treated as a real signal worth re-running once
before assuming code regression, not silently absorbed.

Not part of `make test`; mirrors every other `tapes/test_*_e2e.py` focused
manual regression harness.
"""

import os
import selectors
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.66"
MCAST_PORT = 4266
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef66")
GUEST_MAC = "52:54:00:12:34:75"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

BACKING_SIZE = 1 * 1024 * 1024  # 1 MiB -> 2048 blocks of 512 bytes
FILE_CONTENTS = "hello from the native USB Mass Storage disk"


def eth_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"missing {IMAGE}; run `make image` first")

    selector = selectors.DefaultSelector()
    output = bytearray()

    with tempfile.TemporaryDirectory() as tmp:
        backing = Path(tmp) / "usb_storage_backing.img"
        backing.write_bytes(bytearray(BACKING_SIZE))

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
                "-drive",
                f"if=none,id=mscdrive,format=raw,file={backing}",
                "-device",
                "usb-storage,bus=uhci.0,drive=mscdrive",
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

        def run_command(command: bytes, timeout: float = 20) -> str:
            start = len(output)
            mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, command + b"\n"))
            return read_until(b"fos> ", timeout, start).decode("utf-8", "replace")

        guest_mac = send_tape.parse_mac(GUEST_MAC)

        try:
            mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

            read_until(b"Waiting for a remote terminal client to connect", 30)
            mcast.send(eth_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
            read_until(b"Remote terminal connected", 10)
            read_until(b"fos> ", 5)

            attach = run_command(b"usb storage attach")
            if "Attached as disk" not in attach:
                raise SystemExit(f"attach did not report success:\n{attach}")
            print("PASS: 'usb storage attach' discovered and attached the device.")

            # Parse the reported diskN back out rather than assuming 3:
            # discovery order (a BIOS-visible secondary disk vs. this one)
            # is not something this test should hardcode.
            import re

            match = re.search(r"Attached as (disk\d+)", attach)
            if not match:
                raise SystemExit(f"could not parse disk number from:\n{attach}")
            disk = match.group(1)

            fmt = run_command(f"format {disk}".encode())
            if "FOSFS formatted." not in fmt:
                raise SystemExit(f"format failed:\n{fmt}")
            print(f"PASS: 'format {disk}' succeeded.")

            write = run_command(f"write {disk} usbtest.txt {FILE_CONTENTS}".encode())
            if "File written." not in write:
                raise SystemExit(f"write failed:\n{write}")

            cat = run_command(f"cat {disk} usbtest.txt".encode())
            if FILE_CONTENTS not in cat:
                raise SystemExit(f"cat did not return the written content:\n{cat}")
            print(f"PASS: write/cat round trip through {disk} matches byte-exact.")

            # A single bulk_transfer TD chain is bounded by TD_POOL_CAPACITY
            # (34) TDs of at most max_packet bytes each -- 2176 bytes at
            # full speed's 64-byte packet ceiling, comfortably less than
            # one MAX_TRANSFER_SECTORS-sized (16 sectors, 8192 bytes) FOSFS
            # chunk. A real multi-sector tape load against a genuine
            # Kingston flash drive found this the hard way (bulk_transfer
            # returning InitFailed) before bulk_transfer grew its own
            # internal multi-pass chunking. `write`'s own line editor caps
            # a command at 256 bytes, far too small to reproduce this
            # directly, so this instead copies a real preloaded file (well
            # over 2176 bytes) onto the USB disk and back via `copy`,
            # which drives the same `device_write_sectors`/
            # `device_read_sectors` chunked path FOSFS itself uses.
            large_name = "telnet.bin"
            large_size = 24584  # tools/build_image.py's own reported size
            to_usb = run_command(f"copy disk1 {large_name} {disk}".encode(), timeout=30)
            if f"Copied {large_size} byte(s)" not in to_usb:
                raise SystemExit(
                    f"copying {large_name} onto {disk} did not report the "
                    f"expected byte count (multi-chunk write regression):\n{to_usb}"
                )
            from_usb = run_command(
                f"copy {disk} {large_name} disk1 usbbig-check.bin".encode(), timeout=30
            )
            if f"Copied {large_size} byte(s)" not in from_usb:
                raise SystemExit(
                    f"copying {large_name} back off {disk} did not report the "
                    f"expected byte count (multi-chunk read regression):\n{from_usb}"
                )
            run_command(b"rm usbbig-check.bin")
            print(
                f"PASS: {large_size}-byte copy round trip through {disk} "
                "completed (exercises bulk_transfer's own internal "
                "multi-pass chunking in both directions)."
            )

            second_attach = run_command(b"usb storage attach")
            if "already attached" not in second_attach:
                raise SystemExit(f"a second attach should be rejected:\n{second_attach}")
            print("PASS: a second 'usb storage attach' is correctly rejected.")

            mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_BYE))
        finally:
            guest.terminate()
            try:
                guest.wait(timeout=5)
            except subprocess.TimeoutExpired:
                guest.kill()


if __name__ == "__main__":
    main()
