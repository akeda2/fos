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
    soak_checks()
    print("memtest e2e: all checks passed")


def soak_checks():
    """Soak mode on the selftest lab: four e1000 NICs, 1<->2 and 3<->4 cabled."""
    lab = Lab()  # four NICs, 256 MiB, snapshot (disk writes are thrown away)
    try:
        before = free_frames(lab)
        text = run_memtest(lab, "soak for=20 status=5 size=2 tests=addr net=1,2 net.fps=200 "
                           "disk=soak.tmp disk.every=5 disk.kb=16", 120)
        check("RESULT: PASS, no errors" in text, "a clean soak passes", text)
        check("disk: WRITES 16 KiB to 'soak.tmp'" in text, "the disk test announces that it writes", text)
        sent = re.search(r"net NIC 1 -> 2: sent (\d+) rx (\d+) missing 0 bad 0", text)
        check(sent is not None and int(sent.group(1)) > 2000 and sent.group(1) == sent.group(2),
              f"all soak frames arrived NIC 1 -> 2 ({sent and sent.groups()})", text)
        check(re.search(r"net NIC 2 -> 1: sent (\d+) rx \1 missing 0 bad 0", text) is not None,
              "and NIC 2 -> 1", text)
        disk = re.search(r"disk: (\d+) rounds, ok (\d+) mismatches 0 errors 0", text)
        check(disk is not None and int(disk.group(2)) >= 2 and disk.group(1) == disk.group(2),
              f"the disk file was written and read back correctly ({disk and disk.groups()})", text)
        check("churn ... ok" not in text and "RESULT: PASS" in text and "churn: on" in text,
              "allocator churn ran quietly between passes", text)
        check("backwards 0, tick stalls 0" in text, "the clock behaved", text)
        check(re.search(r"\d\d:\d\d:\d\d pass \d+ errors 0 \| clock skew", text) is not None, "status lines were printed", text)
        check(max(len(line) for line in text.splitlines()[1:]) <= 79, "every line fits an 80-column screen", text)
        check(free_frames(lab) == before, "no frames leaked", text)
        check("soak.tmp" not in lab.shell("ls"), "the scratch file was removed", text)

        text = run_memtest(lab, "soak for=6 size=2 tests=addr")
        check("disk: off, never automatic" in text and "disk: " + "0 rounds" not in text and "RESULT: PASS" in text,
              "without disk= the disk is never touched", text)

        text = run_memtest(lab, "soak for=6 size=2 tests=addr disk=readme.txt")
        check("Usage: tape run memtest" in text, "disk= refuses a name that does not start with soak", text)

        text = run_memtest(lab, "soak for=8 size=2 tests=addr net=1,3 net.fps=100")
        check("RESULT: FAIL" in text and "no soak frame ever arrived" in text,
              "an uncabled net pair is reported as a failure", text)

        text = run_memtest(lab, "soak for=6 size=2 tests=random inject=10 churn=off")
        check("RESULT: FAIL" in text and "memory errors" in text, "soak reports injected memory errors", text)
    finally:
        lab.stop()


if __name__ == "__main__":
    main()
