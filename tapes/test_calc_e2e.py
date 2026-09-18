#!/usr/bin/env python3
"""One-off end-to-end test: send tapes/calc's built tape to a running fos
QEMU instance over a real (emulated) Ethernet link and exercise both
halves of the tape ABI's `read_key`/`args_ptr`/`args_len` additions (see
AGENTS.md's "Tape ABI grows again: `read_key` and startup arguments") --
this is the tape that motivated adding them in the first place.

Mirrors tapes/test_hello_rs_e2e.py's structure and rationale (one
continuous `term net` session established right after boot, the tape's
own bytes sent from a second, independent MAC), but drives the tape
itself twice, over that same session:

- `tape run 3 4 +` (startup arguments, one-shot: evaluates once and
  returns immediately, no further interaction).
- `tape run` with no arguments (interactive `read_key` mode): a short
  RPN REPL session sent as several `TERM_TAG_DATA` frames (each line
  including its own trailing `\\r`, since a real `term_net.py
  --interactive` client would send one key at a time and fos's own
  `read_key` treats `\\r`/`\\n` as "evaluate now"), then a lone `0x1b`
  Escape byte to leave the REPL the same way
  `tools/test_vga_colors_e2e.py` proves a remote Escape byte stops a
  `read_key`-based loop.

Not part of `make test` -- this is a manual verification script, kept
outside kernel/tools proper, matching every other tapes/test_*_e2e.py
script's own scope note.
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

MCAST_GROUP = "230.42.42.50"
MCAST_PORT = 4250
IMAGE = Path(__file__).resolve().parent.parent / "build" / "fos.img"
TAPE = Path(__file__).resolve().parent.parent / "build" / "tapes" / "calc.bin"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef0b")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef0c")
CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22
REMOTE_ABORT_BYTE = 0x1B


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
        raise SystemExit(f"{TAPE} does not exist; run `make tape-calc` first.")

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
        # shows the local "fos> " prompt.
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

        # --- One-shot mode: startup arguments (args_ptr/args_len) ---
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run 3 4 +\n"))
        read_until(b"Tape returned; back at the fos shell.", 15)
        text = output.decode("utf-8", "replace")
        print(text)
        assert "> 3 4 +" in text, "one-shot mode did not echo the startup arguments"
        assert "= 7" in text, "one-shot mode produced the wrong result for 3 4 +"
        print("PASS: one-shot mode (startup arguments) evaluated '3 4 +' to 7.", file=sys.stderr)
        del output[:]

        # --- Interactive mode: read_key-based REPL ---
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until(b"Press F12", 15)
        del output[:]

        # A correct expression, an intentional error case (division by
        # zero), and a backspace mid-line (typing "2 2 -", backspacing
        # the second "2", typing "3" instead -- exercising read_key's
        # own backspace handling -- to get "2 3 -" = -1).
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"6 7 *\r"))
        read_until(b"= 42", 10)
        print("PASS: interactive mode evaluated '6 7 *' to 42.", file=sys.stderr)
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"5 0 /\r"))
        read_until(b"division by zero", 10)
        print("PASS: interactive mode reported division by zero for '5 0 /'.", file=sys.stderr)
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"2 2\x08 3 -\r"))
        read_until(b"= -1", 10)
        print("PASS: interactive mode handled a backspace correctly ('2 2<BS> 3 -' = -1).", file=sys.stderr)
        del output[:]

        # A remote Escape byte (0x1b) inside an ordinary TERM_TAG_DATA
        # frame stops the read_key loop, mirroring how
        # tools/test_vga_colors_e2e.py proves the same mechanism for a
        # should_abort-based tape.
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, bytes([REMOTE_ABORT_BYTE])))
        read_until(b"Tape returned; back at the fos shell.", 10)
        text = output.decode("utf-8", "replace")
        assert "Exiting the calculator." in text, "remote escape byte did not stop the read_key loop"
        print("PASS: a remote Escape byte stopped the interactive read_key loop cleanly.", file=sys.stderr)

        # One more command over the same session, proving term_net itself
        # is still alive and responsive after the tape's own read_key loop
        # released control back to it.
        del output[:]
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(b"fos is a small educational 32-bit x86 operating system.", 10)
        print("PASS: term_net session still responsive after the tape returned.", file=sys.stderr)

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
