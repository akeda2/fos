#!/usr/bin/env python3
"""The AHCI (SATA) driver, in QEMU: `sata` lists the controller, `sata drives`
starts it and identifies the disk, `sata attach` makes it a diskN that FOSFS can
format, write, read and use for multi-chunk copies in both directions (exercising
the 48-bit-LBA DMA commands, the bounce buffer and the cache flush after writes),
and a second `sata attach` adds nothing.

QEMU's ide-hd always reports 48-bit LBA, so the 28-bit command variants have no
automated coverage. Run with `make test-ahci`."""

import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_nettest_selftest_e2e import Lab, check  # noqa: E402
from test_xhci_e2e import exercise_disk  # noqa: E402


def main():
    with tempfile.TemporaryDirectory() as tmp:
        backing = os.path.join(tmp, "sata.img")
        subprocess.run(["qemu-img", "create", "-f", "raw", backing, "32M"], check=True, capture_output=True)
        lab = Lab(nic_args=["-netdev", "user,id=n0", "-device", "e1000,netdev=n0",
                            "-device", "ich9-ahci,id=ahci", "-drive", f"if=none,id=sd0,format=raw,file={backing}",
                            "-device", "ide-hd,drive=sd0,bus=ahci.0"])
        try:
            text = lab.shell("sata")
            check("SATA AHCI [driver]" in text and "class 01/06/01" in text, "sata lists the AHCI controller with a driver", text)
            text = lab.shell("sata drives", timeout=60)
            check("QEMU HARDDISK" in text and "65536 sectors (32 MiB)" in text, "sata drives identifies the disk (model, size)", text)
            check(max(len(line) for line in text.splitlines()[1:]) <= 79, "and fits 80 columns", text)
            text = lab.shell("sata attach", timeout=60)
            check("Attached 1 new SATA disk(s)" in text, "sata attach adds the disk", text)
            exercise_disk(lab, "disk3", "ahci")
            text = lab.shell("sata attach", timeout=60)
            check("Attached 0 new SATA disk(s)" in text, "a second sata attach adds nothing", text)
            text = lab.shell("cat disk3 xhci.txt", timeout=60)
            check("hello over xhci" in text, "the attached disk still reads afterwards", text)
        finally:
            lab.stop()
    print("ahci e2e: all checks passed")


if __name__ == "__main__":
    main()
