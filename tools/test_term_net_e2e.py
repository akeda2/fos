#!/usr/bin/env python3
"""One-off end-to-end test: connect to a running fos QEMU instance's
automatic-at-boot `term net` session, run a couple of ordinary shell
commands remotely, and check that their output actually made it back
over the (emulated) wire. Since fos now starts `term net` on its own at
every boot (see `shell::run`), this no longer needs to type `term net`
itself -- it just waits for the "Waiting for a remote terminal client to
connect" banner before sending its own `TERM_TAG_HELLO`.

Not part of `make test` -- like `tapes/test_primes_e2e.py`, this is a
manual verification script exercising a real Ethernet round-trip, kept
outside the serial-only smoke harness. Requires no root: QEMU's
`socket,mcast=` netdev is a plain UDP multicast socket carrying one raw
Ethernet frame per datagram, so a normal unprivileged UDP socket can
inject/observe frames without CAP_NET_RAW.

This drives the wire protocol directly (mirroring `tools/term_net.py`'s
own framing) rather than reusing `tools/term_net.py` itself, since that
tool opens a real `AF_PACKET` socket and cannot address a QEMU multicast
netdev without a tap device.
"""

import selectors
import socket as socketlib
import struct
import subprocess
import sys
import time
import os
from pathlib import Path

MCAST_GROUP = "230.42.42.43"
MCAST_PORT = 4243
IMAGE = Path(__file__).resolve().parent.parent / "build" / "fos.img"
HOST_MAC = bytes.fromhex("02deadbeef02")
BROADCAST_MAC = b"\xff" * 6
CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22


class McastSocket:
    def __init__(self, group: str, port: int):
        self.sock = socketlib.socket(socketlib.AF_INET, socketlib.SOCK_DGRAM, socketlib.IPPROTO_UDP)
        self.sock.setsockopt(socketlib.SOL_SOCKET, socketlib.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", port))
        mreq = struct.pack("4sl", socketlib.inet_aton(group), socketlib.INADDR_ANY)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_ADD_MEMBERSHIP, mreq)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_MULTICAST_TTL, 1)
        self.group = group
        self.port = port

    def send(self, frame: bytes) -> None:
        self.sock.sendto(frame, (self.group, self.port))


def pad(frame: bytes) -> bytes:
    frame = bytearray(frame)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    return pad(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)


def main() -> None:
    qemu = subprocess.Popen(
        [
            "qemu-system-i386",
            "-drive", f"format=raw,file={IMAGE}",
            "-serial", "stdio",
            "-monitor", "none", "-no-reboot", "-no-shutdown",
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

    def read_until(marker: bytes, timeout: float) -> None:
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

    try:
        read_until(b"Waiting for a remote terminal client to connect", 30)
        del output[:]
        # fos now runs `term net` automatically at boot (see
        # shell::run/AGENTS.md), so there is no need to type it -- the
        # HELLO-wait loop is already active by the time the marker above
        # appears.

        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        mcast.send(frame(BROADCAST_MAC, HOST_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        print("session established", file=sys.stderr)
        del output[:]

        mcast.send(frame(BROADCAST_MAC, HOST_MAC, TERM_TAG_DATA, b"about\n"))
        time.sleep(1.0)
        mcast.send(frame(BROADCAST_MAC, HOST_MAC, TERM_TAG_DATA, b"cpuinfo\n"))
        time.sleep(1.0)
        mcast.send(frame(BROADCAST_MAC, HOST_MAC, TERM_TAG_BYE))

        read_until(b"Remote terminal disconnected", 10)
        text = output.decode("utf-8", "replace")
        print(text)
        assert "small educational" in text, "'about' output missing from the relayed session"
        assert "CPUID vendor" in text, "'cpuinfo' output missing from the relayed session"
        print("PASS: term net relayed remote commands and their output correctly.", file=sys.stderr)
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
