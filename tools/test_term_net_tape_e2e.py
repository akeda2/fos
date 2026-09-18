#!/usr/bin/env python3
"""One-off end-to-end test: connect to a running fos QEMU instance with
`term net`, then remotely run `tape load net` and `tape run` from inside
that session, sending the primes tape from a *third*, independent MAC as
if it were an ordinary `send_tape.py` invocation running alongside the
remote terminal.

This exercises (and documents, by asserting on it) the exact behavior
`term net`'s own doc comment predicts for a nested blocking command,
combined with the tape print-callback ABI added in
`tools/tapes/primes.S`/`kernel/src/console.rs::print_byte`:

- `tape load net`/`tape run` work completely normally while driven
  remotely -- the shell dispatches them exactly the same way a locally
  typed command would, and the NIC frame filtering in both already
  ignores unrelated tags/ethertypes, so `TERM_TAG_*` and `TAPE_TAG_*`
  traffic never interfere with each other.
- Neither command's *output* reaches the remote client live: since
  `console::enable_relay`'s queue is only drained by `term_net`'s own
  outer loop, and that loop is paused for the whole time a nested command
  blocks in its own loop, every kernel-printed line (progress messages,
  "Tape loaded", the tape's own digits, "Tape returned; back at the fos
  shell.") arrives in one burst only once the nested command returns and
  control comes back to `term_net`.
- The primes tape's own summary digits, though, DO now show up in that
  burst: the tape calls back into `console::print_byte` (passed to it as
  `tape_run`'s one argument) for every byte it prints, and that callback
  is the exact same `console::write_bytes` path every other command's
  `println!` goes through, relay queue included. So a remote operator
  driving `tape load net`/`tape run` over `term net` now sees the tape's
  actual output too -- delayed until the nested command returns (no live
  streaming mid-tape), but present, unlike before this callback existed.

Not part of `make test` -- manual verification script, in the same spirit
as test_term_net_e2e.py and tools/tapes/test_primes_e2e.py, both of which
this borrows its structure from.
"""

import re
import selectors
import socket as socketlib
import struct
import subprocess
import sys
import time
import os
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import send_tape  # noqa: E402

MCAST_GROUP = "230.42.42.47"
MCAST_PORT = 4247
IMAGE = Path(__file__).resolve().parent.parent / "build" / "fos.img"
TAPE = Path(__file__).resolve().parent.parent / "build" / "tapes" / "primes.bin"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef06")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef07")
BROADCAST_MAC = b"\xff" * 6
CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22


class McastSocket:
    """Same wrapper as tools/tapes/test_primes_e2e.py's, reused here so
    send_tape.send_tape can drive it directly for the independent
    "third-party sender" role."""

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

    def settimeout(self, value):
        self.sock.settimeout(value)

    def recv(self, size: int) -> bytes:
        return self.sock.recv(size)


def pad(frame: bytes) -> bytes:
    frame = bytearray(frame)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    return pad(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-primes` first.")

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

    mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
    mcast.settimeout(0.2)
    relayed = bytearray()

    def drain_relay_frames(deadline: float) -> None:
        """Reads every TERM_TAG_DATA frame sent to us until `deadline`,
        appending their payloads (trailing NULs trimmed) to `relayed`."""
        while time.monotonic() < deadline:
            try:
                packet = mcast.recv(2048)
            except socketlib.timeout:
                continue
            if len(packet) < 15 or packet[12:14] != bytes(CHAT_ETHERTYPE):
                continue
            if packet[14] != TERM_TAG_DATA:
                continue
            payload = packet[15:]
            end = len(payload)
            while end > 0 and payload[end - 1] == 0:
                end -= 1
            relayed.extend(payload[:end])

    try:
        # fos now runs `term net` automatically at boot (see shell::run/
        # AGENTS.md), waiting for a remote terminal client before it ever
        # shows the local "fos> " prompt. Connect once here and stay
        # connected for the entire test: term_net (kernel/src/shell.rs)
        # now loops back to waiting for a new client instead of falling
        # through to the local shell whenever a remote session ends, so
        # there is no "connect, then disconnect to reach the local
        # prompt" trick left to fall back on, and no need for a second,
        # separately-typed "term net" session either -- one continuous
        # session covers both the MAC lookup and the nested tape commands
        # below.
        read_until(b"Waiting for a remote terminal client to connect", 30)
        del output[:]
        mcast.send(term_frame(BROADCAST_MAC, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)  # the initial in-session prompt
        print("term net session established", file=sys.stderr)
        del output[:]
        drain_relay_frames(time.monotonic() + 0.3)
        del relayed[:]

        # Learn fos's own MAC address remotely too (there's no local
        # prompt to fall back on anymore).
        mcast.send(term_frame(BROADCAST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"nic\n"))
        read_until(b"fos> ", 10)
        match = re.search(rb"RTL8139 found: MAC ([0-9a-fA-F:]{17})", bytes(output))
        if not match:
            raise SystemExit(f"couldn't find fos's MAC address in:\n{output.decode('utf-8', 'replace')}")
        fos_mac = send_tape.parse_mac(match.group(1).decode())
        print(f"fos MAC: {send_tape.format_mac(fos_mac)}", file=sys.stderr)
        del output[:]
        drain_relay_frames(time.monotonic() + 0.3)
        del relayed[:]

        # Remotely start "tape load net" -- this nests a second blocking
        # loop inside term_net's own, so no further relay drains happen
        # until it returns.
        mcast.send(term_frame(BROADCAST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)

        # A third, independent MAC sends the tape, exactly as a real
        # send_tape.py invocation running on another machine would --
        # nothing here is aware of the term_net session at all.
        data = TAPE.read_bytes()
        print(f"sending {len(data)}-byte tape from an independent sender...", file=sys.stderr)
        send_tape.send_tape(mcast, fos_mac, TAPE_SENDER_MAC, data, delay=0.003)

        read_until(b"Tape loaded", 60)
        print("tape loaded (seen on qemu's own serial stream)", file=sys.stderr)

        # Now the nested command has returned and term_net's outer loop
        # resumes draining the relay queue -- collect the whole burst.
        drain_relay_frames(time.monotonic() + 2.0)
        relayed_text_after_load = relayed.decode("utf-8", "replace")
        print("---- relayed to the remote term_net client after 'tape load net' ----", file=sys.stderr)
        print(relayed_text_after_load, file=sys.stderr)
        print("-----------------------------------------------------------------------", file=sys.stderr)
        assert "tape load net" in relayed_text_after_load, "echoed command missing from the relayed burst"
        assert "Tape loaded" in relayed_text_after_load, "'Tape loaded' wrapper message missing from the relayed burst"
        del relayed[:]

        # Remotely run the tape.
        mcast.send(term_frame(BROADCAST_MAC, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until(b"Tape returned; back at the fos shell.", 30)
        local_text = output.decode("utf-8", "replace")
        print("---- qemu's own serial stream (what a local operator would see) ----", file=sys.stderr)
        print(local_text, file=sys.stderr)
        print("----------------------------------------------------------------------", file=sys.stderr)
        assert "78498" in local_text, "primes tape's own output missing from the local/serial view"

        drain_relay_frames(time.monotonic() + 2.0)
        relayed_text_after_run = relayed.decode("utf-8", "replace")
        print("---- relayed to the remote term_net client after 'tape run' ----", file=sys.stderr)
        print(relayed_text_after_run, file=sys.stderr)
        print("-------------------------------------------------------------------", file=sys.stderr)
        assert "Running" in relayed_text_after_run, "'Running N-byte tape...' wrapper message missing from the relayed burst"
        assert "Tape returned; back at the fos shell." in relayed_text_after_run, "'Tape returned' wrapper message missing from the relayed burst"
        assert "78498" in relayed_text_after_run, "primes tape's own output (via the console::print_byte callback) missing from the relayed burst"
        assert "999983" in relayed_text_after_run, "primes tape's own output (via the console::print_byte callback) missing from the relayed burst"
        del relayed[:]

        mcast.send(term_frame(BROADCAST_MAC, TERM_CLIENT_MAC, TERM_TAG_BYE))
        read_until(b"Remote terminal disconnected", 10)

        print(
            "PASS: 'tape load net'/'tape run' work correctly from inside a term_net "
            "session -- both the wrapper messages and the tape's own output (via the "
            "console::print_byte callback) relay to the remote client, in one burst "
            "after each command returns.",
            file=sys.stderr,
        )
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
