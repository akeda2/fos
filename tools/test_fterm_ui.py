#!/usr/bin/env python3
"""The `fterm` settings panel (a bare `run fterm`), driven by keystrokes on the console, with a host-side peer on COM2.

Checks: the panel appears; Space cycles the transport and show its rows; an unfinished Telnet form is refused with a
reason; the baud rate changes with Backspace (arrow keys cannot be sent over the console UART: the kernel reads a raw ESC as abort); W saves `fterm.cfg`; C connects (typed text reaches the peer), Ctrl-A x
leaves and the panel returns; the panel reloads `fterm.cfg`; `run fterm @fterm.cfg` connects with the saved settings; Q quits.
Run with `make test-fterm-ui`."""
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
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, check, clean, find  # noqa: E402
import testslot  # noqa: E402

PEER_PORT = testslot.port(5873)


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image = tmp / "image.img"
        shutil.copy(IMAGE, image)
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-no-reboot",
                   "-serial", "stdio", "-serial", f"tcp:127.0.0.1:{PEER_PORT},server=on,wait=off",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={image}"]
        qemu = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        selector = selectors.DefaultSelector()
        selector.register(qemu.stdout, selectors.EVENT_READ)
        output = bytearray()
        peer = None
        received = bytearray()

        def pump(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                for key, _ in selector.select(0.02):
                    chunk = os.read(key.fd, 4096)
                    if chunk:
                        output.extend(chunk)
                if peer is not None:
                    try:
                        data = peer.recv(65536)
                        if data:
                            received.extend(data)
                    except BlockingIOError:
                        pass

        def text():
            return clean(bytes(output))

        def wait_for(marker, timeout=30, start=0):
            end = time.monotonic() + timeout
            while marker not in text()[start:]:
                if time.monotonic() > end or qemu.poll() is not None:
                    raise SystemExit(f"FAIL: timed out waiting for {marker!r}:\n{text()[-2500:]}")
                pump(0.1)

        def type_(data):
            qemu.stdin.write(data)
            qemu.stdin.flush()
            pump(0.4)

        try:
            wait_for("fos> ", 90)
            mark = len(text())
            type_(b"run fterm\r")
            wait_for("Connect to", 20, mark)
            shown = text()[mark:]
            check("Serial port" in shown and "Baud rate" in shown and "Data format" in shown, "the panel shows the serial rows", shown)
            check("1: COM2" in shown, "the serial port row names the port", shown)

            mark = len(text())
            type_(b" ")
            check("Telnet server" in text()[mark:] and "TCP port" in text()[mark:], "Space switches to Telnet and shows its rows", text()[mark:])
            mark = len(text())
            type_(b"c")
            check("bad server address" in text()[mark:], "an unfinished Telnet form is refused with a reason", text()[mark:])
            mark = len(text())
            type_(b" ")
            check("Machine (name or MAC)" in text()[mark:], "Space again shows the term net row", text()[mark:])
            type_(b" ")
            type_(b"\t")
            type_(b"\t")
            mark = len(text())
            type_(b"\x7f")
            check("57600" in text()[mark:], "Backspace on the baud row lowers it", text()[mark:])
            mark = len(text())
            type_(b"w")
            check("Saved fterm.cfg" in text()[mark:], "W saves fterm.cfg", text()[mark:])

            mark = len(text())
            type_(b"c")
            wait_for("Ctrl-A x exits", 20, mark)
            check("57600 baud" in text()[mark:], "C connects with the chosen settings", text()[mark:])
            deadline = time.monotonic() + 10
            while peer is None:
                try:
                    peer = socket.create_connection(("127.0.0.1", PEER_PORT), timeout=2)
                    peer.setblocking(False)
                except OSError:
                    if time.monotonic() > deadline:
                        raise SystemExit("FAIL: cannot reach COM2")
                    time.sleep(0.2)
            pump(0.5)
            type_(b"hi")
            pump(0.5)
            check(b"hi" in received, "typed text reaches the peer", received)
            mark = len(text())
            type_(b"\x01x")
            wait_for("Press a key to return", 10, mark)
            mark = len(text())
            type_(b"k")
            wait_for("Connect to", 10, mark)
            check("57600" in text()[mark:], "the panel returns, with the saved settings loaded again", text()[mark:])
            mark = len(text())
            type_(b"q")
            wait_for("fos> ", 10, mark)

            mark = len(text())
            type_(b"run fterm @fterm.cfg\r")
            wait_for("Ctrl-A x exits", 20, mark)
            check("57600 baud" in text()[mark:], "`run fterm @fterm.cfg` repeats the saved connection", text()[mark:])
            mark = len(text())
            type_(b"\x01x")
            wait_for("fos> ", 10, mark)
        finally:
            if peer is not None:
                peer.close()
            qemu.terminate()
            try:
                qemu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                qemu.kill()
    print("fterm-ui: all checks passed")


if __name__ == "__main__":
    main()
