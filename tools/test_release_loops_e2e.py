#!/usr/bin/env python3
"""End-to-end test (QEMU, two NICs) that a `term net` client can end the
blocking commands it started -- `tape load net`, `nic sniff`, `nic discover` --
without anyone at the machine's keyboard.

Each loop must stop on the client's escape byte (the remote F12) and, for
`tape load net`, on a disconnect, whether the session lives on the NIC the loop
polls or on a different one; and after an escape the session must still work.

Run via `make test-release-loops`."""

import os
import selectors
import struct
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tapes"))
from test_hello_rs_e2e import McastSocket  # noqa: E402

GROUP, PORT = "230.42.42.97", 4297
NIC = [bytes.fromhex("525400123461"), bytes.fromhex("525400123462")]
CLIENT = bytes.fromhex("02deadbeef71")
ETHERTYPE = bytes((0x88, 0xB6))
HELLO, DATA, BYE = 0x20, 0x21, 0x22
ESC = b"\x1b"


def frame(nic: int, tag: int, payload: bytes = b"") -> bytes:
    f = bytearray(NIC[nic] + CLIENT + ETHERTYPE + bytes([tag]) + payload)
    f.extend(b"\0" * max(0, 60 - len(f)))
    return bytes(f)


class Guest:
    def __init__(self) -> None:
        args = ["qemu-system-i386", "-drive", f"format=raw,file={ROOT}/build/fos.img", "-serial", "stdio",
                "-monitor", "none", "-display", "none", "-snapshot"]
        for index in range(2):
            args += ["-netdev", f"socket,id=n{index},mcast={GROUP}:{PORT}",
                     "-device", f"e1000,netdev=n{index},mac={NIC[index].hex(':')}"]
        self.process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.output = bytearray()
        self.mcast = McastSocket(GROUP, PORT)

    def pump(self, seconds: float) -> None:
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            for key, _ in self.selector.select(0.05):
                chunk = os.read(key.fd, 4096)
                if chunk:
                    self.output.extend(chunk)

    def wait_for(self, marker: bytes, start: int, timeout: float = 15) -> bool:
        end = time.monotonic() + timeout
        while marker not in self.output[start:]:
            if time.monotonic() > end:
                return False
            self.pump(0.1)
        return True

    def send(self, nic: int, tag: int, payload: bytes = b"") -> None:
        self.mcast.send(frame(nic, tag, payload))

    def connect(self, nic: int) -> None:
        mark = len(self.output)
        self.send(nic, HELLO)
        if not self.wait_for(b"Remote terminal connected", mark):
            raise SystemExit("could not open a term net session:\n" + self.text(mark))
        self.pump(1)

    def type(self, nic: int, line: str) -> None:
        self.send(nic, DATA, line.encode() + b"\n")

    def text(self, start: int) -> str:
        return self.output[start:].decode("utf-8", "replace")

    def stop(self) -> None:
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def check(label: str, guest: Guest, mark: int, *needles: str) -> None:
    text = guest.text(mark)
    for needle in needles:
        if needle not in text:
            raise SystemExit(f"FAIL: {label}: expected {needle!r} in:\n{text}")
    print(f"PASS: {label}")


def scenario(guest: Guest, label: str, session: int, active: int, command: str, stop_payload: bytes,
             stop_tag: int, expected: str, session_survives: bool) -> None:
    """Session on NIC `session`, command running on NIC `active`; then stop it."""
    guest.connect(session)
    guest.type(session, f"nic use {active + 1}")
    guest.pump(1.5)
    mark = len(guest.output)
    guest.type(session, command)
    guest.pump(2)
    guest.send(session, stop_tag, stop_payload)  # always on the session's NIC
    if not guest.wait_for(expected.encode(), mark, 10):
        raise SystemExit(f"FAIL: {label}: loop was not released; output:\n{guest.text(mark)}")
    guest.pump(1)
    if session_survives:
        mark2 = len(guest.output)
        guest.type(session, "uptime")
        if not guest.wait_for(b"Uptime:", mark2, 10):
            raise SystemExit(f"FAIL: {label}: session did not survive the escape:\n{guest.text(mark2)}")
        guest.type(session, "nic use 1")  # restore the default selection
        guest.pump(1.5)
        guest.send(session, BYE)
        guest.wait_for(b"waiting for a new one", mark2, 10)
    else:
        # A disconnect ends the session; the machine goes back to waiting.
        if not guest.wait_for(b"waiting for a new one", mark, 10):
            raise SystemExit(f"FAIL: {label}: machine did not return to waiting:\n{guest.text(mark)}")
        guest.connect(session)
        guest.type(session, "nic use 1")
        guest.pump(1.5)
        guest.send(session, BYE)
        guest.pump(1)
    print(f"PASS: {label}")


def main() -> None:
    guest = Guest()
    try:
        if not guest.wait_for(b"Waiting for a remote terminal client", 0, 40):
            raise SystemExit("guest never reached the term net wait:\n" + guest.text(0))
        # tape load net, session on NIC 2 but the load listening on NIC 1: only the
        # session NIC can carry the stop, and the loop used to never read it.
        scenario(guest, "tape load net on another NIC ends on remote disconnect", session=1, active=0,
                 command="tape load net", stop_payload=b"", stop_tag=BYE,
                 expected="Remote terminal disconnected; no tape loaded", session_survives=False)
        scenario(guest, "tape load net on another NIC ends on remote escape", session=1, active=0,
                 command="tape load net", stop_payload=ESC, stop_tag=DATA,
                 expected="Gave up waiting (remote escape)", session_survives=True)
        scenario(guest, "tape load net on the session NIC ends on remote escape", session=0, active=0,
                 command="tape load net", stop_payload=ESC, stop_tag=DATA,
                 expected="Gave up waiting (remote escape)", session_survives=True)
        scenario(guest, "nic sniff on another NIC ends on remote escape", session=1, active=0,
                 command="nic sniff", stop_payload=ESC, stop_tag=DATA,
                 expected="Back at the fos shell", session_survives=True)
        scenario(guest, "nic sniff on the session NIC ends on remote escape", session=0, active=0,
                 command="nic sniff", stop_payload=ESC, stop_tag=DATA,
                 expected="Back at the fos shell", session_survives=True)
        scenario(guest, "nic discover ends on remote escape", session=0, active=0,
                 command="nic discover", stop_payload=ESC, stop_tag=DATA,
                 expected="Stopped discovery", session_survives=True)
        print("PASS: blocking commands started from a term net session can be ended remotely.")
    finally:
        guest.stop()


if __name__ == "__main__":
    main()
