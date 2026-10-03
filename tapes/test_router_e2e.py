#!/usr/bin/env python3
"""End-to-end test for the IPv4 router tape.

One fos guest has two Intel e1000 interfaces attached to two isolated
multicast Ethernet segments.  Host-injected ARP requests teach the router
both neighboring MAC addresses; IPv4 frames then have to cross the guest with
the egress Ethernet address changed and the IPv4 TTL decremented. The test
also supplies an upstream DHCP lease, verifies DNS propagation, and confirms
that more than the old small fixed lease count can be allocated downstream.
"""

import os
import selectors
import socket
import struct
import subprocess
import tempfile
import time
from pathlib import Path

import sys

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "router.bin"
# FOS_V2=1 runs this same test against the v2 kernel (UEFI image, 64-bit tape): a `qemu-system-i386`
# shim on PATH (v2/tools/qemu_v1_shim.py) turns the v1 command line into an OVMF one, and only
# the serial markers and file names below differ.
V2 = os.environ.get("FOS_V2") == "1"
if V2:
    IMAGE = ROOT / "build" / "v2" / "v2.img"
    TAPE = ROOT / "build" / "cargo-target" / "x86_64-fos-tape" / "release" / "router"
WAIT_MARKER = b"M3 complete" if V2 else b"Waiting for a remote terminal client to connect"
LOAD_MARKER = b"Waiting for a tape over the network" if V2 else b"Have the sender target this card's MAC address"
CONFIG = ROOT / "tapes" / "router" / "router.cfg"
SEGMENT_A = ("230.42.42.80", 4280)
SEGMENT_B = ("230.42.42.81", 4281)
CHAT_ETHERTYPE = bytes((0x88, 0xB6))
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22
ROUTER_MAC_A = bytes.fromhex("525400cafe01")
ROUTER_MAC_B = bytes.fromhex("525400cafe02")
UPSTREAM_MAC = bytes.fromhex("525400cafe06")
LEFT_MAC = bytes.fromhex("525400cafe03")
RIGHT_MAC = bytes.fromhex("525400cafe04")
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef41")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef42")
output = bytearray()
LEFT_IP = bytes((10, 0, 1, 2))
ROUTER_IP_A = bytes((10, 0, 1, 1))
RIGHT_IP = bytes((10, 0, 15, 2))
ROUTER_IP_B = bytes((10, 0, 15, 1))
DHCP_CLIENT_MAC = bytes.fromhex("525400cafe05")
DHCP_XID = 0x12345678


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


def send_monitor_command(path: Path, command: str, timeout: float = 5) -> None:
    """Sends one human-monitor command after QEMU creates its Unix socket."""
    deadline = time.monotonic() + timeout
    while True:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as monitor:
                monitor.settimeout(1)
                monitor.connect(str(path))
                monitor.sendall((command + "\n").encode())
                return
        except (FileNotFoundError, ConnectionRefusedError, TimeoutError):
            if time.monotonic() >= deadline:
                raise RuntimeError(f"could not connect to QEMU monitor at {path}")
            time.sleep(0.05)


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


def udp_datagram(source_port: int, destination_port: int, payload: bytes) -> bytes:
    return struct.pack("!HHHH", source_port, destination_port, 8 + len(payload), 0) + payload


def udp_datagram_with_checksum(
    source_ip: bytes, destination_ip: bytes, source_port: int, destination_port: int, payload: bytes
) -> bytes:
    datagram = udp_datagram(source_port, destination_port, payload)
    pseudo = struct.pack("!4s4sBBH", source_ip, destination_ip, 0, 17, len(datagram))
    checksum_value = checksum(pseudo + datagram) or 0xFFFF
    return datagram[:6] + checksum_value.to_bytes(2, "big") + datagram[8:]


def icmp_echo(message_type: int, identifier: int, sequence: int, payload: bytes = b"") -> bytes:
    message = struct.pack("!BBHHH", message_type, 0, 0, identifier, sequence) + payload
    return message[:2] + checksum(message).to_bytes(2, "big") + message[4:]


def tcp_segment(
    source_ip: bytes,
    destination_ip: bytes,
    source_port: int,
    destination_port: int,
    sequence: int,
    acknowledgment: int,
    flags: int,
) -> bytes:
    segment = struct.pack(
        "!HHIIHHHHH",
        source_port,
        destination_port,
        sequence,
        acknowledgment,
        5 << 12,
        flags,
        65535,
        0,
        0,
    )
    pseudo = struct.pack("!4s4sBBH", source_ip, destination_ip, 0, 6, len(segment))
    checksum_value = checksum(pseudo + segment)
    return segment[:16] + checksum_value.to_bytes(2, "big") + segment[18:]


def dhcp_packet(
    message_type: int,
    client_mac: bytes,
    requested_ip: bytes | None = None,
    server_identifier: bytes | None = None,
) -> bytes:
    header = struct.pack(
        "!BBBBIHH4s4s4s4s16s64s128s",
        1,
        1,
        6,
        0,
        DHCP_XID,
        0,
        0x8000,
        b"\0" * 4,
        b"\0" * 4,
        b"\0" * 4,
        b"\0" * 4,
        client_mac + b"\0" * 10,
        b"\0" * 64,
        b"\0" * 128,
    )
    options = bytearray((99, 130, 83, 99, 53, 1, message_type))
    if requested_ip is not None:
        options.extend((50, 4))
        options.extend(requested_ip)
    if server_identifier is not None:
        options.extend((54, 4))
        options.extend(server_identifier)
    options.append(255)
    return header + bytes(options)


def dhcp_frame(
    message_type: int,
    client_mac: bytes,
    requested_ip: bytes | None = None,
    server_identifier: bytes | None = None,
) -> bytes:
    payload = udp_datagram(
        68,
        67,
        dhcp_packet(message_type, client_mac, requested_ip, server_identifier),
    )
    return ipv4_frame(
        b"\xff" * 6,
        client_mac,
        b"\0" * 4,
        b"\xff" * 4,
        payload,
    )


def dhcp_server_frame(
    message_type: int,
    client_mac: bytes,
    transaction_id: int,
    offered_ip: bytes,
    server_ip: bytes,
    router_ip: bytes,
    dns_ip: bytes,
) -> bytes:
    header = struct.pack(
        "!BBBBIHH4s4s4s4s16s64s128s",
        2,
        1,
        6,
        0,
        transaction_id,
        0,
        0x8000,
        b"\0" * 4,
        offered_ip,
        server_ip,
        b"\0" * 4,
        client_mac + b"\0" * 10,
        b"\0" * 64,
        b"\0" * 128,
    )
    options = bytearray((99, 130, 83, 99))

    def option(code: int, value: bytes) -> None:
        options.extend((code, len(value)))
        options.extend(value)

    option(53, bytes((message_type,)))
    option(54, server_ip)
    option(1, bytes((255, 255, 255, 0)))
    option(3, router_ip)
    option(6, dns_ip)
    option(28, bytes((10, 0, 1, 255)))
    option(51, struct.pack("!I", 3600))
    option(58, struct.pack("!I", 1800))
    option(59, struct.pack("!I", 3150))
    options.append(255)
    payload = header + bytes(options)
    return ipv4_frame(
        b"\xff" * 6,
        UPSTREAM_MAC,
        server_ip,
        b"\xff" * 4,
        udp_datagram(67, 68, payload),
    )


def dhcp_message_type(frame: bytes) -> bytes | None:
    ip_header_length = (frame[14] & 0x0F) * 4
    payload = frame[14 + ip_header_length + 8 :]
    if len(payload) < 240 or payload[236:240] != b"\x63\x82\x53\x63":
        return None
    offset = 240
    while offset < len(payload):
        code = payload[offset]
        offset += 1
        if code == 255:
            return None
        if code == 0:
            continue
        if offset >= len(payload):
            return None
        length = payload[offset]
        offset += 1
        if offset + length > len(payload):
            return None
        if code == 53:
            return payload[offset : offset + length]
        offset += length
    return None


def dhcp_client_mac(frame: bytes) -> bytes:
    ip_header_length = (frame[14] & 0x0F) * 4
    payload = frame[14 + ip_header_length + 8 :]
    return payload[28:34]


def dhcp_transaction_id(frame: bytes) -> int:
    ip_header_length = (frame[14] & 0x0F) * 4
    payload = frame[14 + ip_header_length + 8 :]
    return int.from_bytes(payload[4:8], "big")


def is_upstream_discover(frame: bytes) -> bool:
    return (
        frame[6:12] == ROUTER_MAC_A
        and dhcp_message_type(frame) == b"\x01"
        and dhcp_client_mac(frame) == ROUTER_MAC_A
    )


def parse_dhcp(frame: bytes) -> tuple[bytes, dict[int, bytes]]:
    assert frame[12:14] == b"\x08\x00", frame.hex()
    ip_header_length = (frame[14] & 0x0F) * 4
    udp_start = 14 + ip_header_length
    assert frame[udp_start : udp_start + 4] == struct.pack("!HH", 67, 68)
    payload = frame[udp_start + 8 :]
    assert payload[236:240] == b"\x63\x82\x53\x63"
    options = {}
    offset = 240
    while offset < len(payload):
        code = payload[offset]
        offset += 1
        if code == 255:
            break
        if code == 0:
            continue
        length = payload[offset]
        offset += 1
        options[code] = payload[offset : offset + length]
        offset += length
    return payload[16:20], options


def ipv4_frame(
    destination_mac: bytes,
    source_mac: bytes,
    source_ip: bytes,
    destination_ip: bytes,
    payload: bytes,
    ttl: int = 64,
    protocol: int = 17,
) -> bytes:
    header = struct.pack(
        "!BBHHHBBH4s4s",
        0x45,
        0,
        20 + len(payload),
        0x1234,
        0,
        ttl,
        protocol,
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
    raise SystemExit(
        f"timed out waiting for {description}; router serial output:\n"
        f"{output.decode('utf-8', 'replace')}"
    )


def main() -> None:
    global output
    config_size = CONFIG.stat().st_size
    if config_size > 4096:
        raise SystemExit(
            f"{CONFIG} is {config_size} bytes; router @CONFIG input is limited to 4096 bytes"
        )
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} does not exist; run `make image` first.")
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-router` first.")

    links = {"a": McastSocket(*SEGMENT_A), "b": McastSocket(*SEGMENT_B)}
    selector = selectors.DefaultSelector()
    process = None
    output = bytearray()
    monitor_dir = tempfile.TemporaryDirectory(prefix="fos-router-monitor-")
    monitor_path = Path(monitor_dir.name) / "monitor.sock"

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
            "-m",
            "256",
            "-drive",
            f"format=raw,file={IMAGE}",
            "-snapshot",
            "-serial",
            "stdio",
            "-monitor",
            f"unix:{monitor_path},server=on,wait=off",
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
        read_until(WAIT_MARKER, 120 if V2 else 30)
        links["a"].send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"fos> ", 10)

        load_start = send_command(b"tape load net")
        read_until(LOAD_MARKER, 10, load_start)
        send_tape.send_tape(
            links["a"],
            ROUTER_MAC_A,
            TAPE_SENDER_MAC,
            TAPE.read_bytes(),
            delay=0.003,
        )
        read_until(b"Tape loaded", 30, load_start)
        read_until(b"fos> ", 5, load_start)
        run_start = len(output)
        router_started = False
        for _ in range(20):
            links["a"].send(
                term_frame(
                    ROUTER_MAC_A,
                    TERM_CLIENT_MAC,
                    TERM_TAG_DATA,
                    b"tape run router dhcp nat forward tcp 8080 10.0.15.2 80 "
                    b"forward udp 5353 10.0.15.2 dhcp-server 2 10.0.15.1/24\n",
                )
            )
            deadline = time.monotonic() + 0.25
            while time.monotonic() < deadline:
                if b"DHCP: broadcasting DISCOVER" in output[run_start:]:
                    router_started = True
                    break
                for key, _ in selector.select(deadline - time.monotonic()):
                    chunk = os.read(key.fd, 4096)
                    if not chunk:
                        raise SystemExit(
                            "router serial stream ended early:\n"
                            + output.decode("utf-8", "replace")
                        )
                    output.extend(chunk)
            if router_started:
                break
        if not router_started:
            raise SystemExit(
                "timed out waiting for the router tape to start after "
                "retransmitting its best-effort remote-terminal command; "
                "router serial output:\n"
                + output.decode("utf-8", "replace")
            )
        upstream_discover = wait_for_frame(
            links["a"],
            is_upstream_discover,
            10,
            "an upstream DHCP discover from NIC 1",
        )
        assert upstream_discover[0:6] == b"\xff" * 6
        transaction_id = dhcp_transaction_id(upstream_discover)
        links["a"].send(
            dhcp_server_frame(
                2,
                ROUTER_MAC_A,
                transaction_id,
                ROUTER_IP_A,
                bytes((10, 0, 1, 254)),
                bytes((10, 0, 1, 254)),
                bytes((10, 0, 1, 53)),
            )
        )
        wait_for_frame(
            links["a"],
            lambda frame: frame[6:12] == ROUTER_MAC_A
            and dhcp_message_type(frame) == b"\x03"
            and dhcp_client_mac(frame) == ROUTER_MAC_A,
            10,
            "an upstream DHCP request from NIC 1",
        )
        links["a"].send(
            dhcp_server_frame(
                5,
                ROUTER_MAC_A,
                transaction_id,
                ROUTER_IP_A,
                bytes((10, 0, 1, 254)),
                bytes((10, 0, 1, 254)),
                bytes((10, 0, 1, 53)),
            )
        )
        read_until(b"DHCP server on NIC 2", 10, run_start)
        read_until(b"forward tcp 8080 -> 10.0.15.2:80", 10, run_start)
        read_until(b"forward udp 5353 -> 10.0.15.2:5353", 10, run_start)
        read_until(b"NAT: allocated", 10, run_start)
        read_until(b"Router event diagnostics: DHCP lease and new NAT-flow messages enabled.", 10, run_start)
        print("PASS: router tape started with DHCP service on the secondary NIC.")

        links["b"].send(dhcp_frame(1, DHCP_CLIENT_MAC))
        offer = wait_for_frame(
            links["b"],
            lambda frame: frame[0:6] == b"\xff" * 6
            and frame[6:12] == ROUTER_MAC_B
            and frame[12:14] == b"\x08\x00"
            and parse_dhcp(frame)[1].get(53) == b"\x02",
            10,
            "a DHCP offer on segment B",
        )
        offered_ip, offer_options = parse_dhcp(offer)
        assert offered_ip == bytes((10, 0, 15, 2)), offered_ip
        assert offer_options[1] == bytes((255, 255, 255, 0))
        assert offer_options[3] == ROUTER_IP_B
        assert offer_options[28] == bytes((10, 0, 15, 255))
        assert offer_options[6] == bytes((10, 0, 1, 53))

        links["b"].send(dhcp_frame(3, DHCP_CLIENT_MAC, offered_ip, ROUTER_IP_B))
        ack = wait_for_frame(
            links["b"],
            lambda frame: frame[0:6] == b"\xff" * 6
            and frame[6:12] == ROUTER_MAC_B
            and frame[12:14] == b"\x08\x00"
            and parse_dhcp(frame)[1].get(53) == b"\x05",
            10,
            "a DHCP acknowledgement on segment B",
        )
        acknowledged_ip, _ = parse_dhcp(ack)
        assert acknowledged_ip == offered_ip
        read_until(b"DHCP: NIC 2 offered 10.0.15.2 to 52:54:00:ca:fe:05.", 10)
        read_until(b"DHCP: NIC 2 leased 10.0.15.2 to 52:54:00:ca:fe:05.", 10)
        print("PASS: DHCP server completed DISCOVER/OFFER/REQUEST/ACK.")

        for index in range(17):
            client_mac = bytes((0x52, 0x54, 0x00, 0xca, 0xfe, 0x20 + index))
            links["b"].send(dhcp_frame(1, client_mac))
            extra_offer = wait_for_frame(
                links["b"],
                lambda frame, client_mac=client_mac: frame[6:12] == ROUTER_MAC_B
                and dhcp_message_type(frame) == b"\x02"
                and dhcp_client_mac(frame) == client_mac,
                10,
                f"DHCP offer {index + 1} beyond the old lease limit",
            )
            extra_ip, _ = parse_dhcp(extra_offer)
            links["b"].send(dhcp_frame(3, client_mac, extra_ip, ROUTER_IP_B))
            wait_for_frame(
                links["b"],
                lambda frame, client_mac=client_mac: frame[6:12] == ROUTER_MAC_B
                and dhcp_message_type(frame) == b"\x05"
                and dhcp_client_mac(frame) == client_mac,
                10,
                f"DHCP acknowledgement {index + 1} beyond the old lease limit",
            )
        print("PASS: DHCP server allocated more than 16 simultaneous leases.")

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

        self_ping_identifier = 0x4242
        self_ping_request = icmp_echo(8, self_ping_identifier, 7, b"ping the router itself")
        links["a"].send(
            ipv4_frame(
                ROUTER_MAC_A,
                LEFT_MAC,
                LEFT_IP,
                ROUTER_IP_A,
                self_ping_request,
                protocol=1,
            )
        )
        self_ping_reply = wait_for_frame(
            links["a"],
            lambda frame: frame[0:6] == LEFT_MAC
            and frame[6:12] == ROUTER_MAC_A
            and frame[12:14] == b"\x08\x00"
            and frame[23] == 1,
            10,
            "an ICMP echo reply from the router's own interface",
        )
        assert self_ping_reply[26:30] == ROUTER_IP_A
        assert self_ping_reply[30:34] == LEFT_IP
        assert self_ping_reply[34] == 0, "echo reply type"
        assert int.from_bytes(self_ping_reply[38:40], "big") == self_ping_identifier
        assert int.from_bytes(self_ping_reply[40:42], "big") == 7
        assert self_ping_reply[42:] == b"ping the router itself"
        assert checksum(self_ping_reply[34:]) == 0
        print("PASS: router answered a ping addressed to its own interface.")

        # A request can arrive on one interface addressed to a *different*
        # configured interface's own address -- e.g. an upstream host
        # pinging the router's downstream address, routed in via the
        # uplink (matching the user-reported bug: the reply must come from
        # the address that was actually pinged, not from whichever
        # interface the request happened to arrive on).
        cross_ping_identifier = 0x5252
        cross_ping_request = icmp_echo(8, cross_ping_identifier, 3, b"ping the other interface")
        links["a"].send(
            ipv4_frame(
                ROUTER_MAC_A,
                LEFT_MAC,
                LEFT_IP,
                ROUTER_IP_B,
                cross_ping_request,
                protocol=1,
            )
        )
        cross_ping_reply = wait_for_frame(
            links["a"],
            lambda frame: frame[0:6] == LEFT_MAC
            and frame[6:12] == ROUTER_MAC_A
            and frame[12:14] == b"\x08\x00"
            and frame[23] == 1,
            10,
            "an ICMP echo reply for a cross-interface self-ping",
        )
        assert cross_ping_reply[26:30] == ROUTER_IP_B, cross_ping_reply[26:30]
        assert cross_ping_reply[30:34] == LEFT_IP
        assert cross_ping_reply[34] == 0, "echo reply type"
        assert int.from_bytes(cross_ping_reply[38:40], "big") == cross_ping_identifier
        assert checksum(cross_ping_reply[34:]) == 0
        print("PASS: router answered a cross-interface ping from its own correct address.")

        client_port = 40000
        server_port = 23
        syn = tcp_segment(
            RIGHT_IP,
            LEFT_IP,
            client_port,
            server_port,
            100,
            0,
            0x002,
        )
        links["b"].send(
            ipv4_frame(
                ROUTER_MAC_B,
                RIGHT_MAC,
                RIGHT_IP,
                LEFT_IP,
                syn,
                protocol=6,
            )
        )
        translated_syn = wait_for_frame(
            links["a"],
            lambda frame: frame[0:6] == LEFT_MAC
            and frame[6:12] == ROUTER_MAC_A
            and frame[12:14] == b"\x08\x00",
            10,
            "a NAT-translated TCP SYN on segment A",
        )
        assert translated_syn[14 + 12 : 14 + 16] == ROUTER_IP_A
        assert translated_syn[14 + 16 : 14 + 20] == LEFT_IP
        external_port = int.from_bytes(translated_syn[14 + 20 : 14 + 22], "big")
        assert 49152 <= external_port <= 65535
        assert external_port != client_port
        ip_length = int.from_bytes(translated_syn[14 + 2 : 14 + 4], "big")
        translated_tcp = translated_syn[14 + 20 : 14 + ip_length]
        translated_pseudo = struct.pack(
            "!4s4sBBH", ROUTER_IP_A, LEFT_IP, 0, 6, len(translated_tcp)
        )
        assert checksum(translated_pseudo + translated_tcp) == 0
        read_until(b"NAT: new TCP flow 10.0.15.2:40000 -> 10.0.1.2:23 via NIC 1 as 10.0.1.1:", 10)
        print("PASS: router source-NATed a downstream TCP connection.")

        syn_ack = tcp_segment(
            LEFT_IP,
            ROUTER_IP_A,
            server_port,
            external_port,
            200,
            101,
            0x012,
        )
        links["a"].send(
            ipv4_frame(
                ROUTER_MAC_A,
                LEFT_MAC,
                LEFT_IP,
                ROUTER_IP_A,
                syn_ack,
                protocol=6,
            )
        )
        translated_syn_ack = wait_for_frame(
            links["b"],
            lambda frame: frame[0:6] == RIGHT_MAC
            and frame[6:12] == ROUTER_MAC_B
            and frame[12:14] == b"\x08\x00",
            10,
            "a reverse-NATed TCP SYN/ACK on segment B",
        )
        assert translated_syn_ack[14 + 12 : 14 + 16] == LEFT_IP
        assert translated_syn_ack[14 + 16 : 14 + 20] == RIGHT_IP
        ip_length = int.from_bytes(translated_syn_ack[14 + 2 : 14 + 4], "big")
        translated_tcp = translated_syn_ack[14 + 20 : 14 + ip_length]
        translated_pseudo = struct.pack(
            "!4s4sBBH", LEFT_IP, RIGHT_IP, 0, 6, len(translated_tcp)
        )
        assert checksum(translated_pseudo + translated_tcp) == 0
        print("PASS: router reverse-translated the upstream TCP reply.")

        echo_identifier = 0x1234
        echo_request = icmp_echo(8, echo_identifier, 1, b"router ping")
        links["b"].send(
            ipv4_frame(
                ROUTER_MAC_B,
                RIGHT_MAC,
                RIGHT_IP,
                LEFT_IP,
                echo_request,
                protocol=1,
            )
        )
        translated_echo_request = wait_for_frame(
            links["a"],
            lambda frame: frame[0:6] == LEFT_MAC
            and frame[6:12] == ROUTER_MAC_A
            and frame[12:14] == b"\x08\x00"
            and frame[23] == 1,
            10,
            "a NAT-translated ICMP echo request on segment A",
        )
        assert translated_echo_request[26:30] == ROUTER_IP_A
        external_identifier = int.from_bytes(translated_echo_request[38:40], "big")
        assert 49152 <= external_identifier <= 65535
        assert external_identifier != echo_identifier
        assert checksum(translated_echo_request[34:]) == 0

        echo_reply = icmp_echo(0, external_identifier, 1, b"router ping")
        links["a"].send(
            ipv4_frame(
                ROUTER_MAC_A,
                LEFT_MAC,
                LEFT_IP,
                ROUTER_IP_A,
                echo_reply,
                protocol=1,
            )
        )
        translated_echo_reply = wait_for_frame(
            links["b"],
            lambda frame: frame[0:6] == RIGHT_MAC
            and frame[6:12] == ROUTER_MAC_B
            and frame[12:14] == b"\x08\x00"
            and frame[23] == 1,
            10,
            "a reverse-NATed ICMP echo reply on segment B",
        )
        assert translated_echo_reply[26:30] == LEFT_IP
        assert translated_echo_reply[30:34] == RIGHT_IP
        assert int.from_bytes(translated_echo_reply[38:40], "big") == echo_identifier
        assert checksum(translated_echo_reply[34:]) == 0
        read_until(
            b"NAT: new ICMP flow 10.0.15.2:4660 -> 10.0.1.2:4660 via NIC 1 as 10.0.1.1:",
            10,
        )
        print("PASS: router source-NATed an ICMP echo request and reply.")

        forwarded_syn = tcp_segment(
            LEFT_IP, ROUTER_IP_A, 41000, 8080, 300, 0, 0x002
        )
        links["a"].send(
            ipv4_frame(
                ROUTER_MAC_A, LEFT_MAC, LEFT_IP, ROUTER_IP_A, forwarded_syn, protocol=6
            )
        )
        dnat_syn = wait_for_frame(
            links["b"],
            lambda frame: frame[0:6] == RIGHT_MAC
            and frame[6:12] == ROUTER_MAC_B
            and frame[12:14] == b"\x08\x00",
            10,
            "a statically forwarded TCP SYN on segment B",
        )
        assert dnat_syn[14 + 12 : 14 + 16] == LEFT_IP
        assert dnat_syn[14 + 16 : 14 + 20] == RIGHT_IP
        assert int.from_bytes(dnat_syn[36:38], "big") == 80
        tcp_length = int.from_bytes(dnat_syn[16:18], "big") - 20
        assert checksum(
            struct.pack("!4s4sBBH", LEFT_IP, RIGHT_IP, 0, 6, tcp_length)
            + dnat_syn[34 : 34 + tcp_length]
        ) == 0

        forwarded_syn_ack = tcp_segment(
            RIGHT_IP, LEFT_IP, 80, 41000, 400, 301, 0x012
        )
        links["b"].send(
            ipv4_frame(
                ROUTER_MAC_B, RIGHT_MAC, RIGHT_IP, LEFT_IP, forwarded_syn_ack, protocol=6
            )
        )
        snat_syn_ack = wait_for_frame(
            links["a"],
            lambda frame: frame[0:6] == LEFT_MAC
            and frame[6:12] == ROUTER_MAC_A
            and frame[12:14] == b"\x08\x00",
            10,
            "a static-forward TCP reply on segment A",
        )
        assert snat_syn_ack[26:30] == ROUTER_IP_A
        assert int.from_bytes(snat_syn_ack[34:36], "big") == 8080
        tcp_length = int.from_bytes(snat_syn_ack[16:18], "big") - 20
        assert checksum(
            struct.pack("!4s4sBBH", ROUTER_IP_A, LEFT_IP, 0, 6, tcp_length)
            + snat_syn_ack[34 : 34 + tcp_length]
        ) == 0
        print("PASS: router statically DNATed TCP and translated its reply.")

        incoming_udp = udp_datagram_with_checksum(LEFT_IP, ROUTER_IP_A, 42000, 5353, b"udp in")
        links["a"].send(
            ipv4_frame(
                ROUTER_MAC_A, LEFT_MAC, LEFT_IP, ROUTER_IP_A, incoming_udp, protocol=17
            )
        )
        dnat_udp = wait_for_frame(
            links["b"],
            lambda frame: frame[0:6] == RIGHT_MAC
            and frame[6:12] == ROUTER_MAC_B
            and frame[12:14] == b"\x08\x00",
            10,
            "a statically forwarded UDP datagram on segment B",
        )
        assert dnat_udp[30:34] == RIGHT_IP
        assert int.from_bytes(dnat_udp[36:38], "big") == 5353
        udp_length = int.from_bytes(dnat_udp[38:40], "big")
        assert checksum(
            struct.pack("!4s4sBBH", LEFT_IP, RIGHT_IP, 0, 17, udp_length)
            + dnat_udp[34 : 34 + udp_length]
        ) == 0

        reply_udp = udp_datagram_with_checksum(RIGHT_IP, LEFT_IP, 5353, 42000, b"udp out")
        links["b"].send(
            ipv4_frame(
                ROUTER_MAC_B, RIGHT_MAC, RIGHT_IP, LEFT_IP, reply_udp, protocol=17
            )
        )
        snat_udp = wait_for_frame(
            links["a"],
            lambda frame: frame[0:6] == LEFT_MAC
            and frame[6:12] == ROUTER_MAC_A
            and frame[12:14] == b"\x08\x00",
            10,
            "a static-forward UDP reply on segment A",
        )
        assert snat_udp[26:30] == ROUTER_IP_A
        assert int.from_bytes(snat_udp[34:36], "big") == 5353
        udp_length = int.from_bytes(snat_udp[38:40], "big")
        assert checksum(
            struct.pack("!4s4sBBH", ROUTER_IP_A, LEFT_IP, 0, 17, udp_length)
            + snat_udp[34 : 34 + udp_length]
        ) == 0
        print("PASS: router statically DNATed UDP and translated its reply.")

        stop_start = len(output)
        # The router polls the tape abort callback while forwarding. Verify a
        # local function-key escape still stops it after its disk-loaded image
        # performed BIOS EDD reads and the remote terminal session stays live.
        send_monitor_command(monitor_path, "sendkey f12")
        read_until(b"Router stopped;", 10, stop_start)
        read_until(b"Tape returned; back at the fos shell.", 10, stop_start)
        send_command(b"about")
        read_until(b"fos> ", 10)
        print("PASS: local F12 stopped the router and the remote shell remained usable.")
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
        monitor_dir.cleanup()


def test_uplink_boot_timeout_is_not_fatal() -> None:
    """A router with two configured DHCP uplinks must still start using
    whichever uplink actually acquires a lease, even if the other one never
    hears back from any DHCP server at all during boot.

    configure_dhcp used to treat a bounded acquisition attempt exhausting its
    own retries as immediately fatal to the whole router, so on real
    hardware a single flaky uplink NIC (or one plugged into a segment with
    no DHCP server yet, e.g. right after a cable/NIC swap) prevented the
    router from starting at all -- defeating the entire point of
    configuring more than one uplink for fail-over. This is a separate,
    self-contained QEMU instance (rather than a phase of the main scenario
    above) because it needs its own three-NIC topology: one uplink attached
    to an isolated segment with nothing on the other end (modeling "no DHCP
    server ever responds"), one uplink on QEMU's own user-mode network
    (which serves a real DHCP lease, standing in for the uplink that does
    work), and a downstream interface to satisfy the router's requirement
    for at least one non-uplink interface.
    """
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} does not exist; run `make image` first.")

    monitor_dir = tempfile.TemporaryDirectory(prefix="fos-router-failover-monitor-")
    monitor_path = Path(monitor_dir.name) / "monitor.sock"
    output = bytearray()
    selector = selectors.DefaultSelector()
    process = None
    try:
        process = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive", f"format=raw,file={IMAGE}",
                "-snapshot",
                "-serial", "stdio",
                "-monitor", f"unix:{monitor_path},server=on,wait=off",
                "-no-reboot", "-no-shutdown", "-display", "none",
                # NIC 1: an isolated multicast segment with nothing on the
                # other end -- no DHCP server will ever answer here.
                "-netdev", "socket,id=net0,mcast=230.55.55.90:4590",
                "-device", "e1000,netdev=net0",
                # NIC 2: QEMU's own user-mode network -- a real DHCP server
                # answers here, the same trick tapes/test_net_dhcp_e2e.py
                # already relies on for a hermetic, dependency-free lease.
                "-netdev", "user,id=net1",
                "-device", "e1000,netdev=net1",
                # NIC 3: another isolated segment, used only as the
                # required downstream (non-uplink) interface.
                "-netdev", "socket,id=net2,mcast=230.55.55.91:4591",
                "-device", "e1000,netdev=net2",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        selector.register(process.stdout, selectors.EVENT_READ)

        def read_until(marker: bytes, timeout: float, start: int = 0) -> None:
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

        def type_line(line: bytes) -> None:
            for byte in line:
                process.stdin.write(bytes([byte]))
                process.stdin.flush()
                time.sleep(0.002)

        read_until(WAIT_MARKER, 120 if V2 else 30)
        send_monitor_command(monitor_path, "sendkey f12")
        read_until(b"fos> ", 10)
        type_line(b"tape load disk " + (b"router" if V2 else b"router.bin") + b"\r")
        read_until(b"'tape run' to execute it.", 10)
        start = len(output)
        type_line(b"tape run dhcp 1 dhcp 2 3 10.0.20.1/24\r")
        read_until(b"Forwarding IPv4 frames", 60, start)
        text = bytes(output[start:]).decode("utf-8", "replace")
        assert "Router cannot start" not in text
        assert "no server responded before the boot timeout" in text
        assert "fos router: 3 interface(s)" in text
        print("PASS: router started using the working uplink; the dead uplink kept retrying in the background instead of aborting startup.")

        stop_start = len(output)
        send_monitor_command(monitor_path, "sendkey f12")
        read_until(b"fos> ", 10, stop_start)
        print("PASS: local F12 stopped the router and the local shell remained usable.")
    finally:
        selector.close()
        if process is not None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
        monitor_dir.cleanup()


if __name__ == "__main__":
    main()
    test_uplink_boot_timeout_is_not_fatal()
