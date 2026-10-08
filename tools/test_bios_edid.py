#!/usr/bin/env python3
"""BIOS path: the VBE mode follows the monitor (boot/bios/stage2.S, read_edid and vbe_setup).

Stage two asks the monitor for its native size (EDID, through the video BIOS: INT 10h AX=4F15h) and picks the largest 32-bit VBE
mode that is not bigger than the monitor and has the monitor's shape (within 2%); modes of another shape score a quarter. Without
an EDID it keeps the old safe rule (the largest mode up to 1280x1024). Boots through SeaBIOS (no OVMF) with QEMU's standard VGA
playing different monitors and reads stage two's narration and the kernel's `Framebuffer:` line from the serial log.
Run with `make test-bios-edid`."""
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fosfor" / "fosfor.img"
FAILURES = []

# (QEMU VGA properties, the EDID size stage two should report or None, the mode it should end up with)
CASES = (
    ("edid=on", (1280, 800), (1280, 800)),                          # QEMU's default monitor
    ("edid=on,xres=1920,yres=1080", (1920, 1080), (1920, 1080)),    # a 16:9 monitor gets a native 16:9 mode (was 1280x1024, scaled)
    ("edid=on,xres=1366,yres=768", (1366, 768), (1280, 720)),       # no 1366x768 mode: the closest 16:9 one, not a 5:4 one
    ("edid=on,xres=1024,yres=768", (1024, 768), (1024, 768)),
    ("edid=off", None, (1280, 1024)),                               # no EDID: the old rule
)


def check(condition, message, detail=""):
    print(("PASS: " if condition else "FAIL: ") + message)
    if not condition:
        FAILURES.append(message)
        if detail:
            print(detail[-1500:])


def boot(vga):
    with tempfile.TemporaryDirectory() as tmp:
        log = Path(tmp) / "serial.log"
        process = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35", "-m", "256", "-display", "none", "-no-reboot", "-vga", "none",
             "-device", f"VGA,{vga}", "-drive", f"format=raw,file={IMAGE}", "-serial", f"file:{log}"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.time() + 40
            while time.time() < deadline:
                text = log.read_text(errors="replace") if log.exists() else ""
                if "M3 complete" in text or "FAILED" in text:
                    break
                time.sleep(0.3)
        finally:
            process.kill()
            process.wait()
        return log.read_text(errors="replace") if log.exists() else ""


def main():
    if not IMAGE.exists():
        sys.exit(f"{IMAGE} is missing; run `make image` first")
    for vga, edid, mode in CASES:
        text = boot(vga)
        label = f"VGA {vga}"
        found = re.search(r"native size \(EDID\): (\d+)x(\d+)", text)
        if edid is None:
            check(found is None, f"{label}: no EDID is reported", text)
        else:
            check(found is not None and (int(found.group(1)), int(found.group(2))) == edid, f"{label}: the monitor's native size is read ({edid[0]}x{edid[1]})", text)
        chosen = re.search(r"Video: VBE 0x[0-9A-F]{4}, mode 0x[0-9A-F]{4}: (\d+)x(\d+) 32 bpp", text)
        check(chosen is not None and (int(chosen.group(1)), int(chosen.group(2))) == mode, f"{label}: the mode is {mode[0]}x{mode[1]} ({chosen and chosen.groups()})", text)
        kernel = re.search(r"Framebuffer: (\d+)x(\d+) BGR", text)
        check(kernel is not None and (int(kernel.group(1)), int(kernel.group(2))) == mode, f"{label}: the kernel's console is that size", text)
    if FAILURES:
        sys.exit(f"bios edid: {len(FAILURES)} check(s) failed")
    print("bios edid: all checks passed")


if __name__ == "__main__":
    main()
