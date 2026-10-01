#!/usr/bin/env python3
"""End-to-end test for bare-name script/tape dispatch and the `settings` file.

1. Bare names (`-nic none`, serial only): `NAME.sh` runs as a script,
   `NAME.bin` loads and runs as a tape with startup arguments, `.sh` beats
   `.bin` with a note, a builtin always beats a same-named file, a script
   that names itself stops at the nesting limit, and an unknown word is
   still "Unknown command".
2. `settings` parsing (`-nic none`): `deny-term-net` accepts commas and
   spaces, and an unknown option or bad NIC number is reported, not fatal.
3. `deny-term-net 1` with two NICs on two multicast segments: a `HELLO` on
   NIC 1's segment is ignored, one on NIC 2's segment opens a session, and
   KUP (enabled via a `debug` file) answers only on NIC 2.
4. NETFS, served at the idle prompt, likewise answers only on NIC 2.

Run via `make test-scripts-settings`.
"""

import os
import selectors
import socket as socketlib
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BUILD = ROOT / "build"
BUILD_IMAGE = ROOT / "tools" / "build_image.py"

CHAT_ETHERTYPE = b"\x88\xb6"
TERM_TAG_HELLO = 0x20
HOST_MAC = bytes.fromhex("02deadbeef20")
GUEST_MACS = [bytes.fromhex("525400123811"), bytes.fromhex("525400123812")]
GROUPS = [("230.42.42.81", 4281), ("230.42.42.82", 4282)]


def build_image(path: Path, files: dict[str, bytes | Path]) -> None:
    args = [
        sys.executable, str(BUILD_IMAGE),
        "--stage1", str(BUILD / "stage1.bin"), "--stage2", str(BUILD / "stage2.bin"),
        "--kernel", str(BUILD / "kernel.bin"), "--output", str(path),
    ]
    for name, content in files.items():
        if isinstance(content, bytes):
            source = path.with_name(f"{path.stem}.{name}.src")
            source.write_bytes(content)
        else:
            source = content
        args += ["--file", name, str(source)]
    subprocess.run(args, check=True, stdout=subprocess.DEVNULL)


class Guest:
    def __init__(self, image: Path, extra_args: list[str]):
        self.output = bytearray()
        self.selector = selectors.DefaultSelector()
        self.process = subprocess.Popen(
            ["qemu-system-i386", "-drive", f"format=raw,file={image}", "-serial", "stdio",
             "-display", "none", "-snapshot", *extra_args],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        self.selector.register(self.process.stdout, selectors.EVENT_READ)

    def read_until(self, marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in self.output[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r}; output since checkpoint:\n"
                    f"{self.output[start:].decode('utf-8', 'replace')}"
                )
            for key, _ in self.selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(f"serial ended early:\n{self.output.decode('utf-8', 'replace')}")
                self.output.extend(chunk)
        return bytes(self.output[start:])

    def run(self, command: str, timeout: float = 15) -> str:
        start = len(self.output)
        self.process.stdin.write((command + "\r\n").encode())
        self.process.stdin.flush()
        return self.read_until(b"fos> ", timeout, start).decode("utf-8", "replace")

    def terminate(self) -> None:
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def expect(text: str, *needles: str) -> None:
    for needle in needles:
        if needle not in text:
            raise SystemExit(f"expected {needle!r} in:\n{text}")


def expect_absent(text: str, *needles: str) -> None:
    for needle in needles:
        if needle in text:
            raise SystemExit(f"did not expect {needle!r} in:\n{text}")


def test_bare_names(tmp: Path) -> None:
    image = tmp / "bare.img"
    build_image(image, {
        "hi.sh": b"# comment\nabout\n",
        "calc.bin": BUILD / "tapes" / "calc.bin",
        "both.sh": b"about\n",
        "both.bin": BUILD / "tapes" / "calc.bin",
        "ticks.sh": b"about\n",  # `ticks` is a builtin and must still win
        "loop.sh": b"loop\n",
        # Argument expansion: $1/$2, $* (whole text), $$ (literal), a missing
        # $3 (empty), and a nested script receiving the expanded words.
        "args.sh": b"calc $1 $2 +\nabout $$ [$3] [$*]\ninner $2 $1\n",
        "inner.sh": b"calc $1 $2 -\n",
    })
    guest = Guest(image, ["-nic", "none"])
    try:
        guest.read_until(b"fos> ", 20)
        out = guest.run("hi")
        expect(out, "hi.sh: running", "hi.sh line 2: about", "educational 32-bit x86")
        out = guest.run("calc 3 4 +")
        expect(out, "Loaded", "'calc.bin'", "Running")
        expect_absent(out, "'tape run' to execute it.")
        expect(out, "7")
        out = guest.run("both")
        expect(out, "note: both both.sh and both.bin exist", "both.sh: running")
        out = guest.run("ticks")
        expect_absent(out, "ticks.sh")
        out = guest.run("loop")
        expect(out, "scripts are nested too deeply")
        out = guest.run("args 9 4")
        expect(out, "args.sh line 1: calc 9 4 +", "13",
               "args.sh line 2: about $ [] [9 4]",
               "args.sh line 3: inner 4 9", "inner.sh line 1: calc 4 9 -", "-5")
        out = guest.run("args")
        expect(out, "args.sh line 1: calc   +")
        out = guest.run("nosuchcmd")
        expect(out, "Unknown command")
        out = guest.run("toolongcommandname")
        expect(out, "Unknown command")
        print("PASS: bare-name scripts and tapes dispatch; builtins win; nesting is bounded.")
    finally:
        guest.terminate()


def test_settings_parsing(tmp: Path) -> None:
    image = tmp / "parse.img"
    build_image(image, {
        "settings": b"# demo\ndeny-term-net 1,3\ndeny-term-net 2, 5\nbogus 1\ndeny-term-net x 99\n",
    })
    guest = Guest(image, ["-nic", "none"])
    try:
        out = guest.read_until(b"fos> ", 20).decode("utf-8", "replace")
        expect(
            out,
            "settings: term net, KUP and NETFS denied on NIC 1 2 3 5.",
            "settings: unknown option 'bogus'.",
            "settings: bad NIC number 'x'",
            "settings: bad NIC number '99'",
        )
        print("PASS: settings file parses deny-term-net lists and reports bad lines.")
    finally:
        guest.terminate()


class Mcast:
    def __init__(self, group: str, port: int):
        self.group, self.port = group, port
        self.sock = socketlib.socket(socketlib.AF_INET, socketlib.SOCK_DGRAM, socketlib.IPPROTO_UDP)
        self.sock.setsockopt(socketlib.SOL_SOCKET, socketlib.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", port))
        mreq = struct.pack("4sl", socketlib.inet_aton(group), socketlib.INADDR_ANY)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_ADD_MEMBERSHIP, mreq)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_MULTICAST_TTL, 1)

    def send(self, frame: bytes) -> None:
        self.sock.sendto(frame.ljust(60, b"\0"), (self.group, self.port))

    def receive(self, timeout: float) -> bytes | None:
        self.sock.settimeout(timeout)
        try:
            return self.sock.recv(2048)
        except TimeoutError:
            return None

    def hello(self, destination: bytes) -> None:
        self.send(destination + HOST_MAC + CHAT_ETHERTYPE + bytes([TERM_TAG_HELLO]))


def kup_probe(sender: "Mcast", guest_mac: bytes) -> bool:
    """Sends an oversized KUP PUT_REQUEST (a reply of KUP_TAG_PUT_ERROR,
    TOO_LARGE, proves KUP is listening) and reports whether any KUP reply came."""
    payload = (0x7FFFFFF0).to_bytes(4, "little") + (0).to_bytes(2, "little")
    sender.send(guest_mac + HOST_MAC + CHAT_ETHERTYPE + bytes([0x3A]) + payload)
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        packet = sender.receive(0.3)
        if packet and packet[0:6] == HOST_MAC and packet[12:14] == CHAT_ETHERTYPE and packet[14] in (0x3D, 0x3E):
            return True
    return False


def test_hello_denied(tmp: Path) -> None:
    image = tmp / "deny.img"
    build_image(image, {"settings": b"deny-term-net 1\n", "debug": b"kernel-update\n"})
    extra = []
    for index, (group, port) in enumerate(GROUPS):
        extra += [
            "-netdev", f"socket,id=net{index},mcast={group}:{port}",
            "-device", f"rtl8139,netdev=net{index},mac={GUEST_MACS[index].hex(':')}",
        ]
    guest = Guest(image, extra)
    try:
        guest.read_until(b"Waiting for a remote terminal client to connect;", 20)
        senders = [Mcast(*group) for group in GROUPS]
        checkpoint = len(guest.output)
        for _ in range(5):
            senders[0].hello(GUEST_MACS[0])
            time.sleep(0.2)
        time.sleep(1)
        if b"Remote terminal connected" in guest.output[checkpoint:]:
            raise SystemExit("a HELLO on denied NIC 1 opened a session")
        deadline = time.monotonic() + 15
        while b"Remote terminal connected" not in guest.output[checkpoint:]:
            if time.monotonic() > deadline:
                raise SystemExit(
                    "a HELLO on allowed NIC 2 never opened a session:\n"
                    + guest.output[checkpoint:].decode("utf-8", "replace")
                )
            senders[1].hello(GUEST_MACS[1])
            guest.selector.select(0.3)
            for key, _ in guest.selector.select(0):
                guest.output.extend(os.read(key.fd, 4096))
        # The first chunk may end mid-line; wait for the NIC number itself.
        guest.read_until(b"on NIC 2", 5, checkpoint)
        print("PASS: term net ignores HELLO on a denied NIC and accepts it on another.")
        # The session is still active, and its loop also dispatches KUP.
        if kup_probe(senders[0], GUEST_MACS[0]):
            raise SystemExit("KUP answered on denied NIC 1")
        if not kup_probe(senders[1], GUEST_MACS[1]):
            raise SystemExit("KUP did not answer on allowed NIC 2")
        print("PASS: KUP is ignored on a denied NIC and still served on another.")
    finally:
        guest.terminate()


def test_netfs_denied(tmp: Path) -> None:
    """NETFS is only served at the idle prompt, so leave the `term net` wait
    with F12 (via the QEMU monitor), then ask both NICs for a missing file:
    only the allowed NIC may answer (with NETFS_TAG_GET_ERROR, 0x34)."""
    image = tmp / "netfs.img"
    build_image(image, {"settings": b"deny-term-net 1\n"})
    monitor = str(tmp / "netfs.mon")
    extra = ["-monitor", f"unix:{monitor},server,nowait"]
    for index, (group, port) in enumerate(GROUPS):
        extra += [
            "-netdev", f"socket,id=net{index},mcast={group}:{port}",
            "-device", f"rtl8139,netdev=net{index},mac={GUEST_MACS[index].hex(':')}",
        ]
    guest = Guest(image, extra)
    try:
        guest.read_until(b"Waiting for a remote terminal client to connect;", 20)
        for _ in range(50):
            try:
                with socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM) as mon:
                    mon.connect(monitor)
                    mon.sendall(b"sendkey f12\n")
                break
            except (FileNotFoundError, ConnectionRefusedError):
                time.sleep(0.1)
        guest.read_until(b"fos> ", 10)
        senders = [Mcast(*group) for group in GROUPS]

        def get_reply(sender: Mcast, mac: bytes) -> bool:
            request = mac + HOST_MAC + CHAT_ETHERTYPE + bytes([0x30, 0]) + b"nosuchfile"
            sender.send(request)
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                packet = sender.receive(0.3)
                if packet and packet[0:6] == HOST_MAC and packet[12:14] == CHAT_ETHERTYPE and packet[14] == 0x34:
                    return True
            return False

        if get_reply(senders[0], GUEST_MACS[0]):
            raise SystemExit("NETFS answered on denied NIC 1")
        if not get_reply(senders[1], GUEST_MACS[1]):
            raise SystemExit("NETFS did not answer on allowed NIC 2")
        print("PASS: NETFS is ignored on a denied NIC and still served on another.")
    finally:
        guest.terminate()


def main() -> None:
    with tempfile.TemporaryDirectory() as name:
        tmp = Path(name)
        test_bare_names(tmp)
        test_settings_parsing(tmp)
        test_hello_denied(tmp)
        test_netfs_denied(tmp)


if __name__ == "__main__":
    main()
