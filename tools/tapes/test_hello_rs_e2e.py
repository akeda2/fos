#!/usr/bin/env python3
"""One-off end-to-end test: send tools/tapes/hello-rs's built tape to a
running fos QEMU instance over a real (emulated) Ethernet link, run it,
and check that its printed summary matches the known-correct answer (168
primes <= 1,000, largest one 997) -- the same idea as
test_primes_e2e.py, just proving the *Rust* tape works end to end.

Drives everything through one continuous `term net` session established
right after boot (see `shell::run`'s automatic wait): since `term_net`
(kernel/src/shell.rs) now loops back to waiting for a new client instead
of falling through to the local shell whenever a remote session ends (see
AGENTS.md's "Remote terminal (`term net`)" section), there is no longer a
"connect, then disconnect to reach the local prompt" trick to fall back
on -- so `nic`/`tape load net`/`tape run` are all typed into that one
session instead of QEMU's own stdin. The tape's bytes themselves still
come from a second, independent MAC (mirroring an ordinary `send_tape.py`
run from another machine, unrelated to the term_net session), exactly the
way `tools/test_term_net_tape_e2e.py` already does it.

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

MCAST_GROUP = "230.42.42.49"
MCAST_PORT = 4249
IMAGE = Path(__file__).resolve().parent.parent.parent / "build" / "fos.img"
TAPE = Path(__file__).resolve().parent.parent.parent / "build" / "tapes" / "hello-rs.bin"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef09")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef0a")
CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22


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
        raise SystemExit(f"{TAPE} does not exist; run `make tape-hello-rs` first.")

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

    def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
        frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
        if len(frame) < 60:
            frame.extend(b"\0" * (60 - len(frame)))
        return bytes(frame)

    try:
        # fos now runs `term net` automatically at boot (see shell::run/
        # AGENTS.md), waiting for a remote terminal client before it ever
        # shows the local "fos> " prompt. Connect once and stay connected
        # for the rest of this test -- see this file's own header comment
        # for why disconnecting no longer reaches a local prompt to fall
        # back on.
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
        print(f"fos MAC: {send_tape.format_mac(fos_mac)}", file=sys.stderr)
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)  # give the shell a moment to enter its receive-wait loop

        data = TAPE.read_bytes()
        print(f"sending {len(data)}-byte tape from an independent sender...", file=sys.stderr)
        send_tape.send_tape(mcast, fos_mac, TAPE_SENDER_MAC, data, delay=0.003)

        read_until(b"fos> ", 30)
        if b"Tape loaded" not in output:
            raise SystemExit(f"tape did not load cleanly:\n{output.decode('utf-8', 'replace')}")
        print("tape loaded OK", file=sys.stderr)
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until(b"Tape returned; back at the fos shell.", 30)
        text = output.decode("utf-8", "replace")
        print(text)

        assert "primes found:  168" in text, "prime count mismatch"
        assert "largest prime: 997" in text, "largest prime mismatch"
        print("PASS: hello-rs tape produced the correct summary.", file=sys.stderr)

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
