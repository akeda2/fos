#!/usr/bin/env python3
"""Hard-float tapes: the kernel switches SSE (and AVX2 + FMA where the CPU has them) on before a tape starts and off when it ends.
`fpcheck` is built for x86_64-fos-tape-fp (SSE2 baseline) and prints its own verdict. Run on QEMU's default CPU (SSE2 only: level 1)
and `-cpu max` (AVX2 + FMA: level 2). A tape that returns must leave the machine usable, so a second run follows the first.
A soft-float tape (`hello`) runs in between to show nothing changed for it. Run with `make test-fp`."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import ROOT, check  # noqa: E402
from test_debug_mode import Machine  # noqa: E402

FPCHECK = ROOT / "build/cargo-target/x86_64-fos-tape-fp/release/fpcheck"
HELLO = ROOT / "build/cargo-target/x86_64-fos-tape/release/hello"


def run_on(cpu, expected_level, expected_name):
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {}, {"fpcheck": FPCHECK, "hello": HELLO})
        machine.cpu = cpu
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            label = cpu or "default CPU"
            first = machine.run("run fpcheck")
            check(f"fp level {expected_level} ({expected_name})" in first, f"{label}: the kernel enabled level {expected_level}", first)
            check("fpcheck: ok" in first and "FAIL" not in first, f"{label}: f64, SSE and mxcsr checks pass", first)
            check(("avx2 fma dot of eight = 120" in first) == (expected_level == 2), f"{label}: the AVX2/FMA path runs exactly when level 2", first)
            hello = machine.run("run hello")
            check("hello from a v2 tape" in hello, f"{label}: a soft-float tape still runs after a hard-float one", hello)
            second = machine.run("run fpcheck")
            check("fpcheck: ok" in second, f"{label}: a second hard-float run after the first ended", second)
        finally:
            machine.stop()


def main():
    run_on(None, 1, "sse2")
    run_on("max", 2, "avx2")
    print("fp: all checks passed")


if __name__ == "__main__":
    main()
