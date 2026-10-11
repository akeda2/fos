#!/usr/bin/env python3
"""The `foreground` and `background` lines of the `settings` file colour the pixel console.

Boots the kernel twice from images built here (loader + kernel + a `settings` file): once without colour lines (light grey
on black must be on screen) and once with `background blue` / `foreground amber` (the screen must be blue with amber text,
and no grey left, so the whole screen was redrawn). The check reads a QEMU screendump.
Run with `make test-console-colors`."""

import os
import selectors
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import CODE_CANDIDATES, IMAGE, ROOT, VARS_CANDIDATES, check, clean, find, read_ppm  # noqa: E402

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
GREY, BLACK, BLUE, AMBER = (0xC0, 0xC0, 0xC0), (0, 0, 0), (0, 0, 0xAA), (0xFF, 0xB0, 0x00)


def colours(shot):
    width, height, pixel = read_ppm(shot)
    seen = set()
    for y in range(0, height - 64, 2):  # above the test bars at the bottom edge
        for x in range(width):
            seen.add(pixel(x, y))
    return seen, pixel(width - 1, 0)


def boot_and_dump(tmp, name, settings, code, variables):
    image = tmp / f"{name}.img"
    vars_copy = tmp / f"{name}.fd"
    shutil.copy(variables, vars_copy)
    command = [sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
               "--output", str(image)]
    if settings is not None:
        (tmp / f"{name}.settings").write_text(settings)
        command += ["--file", f"settings={tmp / f'{name}.settings'}"]
    subprocess.run(command, check=True, capture_output=True)
    monitor = str(tmp / f"{name}.monitor")
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
         "-monitor", f"unix:{monitor},server,nowait", "-no-reboot",
         "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
         "-device", "ich9-ahci,id=ahci", "-drive", f"if=none,id=d1,format=raw,snapshot=on,file={image}",
         "-device", "ide-hd,drive=d1,bus=ahci.0,bootindex=0"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    selector = selectors.DefaultSelector()
    selector.register(qemu.stdout, selectors.EVENT_READ)
    output = bytearray()

    def pump(seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            for key, _ in selector.select(0.02):
                chunk = os.read(key.fd, 4096)
                if chunk:
                    output.extend(chunk)

    try:
        end = time.monotonic() + 60
        while "fos> " not in clean(bytes(output)):
            if time.monotonic() > end or qemu.poll() is not None:
                raise SystemExit(f"FAIL: no prompt:\n{clean(bytes(output))[-2000:]}")
            pump(0.1)
        pump(1.0)
        path = tmp / f"{name}.ppm"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(2)
            sock.connect(monitor)
            sock.sendall(f"screendump {path}\n".encode())
        pump(1.0)
        return colours(path), clean(bytes(output))
    finally:
        qemu.kill()
        qemu.wait()


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (seen, corner), _ = boot_and_dump(tmp, "default", None, code, variables)
        check(corner == BLACK and GREY in seen, "default: light grey text on black", f"{corner} {GREY in seen}")
        (seen, corner), log = boot_and_dump(tmp, "coloured", "background blue\nforeground amber\n", code, variables)
        check(corner == BLUE, "background blue: the screen is blue", str(corner))
        check(AMBER in seen and GREY not in seen, "foreground amber: amber text, no grey left", f"{AMBER in seen} {GREY in seen}")
        check("settings: background blue." in log, "the settings line is narrated", log[-500:])
        (seen, corner), log = boot_and_dump(tmp, "bad", "background plaid\n", code, variables)
        check(corner == BLACK and "ignored" in log, "an unknown colour is ignored and said so", log[-500:])
    print("console colors: all checks passed")


if __name__ == "__main__":
    main()
