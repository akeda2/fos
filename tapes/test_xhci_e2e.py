#!/usr/bin/env python3
"""The xHCI host controller driver and USB mass storage over it, in QEMU.

QEMU's `qemu-xhci` with a `usb-storage` stick (a SuperSpeed device). Two
guests:

1. `usb xhci` addresses the stick and reads its descriptor; `usb storage`
   shows INQUIRY/capacity; `usb storage attach` adds it as a new diskN, which is
   formatted, written, read back and used for multi-chunk copies of a real tape
   in both directions (those cross the 8 KiB sector-chunk size and QEMU's
   64 KiB TRB boundary logic).
2. `usb native` promotes the BIOS-served copy of the same stick (SeaBIOS
   exposes it as INT13h drive 0x81, the situation on a real BIOS-booted machine)
   to the native driver, after which it is formatted and used the same way, and
   a second `usb native` recognises it as already native.

Run with `make test-xhci`."""

import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_nettest_selftest_e2e import Lab, check  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
TELNET = ROOT / "build" / "tapes" / "telnet.bin"


def make_lab(backing):
    return Lab(nic_args=["-netdev", "user,id=n0", "-device", "e1000,netdev=n0",
                         "-device", "qemu-xhci,id=xhci", "-drive", f"if=none,id=usb0,format=raw,file={backing}",
                         "-device", "usb-storage,drive=usb0"])


def exercise_disk(lab, disk, label):
    text = lab.shell(f"format {disk}", timeout=60)
    check("FOSFS formatted." in text, f"{label}: format succeeds", text)
    text = lab.shell(f"write {disk} xhci.txt hello over xhci", timeout=60)
    check("File written." in text, f"{label}: write succeeds", text)
    text = lab.shell(f"cat {disk} xhci.txt", timeout=60)
    check("hello over xhci" in text, f"{label}: read-back matches", text)
    size = TELNET.stat().st_size
    text = lab.shell(f"copy disk1 telnet.bin {disk}", timeout=120)
    check(f"Copied {size} byte(s)" in text, f"{label}: a {size}-byte tape copies onto it", text)
    text = lab.shell(f"copy {disk} telnet.bin disk1 xhcichk.bin", timeout=120)
    check(f"Copied {size} byte(s)" in text, f"{label}: and back off it", text)
    lab.shell("rm xhcichk.bin")


def main():
    with tempfile.TemporaryDirectory() as tmp:
        backing = os.path.join(tmp, "stick.img")
        subprocess.run(["qemu-img", "create", "-f", "raw", backing, "16M"], check=True, capture_output=True)

        lab = make_lab(backing)
        try:
            text = lab.shell("usb xhci", timeout=60)
            check("SuperSpeed device 46f4:0001" in text and "8 root port(s)" in text,
                  "usb xhci addresses the stick and reads its descriptor", text)
            check("USB3, no device" in text and "USB2, no device" in text, "ports report their USB version", text)
            text = lab.shell("usb storage", timeout=60)
            check("vendor='QEMU'" in text and "32768 block(s) of 512 bytes" in text,
                  "usb storage: INQUIRY and READ CAPACITY work over xHCI", text)
            text = lab.shell("usb storage attach", timeout=60)
            attached = re.search(r"Attached as (disk\d+)", text)
            check(attached is not None, "usb storage attach adds a disk", text)
            exercise_disk(lab, attached.group(1), "attached")
            # A diagnostic must not reset a port a disk is using (it once did, on
            # the Skylake, and the boot disk answered FirmwareFailure afterwards).
            text = lab.shell("usb xhci", timeout=60)
            check("device in use" in text and "left alone" in text, "usb xhci leaves a port that a disk uses alone", text)
            text = lab.shell(f"cat {attached.group(1)} xhci.txt", timeout=60)
            check("hello over xhci" in text, "and that disk still reads afterwards", text)
            for _ in range(10):  # more than MAX_SLOTS re-discoveries: no slot leak
                lab.shell("usb storage attach", timeout=60)
            text = lab.shell("usb storage", timeout=60)
            check("32768 block(s)" in text, "ten more discoveries leave a working controller", text)
        finally:
            lab.stop()

        subprocess.run(["qemu-img", "create", "-f", "raw", backing, "16M"], check=True, capture_output=True)
        lab = make_lab(backing)
        try:
            text = lab.shell("usb native", timeout=90)
            check("disk2: now reads/writes through the native USB driver." in text,
                  "usb native promotes the BIOS-served stick to the xHCI driver", text)
            exercise_disk(lab, "disk2", "promoted")
            text = lab.shell("usb native", timeout=90)
            check("disk2: already native USB." in text, "a second usb native recognises it", text)
        finally:
            lab.stop()
    print("xhci e2e: all checks passed")


if __name__ == "__main__":
    main()
