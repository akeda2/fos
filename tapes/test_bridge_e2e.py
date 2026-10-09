#!/usr/bin/env python3
"""End-to-end test for the Rust layer-2 bridge tape.

The bridge guest has two Intel e1000 interfaces, each attached to a separate
multicast Ethernet segment. A raw frame injected on one segment must arrive at
the receiver guest on the other segment, in both directions. The bridge is
loaded and controlled through the same remote `term net` path a real
headless host would use.

This is intentionally outside `make test`: it starts three QEMU guests and
uses multicast socket netdevs to model two isolated Ethernet segments.
"""

import os
import re
import selectors
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "bridge.bin"
# FOS_V2=1 runs this test against the v2 kernel (see tapes/test_router_e2e.py).
V2 = os.environ.get("FOS_V2") == "1"
if V2:
    IMAGE = ROOT / "build" / "fosfor" / "fosfor.img"
    TAPE = ROOT / "build" / "cargo-target" / "x86_64-fos-tape" / "release" / "bridge"
WAIT_MARKER = b"M3 complete" if V2 else b"Waiting for a remote terminal client to connect"
SEGMENT_A = ("230.42.42.70", 4270)
SEGMENT_B = ("230.42.42.71", 4271)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22
CHAT_ETHERTYPE = bytes((0x88, 0xB6))
BRIDGE_MAC_A = bytes.fromhex("525400beef01")
BRIDGE_MAC_B = bytes.fromhex("525400beef02")
SENDER_MAC = bytes.fromhex("525400beef03")
# QEMU's multicast netdev loops a guest's own frames back to it, so the bridge sees its forwarded copy
# arrive on the far port and learns the source MAC there. The first frame therefore uses a source MAC
# nobody listens on. The same loop-back mis-learns the guests' own terminal replies, so the test frames
# are broadcasts, which are always flooded, instead of unicasts that depend on a racy learning table.
GHOST_MAC = bytes.fromhex("525400beef05")
GHOST_MAC_B = bytes.fromhex("525400beef06")
BROADCAST = b"\xff" * 6
RECEIVER_MAC = bytes.fromhex("525400beef04")
BRIDGE_CLIENT_MAC = bytes.fromhex("02deadbeef31")
SENDER_CLIENT_MAC = bytes.fromhex("02deadbeef32")
RECEIVER_CLIENT_MAC = bytes.fromhex("02deadbeef33")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef34")


class McastSocket:
    def __init__(self, group: str, port: int):
        self.group = group
        self.port = port
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", port))
        membership = struct.pack("4sl", socket.inet_aton(group), socket.INADDR_ANY)
        self.sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, membership)
        self.sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)

    def send(self, frame: bytes) -> None:
        self.sock.sendto(frame, (self.group, self.port))

    def close(self) -> None:
        self.sock.close()


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + CHAT_ETHERTYPE + bytes((tag,)) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def ethernet_frame(destination: bytes, source: bytes, ethertype: int, payload: bytes) -> bytes:
    frame = bytearray(destination + source + ethertype.to_bytes(2, "big") + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} does not exist; run `make image` first.")
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-bridge` first.")

    multicast = {
        "a": McastSocket(*SEGMENT_A),
        "b": McastSocket(*SEGMENT_B),
    }
    selector = selectors.DefaultSelector()
    guests: dict[str, subprocess.Popen[bytes]] = {}
    output: dict[str, bytearray] = {}

    def start_guest(name: str, devices: list[tuple[str, str, bytes]]) -> None:
        command = [
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
            "-display",
            "none",
        ]
        for number, (segment, model, mac) in enumerate(devices):
            group, port = SEGMENT_A if segment == "a" else SEGMENT_B
            command.extend(
                [
                    "-netdev",
                    f"socket,id=net{number},mcast={group}:{port}",
                    "-device",
                    f"{model},netdev=net{number},mac={send_tape.format_mac(mac)}",
                ]
            )
        process = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        assert process.stdout is not None
        guests[name] = process
        output[name] = bytearray()
        selector.register(process.stdout, selectors.EVENT_READ, name)

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

    def send_command(
        segment: str,
        destination: bytes,
        source: bytes,
        guest_name: str,
        command: bytes,
    ) -> int:
        start = len(output[guest_name])
        multicast[segment].send(term_frame(destination, source, TERM_TAG_DATA, command + b"\n"))
        return start

    def run_command(
        segment: str,
        destination: bytes,
        source: bytes,
        guest_name: str,
        command: bytes,
        timeout: float = 10,
    ) -> bytes:
        start = send_command(segment, destination, source, guest_name, command)
        return read_until(guest_name, b"fos> ", timeout, start)

    def receive_expected_frame(
        segment: str,
        destination: bytes,
        source: bytes,
        guest_name: str,
        frame: bytes,
        expected: bytes,
        inject_segment: str,
    ) -> bytes:
        """Run `nic recv` on `segment`, injecting once on `inject_segment` and tolerating stale queued frames.

        A terminal frame used to enter `term net` can still be waiting in a
        polling NIC ring when a nested shell command starts, and a
        machine's periodic name announcement (a broadcast) can come first too. `nic recv` is a
        deliberately raw shell diagnostic and therefore reports that frame
        instead of applying the terminal classifier. Retry the command until
        the frame under test is the one returned.
        """
        start = send_command(segment, destination, source, guest_name, b"nic recv")
        time.sleep(0.2)
        multicast[inject_segment].send(frame)
        for attempt in range(10):
            result = read_until(guest_name, b"fos> ", 10, start)
            if expected in result:
                return result
            if attempt == 9:
                break
            start = send_command(segment, destination, source, guest_name, b"nic recv")
        for _ in range(3):
            multicast["a"].send(term_frame(BRIDGE_MAC_A, BRIDGE_CLIENT_MAC, TERM_TAG_DATA, b"\x1b"))
            time.sleep(0.05)
        try:
            read_until("bridge", b"Bridge stopped;", 5, len(output["bridge"]) - 10)
        except SystemExit:
            pass
        raise SystemExit(
            f"{guest_name} did not receive the expected bridged frame:\n"
            f"{output[guest_name].decode('utf-8', 'replace')}\n--- bridge:\n"
            f"{output['bridge'][-1500:].decode('utf-8', 'replace')}"
        )

    try:
        start_guest(
            "bridge",
            [("a", "e1000", BRIDGE_MAC_A), ("b", "e1000", BRIDGE_MAC_B)],
        )
        read_until("bridge", WAIT_MARKER, 120 if V2 else 30)
        multicast["a"].send(term_frame(b"\xff" * 6, BRIDGE_CLIENT_MAC, TERM_TAG_HELLO))
        read_until("bridge", b"fos> ", 10)

        start_guest("sender", [("a", "e1000", SENDER_MAC)])
        read_until("sender", WAIT_MARKER, 120 if V2 else 30)
        multicast["a"].send(term_frame(b"\xff" * 6, SENDER_CLIENT_MAC, TERM_TAG_HELLO))
        read_until("sender", b"fos> ", 10)

        start_guest("receiver", [("b", "e1000", RECEIVER_MAC)])
        read_until("receiver", WAIT_MARKER, 120 if V2 else 30)
        multicast["b"].send(term_frame(b"\xff" * 6, RECEIVER_CLIENT_MAC, TERM_TAG_HELLO))
        read_until("receiver", b"fos> ", 10)

        load_start = send_command(
            "a", BRIDGE_MAC_A, BRIDGE_CLIENT_MAC, "bridge", b"tape load net"
        )
        time.sleep(0.5)
        send_tape.send_tape(
            multicast["a"],
            BRIDGE_MAC_A,
            TAPE_SENDER_MAC,
            TAPE.read_bytes(),
            delay=0.003,
        )
        read_until("bridge", b"Tape loaded", 30, load_start)
        read_until("bridge", b"fos> ", 5, load_start)
        # Exercises the documented `bridge` keyword form (`tape run
        # bridge 1 2`), not just the bare `tape run 1 2` form -- a real
        # doc/parser mismatch found while building AGENTS.md's cross-tape
        # command-syntax reference: parse_ports used to reject this
        # exact form its own module doc comment and usage string
        # document, since it fed the argument bytes straight to
        # parse_number with no leading-keyword handling at all.
        run_start = send_command(
            "a", BRIDGE_MAC_A, BRIDGE_CLIENT_MAC, "bridge", b"tape run bridge 1 2"
        )
        read_until("bridge", b"fos bridge: ports 1", 10, run_start)
        print("PASS: bridge tape started with two selected e1000 interfaces (via the documented 'bridge' keyword form).")

        expected = b"dst ff:ff:ff:ff:ff:ff src 52:54:00:be:ef:05 ethertype 0x88b7"
        receive_expected_frame(
            "b",
            RECEIVER_MAC,
            RECEIVER_CLIENT_MAC,
            "receiver",
            ethernet_frame(BROADCAST, GHOST_MAC, 0x88B7, b"bridge a to b"),
            expected,
            "a",
        )
        print("PASS: bridge forwarded a frame from segment A to segment B.")

        expected = b"dst ff:ff:ff:ff:ff:ff src 52:54:00:be:ef:06 ethertype 0x88b8"
        receive_expected_frame(
            "a",
            SENDER_MAC,
            SENDER_CLIENT_MAC,
            "sender",
            ethernet_frame(BROADCAST, GHOST_MAC_B, 0x88B8, b"bridge b to a"),
            expected,
            "b",
        )
        print("PASS: bridge forwarded a frame from segment B to segment A.")

        stop_start = len(output["bridge"])
        for _ in range(3):
            multicast["a"].send(
                term_frame(BRIDGE_MAC_A, BRIDGE_CLIENT_MAC, TERM_TAG_DATA, b"\x1b")
            )
            time.sleep(0.05)
        read_until("bridge", b"Bridge stopped;", 10, stop_start)
        read_until("bridge", b"Tape returned; back at the fos shell.", 10, stop_start)
        read_until("bridge", b"fos> ", 10, stop_start)
        irq_status = run_command("a", BRIDGE_MAC_A, BRIDGE_CLIENT_MAC, "bridge", b"nic")
        notification_counts = [
            int(count) for count in re.findall(rb"NIC IRQ\d+: (\d+) notification", irq_status)
        ]
        if not notification_counts or not any(notification_counts):
            raise SystemExit(
                "bridge received live e1000 traffic but reported no NIC interrupt notifications:\n"
                f"{irq_status.decode('utf-8', 'replace')}"
            )
        print("PASS: bridge e1000 IRQ handler acknowledged live network traffic.")
        run_command("a", BRIDGE_MAC_A, BRIDGE_CLIENT_MAC, "bridge", b"about")
        print("PASS: bridge stopped cleanly and the remote shell remained usable.")
    finally:
        for socket_instance in multicast.values():
            socket_instance.close()
        for process in guests.values():
            process.terminate()
        for process in guests.values():
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()


if __name__ == "__main__":
    main()
