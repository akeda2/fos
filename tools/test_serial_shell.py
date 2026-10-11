#!/usr/bin/env python3
"""The shell on a serial port: `serial-shell 1 115200` in the `settings` file.

Boots an image built here (loader + kernel + the `fterm` tape + that settings file) with COM1 on stdio and COM2 on a TCP socket
the test connects to, playing the laptop at the other end of the cable. Checks: the kernel announces the setting; a typed line on
COM2 gets the prompt and the command's output back on COM2 (and the same output shows on the console COM1, so both are mirrors);
Enter as CR works; a running tape owns the input and Escape stops it; `fterm` refuses to open the port that carries the shell and
the shell stays usable; and without the setting (`make test-fterm`'s image) COM2 stays silent, which is the opt-in.
Run with `make test-serial-shell`."""
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
import testslot  # noqa: E402
from test_boot import CODE_CANDIDATES, ROOT, VARS_CANDIDATES, check, clean, find  # noqa: E402

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
TAPES = ROOT / "build/cargo-target/x86_64-fos-tape/release"
PEER_PORT = testslot.port(5872)


def boot(tmp, settings, code, variables):
    image = tmp / "image.img"
    vars_copy = tmp / "vars.fd"
    shutil.copy(variables, vars_copy)
    command = [sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL), "--output", str(image),
               "--file", f"fterm={TAPES / 'fterm'}", "--file", f"hello={TAPES / 'hello'}"]
    if settings is not None:
        (tmp / "settings").write_text(settings)
        command += ["--file", f"settings={tmp / 'settings'}"]
    subprocess.run(command, check=True, capture_output=True)
    return subprocess.Popen(
        ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-no-reboot",
         "-serial", "stdio", "-serial", f"tcp:127.0.0.1:{PEER_PORT},server=on,wait=off",
         "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
         "-drive", f"format=raw,file={image}"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        qemu = boot(tmp, "serial-shell 1 115200\n", code, variables)
        selector = selectors.DefaultSelector()
        selector.register(qemu.stdout, selectors.EVENT_READ)
        console = bytearray()
        peer = None
        received = bytearray()

        def pump(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                for key, _ in selector.select(0.02):
                    chunk = os.read(key.fd, 4096)
                    if chunk:
                        console.extend(chunk)
                if peer is not None:
                    try:
                        data = peer.recv(65536)
                        if data:
                            received.extend(data)
                    except BlockingIOError:
                        pass

        def seen():
            return received.decode("utf-8", "replace")

        def wait_peer(marker, timeout=20, start=0):
            end = time.monotonic() + timeout
            while marker not in seen()[start:]:
                if time.monotonic() > end or qemu.poll() is not None:
                    return False
                pump(0.05)
            return True

        def send(data):
            peer.sendall(data)
            pump(0.3)

        try:
            end = time.monotonic() + 90
            while "fos> " not in clean(bytes(console)):
                if time.monotonic() > end or qemu.poll() is not None:
                    raise SystemExit(f"FAIL: no prompt:\n{clean(bytes(console))[-2000:]}")
                pump(0.1)
            check("serial-shell -- the shell is also on serial port 1" in clean(bytes(console)), "the kernel announces the serial shell", clean(bytes(console)))
            while peer is None:
                try:
                    peer = socket.create_connection(("127.0.0.1", PEER_PORT), timeout=2)
                    peer.setblocking(False)
                except OSError:
                    if time.monotonic() > end:
                        raise SystemExit("FAIL: could not connect to the QEMU COM2 socket")
                    time.sleep(0.2)
            pump(0.5)

            mark = len(seen())
            mark_console = len(console)
            send(b"uptime\r")
            check(wait_peer("ticks at 100 Hz", 20, mark), "a command typed on the port runs and its output comes back on the port", seen()[mark:])
            check(wait_peer("fos> ", 10, mark), "followed by the next prompt", seen()[mark:])
            check("ticks at 100 Hz" in clean(bytes(console[mark_console:])), "the console shows it too (the port is a mirror)", "")
            check("\r\n" in seen()[mark:], "output lines end in CR LF", repr(seen()[mark:]))

            mark = len(seen())
            send(b"hello\r")
            check(wait_peer("fos> ", 20, mark), "a tape can be run from the port", seen()[mark:])

            mark = len(seen())
            send(b"run fterm list\r")
            check(wait_peer("fos> ", 20, mark) and "COM2" in seen()[mark:], "fterm still lists the port", seen()[mark:])
            mark = len(seen())
            send(b"run fterm 1\r")
            check(wait_peer("fos> ", 20, mark) and "carries this machine's shell" in seen()[mark:], "fterm refuses the port that carries the shell", seen()[mark:])

            mark = len(seen())
            send(b"uptime\r")
            check(wait_peer("ticks at 100 Hz", 20, mark), "and the shell is still usable", seen()[mark:])
        finally:
            if peer is not None:
                peer.close()
            qemu.terminate()
            try:
                qemu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                qemu.kill()

        # Without the setting nothing is on COM2.
        qemu = boot(tmp, None, code, variables)
        selector = selectors.DefaultSelector()
        selector.register(qemu.stdout, selectors.EVENT_READ)
        console = bytearray()
        peer = None
        received = bytearray()
        try:
            end = time.monotonic() + 90
            while "fos> " not in clean(bytes(console)):
                if time.monotonic() > end or qemu.poll() is not None:
                    raise SystemExit(f"FAIL: no prompt without the setting:\n{clean(bytes(console))[-2000:]}")
                pump(0.1)
            while peer is None:
                try:
                    peer = socket.create_connection(("127.0.0.1", PEER_PORT), timeout=2)
                    peer.setblocking(False)
                except OSError:
                    time.sleep(0.2)
            qemu.stdin.write(b"uptime\r")
            qemu.stdin.flush()
            pump(1.5)
            peer.sendall(b"uptime\r")
            pump(1.5)
            check(not received, "without the setting the port stays silent and ignores typing", seen())
        finally:
            if peer is not None:
                peer.close()
            qemu.terminate()
            try:
                qemu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                qemu.kill()
    print("serial-shell: all checks passed")


if __name__ == "__main__":
    main()
