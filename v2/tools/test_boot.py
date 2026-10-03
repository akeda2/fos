#!/usr/bin/env python3
"""Boots the v2 image under QEMU + OVMF and checks the whole chain (milestones M0
and M1): the loader narrates the firmware and finds the payload partition, reads
the kernel from its A/B slot, loads it as a position-independent ELF and leaves the
firmware; the 64-bit kernel then reports its state over serial, and draws on the
framebuffer, which the test verifies from a QEMU screendump (three colour bars
drawn in logical red, green and blue, so a wrong pixel format would show).

OVMF mirrors the firmware console to the serial port, so loader and kernel
narration arrive on the same `-serial` stream (escape sequences stripped before
matching). Firmware images come from the usual places or FOS_OVMF_CODE /
FOS_OVMF_VARS. Run with `make test-v2`."""

import os
import re
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
IMAGE = Path(os.environ.get("FOS_V2_IMAGE", ROOT / "build" / "v2" / "v2.img"))
CODE_CANDIDATES = ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                   "/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd"]
VARS_CANDIDATES = ["/usr/share/OVMF/OVMF_VARS_4M.fd", "/usr/share/OVMF/OVMF_VARS.fd",
                   "/usr/share/edk2/ovmf/OVMF_VARS.fd", "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"]
FINAL_MARKER = "M1 complete"


def find(variable, candidates):
    explicit = os.environ.get(variable)
    for path in ([explicit] if explicit else []) + candidates:
        if path and Path(path).exists():
            return path
    raise SystemExit(f"no OVMF firmware image found (set {variable}); tried {candidates}")


def check(condition, message, text=""):
    if not condition:
        raise SystemExit(f"FAIL: {message}\n--- output ---\n{text[-3500:]}")
    print(f"PASS: {message}")


def clean(raw: bytes) -> str:
    return re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", raw.decode("utf-8", "replace")).replace("\r", "")


def monitor(path: str, command: str):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(5)
        for _ in range(100):
            try:
                sock.connect(path)
                break
            except OSError:
                time.sleep(0.05)
        sock.sendall(command.encode() + b"\n")
        time.sleep(0.5)


def read_ppm(path: Path):
    data = path.read_bytes()
    match = re.match(rb"P6\s+(\d+)\s+(\d+)\s+(\d+)\s", data)
    width, height = int(match.group(1)), int(match.group(2))
    pixels = data[match.end():]

    def pixel(x, y):
        offset = (y * width + x) * 3
        return tuple(pixels[offset:offset + 3])

    return width, height, pixel


def main():
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} missing; run `make v2-image` first")
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        monitor_path = str(Path(tmp) / "monitor")
        shot = Path(tmp) / "screen.ppm"
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none",
                   "-serial", "stdio", "-monitor", f"unix:{monitor_path},server,nowait", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}",
                   "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={IMAGE}"]
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 90
        try:
            while FINAL_MARKER not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: the kernel never reported '{FINAL_MARKER}'\n{clean(bytes(raw))[-3500:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))
            time.sleep(0.5)
            text = clean(bytes(raw))
            monitor(monitor_path, f"screendump {shot}")
            for _ in range(50):
                if shot.exists() and shot.stat().st_size > 1000:
                    break
                time.sleep(0.1)
            time.sleep(0.3)
            width, height, pixel = read_ppm(shot) if shot.exists() else (0, 0, None)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()

    # --- the loader (M0 + M1) ---
    check("FOS v2 loader (milestone M1): UEFI application running." in text, "OVMF boots BOOTX64.EFI from the ESP", text)
    check(re.search(r"UEFI specification \d+\.\d+", text) is not None, "the loader reports the UEFI version", text)
    entries = re.search(r"Memory map: (\d+) entries", text)
    check(entries is not None and int(entries.group(1)) > 10, "it reads the memory map", text)
    reusable = re.search(r"Reusable after ExitBootServices: (\d+) MiB", text)
    check(reusable is not None and 300 < int(reusable.group(1)) < 520, f"most of the 512 MiB guest is reusable ({reusable and reusable.group(1)} MiB)", text)
    check(re.search(r"Graphics: \d+x\d+ (RGB|BGR)", text) is not None, "it finds the GOP framebuffer", text)
    check("ACPI 2.0+ RSDP at" in text and "signature valid" in text, "and a valid ACPI 2.0+ RSDP", text)
    check(re.search(r"Payload partition: LBA \d+ \+ \d+ sectors", text) is not None, "it finds the payload partition by its GPT type", text)
    check(re.search(r"Slot metadata: active slot 0, \d+ sectors", text) is not None, "reads the slot metadata", text)
    kernel = re.search(r"Kernel at (0x[0-9a-f]+), (\d+) KiB, entry (0x[0-9a-f]+)", text)
    check(kernel is not None, "loads the kernel ELF at an address of its own choosing", text)
    check("Exiting boot services" in text, "and exits boot services", text)

    # --- the kernel (M1) ---
    check("FOS v2 kernel (milestone M1): running in 64-bit long mode." in text, "the kernel starts in long mode", text)
    check(re.search(r"BootInfo: version 4, kernel at " + kernel.group(1), text) is not None, "BootInfo arrives intact and names the same load address", text)
    check(re.search(r"Memory map: \d+ entries, \d+ MiB of conventional", text) is not None, "the kernel reads the memory map", text)
    check("EFER" in text and "LME 1, LMA 1" in text, "EFER confirms long mode is active", text)
    check("GDT loaded: kernel code selector 0x8, data 0x10." in text, "the kernel installs its own GDT", text)
    check(re.search(r"Paging: identity map of \d+ MiB RAM", text) is not None, "and its own identity-mapped page tables", text)
    check(re.search(r"Framebuffer console ready: \d+ columns x \d+ rows", text) is not None, "the framebuffer console comes up", text)

    # --- the framebuffer, as QEMU displays it ---
    check(pixel is not None and width >= 640, f"QEMU produced a screendump ({width}x{height})", text)
    bar_y = height - 48 + 16
    check(pixel(32, bar_y) == (255, 0, 0), f"red bar is red {pixel(32, bar_y)}", text)
    check(pixel(112, bar_y) == (0, 255, 0), f"green bar is green {pixel(112, bar_y)}", text)
    check(pixel(192, bar_y) == (0, 0, 255), f"blue bar is blue {pixel(192, bar_y)}", text)
    check(pixel(width - 10, height // 2) == (0, 0, 0x20), f"the background fill is the console colour {pixel(width - 10, height // 2)}", text)
    lit = sum(1 for y in range(0, 16 * 12) for x in range(0, 8 * 60) if pixel(x, y) == (0xc0, 0xc0, 0xc0))
    check(lit > 400, f"text is drawn on screen ({lit} foreground pixels in the first rows)", text)
    print("v2 boot (M0+M1): all checks passed")


if __name__ == "__main__":
    main()
