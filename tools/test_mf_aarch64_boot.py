#!/usr/bin/env python3
"""Server kernel (mf) on aarch64, first slice: the aarch64 UEFI loader (QEMU virt + AAVMF) starts it, and it reaches
the end of boot and the serial shell: physical memory, the MMU page tables, the heap self-test, GIC timer ticks and the scheduler self-tests.
Run `make test-mf-aarch64-boot`."""
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_aarch64_boot as base  # noqa: E402

IMAGE = Path(os.environ.get("FOSFOR_IMAGE", base.ROOT / "build/fosfor-mf-aarch64/fosfor.img"))


def main() -> None:
    text = base.boot(IMAGE, commands=("version", "cpuinfo", "uptime", "nic", "run hello", "io r8 0x60"))
    base.check("server kernel (mf, slice 8): running from the UEFI loader at EL" in text, "the mf kernel starts on aarch64", text)
    base.check("Physical memory:" in text, "the physical allocator is up", text)
    base.check("Page tables: loaded" in text, "the MMU is on with our page tables", text)
    base.check("Heap: self-test passed" in text, "the heap works", text)
    ticks = re.search(r"(\d+) timer interrupts", text)
    base.check(ticks is not None and 8 <= int(ticks.group(1)) <= 12, "the GIC delivers timer ticks", text)
    base.check("Scheduler: 3 kernel threads took 15 interleaved steps" in text and " 0 stack frame(s) not returned" in text, "kernel threads switch and their stacks are returned", text)
    base.check("preemption ok" in text and "sleep ok" in text and "mutex ok" in text, "timer preemption, sleep and mutex work", text)
    base.check("reached the end of boot" in text, "boot reaches its end", text)
    base.check("fos> " in text, "the shell prompt appears on the serial console", text)
    base.check("server kernel (mf)" in text.split("fos> ", 1)[-1], "`version` answers", text)
    base.check("implementer" in text and "features" in text, "`cpuinfo` reads the ID registers", text)
    base.check(re.search(r"\d+ s$", text, re.M) is not None, "`uptime` answers", text)
    base.check("No network interfaces" in text, "`nic` says there are no drivers yet", text)
    base.check("cannot start" in text or "no file" in text, "`run` refuses cleanly (no EL0 yet)", text)
    base.check("no I/O port space" in text, "`io` explains there are no ports", text)


if __name__ == "__main__":
    main()
