#!/usr/bin/env python3
"""BIOS-path spike B1 (docs/shared-codebase.md): SeaBIOS -> stage one -> stage two -> long mode -> a 64-bit test kernel.

Boots build/bios-spike/spike.img with plain QEMU (no OVMF: the default firmware is SeaBIOS) on two chipsets and two
memory sizes and checks the serial log: stage two's narration, long mode reached, the memory map seen by the kernel,
and the tail marker of the kernel image (so the multi-piece copy above 1 MiB arrived intact). Run: make test-bios-spike.
"""
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "bios-spike" / "spike.img"
FAILURES = []


def check(condition, message, detail=""):
    print(("PASS: " if condition else "FAIL: ") + message)
    if not condition:
        FAILURES.append(message)
        if detail:
            print(detail)


def boot(machine, memory):
    with tempfile.TemporaryDirectory() as tmp:
        log = Path(tmp) / "serial.log"
        process = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", machine, "-m", str(memory), "-display", "none", "-no-reboot",
             "-drive", f"format=raw,file={IMAGE}", "-serial", f"file:{log}"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.time() + 20
            while time.time() < deadline:
                text = log.read_text(errors="replace") if log.exists() else ""
                if "SPIKE: done" in text or "FAILED" in text or "BAD" in text:
                    break
                time.sleep(0.2)
        finally:
            process.kill()
            process.wait()
        return log.read_text(errors="replace") if log.exists() else ""


def main():
    if not IMAGE.exists():
        sys.exit(f"{IMAGE} is missing; run `make bios-spike` first")
    for machine, memory in (("pc", 128), ("q35", 1024)):
        text = boot(machine, memory)
        label = f"{machine}, {memory} MiB"
        check("Fosfor stage 2" in text and "A20 line: ok" in text, f"{label}: stage two narrates and enables A20", text)
        check("Memory map (INT 15h E820): ok" in text, f"{label}: the BIOS memory map is collected", text)
        check("copying it above 1 MiB (unreal mode): ok" in text, f"{label}: the kernel is read with EDD and copied above 1 MiB", text)
        check("SPIKE: long mode reached from BIOS" in text, f"{label}: the 64-bit kernel runs in long mode", text)
        found = re.search(r"E820 entries: (\d+), usable RAM: (\d+) MiB", text)
        check(found is not None and int(found.group(1)) >= 3 and memory - 8 <= int(found.group(2)) <= memory,
              f"{label}: the kernel sees the boot information and about {memory} MiB of usable RAM ({found.group(0) if found else None})", text)
        check("kernel tail intact" in text, f"{label}: the whole image arrived (multi-piece copy)", text)
    if FAILURES:
        sys.exit(f"bios spike: {len(FAILURES)} check(s) failed")
    print("bios spike: all checks passed")


if __name__ == "__main__":
    main()
