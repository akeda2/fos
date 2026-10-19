#!/usr/bin/env python3
"""aarch64 deliberate-fault test: kernels built with one Cargo feature each (fault-ud: an undefined instruction, fault-null:
a read from an unmapped low address) must report the exception in words through the vectors, with the right class and the
faulting address. Run `make test-aarch64-faults`."""
import os
import subprocess
import sys
from pathlib import Path

from test_aarch64_boot import ROOT, boot, check

TARGET = "aarch64-unknown-none-softfloat"
CASES = [
    ("fault-ud", "class 0x00 (unknown reason (undefined instruction))"),
    ("fault-null", "class 0x25 (data abort)"),
]


def main():
    scratch = ROOT / "build/aarch64-faults"
    loader = ROOT / "build/cargo-target/aarch64-unknown-uefi/release/fosfor-loader.efi"
    for feature, expected in CASES:
        target_dir = scratch / feature
        subprocess.run(["cargo", "+nightly-2026-09-13", "build", "--release", "--target", TARGET, "--features", feature,
                        "-Z", "build-std=core,compiler_builtins", "-Z", "build-std-features=compiler-builtins-mem", "--target-dir", str(target_dir)],
                       cwd=ROOT / "kernel", check=True, capture_output=True)
        image = scratch / f"{feature}.img"
        subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--arch", "aarch64", "--loader", str(loader),
                        "--kernel", str(target_dir / TARGET / "release/fosfor-kernel"), "--output", str(image)], check=True, capture_output=True)
        text = boot(image)
        check("EXCEPTION: synchronous from current EL" in text, f"{feature}: the exception is reported by the vectors", text)
        check(expected in text, f"{feature}: the report names the cause ({expected})", text)
        check("Nothing more to do yet" not in text, f"{feature}: the kernel did not carry on after the fault", text)
        if feature == "fault-null":
            check("FAR 0x0000000000000010" in text, f"{feature}: FAR holds the faulting address", text)
    print("aarch64 faults: all checks passed")


if __name__ == "__main__":
    main()
