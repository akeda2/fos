#!/usr/bin/env python3
"""End-to-end test for build/tapes/cpu64-lab.bin.

This follows the same term-net + tape-load-net harness shape as the longmode
tests, but asserts the new structured report line emitted by
tapes/longmode64_shim.S after returning from the 64-bit body:

    longmode: 64-bit body report mode=0x..., support=0x..., tests=0x...

The cpu64-lab body uses startup arguments to select one mode; this test runs
`tape run all` and verifies x87/MMX/SSE support+pass bits on QEMU's default
x86_64 CPU model.
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

MCAST_GROUP = "230.42.42.57"
MCAST_PORT = 4257
IMAGE = Path(__file__).resolve().parent.parent / "build" / "fos.img"
TAPE = Path(__file__).resolve().parent.parent / "build" / "tapes" / "cpu64-lab.bin"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef09")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef0a")
CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

SUPPORT_X87 = 1 << 0
SUPPORT_MMX = 1 << 1
SUPPORT_SSE = 1 << 2
SUPPORT_X87_USABLE = 1 << 8
SUPPORT_MMX_USABLE = 1 << 9
SUPPORT_SSE_USABLE = 1 << 10
TEST_X87_PASS = 1 << 1
TEST_MMX_PASS = 1 << 3
TEST_SSE_PASS = 1 << 5


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

    def recv(self, size: int) -> bytes:
        return self.sock.recv(size)


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-cpu64-lab` first.")

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
            raise SystemExit(f"couldn't find fos's MAC address in:\n{output.decode('utf-8', 'replace')}")
        fos_mac = send_tape.parse_mac(match.group(1).decode())
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        data = TAPE.read_bytes()
        send_tape.send_tape(mcast, fos_mac, TAPE_SENDER_MAC, data, delay=0.003)
        read_until(b"fos> ", 30)
        if b"Tape loaded" not in output:
            raise SystemExit(f"tape did not load cleanly:\n{output.decode('utf-8', 'replace')}")
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run all\n"))
        read_until(b"Tape returned; back at the fos shell.", 30)
        text = output.decode("utf-8", "replace")
        match = re.search(
            r"longmode: 64-bit body report mode=0x([0-9a-fA-F]+), support=0x([0-9a-fA-F]+), tests=0x([0-9a-fA-F]+)",
            text,
        )
        if not match:
            raise SystemExit(f"cpu64-lab report line missing:\n{text}")
        mode = int(match.group(1), 16)
        support = int(match.group(2), 16)
        tests = int(match.group(3), 16)

        if mode != 0x5:
            raise SystemExit(f"expected mode=all (0x5), got 0x{mode:x}")
        required_support = SUPPORT_X87 | SUPPORT_MMX | SUPPORT_SSE
        if (support & required_support) != required_support:
            raise SystemExit(f"missing expected support bits in 0x{support:08x}")
        if (support & SUPPORT_X87_USABLE) != 0 and (tests & TEST_X87_PASS) == 0:
            raise SystemExit(f"x87 was usable but did not pass (tests=0x{tests:08x})")
        if (support & SUPPORT_MMX_USABLE) != 0 and (tests & TEST_MMX_PASS) == 0:
            raise SystemExit(f"MMX was usable but did not pass (tests=0x{tests:08x})")
        if (support & SUPPORT_SSE_USABLE) != 0 and (tests & TEST_SSE_PASS) == 0:
            raise SystemExit(f"SSE was usable but did not pass (tests=0x{tests:08x})")

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
