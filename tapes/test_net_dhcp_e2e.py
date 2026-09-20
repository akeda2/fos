#!/usr/bin/env python3
"""End-to-end DHCP test for the all-NIC diagnostic tape.

QEMU's two user-mode networks each provide a private DHCP server.  The test
therefore checks the real multi-interface path rather than merely loading the
ELF: both e1000 interfaces must complete DHCP, emit their beep marker, and
return control to the shell.  QEMU audio is intentionally not required; the
tape's serial-visible "beep complete" line is the observable proof that the
speaker routine ran and shut the speaker off.
"""

import os
import selectors
import socket
import subprocess
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
MONITOR = Path("/tmp/fos-test-net-dhcp-monitor.sock")


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit("build/fos.img does not exist; run `make image` first.")
    if MONITOR.exists():
        MONITOR.unlink()

    qemu = subprocess.Popen(
        [
            "qemu-system-i386",
            "-drive", f"format=raw,file={IMAGE}",
            "-serial", "stdio",
            "-monitor", f"unix:{MONITOR},server,nowait",
            "-no-reboot", "-no-shutdown", "-display", "none",
            "-netdev", "user,id=net0",
            "-device", "e1000,netdev=net0",
            "-netdev", "user,id=net1",
            "-device", "e1000,netdev=net1",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    output = bytearray()
    selector = selectors.DefaultSelector()
    selector.register(qemu.stdout, selectors.EVENT_READ)

    def read_until(marker: bytes, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        while marker not in output:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r}:\n"
                    f"{output.decode('utf-8', 'replace')}"
                )
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(
                        f"QEMU exited early:\n{output.decode('utf-8', 'replace')}"
                    )
                output.extend(chunk)

    def monitor(command: str) -> None:
        deadline = time.monotonic() + 5
        while True:
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as control:
                    control.settimeout(1)
                    control.connect(str(MONITOR))
                    control.recv(65536)
                    control.sendall((command + "\n").encode())
                    return
            except (FileNotFoundError, ConnectionRefusedError):
                if time.monotonic() >= deadline:
                    raise SystemExit("QEMU monitor socket did not become available")
                time.sleep(0.1)

    def command(line: bytes, timeout: float = 20) -> None:
        start = len(output)
        for byte in line:
            qemu.stdin.write(bytes([byte]))
            qemu.stdin.flush()
            time.sleep(0.002)
        deadline = time.monotonic() + timeout
        while b"fos> " not in output[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for prompt after {line!r}:\n"
                    f"{output.decode('utf-8', 'replace')}"
                )
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit("QEMU exited while waiting for a command prompt")
                output.extend(chunk)

    try:
        read_until(b"Waiting for a remote terminal client to connect", 30)
        monitor("sendkey f12")
        read_until(b"fos> ", 10)
        command(b"tape load disk NET-DHCP.BIN\r")
        del output[:]
        command_start = len(output)
        for byte in b"tape run\r":
            qemu.stdin.write(bytes([byte]))
            qemu.stdin.flush()
            time.sleep(0.002)
        read_until(b"Network DHCP test: every NIC acquired an IP; monitoring", 40)
        text = output[command_start:].decode("utf-8", "replace")
        for marker in (
            "NIC 1 acquired ",
            "NIC 1 beep complete.",
            "NIC 2 acquired ",
            "NIC 2 beep complete.",
            "monitoring until manual exit.",
        ):
            if marker not in text:
                raise SystemExit(f"missing {marker!r}:\n{text}")
        # The tape must remain active after DHCP success.  Only a local escape
        # key should return the shell prompt.
        monitor("sendkey f12")
        read_until(b"Network DHCP test stopped.", 10)
        read_until(b"Tape returned; back at the fos shell.", 10)
        read_until(b"fos> ", 5)
        print("PASS: DHCP connectivity tape monitored both leases until manual exit.")
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
