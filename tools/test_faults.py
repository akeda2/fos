#!/usr/bin/env python3
"""Deliberate-fault tests for the v2 kernel's exception reporting (milestone M2).

For each fault the kernel is built with one Cargo feature that provokes it right after
boot (kernel/Cargo.toml `[features]`), put into its own image, booted under QEMU +
OVMF, and the report on the serial port is checked: the right exception, the right
address, a plain-words cause. The stack overflow is the interesting one: the normal
stack is the thing that failed, so the report can only appear because the double-fault
handler runs on its own stack through the TSS.

Run with `make test-faults` (it rebuilds the kernel five times, then restores the
normal build)."""

import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import CODE_CANDIDATES, VARS_CANDIDATES, clean, find  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
BUILD = ROOT / "build"
TARGET_DIR = BUILD / "cargo-target"
LOADER = TARGET_DIR / "x86_64-unknown-uefi" / "release" / "fosfor-loader.efi"
KERNEL = TARGET_DIR / "x86_64-fos-kernel" / "release" / "fosfor-kernel"

# feature -> checks on the report (all must appear)
FAULTS = {
    "fault-null": ["#PF page fault (vector 14)", "the null page", "a read from", "access to a not-present"],
    "fault-text-write": ["#PF page fault (vector 14)", "kernel code (read-only)", "a write to", "a protection violation"],
    "fault-nx": ["#PF page fault (vector 14)", "an instruction fetch from", "NX violation"],
    "fault-ud": ["#UD invalid opcode (vector 6)"],
    "fault-stack": ["#DF double fault (vector 8)", "kernel stack guard page", "the kernel stack overflowed"],
}


def check(condition, message, text=""):
    if not condition:
        raise SystemExit(f"FAIL: {message}\n--- output ---\n{text[-2500:]}")
    print(f"PASS: {message}")


def build_kernel(features):
    environment = dict(os.environ, CARGO_TARGET_DIR=str(TARGET_DIR))
    command = ["cargo", "+nightly-2026-09-13", "build", "--release", "-Z", "build-std=core,compiler_builtins",
               "-Z", "build-std-features=compiler-builtins-mem", "-Z", "json-target-spec"]
    if features:
        command += ["--features", features]
    subprocess.run(command, cwd=ROOT / "kernel", env=environment, check=True, capture_output=True)


def boot(image, code, variables):
    with tempfile.TemporaryDirectory() as tmp:
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}",
                   "-drive", f"if=pflash,format=raw,file={vars_copy}", "-drive", f"format=raw,file={image}"]
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 60
        try:
            while "*** halting" not in clean(bytes(raw)) and time.monotonic() < deadline and process.poll() is None:
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
        return clean(bytes(raw))


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    if not LOADER.exists():
        raise SystemExit("build the loader first (`make loader`)")
    try:
        for feature, expectations in FAULTS.items():
            build_kernel(feature)
            image = BUILD / "fosfor" / f"{feature}.img"
            subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER),
                            "--kernel", str(KERNEL), "--output", str(image)], check=True, capture_output=True)
            text = boot(image, code, variables)
            report = text[text.rfind("Provoking a fault"):] if "Provoking a fault" in text else text
            check("Provoking a fault" in text, f"{feature}: the kernel boots and provokes the fault", text)
            check("*** CPU EXCEPTION" in report and "*** halting" in report, f"{feature}: a report is printed and the machine halts", text)
            for expected in expectations:
                check(expected in report, f"{feature}: the report says '{expected}'", report)
            image.unlink()
    finally:
        build_kernel("")  # restore the normal kernel build
    print("faults: all checks passed")


if __name__ == "__main__":
    main()
