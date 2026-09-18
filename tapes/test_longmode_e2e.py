#!/usr/bin/env python3
"""One-off end-to-end test: send tapes/longmode.bin to a running fos
QEMU instance over a real (emulated) Ethernet link, run it, and check that
the tape's own serial-visible messages ("CPUID reports long-mode support",
"back in ordinary 32-bit protected mode, all state restored") appear, and
that the shell is still alive and responsive afterward.

This is the one and only e2e test in this repository that must launch
`qemu-system-x86_64` instead of `qemu-system-i386`: QEMU's i386 TCG
backend cannot emulate the CPUID long-mode feature bit at all (confirmed:
`qemu-system-i386 -cpu qemu64 ...` prints `warning: TCG doesn't support
requested feature: CPUID[eax=80000001h].EDX.lm [bit 29]`), so under
qemu-system-i386 this tape would always take its "long-mode not supported"
early-return path instead of actually visiting long mode. qemu-system-x86_64
with its default `qemu64` CPU model supports the bit correctly and boots
this same 32-bit fos.img identically otherwise -- see tapes/longmode.S's
header comment and AGENTS.md's "A tape that visits 64-bit long mode and
comes back" section for the full design rationale.

Deliberately does not try to assert on the tape's "64-BIT MODE REACHED,
RAX=deadbeefcafebabe" text, since that is written directly to VGA text
memory (like tapes/vga-colors does), never through the serial-mirrored
`putc` callback -- it is only visible on the real VGA screen (`make run`
with this tape loaded, watched by a human), not on this test's serial
capture. What this test *can* automate -- and does -- is proving the round
trip completed cleanly: if state restoration were buggy, either the tape
would hang/crash before printing its own "back in ordinary 32-bit
protected mode" message, or the shell would stop responding to further
commands sent after `tape run` returns; this test checks both.

Not part of `make test` -- this is a manual verification script, mirroring
tapes/test_primes_e2e.py's structure and one-off status.
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

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402

MCAST_GROUP = "230.42.42.53"
MCAST_PORT = 4253
IMAGE = Path(__file__).resolve().parent.parent / "build" / "fos.img"
TAPE = Path(__file__).resolve().parent.parent / "build" / "tapes" / "longmode.bin"
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
        raise SystemExit(f"{TAPE} does not exist; run `make tape-longmode` first.")

    qemu = subprocess.Popen(
        [
            "qemu-system-x86_64",
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
        # fos runs `term net` automatically at boot (see shell::run/
        # AGENTS.md), waiting for a remote terminal client before it ever
        # shows the local "fos> " prompt -- see tapes/test_primes_e2e.py's
        # header comment for why this test connects once and stays
        # connected instead of falling back to a local prompt.
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

        assert "CPUID reports long-mode support; making the jump" in text, "tape did not report long-mode support"
        assert "back in ordinary 32-bit protected mode, all state restored" in text, "tape did not report a clean return"
        print("PASS: longmode tape reported long-mode support and a clean return.", file=sys.stderr)

        # Prove the shell is still fully alive and correctly reading the
        # restored GDT/IDT/interrupt state: run one more, completely
        # ordinary command and check it still works normally.
        del output[:]
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(b"fos is a small educational 32-bit x86 operating system.", 10)
        print("PASS: shell still responsive to ordinary commands after the tape returned.", file=sys.stderr)

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
