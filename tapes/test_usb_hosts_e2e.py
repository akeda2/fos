#!/usr/bin/env python3
"""`usb hosts` lists every PCI USB host controller by type.

One QEMU guest with a UHCI, an OHCI, an EHCI and an xHCI controller (plus one
NIC, which the harness needs): the command must name all four with the right
vendor/device IDs, mark only UHCI as having a driver, and plain `usb` must open
with the tally. Run with `make test-usb-hosts`."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_nettest_selftest_e2e import Lab, check  # noqa: E402

DEVICES = ["-netdev", "user,id=n0", "-device", "e1000,netdev=n0",
           "-device", "piix3-usb-uhci", "-device", "pci-ohci", "-device", "usb-ehci", "-device", "qemu-xhci"]


def main():
    lab = Lab(nic_args=DEVICES)
    try:
        text = lab.shell("usb hosts")
        lines = {kind: [l for l in text.splitlines() if f" {kind} " in l] for kind in ("UHCI", "OHCI", "EHCI", "xHCI")}
        for kind, found in lines.items():
            check(len(found) == 1, f"exactly one {kind} controller is listed", text)
        check("8086:7020" in lines["UHCI"][0] and "[driver]" in lines["UHCI"][0], "UHCI is the PIIX3's and has a driver", text)
        check("[driver]" not in "".join(lines["OHCI"]), "OHCI is marked as having no driver", text)
        check("[driver]" in lines["EHCI"][0], "EHCI has a driver", text)
        check("[driver]" in lines["xHCI"][0], "xHCI has a driver", text)
        check("1033:" in lines["xHCI"][0] or "1b36:000d" in lines["xHCI"][0], "xHCI is QEMU's qemu-xhci", text)
        check(max(len(line) for line in text.splitlines()[1:]) <= 79, "every line fits 80 columns", text)
        text = lab.shell("usb")
        check("USB hosts: 1 UHCI, 1 OHCI, 1 EHCI, 1 xHCI (drivers: UHCI/EHCI/xHCI)" in text, "plain `usb` opens with the tally", text)
    finally:
        lab.stop()
    print("usb hosts e2e: all checks passed")


if __name__ == "__main__":
    main()
