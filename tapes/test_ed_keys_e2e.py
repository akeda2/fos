#!/usr/bin/env python3
"""End-to-end test that cursor keys work in a tape on the *local* keyboard.

Boots the image, starts `ed` on a one-line file through the serial console,
then types with the QEMU monitor's `sendkey` (PS/2 scancodes, including the
extended Left/Home/Delete keys that the kernel turns into ANSI sequences) and
checks the edited line. The same keys over `term net` are covered by
test_ed_e2e.py."""

import os
import selectors
import socket
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOS_TEST_IMAGE", ROOT / "build" / "fos.img"))


def main():
    tmp = tempfile.mkdtemp(prefix="fos-ed-keys-")
    monitor = os.path.join(tmp, "mon")
    qemu = subprocess.Popen(
        ["qemu-system-i386", "-drive", f"format=raw,file={IMAGE}", "-serial", "stdio",
         "-monitor", f"unix:{monitor},server,nowait", "-display", "none", "-snapshot", "-nic", "none"],
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

    def wait_for(marker, timeout=20):
        end = time.monotonic() + timeout
        while marker not in output:
            if time.monotonic() > end:
                raise SystemExit(f"timed out waiting for {marker!r}:\n{output.decode('utf-8', 'replace')}")
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
        pump(0.25)

    def serial_line(text, marker):
        del output[:]
        qemu.stdin.write(text.encode() + b"\n")
        qemu.stdin.flush()
        wait_for(marker)

    try:
        wait_for(b"fos> ")
        serial_line("write k.txt hello", b"File written.")
        serial_line("tape load disk ed.bin", b"Loaded ")
        serial_line("tape run ed k.txt", b"ed> ")
        # A running tape reads the keyboard (and term net), not the serial port.
        del output[:]
        for key in ("e", "1", "ret"):
            sendkey(key)
        wait_for(b"edit: hello")
        # "hello": Home, Delete -> "ello"; type "J" -> "Jello"; End; Left; Backspace
        # -> "Jelo"; type "p" before the final "o" -> "Jelpo"
        for key in ("home", "delete", "shift-j", "end", "left", "backspace", "p", "ret"):
            sendkey(key)
        wait_for(b"ed> ")
        del output[:]
        for key in ("p", "ret"):
            sendkey(key)
        wait_for(b"ed> ")
        if b"1: Jelpo" not in output:
            raise SystemExit(f"local cursor keys did not edit the line:\n{output.decode('utf-8', 'replace')}")
        del output[:]
        for key in ("q", "ret"):
            sendkey(key)
        wait_for(b"fos> ")
        print("PASS: Home, Delete, Left, End and Backspace edit a line in a tape from the local keyboard.")
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
