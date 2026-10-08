#!/usr/bin/env python3
"""Static-IP two-guest regression test for the IPv4/UDP chat tape.

Two QEMU RTL8139 guests share a multicast-backed Ethernet segment. Each guest
loads and runs ``ip-chat.bin`` through its own ``term net`` session, then the
test drives both tapes remotely and checks ARP resolution plus UDP datagrams
in both directions. DHCP is covered by the tape's host unit tests; this
multicast fixture intentionally has no DHCP server.

Not part of ``make test``; run it after ``make tape-ip-chat`` (or ``make
tapes``) and ``make image``.
"""

import os
import selectors
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.59"
MCAST_PORT = 4259
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "ip-chat.bin"
# FOS_V2=1 runs this test against the v2 kernel (see tapes/test_router_e2e.py).
V2 = os.environ.get("FOS_V2") == "1"
if V2:
    IMAGE = ROOT / "build" / "fosfor" / "fosfor.img"
    TAPE = ROOT / "build" / "cargo-target" / "x86_64-fos-tape" / "release" / "ip-chat"
WAIT_MARKER = b"M3 complete" if V2 else b"Waiting for a remote terminal client to connect"
CHAT_ETHERTYPE = bytes((0x88, 0xB6))
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

GUESTS = {
    "a": {
        "mac": bytes.fromhex("525400123461"),
        "client": bytes.fromhex("02deadbeef31"),
        "ip": b"192.0.2.10",
        "peer": b"192.0.2.11",
    },
    "b": {
        "mac": bytes.fromhex("525400123462"),
        "client": bytes.fromhex("02deadbeef32"),
        "ip": b"192.0.2.11",
        "peer": b"192.0.2.10",
    },
}


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + CHAT_ETHERTYPE + bytes((tag,)) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} does not exist; run `make image` first.")
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-ip-chat` first.")

    selector = selectors.DefaultSelector()
    guests: dict[str, subprocess.Popen[bytes]] = {}
    output: dict[str, bytearray] = {}

    def start_guest(name: str) -> None:
        config = GUESTS[name]
        guest = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive",
                f"format=raw,file={IMAGE}",
                "-snapshot",
                "-serial",
                "stdio",
                "-monitor",
                "none",
                "-no-reboot",
                "-no-shutdown",
                "-netdev",
                f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
                "-device",
                f"{'e1000' if V2 else 'rtl8139'},netdev=net0,mac={send_tape.format_mac(config['mac'])}",
                "-display",
                "none",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        guests[name] = guest
        output[name] = bytearray()
        assert guest.stdout is not None
        selector.register(guest.stdout, selectors.EVENT_READ, name)

    def read_until(name: str, marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in output[name][start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r} from {name}; output since checkpoint:\n"
                    f"{output[name][start:].decode('utf-8', 'replace')}"
                )
            for key, _ in selector.select(remaining):
                guest_name = key.data
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(
                        f"{guest_name} serial stream ended early:\n"
                        f"{output[guest_name].decode('utf-8', 'replace')}"
                    )
                output[guest_name].extend(chunk)
        return bytes(output[name][start:])

    def send_input(name: str, text: bytes) -> None:
        config = GUESTS[name]
        mcast.send(
            term_frame(
                config["mac"],
                config["client"],
                TERM_TAG_DATA,
                text,
            )
        )

    def run_shell_command(name: str, command: bytes, timeout: float = 10) -> bytes:
        start = len(output[name])
        send_input(name, command + b"\n")
        return read_until(name, b"fos> ", timeout, start)

    def load_tape(name: str) -> None:
        start = len(output[name])
        send_input(name, b"tape load net\n")
        time.sleep(0.5)
        send_tape.send_tape(
            mcast,
            GUESTS[name]["mac"],
            bytes.fromhex("02deadbeef39" if name == "a" else "02deadbeef3a"),
            TAPE.read_bytes(),
            delay=0.003,
        )
        read_until(name, b"Tape loaded", 30, start)
        read_until(name, b"fos> ", 5, start)

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        for name in ("a", "b"):
            start_guest(name)
            read_until(name, WAIT_MARKER, 120 if V2 else 30)
            config = GUESTS[name]
            mcast.send(term_frame(config["mac"], config["client"], TERM_TAG_HELLO))
            read_until(name, b"Remote terminal connected", 10)
            read_until(name, b"fos> ", 5)

        load_tape("a")
        load_tape("b")

        # Exercises the CIDR `/PREFIX` form (matching telnet/audio-rtp/
        # router's identical convention), not just the legacy dotted
        # netmask -- a real gap found while building AGENTS.md's
        # cross-tape command-syntax reference: ip-chat used to accept
        # only a dotted NETMASK word, unlike every other tape that takes
        # a network prefix.
        for name in ("a", "b"):
            config = GUESTS[name]
            start = len(output[name])
            send_input(
                name,
                b"tape run static "
                + config["ip"]
                + b"/24 0.0.0.0 "
                + config["peer"]
                + b"\n",
            )
            read_until(name, b"IPv4 chat ready.", 10, start)

        start = len(output["b"])
        send_input("a", b"hello from A\n")
        read_until("b", b"192.0.2.10: hello from A", 10, start)
        print("PASS: static IPv4 chat sent A-to-B after ARP resolution.")

        start = len(output["a"])
        send_input("b", b"hello from B\n")
        read_until("a", b"192.0.2.11: hello from B", 10, start)
        print("PASS: static IPv4 chat sent B-to-A over UDP.")

        start = len(output["a"])
        send_input("a", b"/peers\n")
        read_until("a", b"192.0.2.11", 10, start)
        print("PASS: the tape recorded the IPv4 peer after receiving traffic.")

        start = len(output["a"])
        send_input("a", b"/broadcast\n")
        read_until("a", b"Outgoing target is now IPv4 broadcast.", 10, start)
        start = len(output["b"])
        send_input("a", b"broadcast from A\n")
        read_until("b", b"192.0.2.10: broadcast from A", 10, start)
        print("PASS: /broadcast selected the IPv4 broadcast destination.")

        for name in ("a", "b"):
            start = len(output[name])
            send_input(name, b"/quit\n")
            read_until(name, b"Left IPv4 chat; back at the fos shell.", 10, start)
            read_until(name, b"Tape returned; back at the fos shell.", 10, start)

        recovery = run_shell_command("a", b"about")
        if (b"fos is a small educational 64-bit x86 operating system." if V2 else b"fos is a small educational 32-bit x86 operating system.") not in recovery:
            raise SystemExit("shell did not recover after the IPv4 chat tape returned")
        print("PASS: IPv4 chat returned cleanly and the remote shell stayed usable.")

        for name in ("a", "b"):
            config = GUESTS[name]
            mcast.send(term_frame(config["mac"], config["client"], TERM_TAG_BYE))
    finally:
        for guest in guests.values():
            guest.terminate()
        for guest in guests.values():
            try:
                guest.wait(timeout=5)
            except subprocess.TimeoutExpired:
                guest.kill()


if __name__ == "__main__":
    main()
