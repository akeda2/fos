#!/usr/bin/env python3
"""The framebuffer console decodes UTF-8: Swedish letters and the euro sign are drawn with the font's own glyphs.

Boots the image with a second disk holding a UTF-8 file (the shell's own input is ASCII only, so the text has to come from
a file), takes a screendump, prints it with the `ed` tape (`cat` shows non-ASCII bytes as '.', a tape's output is raw), takes another and looks for cells that hold the Latin-9 glyphs of å ä ö Å Ä Ö and €, read from
`kernel/src/font_data.rs`. They must be absent before and present after, and the text must not have been drawn as '?'.
Run with `make test-console-utf8`."""

import os
import re
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

TEXT = "åäöÅÄÖ€"
LATIN9 = bytes([0xE5, 0xE4, 0xF6, 0xC5, 0xC4, 0xD6, 0xA4])
LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
FOREGROUND, BACKGROUND = (0xC0, 0xC0, 0xC0), (0x00, 0x00, 0x00)


def font():
    rows = re.findall(r"\[((?:0x[0-9a-f]{2},? ?){16})\]", (ROOT / "kernel/src/font_data.rs").read_text())
    assert len(rows) == 256
    return [[int(v, 16) for v in row.replace(" ", "").split(",") if v] for row in rows]


def cells_with(shot, glyph):
    """How many text cells of the screendump are exactly `glyph` (8x16, rows as bit masks)."""
    width, height, pixel = read_ppm(shot)
    found = 0
    for row in range(height // 16):
        for column in range(width // 8):
            if all(((pixel(column * 8 + bit, row * 16 + line) == FOREGROUND) == bool(glyph[line] & (0x80 >> bit)))
                   for line in range(16) for bit in range(8)):
                found += 1
    return found


def columns_with(shot, glyph):
    """The text columns of the screendump holding `glyph`."""
    width, height, pixel = read_ppm(shot)
    return {column for row in range(height // 16) for column in range(width // 8)
            if all(((pixel(column * 8 + bit, row * 16 + line) == FOREGROUND) == bool(glyph[line] & (0x80 >> bit)))
                   for line in range(16) for bit in range(8))}


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    glyphs = font()
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image = tmp / "image.img"
        shutil.copy(IMAGE, image)
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        # Escape sequences the console does not draw (a window title, DEC private mode, a character set, a CSI colour) must be
        # swallowed whole; 'Z' appears nowhere else on the screen and the 80 digits would show.
        (tmp / "u.txt").write_text("\x1b]0;ZZZ\x07\x1b[?" + "8" * 40 + "h\x1b(B\x1b[>" + "8" * 40 + "c\x1b]0;ZZZ\x1b\\\t" + TEXT + "\n", encoding="utf-8")
        second = tmp / "second.img"
        subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
                        "--output", str(second), "--label", "utf8", "--payload-guid", "f05b2a00-0000-4f05-8000-0000000000aa",
                        "--file", f"u.txt={tmp / 'u.txt'}"], check=True, capture_output=True)
        monitor = str(tmp / "monitor")
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
                   "-monitor", f"unix:{monitor},server,nowait", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-device", "ich9-ahci,id=ahci",
                   "-drive", f"if=none,id=d1,format=raw,file={image}", "-device", "ide-hd,drive=d1,bus=ahci.0,bootindex=0",
                   "-drive", f"if=none,id=d2,format=raw,file={second}", "-device", "ide-hd,drive=d2,bus=ahci.1"]
        qemu = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
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

        def wait_for(marker, timeout=60):
            end = time.monotonic() + timeout
            while marker not in clean(bytes(output)):
                if time.monotonic() > end or qemu.poll() is not None:
                    raise SystemExit(f"FAIL: timed out waiting for {marker!r}:\n{clean(bytes(output))[-2000:]}")
                pump(0.1)

        def screendump(name):
            path = tmp / name
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                sock.settimeout(2)
                sock.connect(monitor)
                sock.sendall(f"screendump {path}\n".encode())
            pump(1.0)
            return path

        try:
            wait_for("M3 complete")
            wait_for("fos> ")
            before = screendump("before.ppm")
            del output[:]
            qemu.stdin.write(b"run ed @utf8/u.txt\n")
            qemu.stdin.flush()
            wait_for("ed> ")
            qemu.stdin.write(b"p\n")
            qemu.stdin.flush()
            wait_for("1: ")
            pump(1.0)
            after = screendump("after.ppm")
            for character, code_point in zip(TEXT, LATIN9):
                glyph = glyphs[code_point]
                check(cells_with(before, glyph) == 0 and cells_with(after, glyph) > 0,
                      f"'{character}' is drawn with glyph 0x{code_point:02x}", "")
            check(columns_with(after, glyphs[LATIN9[0]]) == {8}, "a tab moves to the next multiple of 8 columns", "")
            check(cells_with(after, glyphs[ord("?")]) == 0, "and none of it is drawn as '?'", "")
            check(cells_with(after, glyphs[ord("Z")]) == 0
                  and cells_with(after, glyphs[ord("8")]) < cells_with(before, glyphs[ord("8")]) + 20,
                  "OSC, private CSI and charset sequences are swallowed, not printed", "")
        finally:
            qemu.kill()
            qemu.wait()
    print("console utf-8: all checks passed")


if __name__ == "__main__":
    main()
