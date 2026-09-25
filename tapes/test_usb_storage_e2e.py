#!/usr/bin/env python3
"""End-to-end test for the USB Mass Storage (Bulk-Only Transport) driver
in `kernel/src/usb_storage.rs`, exercised through the standalone `usb
storage` shell command (`kernel/src/shell.rs`'s `usb_storage_command`).

This test proves, against QEMU's synthetic `usb-storage` device (a real
Bulk-Only Transport/SCSI-transparent device, not a mock):

- A Mass Storage interface is discovered on a UHCI controller (a
  different interface class/subclass/protocol walk than `tapes/
  test_uhci_e2e.py`'s USB Audio Class one, proving `find_msc_endpoints`
  correctly distinguishes the two).
- `INQUIRY` reads back QEMU's own reported vendor/product strings
  ("QEMU"/"QEMU HARDDISK") -- a real control-then-bulk-transfer round
  trip, not just a control transfer the way plain enumeration is.
- `READ CAPACITY (10)` reports the exact block count/size implied by the
  backing image's own size (proving the parsed big-endian SCSI fields are
  correct, not just present).
- `READ(10)` of the first sector returns bytes that byte-exact match a
  known pattern this test seeds into the backing image before boot --
  proving the full CBW-out/data-in/CSW-in Bulk-Only Transport exchange,
  and `Uhci::bulk_transfer`'s own TD-chain-building underneath it, moved
  real data correctly end to end.

Write (`WRITE(10)`) is not exercised here: the standalone `usb storage`
command is deliberately read-only (see its own doc comment in
`shell.rs`), since issuing a real write against whichever USB device
happens to enumerate first, with no disk-selection safeguard, is not a
safe default action for an always-available diagnostic command. A write
round-trip is exercised once this driver is wired into `disk.rs` as an
ordinary secondary disk, through the same `write`/`cat`-based tests every
other disk backend already has.

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

MCAST_GROUP = "230.42.42.64"
MCAST_PORT = 4264
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef64")
GUEST_MAC = "52:54:00:12:34:73"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

# The exact bytes seeded into the backing image's first sector, checked
# for byte-exact match in this test's hex-dump output below. Exactly 16
# bytes so the pattern lands entirely within `dump_bytes`'s first
# 16-byte row, rather than being split across two dump lines.
SEED_PATTERN = b"fos-usb-storage-"
BACKING_SIZE = 1 * 1024 * 1024  # 1 MiB -> 2048 blocks of 512 bytes
EXPECTED_BLOCK_COUNT = BACKING_SIZE // 512
EXPECTED_BLOCK_SIZE = 512


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
        data = bytearray(BACKING_SIZE)
        data[: len(SEED_PATTERN)] = SEED_PATTERN
        backing.write_bytes(data)

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

        def run_command(command: bytes, timeout: float = 20) -> bytes:
            start = len(output)
            mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, command + b"\n"))
            return read_until(b"fos> ", timeout, start)

        def check(text: str) -> None:
            if "No USB Mass Storage" in text:
                raise SystemExit(f"no USB Mass Storage device discovered:\n{text}")
            if "vendor='QEMU'" not in text or "product='QEMU HARDDISK'" not in text:
                raise SystemExit(f"unexpected or missing INQUIRY strings:\n{text}")

            expected_capacity = f"READ CAPACITY: {EXPECTED_BLOCK_COUNT} block(s) of {EXPECTED_BLOCK_SIZE} bytes"
            if expected_capacity not in text:
                raise SystemExit(f"expected {expected_capacity!r}, not found:\n{text}")

            if "READ(10) failed" in text:
                raise SystemExit(f"READ(10) reported failure:\n{text}")
            seed_hex = " ".join(f"{byte:02x}" for byte in SEED_PATTERN)
            if seed_hex not in text:
                raise SystemExit(
                    f"seeded first-sector pattern not found in hex dump "
                    f"(expected bytes {seed_hex!r}):\n{text}"
                )

        guest_mac = send_tape.parse_mac(GUEST_MAC)

        try:
            mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

            read_until(b"Waiting for a remote terminal client to connect", 30)
            mcast.send(eth_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
            read_until(b"Remote terminal connected", 10)
            read_until(b"fos> ", 5)

            response = run_command(b"usb storage").decode("utf-8", "replace")
            check(response)
            print("PASS: USB Mass Storage device discovered via Bulk-Only Transport.")
            print("PASS: INQUIRY reports QEMU's own vendor/product strings.")
            print(
                f"PASS: READ CAPACITY reports {EXPECTED_BLOCK_COUNT} blocks of "
                f"{EXPECTED_BLOCK_SIZE} bytes, matching the backing image's size."
            )
            print("PASS: READ(10) of the first sector matches the seeded pattern exactly.")

            mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_BYE))
        finally:
            guest.terminate()
            try:
                guest.wait(timeout=5)
            except subprocess.TimeoutExpired:
                guest.kill()


if __name__ == "__main__":
    main()
