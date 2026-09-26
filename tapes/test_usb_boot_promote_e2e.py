#!/usr/bin/env python3
"""End-to-end test for `disk::promote_boot_to_usb` and the `usb storage
promote` shell command -- the step that actually closes the real-hardware
Pentium 4 gap `kernel/src/usb_storage.rs` was built for.

Unlike every other USB Mass Storage test in this repository, this one
boots `build/fos.img` itself from a QEMU `usb-storage` device (via
SeaBIOS's own USB Legacy Support, `-boot order=c`), not from an ordinary
IDE `-drive` -- faithfully reproducing the real Pentium 4 USB-boot
scenario `disk::promote_boot_to_usb`'s own doc comment and AGENTS.md's
"USB audio and USB-boot media do not reliably coexist" section describe,
entirely inside QEMU.

This test proves:

- `disk1` (the boot disk) is ordinarily readable via BIOS before any
  UHCI controller has been touched (`ls`/`cat readme.txt` succeed).
- `usb storage promote` discovers the very same physical disk it just
  booted from, confirms it via `disk::promote_boot_to_usb`'s own
  capacity-and-first-sector cross-validation (not a bare "only one
  device found" guess), and reports success.
- `bootinfo` reflects the promoted state afterward.
- `disk1` remains fully readable *and writable* (`ls`/`cat`/`write`/`rm`)
  after promotion -- proving ordinary FOSFS access to the boot disk no
  longer depends on BIOS's own "USB Legacy Support" staying intact, even
  though discovering the device during `promote` already reset the UHCI
  controller that emulation depends on.

`test_promotes_multiple_disks` extends this to `usb native`
(`disk::promote_all_and_attach`), which generalizes promotion from
"always exactly `disk1`" to every BIOS-backed disk a boot can match:
two distinct `usb-storage` devices are attached (the boot disk and an
ordinary secondary `disk2`), and both must come back natively backed from
one command.

Not part of `make test`; mirrors every other `tapes/test_*_e2e.py` focused
manual regression harness.
"""

import os
import selectors
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.68"
MCAST_PORT = 4268
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef68")
GUEST_MAC = "52:54:00:12:34:77"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22


def eth_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def test_promote_usb_booted_disk() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"missing {IMAGE}; run `make image` first")

    selector = selectors.DefaultSelector()
    output = bytearray()

    with tempfile.TemporaryDirectory() as tmp:
        # A private copy: this test's own writes must not perturb the
        # shared build/fos.img other tests/manual use rely on.
        boot_copy = Path(tmp) / "usb_boot.img"
        shutil.copyfile(IMAGE, boot_copy)

        guest = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive",
                f"if=none,id=bootdrive,format=raw,file={boot_copy}",
                "-usb",
                "-device",
                "piix3-usb-uhci,id=uhci",
                "-device",
                "usb-storage,bus=uhci.0,drive=bootdrive",
                "-boot",
                "order=c",
                "-serial",
                "stdio",
                "-no-reboot",
                "-no-shutdown",
                "-netdev",
                f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
                "-device",
                f"rtl8139,netdev=net0,mac={GUEST_MAC}",
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

        def run_command(command: bytes, timeout: float = 25) -> str:
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

            before = run_command(b"cat readme.txt")
            if "Welcome to FOSFS" not in before:
                raise SystemExit(f"disk1 not readable via BIOS before promote:\n{before}")
            print("PASS: disk1 readable via BIOS before any UHCI reset.")

            promote = run_command(b"usb storage promote")
            if "disk1 now reads/writes through the native USB driver." not in promote:
                raise SystemExit(f"promote did not report success:\n{promote}")
            print("PASS: 'usb storage promote' confirmed and switched disk1.")

            info = run_command(b"bootinfo")
            if "disk1 is promoted" not in info:
                raise SystemExit(f"bootinfo did not reflect the promoted state:\n{info}")
            print("PASS: 'bootinfo' reflects the promoted state.")

            after = run_command(b"cat readme.txt")
            if "Welcome to FOSFS" not in after:
                raise SystemExit(f"disk1 not readable after promote:\n{after}")
            print("PASS: disk1 still readable after promotion (native path).")

            write = run_command(b"write usbboot.txt promoted-write-works")
            if "File written." not in write:
                raise SystemExit(f"write to disk1 failed after promote:\n{write}")
            cat = run_command(b"cat usbboot.txt")
            if "promoted-write-works" not in cat:
                raise SystemExit(f"readback after promote did not match:\n{cat}")
            run_command(b"rm usbboot.txt")
            print("PASS: disk1 write/cat round trip works after promotion.")

            mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_BYE))
        finally:
            guest.terminate()
            try:
                guest.wait(timeout=5)
            except subprocess.TimeoutExpired:
                guest.kill()


def test_promotes_multiple_disks() -> None:
    """`usb native` generalizes `usb storage promote` from "always exactly
    `disk1`" to every BIOS-backed disk a boot can match -- see AGENTS.md's
    "dmesg" section's sibling discussion on why a secondary disk carries
    the exact same BIOS-legacy-USB risk `disk1` does. Two distinct
    `usb-storage` devices are attached to the same UHCI controller here:
    the boot disk (a private copy of `build/fos.img`, exactly like
    `test_promote_usb_booted_disk` above) and a second, blank device that
    SeaBIOS's own USB Legacy Support exposes as an ordinary `disk2` before
    fos ever touches UHCI -- the same collision `attach_usb_disk`'s own
    doc comment already describes finding under QEMU. `usb native` must
    promote *both* in one pass, and both must keep working natively
    afterward."""
    if not IMAGE.exists():
        raise SystemExit(f"missing {IMAGE}; run `make image` first")

    group, port = "230.42.42.68", 4270  # distinct from the other scenarios
    client_mac = bytes.fromhex("02deadbeef70")
    guest_mac_str = "52:54:00:12:34:80"

    selector = selectors.DefaultSelector()
    output = bytearray()

    with tempfile.TemporaryDirectory() as tmp:
        boot_copy = Path(tmp) / "usb_boot.img"
        shutil.copyfile(IMAGE, boot_copy)
        # Deliberately blank and a different size than `boot_copy`, so its
        # own LBA 0/capacity baseline can never ambiguously match the
        # *other* device's during cross-validation -- the two disks must
        # stay distinguishable for this test to actually prove a 1:1
        # match, not just two coincidentally-successful guesses.
        second_disk = Path(tmp) / "second_disk.img"
        second_disk.write_bytes(bytearray(2 * 1024 * 1024))

        guest = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive",
                f"if=none,id=bootdrive,format=raw,file={boot_copy}",
                "-drive",
                f"if=none,id=seconddrive,format=raw,file={second_disk}",
                "-device",
                "piix3-usb-uhci,id=uhci",
                # Explicit `port=` for each device (unlike the single-device
                # scenarios above, which need none): omitting it left QEMU
                # to route the second `usb-storage` device through an
                # implicit internal hub instead of the controller's second
                # root port directly, which this driver has no hub support
                # for at all (see AGENTS.md's "UHCI" section) and so never
                # found a Mass Storage interface behind it. Nothing to do
                # with `disk.rs`/`usb_storage.rs`'s own multi-device
                # discovery, which works correctly once each device sits on
                # its own root port as real hardware would present it.
                "-device",
                "usb-storage,bus=uhci.0,port=1,drive=bootdrive",
                "-device",
                "usb-storage,bus=uhci.0,port=2,drive=seconddrive",
                "-boot",
                "order=c",
                "-serial",
                "stdio",
                "-no-reboot",
                "-no-shutdown",
                "-netdev",
                f"socket,id=net0,mcast={group}:{port}",
                "-device",
                f"rtl8139,netdev=net0,mac={guest_mac_str}",
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

        def run_command(command: bytes, timeout: float = 25) -> str:
            start = len(output)
            mcast.send(eth_frame(guest_mac, client_mac, TERM_TAG_DATA, command + b"\n"))
            return read_until(b"fos> ", timeout, start).decode("utf-8", "replace")

        guest_mac = send_tape.parse_mac(guest_mac_str)

        try:
            mcast = McastSocket(group, port)

            read_until(b"Waiting for a remote terminal client to connect", 30)
            mcast.send(eth_frame(b"\xff" * 6, client_mac, TERM_TAG_HELLO))
            read_until(b"Remote terminal connected", 10)
            read_until(b"fos> ", 5)

            if b"disk2: found (BIOS drive 0x81," not in output:
                raise SystemExit(
                    f"disk2 was not discovered via BIOS before any UHCI reset:\n{output.decode('utf-8', 'replace')}"
                )
            print("PASS: disk2 (the second usb-storage device) is an ordinary BIOS disk before 'usb native'.")

            native = run_command(b"usb native")
            if "disk1: now reads/writes through the native USB driver." not in native:
                raise SystemExit(f"'usb native' did not promote disk1:\n{native}")
            if "disk2: now reads/writes through the native USB driver." not in native:
                raise SystemExit(f"'usb native' did not promote disk2:\n{native}")
            print("PASS: 'usb native' promoted both disk1 and disk2 in one pass.")

            info = run_command(b"bootinfo")
            if "disk1 is promoted" not in info:
                raise SystemExit(f"bootinfo did not reflect disk1's promoted state:\n{info}")

            after = run_command(b"cat readme.txt")
            if "Welcome to FOSFS" not in after:
                raise SystemExit(f"disk1 not readable after 'usb native':\n{after}")
            print("PASS: disk1 still readable after 'usb native' (native path).")

            fmt = run_command(b"format disk2")
            if "FOSFS formatted." not in fmt:
                raise SystemExit(f"formatting disk2 over the native driver failed:\n{fmt}")
            write = run_command(b"write disk2 native.txt second-disk-native-works")
            if "File written." not in write:
                raise SystemExit(f"write to disk2 failed after 'usb native':\n{write}")
            cat = run_command(b"cat disk2 native.txt")
            if "second-disk-native-works" not in cat:
                raise SystemExit(f"readback from disk2 did not match:\n{cat}")
            print("PASS: disk2 format/write/cat round trip works after 'usb native'.")

            mcast.send(eth_frame(guest_mac, client_mac, TERM_TAG_BYE))
        finally:
            guest.terminate()
            try:
                guest.wait(timeout=5)
            except subprocess.TimeoutExpired:
                guest.kill()


def test_refuses_mismatched_device() -> None:
    """The negative case: `disk1` boots ordinarily (IDE, not USB), and a
    *different*, distinctly-sized USB Mass Storage device is attached
    alongside it. `usb storage promote` must refuse -- its capacity and
    first-sector cross-validation is the only thing standing between a
    correct promotion and silently switching `disk1` to the wrong
    physical disk, so this must be verified to actually reject a
    mismatch, not just accept a match."""
    if not IMAGE.exists():
        raise SystemExit(f"missing {IMAGE}; run `make image` first")

    group, port = "230.42.42.68", 4269  # distinct from the positive test above
    client_mac = bytes.fromhex("02deadbeef69")
    guest_mac_str = "52:54:00:12:34:79"

    selector = selectors.DefaultSelector()
    output = bytearray()

    with tempfile.TemporaryDirectory() as tmp:
        # Deliberately a different size than build/fos.img (16 MiB), so a
        # capacity mismatch alone would already be enough to refuse --
        # its all-zero first sector also differs from fos.img's real MBR.
        mismatched = Path(tmp) / "different_disk.img"
        mismatched.write_bytes(bytearray(1 * 1024 * 1024))

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
                f"socket,id=net0,mcast={group}:{port}",
                "-device",
                f"rtl8139,netdev=net0,mac={guest_mac_str}",
                "-usb",
                "-device",
                "piix3-usb-uhci,id=uhci",
                "-drive",
                f"if=none,id=mscdrive,format=raw,file={mismatched}",
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
            mcast.send(eth_frame(guest_mac, client_mac, TERM_TAG_DATA, command + b"\n"))
            return read_until(b"fos> ", timeout, start).decode("utf-8", "replace")

        guest_mac = send_tape.parse_mac(guest_mac_str)

        try:
            mcast = McastSocket(group, port)
            read_until(b"Waiting for a remote terminal client to connect", 30)
            mcast.send(eth_frame(b"\xff" * 6, client_mac, TERM_TAG_HELLO))
            read_until(b"Remote terminal connected", 10)
            read_until(b"fos> ", 5)

            promote = run_command(b"usb storage promote")
            if "disk1 was left untouched" not in promote:
                raise SystemExit(f"promote did not refuse the mismatched device:\n{promote}")
            print("PASS: 'usb storage promote' correctly refuses a mismatched device.")

            info = run_command(b"bootinfo")
            if "disk1 is promoted" in info:
                raise SystemExit(f"bootinfo shows disk1 promoted after a refusal:\n{info}")

            after = run_command(b"cat readme.txt")
            if "Welcome to FOSFS" not in after:
                raise SystemExit(f"disk1 broke after a refused promote attempt:\n{after}")
            print("PASS: disk1 remains untouched (still BIOS-backed) after the refusal.")

            mcast.send(eth_frame(guest_mac, client_mac, TERM_TAG_BYE))
        finally:
            guest.terminate()
            try:
                guest.wait(timeout=5)
            except subprocess.TimeoutExpired:
                guest.kill()


if __name__ == "__main__":
    test_promote_usb_booted_disk()
    test_promotes_multiple_disks()
    test_refuses_mismatched_device()
