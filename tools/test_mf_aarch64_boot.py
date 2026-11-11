#!/usr/bin/env python3
"""Server kernel (mf) on aarch64, first slice: the aarch64 UEFI loader (QEMU virt + AAVMF) starts it, and it reaches
the end of boot: physical memory, the MMU page tables, the heap self-test and GIC timer ticks.
Run `make test-mf-aarch64-boot`."""
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_aarch64_boot as base  # noqa: E402

IMAGE = Path(os.environ.get("FOSFOR_IMAGE", base.ROOT / "build/fosfor-mf-aarch64/fosfor.img"))


def main() -> None:
    text = base.boot(IMAGE)
    base.check("server kernel (mf, slice 8): running from the UEFI loader at EL" in text, "the mf kernel starts on aarch64", text)
    base.check("Physical memory:" in text, "the physical allocator is up", text)
    base.check("Page tables: loaded" in text, "the MMU is on with our page tables", text)
    base.check("Heap: self-test passed" in text, "the heap works", text)
    ticks = re.search(r"(\d+) timer interrupts", text)
    base.check(ticks is not None and 8 <= int(ticks.group(1)) <= 12, "the GIC delivers timer ticks", text)
    base.check("reached the end of boot" in text, "boot reaches its end", text)


if __name__ == "__main__":
    main()
