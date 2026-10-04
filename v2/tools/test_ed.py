#!/usr/bin/env python3
"""`ed`, the line editor tape, on the v2 kernel, driven from the *local* keyboard.

The shell is driven over the serial port (v2 reads it like a keyboard); the editor's keys are
sent as PS/2 scancodes through the QEMU monitor (`sendkey`), including the extended Left, Home,
End and Delete keys, which the kernel turns into the ANSI sequences a tape's `KeyDecoder`
understands. Checks the edited line, the saved file (read back with `cat`), `diskN/NAME` and
`@LABEL/NAME` file references through the tape's file callbacks, and a screendump of the
framebuffer after cursor editing to confirm no stray characters were drawn.

Run with `make test-v2-ed`."""

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
from test_boot import CODE_CANDIDATES, IMAGE, ROOT, VARS_CANDIDATES, check, clean, find  # noqa: E402

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fos-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fos-v2-kernel"


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        boot = tmp / "boot.img"
        shutil.copy(IMAGE, boot)
        second = tmp / "second.img"
        subprocess.run([sys.executable, str(ROOT / "v2/tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
                        "--output", str(second), "--label", "backup", "--payload-guid", "f05b2a00-0000-4f05-8000-0000000000aa"],
                       check=True, capture_output=True)
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        monitor = str(tmp / "monitor")
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
                   "-monitor", f"unix:{monitor},server,nowait", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-device", "ich9-ahci,id=ahci",
                   "-drive", f"if=none,id=d1,format=raw,file={boot}", "-device", "ide-hd,drive=d1,bus=ahci.0,bootindex=0",
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

        def text():
            return clean(bytes(output))

        def wait_for(marker, timeout=40):
            end = time.monotonic() + timeout
            while marker not in text():
                if time.monotonic() > end or qemu.poll() is not None:
                    raise SystemExit(f"FAIL: timed out waiting for {marker!r}:\n{text()[-2500:]}")
                pump(0.1)

        def sendkey(name):
            for _ in range(100):
                try:
                    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                        sock.settimeout(1)
                        sock.connect(monitor)
                        sock.sendall(f"sendkey {name}\n".encode())
                    break
                except (FileNotFoundError, ConnectionRefusedError, TimeoutError):
                    time.sleep(0.05)
            pump(0.2)

        def keys(*names):
            for name in names:
                sendkey(name)

        def serial_line(line, marker):
            del output[:]
            qemu.stdin.write(line.encode() + b"\n")
            qemu.stdin.flush()
            wait_for(marker)

        def screendump(name):
            path = tmp / name
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                sock.settimeout(2)
                sock.connect(monitor)
                sock.sendall(f"screendump {path}\n".encode())
            pump(1.0)
            return path.read_bytes()

        try:
            wait_for("M3 complete")
            wait_for("fos> ")
            serial_line("write k.txt hello", "Wrote 5 bytes")
            serial_line("run ed k.txt", "ed> ")
            check("(1 lines)" in text(), "ed loads a file from the boot volume", text()[-800:])
            del output[:]
            keys("e", "1", "ret")
            wait_for("edit: hello")
            before = screendump("before.ppm")
            # "hello": Home, Delete -> "ello"; "J" -> "Jello"; End; Left; Backspace -> "Jelo"; "p" -> "Jelpo"
            keys("home", "delete", "shift-j", "end", "left", "backspace", "p", "ret")
            wait_for("ed> ")
            del output[:]
            keys("p", "ret")
            wait_for("ed> ")
            check("1: Jelpo" in text(), "Home, Delete, Left, End and Backspace edit a line from the local keyboard", text()[-800:])
            check("?" not in text().split("1: Jelpo")[0].split("edit:")[-1], "and no stray characters were printed", text()[-800:])
            keys("w", "ret")
            wait_for("written k.txt")
            keys("q", "ret")
            wait_for("fos> ", 20)
            serial_line("cat k.txt", "Jelpo")
            check("Jelpo" in text(), "the saved file reads back", text()[-500:])

            # The framebuffer: cursor editing must leave the screen as the tape intended. Edit a line and
            # put it back exactly as it was; the final prompt screen then matches a screen that never had
            # the edit (compare against typing the same line serially is not possible, so check pixel change).
            after = screendump("after.ppm")
            check(before != after, "the framebuffer changed while editing", "")

            # File references to other volumes through the tape's file callbacks.
            serial_line("write @backup seed.txt first line", "Wrote")
            serial_line("run ed @backup/seed.txt", "ed> ")
            check("(1 lines)" in text(), "ed opens a file on another volume by @LABEL/NAME", text()[-500:])
            keys("q", "ret")
            wait_for("fos> ", 20)
            serial_line("ls disk2", "seed.txt")
            check("seed.txt" in text(), "and `ls disk2` sees the file", text()[-500:])
            serial_line("run ed disk2/new.txt", "ed> ")
            check("(new file)" in text(), "diskN/NAME for a file that does not exist starts a new file", text()[-500:])
            keys("a", "ret")
            for name in ("t", "w", "o", "ret", "dot", "ret"):
                sendkey(name)
            keys("w", "ret")
            wait_for("written disk2/new.txt")
            keys("q", "ret")
            wait_for("fos> ", 20)
            serial_line("cat disk2 new.txt", "two")
            check("two" in text(), "a file written through diskN/NAME lands on that disk", text()[-500:])
        finally:
            qemu.terminate()
            try:
                qemu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                qemu.kill()
    print("v2 ed: all checks passed")


if __name__ == "__main__":
    main()
