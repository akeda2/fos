#!/usr/bin/env python3
"""End-to-end test of the memtest tape in QEMU.

Reuses the boot/serial harness of test_nettest_selftest_e2e.py (one e1000 NIC
is enough: memtest only needs RAM). Checks that

* the frame pool (kernel ABI slots 39/40, `alloc_frames`/`free_frames`) and the
  big pool (`alloc`) are both claimed, tested and handed back (`memory` shows
  the same free-frame count before and after),
* clean RAM reports PASS,
* a deliberate bit flip (`inject=`) is detected, with the right physical
  address, bit position and 1 MiB block,
* Escape stops a `passes=0` run,
* a small guest (32 MiB: no big pool at all) still gets something to test.

Run with `make test-tape-memtest`."""

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_nettest_selftest_e2e import Lab, check  # noqa: E402

NIC = ["-netdev", "user,id=n0", "-device", "e1000,netdev=n0"]


def free_frames(lab):
    text = lab.shell("pmem")
    found = re.search(r"(\d+) free", text)
    if not found:
        raise SystemExit(f"cannot read the free-frame count (pmem):\n{text}")
    return int(found.group(1))


def run_memtest(lab, arguments, timeout=120):
    lab.shell("tape load disk memtest.bin", b"Loaded ")
    return lab.shell(f"tape run memtest {arguments}", b"Tape returned", timeout)


def main():
    lab = Lab(nic_args=NIC, memory=256)
    try:
        before = free_frames(lab)
        text = run_memtest(lab, "size=4")
        check("(frame pool)" in text and "testing 4 MiB" in text, "claims 4 MiB from the frame pool", text)
        check("RESULT: PASS, no errors" in text, "clean RAM passes every test", text)
        for name in ("addr", "walk", "moving", "random"):
            check(f"pass 1 {name} ... ok" in text, f"the {name} test ran and passed", text)
        check(free_frames(lab) == before, "the frame pool is whole again afterwards", text)

        text = run_memtest(lab, "size=4 frames=off")
        block = re.search(r"block 1: 0x([0-9a-f]{8}) \+ 4096 KiB \(big pool\)", text)
        check(block is not None and int(block.group(1), 16) >= 0x0800_0000,
              "frames=off takes the big pool (physical 128 MiB and up)", text)
        check("RESULT: PASS" in text, "the big pool passes too", text)

        text = run_memtest(lab, "size=1 frames=off tests=random inject=100")
        block = re.search(r"block 1: 0x([0-9a-f]{8}) ", text)
        address = int(block.group(1), 16) + 100 * 4
        check("RESULT: FAIL" in text, "an injected bit flip fails the run", text)
        check(f"0x{address:08x} want" in text, f"the error names the exact address 0x{address:08x}", text)
        check("bit flips: bit3=" in text, "the flip is attributed to bit 3", text)
        check(f"bad MiB blocks: 0x{address & ~0xfffff:08x}=" in text, "the bad 1 MiB block is listed", text)

        text = run_memtest(lab, "tests=nonsense")
        check("Usage: tape run memtest" in text, "a bad argument prints the usage", text)

        lab.shell("tape load disk memtest.bin", b"Loaded ")
        mark = len(lab.output)
        lab.process.stdin.write(b"tape run memtest size=2 passes=0 tests=addr\n")
        lab.process.stdin.flush()
        lab.wait_for(b"pass 2 addr", mark, 60)
        lab.sendkey("f12")
        lab.wait_for(b"fos> ", mark, 30)
        text = lab.text(mark)
        check("memtest stopped:" in text, "Escape stops a passes=0 run", text)
        check(free_frames(lab) == before, "an aborted run also gives its memory back", text)
    finally:
        lab.stop()

    lab = Lab(nic_args=NIC, memory=32)
    try:
        text = run_memtest(lab, "size=100")
        total = re.search(r"testing (\d+) MiB", text)
        check(total is not None and 4 <= int(total.group(1)) <= 28 and "(big pool)" not in text,
              f"a 32 MiB guest has no big pool; {total and total.group(1)} MiB of frames were tested", text)
        check("RESULT: PASS" in text, "and they pass", text)
    finally:
        lab.stop()
    print("memtest e2e: all checks passed")


if __name__ == "__main__":
    main()
