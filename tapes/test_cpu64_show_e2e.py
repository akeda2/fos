#!/usr/bin/env python3
"""Smoke test for the 64-bit cpu64-show visual demo tape.

Runs `tape run auto 2` (2 seconds -- the demo's SECONDS argument, clamped to
1..120) to keep runtime bounded for CI-style checks while still exercising
the longmode shim handoff, the real large-arena pass-through (`-m 256`
below), and structured report output plus the human-readable summary line.
"""

import os
import re
import selectors
import socket as socketlib
import struct
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402

MCAST_GROUP = "230.42.42.58"
MCAST_PORT = 4258
IMAGE = Path(__file__).resolve().parent.parent / "build" / "fos.img"
TAPE = Path(__file__).resolve().parent.parent / "build" / "tapes" / "cpu64-show.bin"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef09")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef0a")
CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22


class McastSocket:
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


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-cpu64-show` first.")

    qemu = subprocess.Popen(
        [
            "qemu-system-x86_64",
            "-drive",
            f"format=raw,file={IMAGE}",
            "-serial",
            "stdio",
            "-monitor",
            "none",
            "-no-reboot",
            "-no-shutdown",
            "-netdev",
            f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
            "-device",
            "rtl8139,netdev=net0",
            "-display",
            "none",
            # This tape's 8 MiB arena comes from pmem's "big pool", which only
            # has usable capacity once total machine RAM exceeds its 128 MiB
            # tracked-frame-pool ceiling (mirroring tapes/test_bigsieve_e2e.py's
            # own -m 256). Without this, alloc() always returns 0 here and the
            # arena/SUPPORT_ARENA code path is never actually exercised.
            "-m",
            "256",
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

    try:
        read_until(b"Waiting for a remote terminal client to connect", 30)
        del output[:]
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"nic\n"))
        read_until(b"fos> ", 10)
        match = re.search(rb"RTL8139 found: MAC ([0-9a-fA-F:]{17})", bytes(output))
        if not match:
            raise SystemExit(f"couldn't find fos MAC in:\n{output.decode('utf-8', 'replace')}")
        fos_mac = send_tape.parse_mac(match.group(1).decode())
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        send_tape.send_tape(mcast, fos_mac, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until(b"fos> ", 30)
        if b"Tape loaded" not in output:
            raise SystemExit(f"tape did not load cleanly:\n{output.decode('utf-8', 'replace')}")
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run auto 2\n"))
        read_until(b"Tape returned; back at the fos shell.", 40)
        text = output.decode("utf-8", "replace")
        if "longmode: 64-bit body report mode=0x" not in text:
            raise SystemExit(f"missing structured report output:\n{text}")
        if "cpu64-show: cpu supports" not in text:
            raise SystemExit(f"missing human-readable summary line:\n{text}")

        del output[:]
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(b"fos is a small educational 32-bit x86 operating system.", 10)
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
