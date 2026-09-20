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
UPSTREAM_MAC = bytes.fromhex("525400cafe06")
LEFT_MAC = bytes.fromhex("525400cafe03")
RIGHT_MAC = bytes.fromhex("525400cafe04")
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef41")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef42")
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
            "-m",
            "256",
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

        run_start = send_command(b"tape run router @ROUTER.CFG")
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
        read_until(b"fos router: 2 interface(s)", 10, run_start)
        read_until(b"DHCP server on NIC 2", 10, run_start)
        read_until(b"NAT: allocated", 10, run_start)
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
        assert int.from_bytes(translated_syn[14 + 20 : 14 + 22], "big") == client_port
        ip_length = int.from_bytes(translated_syn[14 + 2 : 14 + 4], "big")
        translated_tcp = translated_syn[14 + 20 : 14 + ip_length]
        translated_pseudo = struct.pack(
            "!4s4sBBH", ROUTER_IP_A, LEFT_IP, 0, 6, len(translated_tcp)
        )
        assert checksum(translated_pseudo + translated_tcp) == 0
        print("PASS: router source-NATed a downstream TCP connection.")

        syn_ack = tcp_segment(
            LEFT_IP,
            ROUTER_IP_A,
            server_port,
            client_port,
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
