#!/usr/bin/env python3
"""`ebb`, the full-screen editor tape, driven from the local keyboard (QEMU `sendkey`).

Checks typing, saving (Ctrl-S) and reading the file back, loading an existing file, undo, the
two-step quit of a modified buffer, and -- as the permanent test of the kernel console's SGR
support -- that the status bar is drawn in reverse video (a light band on screen) and that the
console's colours are back to normal after the tape ends.

Run with `make test-ebb`."""

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
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, check, clean, find, read_ppm  # noqa: E402


def light_rows(path):
    """Pixel rows that are mostly light: a reverse-video bar."""
    width, height, pixel = read_ppm(path)
    rows = []
    for y in range(height):
        light = sum(1 for x in range(0, width, 4) if sum(pixel(x, y)) > 450)
        if light > (width // 4) * 0.6:
            rows.append(y)
    return rows


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        boot = tmp / "boot.img"
        shutil.copy(IMAGE, boot)
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        monitor = str(tmp / "monitor")
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
                   "-monitor", f"unix:{monitor},server,nowait", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-device", "ich9-ahci,id=ahci",
                   "-drive", f"if=none,id=d1,format=raw,file={boot}", "-device", "ide-hd,drive=d1,bus=ahci.0,bootindex=0"]
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

        def text():
            return clean(bytes(output))

        def wait_for(marker, timeout=40):
            end = time.monotonic() + timeout
            while marker not in text():
                if time.monotonic() > end or qemu.poll() is not None:
                    raise SystemExit(f"FAIL: timed out waiting for {marker!r}:\n{text()[-2500:]}")
                pump(0.1)

        def monitor_command(line):
            for _ in range(100):
                try:
                    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                        sock.settimeout(2)
                        sock.connect(monitor)
                        sock.sendall(f"{line}\n".encode())
                    return
                except (FileNotFoundError, ConnectionRefusedError, TimeoutError):
                    time.sleep(0.05)

        def keys(*names):
            for name in names:
                monitor_command(f"sendkey {name}")
                pump(0.2)

        def typing(word):
            for letter in word:
                keys({" ": "spc", "-": "minus"}.get(letter, letter))

        def serial_line(line, marker):
            del output[:]
            qemu.stdin.write(line.encode() + b"\n")
            qemu.stdin.flush()
            wait_for(marker)

        def screendump(name):
            path = tmp / name
            monitor_command(f"screendump {path}")
            pump(1.0)
            return path

        try:
            wait_for("M3 complete")
            wait_for("fos> ")
            serial_line("write old.txt alpha", "Wrote 5 bytes")
            serial_line("run ebb old.txt", "run ebb old.txt")
            pump(1.5)
            shot = screendump("editing.ppm")
            bar = light_rows(shot)
            check(len(bar) >= 8, "the status bar is drawn in reverse video (SGR 7)", f"light rows: {bar}")
            # Open file "alpha": go to the end, add text, a second line, save.
            keys("end")
            typing("-beta")
            keys("ret")
            typing("gamma")
            keys("ctrl-s")
            pump(1.0)
            keys("ctrl-q")
            wait_for("fos> ", 20)
            serial_line("cat old.txt", "gamma")
            check("alpha-beta" in text() and "gamma" in text(), "a loaded file is edited and saved with Ctrl-S", text()[-500:])
            after = light_rows(screendump("after.ppm"))
            check(len(after) < 8, "the console colours are back to normal after the tape ends", f"light rows: {after}")

            # A new file, undo, and the two-step quit of a modified buffer.
            serial_line("run ebb new.txt", "run ebb new.txt")
            pump(1.0)
            typing("one")
            keys("ctrl-s")
            pump(1.0)
            typing("two")
            keys("ctrl-z")
            del output[:]
            keys("ctrl-q")           # the buffer is modified: first Ctrl-Q only warns
            pump(1.0)
            check(qemu.poll() is None and "fos> " not in text(), "Ctrl-Q on a modified buffer does not quit at once", text()[-500:])
            keys("ctrl-q")
            wait_for("fos> ", 20)
            serial_line("cat new.txt", "one")
            check("one" in text() and "two" not in text(), "a new file is created by Ctrl-S; undo removed the later text", text()[-500:])
        finally:
            qemu.terminate()
            try:
                qemu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                qemu.kill()
    print("ebb: all checks passed")


if __name__ == "__main__":
    main()
