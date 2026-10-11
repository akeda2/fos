#!/usr/bin/env python3
"""The `fterm` serial terminal tape, against a host-side peer on the machine's second UART (COM2).

The shell is driven over COM1 (stdio), as in the other tests. QEMU's second `-serial` is a TCP socket the test connects to and
plays the device on the other end of the cable. Checks: `fterm list` names COM2 (and not the console COM1); text from the peer is
shown; typed text, Ctrl-C and Escape reach the peer unchanged and Enter goes out as CR; a burst arrives complete, with no
overruns in the status line; Ctrl-A commands work (status, local echo, CRLF mode, a literal Ctrl-A); a bad port is refused; Ctrl-A x
returns to a usable shell. Run with `make test-fterm`."""
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

PEER_PORT = testslot.port(5871)


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

        def wait_for(marker, timeout=60, start=0):
            end = time.monotonic() + timeout
            while marker not in text()[start:]:
                if time.monotonic() > end or qemu.poll() is not None:
                    raise SystemExit(f"FAIL: timed out waiting for {marker!r}:\n{text()[-2500:]}")
                pump(0.1)

        def wait_peer(marker, timeout=10):
            end = time.monotonic() + timeout
            while marker not in received:
                if time.monotonic() > end:
                    return False
                pump(0.05)
            return True

        def type_(data):
            qemu.stdin.write(data)
            qemu.stdin.flush()
            pump(0.3)

        try:
            wait_for("fos> ", 90)
            mark = len(text())
            type_(b"run fterm list\r")
            wait_for("fos> ", 20, mark)
            listing = text()[mark:]
            check("COM2" in listing and "COM1" not in listing, "`fterm list` offers COM2 and not the console", listing)

            mark = len(text())
            type_(b"run fterm 9\r")
            wait_for("fos> ", 20, mark)
            check("no serial port 9" in text()[mark:], "a port that does not exist is refused", text()[mark:])

            mark = len(text())
            type_(b"run fterm 1 9600 8n1\r")
            wait_for("Ctrl-A x exits", 20, mark)
            deadline = time.monotonic() + 10
            while peer is None:
                try:
                    peer = socket.create_connection(("127.0.0.1", PEER_PORT), timeout=2)
                    peer.setblocking(False)
                except OSError:
                    if time.monotonic() > deadline:
                        raise SystemExit("FAIL: could not connect to the QEMU COM2 socket")
                    time.sleep(0.2)
            pump(0.5)

            mark = len(text())
            peer.sendall(b"hello from the device\r\n")
            wait_for("hello from the device", 10, mark)
            check(True, "text from the peer is shown on the console", "")

            type_(b"ping\r")
            check(wait_peer(b"ping\r"), "typed text goes out, Enter as CR", bytes(received))
            received.clear()
            type_(b"\x03")
            check(wait_peer(b"\x03"), "Ctrl-C reaches the peer (it is not an abort)", bytes(received))
            received.clear()
            type_(b"\x1b")
            check(wait_peer(b"\x1b"), "Escape reaches the peer as ESC", bytes(received))
            received.clear()

            burst = "".join(f"line {number:04d} of the burst\n" for number in range(120)).encode()
            mark = len(text())
            peer.sendall(burst)
            wait_for("line 0119 of the burst", 30, mark)
            shown = text()[mark:]
            check(all(f"line {number:04d} of the burst" in shown for number in range(120)), f"a {len(burst)}-byte burst arrives complete", shown[-400:])

            mark = len(text())
            type_(b"\x01s")
            wait_for("lines:", 10, mark)
            status = text()[mark:]
            check("overrun 0" in status and "9600 baud 8n1" in status, "Ctrl-A s: settings shown, no overruns", status)

            type_(b"\x01e")
            wait_for("[echo on]", 10)
            received.clear()
            mark = len(text())
            type_(b"x\r")
            check(wait_peer(b"x\r"), "with echo on the characters still go out", bytes(received))
            check("x\n" in text()[mark:], "and are shown locally", text()[mark:])
            type_(b"\x01e")
            wait_for("[echo off]", 10)

            type_(b"\x01l")
            wait_for("[crlf on]", 10)
            received.clear()
            type_(b"\r")
            check(wait_peer(b"\r\n"), "CRLF mode sends CR LF for Enter", bytes(received))
            type_(b"\x01l")
            wait_for("[crlf off]", 10)

            received.clear()
            type_(b"\x01a")
            check(wait_peer(b"\x01"), "Ctrl-A a sends a literal Ctrl-A", bytes(received))

            mark = len(text())
            type_(b"\x01x")
            wait_for("fos> ", 20, mark)
            check("fterm: closed." in text()[mark:], "Ctrl-A x leaves the tape", text()[mark:])
            mark = len(text())
            type_(b"uptime\r")
            wait_for("ticks at 100 Hz", 20, mark)
            check(True, "and the shell is still usable", "")
        finally:
            if peer is not None:
                peer.close()
            qemu.terminate()
            try:
                qemu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                qemu.kill()
    print("fterm: all checks passed")


if __name__ == "__main__":
    main()
