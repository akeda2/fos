#!/usr/bin/env python3
"""End-to-end test for `nic map`: which NICs hear which.

One QEMU guest with five NICs. NICs 1-2 are joined by one virtual cable (a QEMU
hub) and NICs 3-4 by another, NIC 5 sits alone on QEMU's user-mode network, whose
gateway never speaks first. `nic map` must report the two direct cables, no
cross-talk between them, and nothing for NIC 5 (while still showing its link).

Run it with `make test-nic-map`."""

import os
import selectors
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOS_TEST_IMAGE", ROOT / "build" / "fos.img"))
MACS = [f"52:54:00:ab:00:0{n}" for n in range(1, 6)]


def main():
    tmp = tempfile.mkdtemp(prefix="fos-map-")
    monitor = os.path.join(tmp, "mon")
    command = ["qemu-system-i386", "-m", "128", "-drive", f"format=raw,file={IMAGE}", "-snapshot",
               "-serial", "stdio", "-monitor", f"unix:{monitor},server,nowait", "-display", "none"]
    for number, hub in enumerate((1, 1, 2, 2), start=1):
        command += ["-netdev", f"hubport,id=p{number},hubid={hub}",
                    "-device", f"e1000,netdev=p{number},mac={MACS[number - 1]}"]
    command += ["-netdev", "user,id=p5", "-device", f"e1000e,netdev=p5,mac={MACS[4]}"]
    qemu = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    selector = selectors.DefaultSelector()
    selector.register(qemu.stdout, selectors.EVENT_READ)
    output = bytearray()

    def pump(seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            for key, _ in selector.select(0.05):
                chunk = os.read(key.fd, 4096)
                if chunk:
                    output.extend(chunk)

    def wait_for(marker, start=0, timeout=40):
        end = time.monotonic() + timeout
        while marker not in output[start:]:
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
                return
            except (FileNotFoundError, ConnectionRefusedError, TimeoutError):
                time.sleep(0.05)

    try:
        wait_for(b"Waiting for a remote terminal client")
        sendkey("f12")
        wait_for(b"fos> ")
        mark = len(output)
        qemu.stdin.write(b"nic map\n")
        qemu.stdin.flush()
        wait_for(b"NIC 5:", mark, 30)
        wait_for(b"fos> ", mark, 30)
        text = output[mark:].decode("utf-8", "replace")
        lines = {}
        current = None
        for line in text.splitlines():
            line = line.rstrip()
            if line.startswith("NIC "):
                current = int(line[4])
                lines[current] = [line]
            elif current and line.strip():
                lines[current].append(line.strip())
        joined = {number: " ".join(parts) for number, parts in lines.items()}

        def check(condition, message):
            if not condition:
                raise SystemExit(f"FAIL: {message}\n{text}")
            print(f"PASS: {message}")

        check("sees NIC 2 -- direct cable" in joined[1], "NIC 1 sees only NIC 2, on a direct cable")
        check("sees NIC 1 -- direct cable" in joined[2], "NIC 2 sees only NIC 1, on a direct cable")
        check("sees NIC 4 -- direct cable" in joined[3] and "NIC 3 -- direct cable" in joined[4],
              "NICs 3 and 4 find each other on their own cable")
        check("NIC 3" not in joined[1] + joined[2] and "NIC 1" not in joined[3] + joined[4],
              "the two cables do not hear each other")
        check("sees nothing" in joined[5],
              "NIC 5 (user network, nobody talking) sees nothing, not even our other NICs")
        check("1000M full" in joined[5], "a driver that reports link state shows it (e1000e: 1000M full)")
        print("PASS: nic map shows which NICs are cabled together.")
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
