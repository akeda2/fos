#!/usr/bin/env python3
"""End-to-end test (QEMU, serial only) for the shell's register-access commands:
`pci read|write`, `io`, `mmio`, and the refusals that keep a typo from
page-faulting the kernel. Values are checked against things QEMU's i440FX
machine and this kernel fix: the host bridge's vendor/device IDs, the first
bytes of the kernel image, and the local APIC's ID register.

Run via `make test-hwaccess`."""

import sys
from pathlib import Path
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_scripts_settings_e2e import Guest, build_image, expect, expect_absent  # noqa: E402


def main() -> None:
    with tempfile.TemporaryDirectory() as name:
        image = Path(name) / "hw.img"
        build_image(image, {})
        guest = Guest(image, ["-nic", "none"])
        try:
            guest.read_until(b"fos> ", 20)

            # PCI configuration space: i440FX host bridge is 8086:1237.
            out = guest.run("pci read 00:00.0 0 16")
            expect(out, "00: 86 80 37 12")
            out = guest.run("pci")
            expect(out, "00:00.0 8086:1237")
            out = guest.run("pci read 00:00.0 0x3c 4")
            expect(out, "30:")  # the row holding offset 0x3c
            out = guest.run("pci write 00:00.0 0x3c 0x00000100")
            expect(out, "00:00.0 [3c] <- 00000100", "reads back")
            expect(guest.run("pci read zz"), "Usage: pci")
            expect(guest.run("pci read 00:00.0 0 9999"), "00:")  # length is clamped, not an error

            # Port I/O.
            expect(guest.run("io r8 0x61"), "port 0x0061 = 0x")
            expect(guest.run("io w8 0x80 0"), "port 0x0080 <- 0x0")
            expect(guest.run("io r16 0x61 5"), "Usage: io")
            expect(guest.run("io w8 0x80"), "Usage: io")
            expect(guest.run("io r32 0x70000"), "Usage: io")

            # Memory-mapped access.
            expect(guest.run("mmio r16 0x10000"), "[0x00010000] = 0xbcfa")  # entry.S starts: cli; mov
            expect(guest.run("mmio r8 0x10000"), "[0x00010000] = 0xfa")
            expect(guest.run("mmio r32 0x10001"), "not 4-byte aligned")
            expect(guest.run("mmio w32 0x10000 1"), "not accessible", "kernel text")
            lapic = guest.run("mmio r32 0xfee00020")  # local APIC ID: mapped by lapic::initialize
            expect(lapic, "[0xfee00020] = 0x")
            expect_absent(lapic, "not accessible")
            expect(guest.run("mmio r32 0xfec00000"), "not accessible")  # I/O APIC window: never mapped
            expect(guest.run("mmio q32 1"), "Usage: mmio")

            # A scratch RAM word round-trips through write and read.
            expect(guest.run("mmio w32 0x200000 0xdeadbeef"), "<- 0xdeadbeef")
            expect(guest.run("mmio r32 0x200000"), "= 0xdeadbeef")

            # With no NIC there is nothing to dump; the command still answers.
            expect(guest.run("nic regs"), "Usage: nic regs")
            print("PASS: pci read/write, io, mmio and nic regs behave, and refuse unmapped or read-only targets.")
        finally:
            guest.terminate()


if __name__ == "__main__":
    main()
