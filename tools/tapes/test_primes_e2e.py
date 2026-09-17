#!/usr/bin/env python3
"""One-off end-to-end test: send tools/tapes/primes.bin to a running fos
QEMU instance over a real (emulated) Ethernet link, run it, and check that
its printed summary matches the known-correct answer (78,498 primes <=
1,000,000, largest one 999,983).

Not part of `make test` -- this is a manual verification script, kept
outside kernel/tools proper, mirroring the throwaway harness used earlier
in this project's history to validate `tape load net` itself. Requires no
root: QEMU's `socket,mcast=` netdev is a plain UDP multicast socket
carrying one raw Ethernet frame per datagram, so a normal unprivileged UDP
socket can inject/observe frames without CAP_NET_RAW.
"""

import re
import socket as socketlib
import selectors
import struct
import subprocess
import sys
import time
import os
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import send_tape  # noqa: E402

MCAST_GROUP = "230.42.42.42"
MCAST_PORT = 4242
IMAGE = Path(__file__).resolve().parent.parent.parent / "build" / "fos.img"
TAPE = Path(__file__).resolve().parent.parent.parent / "build" / "tapes" / "primes.bin"
HOST_MAC = bytes.fromhex("02deadbeef01")


class McastSocket:
    """Wraps a UDP multicast socket so it exposes the plain .send()/.recv()
    surface send_tape.py expects from an AF_PACKET raw socket. Every
    Ethernet frame becomes one UDP datagram's payload, exactly matching
    QEMU's own socket,mcast= netdev wire format."""

    def __init__(self, group: str, port: int):
        self.group = group
        self.port = port
        self.sock = socketlib.socket(socketlib.AF_INET, socketlib.SOCK_DGRAM, socketlib.IPPROTO_UDP)
        self.sock.setsockopt(socketlib.SOL_SOCKET, socketlib.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", port))
        mreq = struct.pack("4sl", socketlib.inet_aton(group), socketlib.INADDR_ANY)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_ADD_MEMBERSHIP, mreq)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_MULTICAST_TTL, 1)

    def send(self, frame: bytes) -> None:
        self.sock.sendto(frame, (self.group, self.port))

    def settimeout(self, value):
        self.sock.settimeout(value)

    def recv(self, size: int) -> bytes:
        return self.sock.recv(size)


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-primes` first.")

    qemu = subprocess.Popen(
        [
            "qemu-system-i386",
            "-drive", f"format=raw,file={IMAGE}",
            "-serial", "stdio",
            "-monitor", "none",
            "-no-reboot", "-no-shutdown",
            "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
            "-device", "rtl8139,netdev=net0",
            "-display", "none",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    output = bytearray()
    selector = selectors.DefaultSelector()
    selector.register(qemu.stdout, selectors.EVENT_READ)

    def read_until(marker: bytes, timeout: float) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in output:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(f"timed out waiting for {marker!r}; output so far:\n{output.decode('utf-8', 'replace')}")
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(f"QEMU serial stream ended early:\n{output.decode('utf-8', 'replace')}")
                output.extend(chunk)
        return bytes(output)

    def send_line(line: str) -> None:
        qemu.stdin.write((line + "\n").encode())
        qemu.stdin.flush()

    try:
        read_until(b"fos> ", 30)
        del output[:]

        send_line("nic")
        read_until(b"fos> ", 10)
        match = re.search(rb"RTL8139 found: MAC ([0-9a-fA-F:]{17})", bytes(output))
        if not match:
            raise SystemExit(f"couldn't find fos's MAC address in:\n{output.decode('utf-8', 'replace')}")
        fos_mac = send_tape.parse_mac(match.group(1).decode())
        print(f"fos MAC: {send_tape.format_mac(fos_mac)}", file=sys.stderr)
        del output[:]

        send_line("tape load net")
        time.sleep(0.5)  # give the shell a moment to enter its receive-wait loop

        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        data = TAPE.read_bytes()
        print(f"sending {len(data)}-byte tape...", file=sys.stderr)
        send_tape.send_tape(mcast, fos_mac, HOST_MAC, data, delay=0.003)

        read_until(b"fos> ", 30)
        if b"Tape loaded" not in output:
            raise SystemExit(f"tape did not load cleanly:\n{output.decode('utf-8', 'replace')}")
        print("tape loaded OK", file=sys.stderr)
        del output[:]

        send_line("tape run")
        read_until(b"Tape returned; back at the fos shell.", 30)
        text = output.decode("utf-8", "replace")
        print(text)

        assert "primes found:   78498" in text, "prime count mismatch"
        assert "largest prime:  999983" in text, "largest prime mismatch"
        print("PASS: primes tape produced the correct summary.", file=sys.stderr)
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
