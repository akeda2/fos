#!/usr/bin/env python3
"""The EHCI host controller driver and USB mass storage over it, in QEMU.

Same two guests as test_xhci_e2e.py, with QEMU's `usb-ehci` and a high-speed
`usb-storage` stick: `usb ehci` addresses and describes it, `usb storage` and
`usb storage attach` read and write it (multi-chunk tape copies, which exercise
the data toggle across transfers), and `usb native` promotes the BIOS-served copy
of the same stick. One-level hub support cannot be tested here: QEMU's usb-hub
is full speed and will not attach to an EHCI port; it is exercised on the Sandy
Bridge, whose EHCI ports sit behind Intel's rate-matching hub.

Run with `make test-ehci`."""

import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_nettest_selftest_e2e import check  # noqa: E402
from test_xhci_e2e import exercise_disk, make_lab  # noqa: E402

CONTROLLER = ("usb-ehci,id=ehci", "ehci.0")


def main():
    with tempfile.TemporaryDirectory() as tmp:
        backing = os.path.join(tmp, "stick.img")
        subprocess.run(["qemu-img", "create", "-f", "raw", backing, "16M"], check=True, capture_output=True)

        lab = make_lab(backing, *CONTROLLER)
        try:
            text = lab.shell("usb ehci", timeout=60)
            check("EHCI 0:" in text and "device 46f4:0001" in text, "usb ehci addresses the stick and reads its descriptor", text)
            text = lab.shell("usb storage", timeout=60)
            check("vendor='QEMU'" in text and "32768 block(s) of 512 bytes" in text,
                  "usb storage: INQUIRY and READ CAPACITY work over EHCI", text)
            text = lab.shell("usb storage attach", timeout=60)
            attached = re.search(r"Attached as (disk\d+)", text)
            check(attached is not None, "usb storage attach adds a disk", text)
            exercise_disk(lab, attached.group(1), "attached")
            text = lab.shell("usb ehci", timeout=60)
            check("device in use" in text, "usb ehci leaves a port that a disk uses alone", text)
            for _ in range(10):  # re-discovery frees and reallocates addresses: no leak
                lab.shell("usb storage attach", timeout=60)
            text = lab.shell("usb storage", timeout=60)
            check("32768 block(s)" in text, "ten more discoveries leave a working controller", text)
        finally:
            lab.stop()

        subprocess.run(["qemu-img", "create", "-f", "raw", backing, "16M"], check=True, capture_output=True)
        lab = make_lab(backing, *CONTROLLER)
        try:
            text = lab.shell("usb native", timeout=90)
            check("disk2: now reads/writes through the native USB driver." in text,
                  "usb native promotes the BIOS-served stick to the EHCI driver", text)
            exercise_disk(lab, "disk2", "promoted")
        finally:
            lab.stop()
    print("ehci e2e: all checks passed")


if __name__ == "__main__":
    main()
