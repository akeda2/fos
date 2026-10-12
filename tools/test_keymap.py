#!/usr/bin/env python3
"""`keymap sv`: the Swedish layout on a PS/2 keyboard, seen through `fterm`.

QEMU's `sendkey` names keys by their US caps: `bracket_left`, `apostrophe` and `semicolon` are the keys right of P and L, where a
Swedish keyboard has å, ä and ö. The shell reports the layout (US by default; `keymap xx` is refused). With the US layout those
keys send `[`, `'`, `;` over `fterm`'s serial port (COM2, a host-side peer); after `keymap sv` the peer receives UTF-8 å, ä, ö,
Å (Shift), `{` (AltGr+7) and `-` (the key at the US `/` position). Run with `make test-keymap`."""
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

PEER_PORT = testslot.port(5872)


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image = tmp / "image.img"
        shutil.copy(IMAGE, image)
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        monitor = str(tmp / "monitor")
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-no-reboot",
                   "-serial", "stdio", "-monitor", f"unix:{monitor},server,nowait", "-serial", f"tcp:127.0.0.1:{PEER_PORT},server=on,wait=off",
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

        def key(name):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                sock.settimeout(2)
                sock.connect(monitor)
                sock.sendall(f"sendkey {name}\n".encode())
            pump(0.4)

        try:
            wait_for("fos> ", 90)
            for line, expected in (("keymap", "keymap: us"), ("keymap xx", "unknown layout"), ("keymap sv", "keymap: sv.")):
                mark = len(text())
                type_(line.encode() + b"\r")
                wait_for(expected, 10, mark)
                wait_for("fos> ", 10, mark)
            check(True, "the shell reports, refuses and switches layouts", "")

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

            for name in ("bracket_left", "apostrophe", "semicolon", "shift-bracket_left", "alt_r-7", "slash"):
                key(name)
            check(wait_peer("åäöÅ{-".encode()), "Swedish layout: å ä ö Å { - reach the peer as UTF-8", bytes(received))

            type_(b"\x01x")
            wait_for("fos> ", 20, mark)
        finally:
            if peer is not None:
                peer.close()
            qemu.terminate()
            try:
                qemu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                qemu.kill()
    print("keymap: all checks passed")


if __name__ == "__main__":
    main()
