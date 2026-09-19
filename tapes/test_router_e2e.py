#!/usr/bin/env python3
"""End-to-end test for the static IPv4 router tape.

One fos guest has two Intel e1000 interfaces attached to two isolated
multicast Ethernet segments.  Host-injected ARP requests teach the router
both neighboring MAC addresses; IPv4 frames then have to cross the guest with
the egress Ethernet address changed and the IPv4 TTL decremented.
"""

import os
import selectors
import socket
import struct
import subprocess
import time
from pathlib import Path

import sys

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "router.bin"
SEGMENT_A = ("230.42.42.80", 4280)
SEGMENT_B = ("230.42.42.81", 4281)
CHAT_ETHERTYPE = bytes((0x88, 0xB6))
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22
ROUTER_MAC_A = bytes.fromhex("525400cafe01")
ROUTER_MAC_B = bytes.fromhex("525400cafe02")
LEFT_MAC = bytes.fromhex("525400cafe03")
RIGHT_MAC = bytes.fromhex("525400cafe04")
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef41")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef42")
LEFT_IP = bytes((10, 0, 1, 2))
ROUTER_IP_A = bytes((10, 0, 1, 1))
RIGHT_IP = bytes((10, 0, 2, 2))
ROUTER_IP_B = bytes((10, 0, 2, 1))


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

    def receive(self, timeout: float) -> bytes | None:
        self.sock.settimeout(timeout)
        try:
            return self.sock.recv(4096)
        except TimeoutError:
            return None

    def close(self) -> None:
        self.sock.close()


def pad(frame: bytes) -> bytes:
    return frame + bytes(max(0, 60 - len(frame)))


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    return pad(destination + source + CHAT_ETHERTYPE + bytes((tag,)) + payload)


def ethernet_frame(destination: bytes, source: bytes, ethertype: int, payload: bytes) -> bytes:
    return pad(destination + source + ethertype.to_bytes(2, "big") + payload)


def checksum(data: bytes) -> int:
    if len(data) & 1:
        data += b"\0"
    total = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return (~total) & 0xFFFF


def arp_request(source_mac: bytes, source_ip: bytes, target_ip: bytes) -> bytes:
    payload = struct.pack(
        "!HHBBH6s4s6s4s",
        1,
        0x0800,
        6,
        4,
        1,
        source_mac,
        source_ip,
        b"\0" * 6,
        target_ip,
    )
    return ethernet_frame(b"\xff" * 6, source_mac, 0x0806, payload)


def ipv4_frame(
    destination_mac: bytes,
    source_mac: bytes,
    source_ip: bytes,
    destination_ip: bytes,
    payload: bytes,
    ttl: int = 64,
) -> bytes:
    header = struct.pack(
        "!BBHHHBBH4s4s",
        0x45,
        0,
        20 + len(payload),
        0x1234,
        0,
        ttl,
        17,
        0,
        source_ip,
        destination_ip,
    )
    header = header[:10] + checksum(header).to_bytes(2, "big") + header[12:]
    return ethernet_frame(destination_mac, source_mac, 0x0800, header + payload)


def wait_for_frame(
    link: McastSocket,
    predicate,
    timeout: float,
    description: str,
) -> bytes:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        frame = link.receive(min(0.2, deadline - time.monotonic()))
        if frame is not None and predicate(frame):
            return frame
    raise SystemExit(f"timed out waiting for {description}")


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} does not exist; run `make image` first.")
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-router` first.")

    links = {"a": McastSocket(*SEGMENT_A), "b": McastSocket(*SEGMENT_B)}
    selector = selectors.DefaultSelector()
    process = None
    output = bytearray()

    def read_until(marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in output[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r}; output since checkpoint:\n"
                    f"{output[start:].decode('utf-8', 'replace')}"
                )
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(
                        "router serial stream ended early:\n"
                        + output.decode("utf-8", "replace")
                    )
                output.extend(chunk)
        return bytes(output[start:])

    def send_command(command: bytes) -> int:
        start = len(output)
        links["a"].send(
            term_frame(ROUTER_MAC_A, TERM_CLIENT_MAC, TERM_TAG_DATA, command + b"\n")
        )
        return start

    try:
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
            "-netdev",
            f"socket,id=net0,mcast={SEGMENT_A[0]}:{SEGMENT_A[1]}",
            "-device",
            f"e1000,netdev=net0,mac={send_tape.format_mac(ROUTER_MAC_A)}",
            "-netdev",
            f"socket,id=net1,mcast={SEGMENT_B[0]}:{SEGMENT_B[1]}",
            "-device",
            f"e1000,netdev=net1,mac={send_tape.format_mac(ROUTER_MAC_B)}",
        ]
        process = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        assert process.stdout is not None
        selector.register(process.stdout, selectors.EVENT_READ)
        read_until(b"Waiting for a remote terminal client to connect", 30)
        links["a"].send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"fos> ", 10)

        load_start = send_command(b"tape load net")
        time.sleep(0.5)
        send_tape.send_tape(
            links["a"],
            ROUTER_MAC_A,
            TAPE_SENDER_MAC,
            TAPE.read_bytes(),
            delay=0.003,
        )
        read_until(b"Tape loaded", 30, load_start)
        read_until(b"fos> ", 5, load_start)

        run_start = send_command(
            b"tape run router 1 10.0.1.1 255.255.255.0 "
            b"2 10.0.2.1 255.255.255.0"
        )
        read_until(b"fos router: 2 interface(s)", 10, run_start)
        print("PASS: router tape started with two configured e1000 interfaces.")

        links["a"].send(arp_request(LEFT_MAC, LEFT_IP, ROUTER_IP_A))
        wait_for_frame(
            links["a"],
            lambda frame: frame[0:6] == LEFT_MAC
            and frame[6:12] == ROUTER_MAC_A
            and frame[12:14] == b"\x08\x06",
            10,
            "an ARP reply on segment A",
        )
        links["b"].send(arp_request(RIGHT_MAC, RIGHT_IP, ROUTER_IP_B))
        wait_for_frame(
            links["b"],
            lambda frame: frame[0:6] == RIGHT_MAC
            and frame[6:12] == ROUTER_MAC_B
            and frame[12:14] == b"\x08\x06",
            10,
            "an ARP reply on segment B",
        )
        print("PASS: router answered ARP on both configured interfaces.")

        links["a"].send(
            ipv4_frame(
                ROUTER_MAC_A,
                LEFT_MAC,
                LEFT_IP,
                RIGHT_IP,
                b"router a to b",
            )
        )
        forwarded = wait_for_frame(
            links["b"],
            lambda frame: frame[0:6] == RIGHT_MAC
            and frame[6:12] == ROUTER_MAC_B
            and frame[12:14] == b"\x08\x00",
            10,
            "an IPv4 frame forwarded from A to B",
        )
        assert forwarded[14 + 8] == 63, forwarded.hex()
        assert forwarded[14 + 12 : 14 + 16] == LEFT_IP
        assert forwarded[14 + 16 : 14 + 20] == RIGHT_IP
        assert checksum(forwarded[14 : 14 + 20]) == 0
        print("PASS: router forwarded A to B with TTL and checksum updated.")

        links["b"].send(
            ipv4_frame(
                ROUTER_MAC_B,
                RIGHT_MAC,
                RIGHT_IP,
                LEFT_IP,
                b"router b to a",
            )
        )
        forwarded = wait_for_frame(
            links["a"],
            lambda frame: frame[0:6] == LEFT_MAC
            and frame[6:12] == ROUTER_MAC_A
            and frame[12:14] == b"\x08\x00",
            10,
            "an IPv4 frame forwarded from B to A",
        )
        assert forwarded[14 + 8] == 63, forwarded.hex()
        assert checksum(forwarded[14 : 14 + 20]) == 0
        print("PASS: router forwarded B to A with TTL and checksum updated.")

        stop_start = len(output)
        for _ in range(5):
            links["a"].send(term_frame(ROUTER_MAC_A, TERM_CLIENT_MAC, TERM_TAG_DATA, b"\x1b"))
            time.sleep(0.05)
        read_until(b"Router stopped;", 10, stop_start)
        read_until(b"Tape returned; back at the fos shell.", 10, stop_start)
        send_command(b"about")
        read_until(b"fos> ", 10)
        print("PASS: router stopped cleanly and the remote shell remained usable.")
    finally:
        for link in links.values():
            link.close()
        selector.close()
        if process is not None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()


if __name__ == "__main__":
    main()
