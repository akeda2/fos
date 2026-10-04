#!/usr/bin/env python3
"""The diagnostic shell commands: `cpuinfo`, `bootinfo`, `memmap`, `irqstat`, `pci read|write`,
`io` and `mmio`, against QEMU's q35 (whose host bridge, LAPIC and 8042 have known values).
Run with `make test-v2-diag`."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_debug_mode import Machine  # noqa: E402


def main():
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {})
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            run = machine.run
            cpu = run("cpuinfo")
            check("vendor" in cpu and "long-mode" in cpu and "family" in cpu and "tsc" in cpu, "cpuinfo reports vendor, family, features and the TSC kind", cpu)
            boot = run("bootinfo")
            check("version 5" in boot and "graphics" in boot and "payload" in boot, "bootinfo shows the loader's handoff", boot)
            memory = run("memmap")
            check("usable" in memory and "MiB of conventional" in memory and "runtime" in memory, "memmap lists the firmware's map and totals the usable memory", memory)
            pci = run("pci read 0:0.0 0 16")
            check("0x00: 29c08086" in pci, "pci read dumps config space (q35 host bridge 8086:29c0)", pci)
            check("no device" in run("pci read 0:1f.7"), "pci read says when nothing answers", "")
            check("[0x3c] <- 0x" in run("pci write 0:0.0 0x3c 0x7"), "pci write writes a dword (the interrupt line register)", "")
            check("07" in run("pci read 0:0.0 0x3c 4").split(":")[-1], "and it reads back", "")
            check("-> 0x" in run("io r8 0x64"), "io r8 reads a port (the 8042 status)", "")
            check("<-" in run("io w8 0x80 0x55"), "io w8 writes one (POST code port)", "")
            check("-> 0x" in run("mmio r32 0xfee00020"), "mmio r32 reads a mapped register (the local APIC id)", "")
            check("not mapped" in run("mmio r32 0x100000000000"), "mmio refuses an unmapped address instead of faulting", "")
            check("aligned" in run("mmio r32 0xfee00022"), "mmio refuses a misaligned address", "")
            irq = run("irqstat")
            check("vector 0x41" in irq and "enabled" in irq, "irqstat shows the keyboard's routed pin", irq)
            check("hexdump" in run("help") and "cpuinfo" in run("help"), "help lists the new commands", "")
        finally:
            machine.stop()
    print("v2 diag: all checks passed")


if __name__ == "__main__":
    main()
