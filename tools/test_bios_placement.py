#!/usr/bin/env python3
"""BIOS path: stage two chooses where the kernel goes from the E820 map (boot/bios/stage2.S, choose_placement).

Boots the image through SeaBIOS (no OVMF: QEMU's default firmware) with three amounts of RAM and reads the serial log:
 * 512 MiB: the kernel image lands at the first 2 MiB boundary above 1 MiB, its read buffer 8 MiB after it, and the kernel starts;
 * 40 MiB: the same rule on a small machine, and the kernel still starts;
 * 8 MiB: not enough contiguous RAM above 1 MiB for the image plus its read buffer, so stage two stops with a message that says so
   (instead of loading over something else).
Run with `make test-bios-placement`."""
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "v2" / "v2.img"
FAILURES = []


def check(condition, message, detail=""):
    print(("PASS: " if condition else "FAIL: ") + message)
    if not condition:
        FAILURES.append(message)
        if detail:
            print(detail[-1500:])


def boot(memory, wait_for):
    with tempfile.TemporaryDirectory() as tmp:
        log = Path(tmp) / "serial.log"
        process = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35", "-m", str(memory), "-display", "none", "-no-reboot",
             "-drive", f"format=raw,file={IMAGE}", "-serial", f"file:{log}"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.time() + 40
            while time.time() < deadline:
                text = log.read_text(errors="replace") if log.exists() else ""
                if wait_for in text:
                    break
                time.sleep(0.3)
        finally:
            process.kill()
            process.wait()
        return log.read_text(errors="replace") if log.exists() else ""


def main():
    if not IMAGE.exists():
        sys.exit(f"{IMAGE} is missing; run `make image` first")
    for memory in (512, 40):
        text = boot(memory, "FOS v2 kernel: M3 complete")
        found = re.search(r"kernel image at 0x([0-9A-F]{8}), read buffer at 0x([0-9A-F]{8})", text)
        check(found is not None and int(found.group(1), 16) % 0x200000 == 0 and int(found.group(1), 16) >= 0x100000
              and int(found.group(2), 16) == int(found.group(1), 16) + 0x800000,
              f"{memory} MiB: the image is on a 2 MiB boundary above 1 MiB with its read buffer 8 MiB later ({found and found.groups()})", text)
        check("running in 64-bit long mode" in text and f"kernel at 0x{int(found.group(1), 16):x}" in text if found else False,
              f"{memory} MiB: the kernel starts where stage two put it", text)
    text = boot(8, "FAILED")
    check("no usable RAM region above 1 MiB is large enough" in text, "8 MiB: too little RAM stops stage two with a clear message", text)
    check("running in 64-bit long mode" not in text, "and the kernel is not started", text)
    if FAILURES:
        sys.exit(f"bios placement: {len(FAILURES)} check(s) failed")
    print("bios placement: all checks passed")


if __name__ == "__main__":
    main()
