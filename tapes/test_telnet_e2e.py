#!/usr/bin/env python3
"""End-to-end Telnet test over the raw-Ethernet QEMU fixture.

The test loads the tape over the same multicast Ethernet path used by the
other tape tests, then runs a tiny deterministic Unix-like Telnet server
implemented at Ethernet/ARP/IPv4/TCP level. Keeping the fixture at the frame
level avoids requiring root, a TAP bridge, or a host network reconfiguration.
It exercises DHCP configuration, Telnet negotiation, UTF-8 byte preservation,
terminal input, disconnect, and shell recovery.

Not part of ``make test``; run it after ``make tape-telnet`` and ``make
image``.
"""

import os
import selectors
import socket as socketlib
import struct
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.61"
MCAST_PORT = 4261
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "telnet.bin"

GUEST_MAC = bytes.fromhex("525400123471")
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef51")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef52")
SERVER_MAC = bytes.fromhex("525400123472")
GUEST_IP = bytes((192, 0, 2, 10))
SERVER_IP = bytes((192, 0, 2, 20))
SERVER_PORT = 2323
MONITOR_SOCK = Path("/tmp/fos-test-telnet-monitor.sock")
VGA_DUMP = Path("/tmp/fos-test-telnet-vga.bin")
ANSI_METADATA = b"\x1b[?2004h"

CHAT_ETHERTYPE = bytes((0x88, 0xB6))
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

TCP_FIN = 0x01
TCP_SYN = 0x02
TCP_RST = 0x04
TCP_PSH = 0x08
TCP_ACK = 0x10
DHCP_CLIENT_PORT = 68
DHCP_SERVER_PORT = 67


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + CHAT_ETHERTYPE + bytes((tag,)) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def checksum(data: bytes) -> int:
    if len(data) & 1:
        data += b"\0"
    total = 0
    for offset in range(0, len(data), 2):
        total += int.from_bytes(data[offset : offset + 2], "big")
        total = (total & 0xffff) + (total >> 16)
    return (~total) & 0xffff


def tcp_checksum(source: bytes, destination: bytes, segment: bytes) -> int:
    pseudo = source + destination + b"\x00\x06" + len(segment).to_bytes(2, "big")
    return checksum(pseudo + segment)


def parse_arp(frame: bytes) -> tuple[bytes, bytes, bytes, bytes] | None:
    if len(frame) < 42 or frame[12:14] != b"\x08\x06":
        return None
    payload = frame[14:42]
    if payload[0:2] != b"\x00\x01" or payload[2:4] != b"\x08\x00":
        return None
    if payload[4] != 6 or payload[5] != 4:
        return None
    return payload[8:14], payload[14:18], payload[18:24], payload[24:28]


def parse_dhcp(frame: bytes) -> tuple[int, bytes, int] | None:
    if len(frame) < 14 + 20 + 8 + 240 or frame[12:14] != b"\x08\x00":
        return None
    ip = frame[14:]
    ihl = (ip[0] & 0x0F) * 4
    if ip[0] >> 4 != 4 or ihl < 20 or ip[9] != 17:
        return None
    total_length = int.from_bytes(ip[2:4], "big")
    if total_length < ihl + 8 or len(ip) < total_length:
        return None
    udp = ip[ihl:total_length]
    source_port, destination_port, udp_length = struct.unpack("!HHH", udp[:6])
    if (
        source_port != DHCP_CLIENT_PORT
        or destination_port != DHCP_SERVER_PORT
        or udp_length < 8 + 240
        or len(udp) < udp_length
    ):
        return None
    payload = udp[8:udp_length]
    if (
        payload[0] != 1
        or payload[1] != 1
        or payload[2] != 6
        or payload[236:240] != b"\x63\x82\x53\x63"
    ):
        return None
    message_type = None
    offset = 240
    while offset < len(payload):
        option = payload[offset]
        offset += 1
        if option == 255:
            break
        if option == 0:
            continue
        if offset >= len(payload):
            return None
        length = payload[offset]
        offset += 1
        if offset + length > len(payload):
            return None
        if option == 53 and length == 1:
            message_type = payload[offset]
        offset += length
    if message_type is None:
        return None
    return int.from_bytes(payload[4:8], "big"), payload[28:34], message_type


def dhcp_reply(transaction_id: int, client_mac: bytes, message_type: int) -> bytes:
    payload = bytearray(240)
    payload[0:4] = b"\x02\x01\x06\x00"
    payload[4:8] = transaction_id.to_bytes(4, "big")
    payload[16:20] = GUEST_IP
    payload[20:24] = SERVER_IP
    payload[28:34] = client_mac
    payload[236:240] = b"\x63\x82\x53\x63"
    options = (
        bytes((53, 1, message_type))
        + bytes((54, 4))
        + SERVER_IP
        + bytes((1, 4))
        + bytes((255, 255, 255, 0))
        + bytes((3, 4))
        + bytes(4)
        + bytes((51, 4))
        + (60).to_bytes(4, "big")
        + bytes((58, 4))
        + (30).to_bytes(4, "big")
        + bytes((59, 4))
        + (52).to_bytes(4, "big")
        + b"\xff"
    )
    payload.extend(options)
    udp = struct.pack("!HHHH", DHCP_SERVER_PORT, DHCP_CLIENT_PORT, 8 + len(payload), 0)
    udp += payload
    ip = bytearray(b"\x45\x00" + (20 + len(udp)).to_bytes(2, "big"))
    ip += b"\x00\x02\x00\x00\x40\x11\x00\x00" + SERVER_IP + bytes((255, 255, 255, 255))
    ip[10:12] = checksum(bytes(ip)).to_bytes(2, "big")
    return (b"\xff" * 6 + SERVER_MAC + b"\x08\x00" + bytes(ip) + udp).ljust(60, b"\0")


def arp_reply(destination_mac: bytes, destination_ip: bytes) -> bytes:
    payload = (
        b"\x00\x01\x08\x00\x06\x04\x00\x02"
        + SERVER_MAC
        + SERVER_IP
        + destination_mac
        + destination_ip
    )
    frame = SERVER_MAC + destination_mac + b"\x08\x06" + payload
    return frame.ljust(60, b"\0")


def arp_broadcast_reply(destination_mac: bytes, destination_ip: bytes) -> bytes:
    # The multicast QEMU socket is a host-side transport, not a bridged
    # Ethernet segment. Broadcast delivery makes the fixture independent of
    # host NIC filtering while the ARP/IP/TCP addresses remain unicast.
    frame = arp_reply(destination_mac, destination_ip)
    return b"\xff" * 6 + frame[6:]


def parse_tcp(
    frame: bytes,
) -> tuple[bytes, bytes, int, int, int, int, int, bytes] | None:
    if len(frame) < 54 or frame[12:14] != b"\x08\x00":
        return None
    ip = frame[14:]
    if len(ip) < 20 or ip[0] >> 4 != 4 or ip[9] != 6:
        return None
    ihl = (ip[0] & 0x0f) * 4
    if ihl < 20 or len(ip) < ihl:
        return None
    total_length = int.from_bytes(ip[2:4], "big")
    if total_length < ihl + 20 or len(ip) < total_length:
        return None
    source_ip = ip[12:16]
    destination_ip = ip[16:20]
    tcp = ip[ihl:total_length]
    source_port, destination_port, sequence, acknowledgment = struct.unpack(
        "!HHII", tcp[:12]
    )
    data_offset = (tcp[12] >> 4) * 4
    if data_offset < 20 or len(tcp) < data_offset:
        return None
    return (
        source_ip,
        destination_ip,
        source_port,
        destination_port,
        sequence,
        acknowledgment,
        tcp[13],
        tcp[data_offset:],
    )


def tcp_frame(
    destination_mac: bytes,
    destination_ip: bytes,
    destination_port: int,
    sequence: int,
    acknowledgment: int,
    flags: int,
    payload: bytes = b"",
) -> bytes:
    header = struct.pack(
        "!HHII",
        SERVER_PORT,
        destination_port,
        sequence,
        acknowledgment,
    )
    header += bytes((5 << 4, flags))
    header += struct.pack("!HHH", 2048, 0, 0)
    segment = bytearray(header + payload)
    segment[16:18] = tcp_checksum(SERVER_IP, GUEST_IP, bytes(segment)).to_bytes(2, "big")
    ip = bytearray(b"\x45\x00" + (20 + len(segment)).to_bytes(2, "big"))
    ip += b"\x00\x01\x40\x00\x40\x06\x00\x00" + SERVER_IP + GUEST_IP
    ip[10:12] = checksum(bytes(ip)).to_bytes(2, "big")
    return (SERVER_MAC + destination_mac + b"\x08\x00" + bytes(ip) + bytes(segment)).ljust(
        60, b"\0"
    )


def ethernet_broadcast(frame: bytes) -> bytes:
    return b"\xff" * 6 + frame[6:]


class RawTelnetServer:
    """One deterministic ARP/TCP/Telnet peer for the QEMU multicast segment."""

    def __init__(self, mcast: McastSocket):
        self.mcast = mcast
        self.client_mac: bytes | None = None
        self.client_port = 0
        self.client_next = 0
        self.server_next = 0x12345679
        self.established = False
        self.banner_sent = False
        self.echo_sent = False
        self.received = bytearray()
        self.negotiation_seen = False
        self.dhcp_requests = 0

    def send(self, frame: bytes) -> None:
        self.mcast.send(frame)

    def handle(self, frame: bytes) -> None:
        arp = parse_arp(frame)
        if arp is not None:
            sender_mac, sender_ip, _, target_ip = arp
            if target_ip == SERVER_IP:
                self.send(arp_broadcast_reply(sender_mac, sender_ip))
            return

        dhcp = parse_dhcp(frame)
        if dhcp is not None:
            transaction_id, client_mac, message_type = dhcp
            if message_type == 1:
                self.send(dhcp_reply(transaction_id, client_mac, 2))
                self.dhcp_requests += 1
            elif message_type == 3:
                self.send(dhcp_reply(transaction_id, client_mac, 5))
                self.dhcp_requests += 1
            return

        parsed = parse_tcp(frame)
        if parsed is None:
            return
        source_ip, destination_ip, source_port, destination_port, sequence, acknowledgment, flags, payload = parsed
        if source_ip != GUEST_IP or destination_ip != SERVER_IP or destination_port != SERVER_PORT:
            return
        self.client_mac = frame[6:12]
        self.client_port = source_port

        if flags & TCP_RST:
            self.established = False
            return
        if flags & TCP_SYN:
            self.client_next = sequence + 1
            self.server_next = 0x12345679
            self.banner_sent = False
            self.echo_sent = False
            self.received.clear()
            response = tcp_frame(
                self.client_mac,
                GUEST_IP,
                self.client_port,
                self.server_next - 1,
                self.client_next,
                TCP_SYN | TCP_ACK,
            )
            self.send(ethernet_broadcast(response))
            return

        if not self.established:
            if flags & TCP_ACK and acknowledgment == self.server_next:
                self.established = True
                self.send_banner()
            return

        if payload:
            if sequence != self.client_next:
                self.send_ack()
                return
            self.client_next += len(payload)
            self.received.extend(payload)
            if b"\xff" in payload:
                self.negotiation_seen = True
            self.send_ack()
            if not self.echo_sent and b"hello \xc3\xa9\r\n" in self.received:
                self.echo_sent = True
                self.send_text(b"echo: \xcf\x80 from unix telnetd\r\n")
        if flags & TCP_FIN:
            self.client_next += 1
            self.send_ack()

    def send_ack(self) -> None:
        assert self.client_mac is not None
        response = tcp_frame(
            self.client_mac,
            GUEST_IP,
            self.client_port,
            self.server_next,
            self.client_next,
            TCP_ACK,
        )
        self.send(ethernet_broadcast(response))

    def send_banner(self) -> None:
        if self.banner_sent:
            return
        self.banner_sent = True
        self.send_text(
            b"\xff\xfb\x01\xff\xfd\x03"
            + ANSI_METADATA
            + b"\x1b[2J\x1b[HUnix telnetd \xcf\x80 ready\r\n"
        )

    def send_text(self, payload: bytes) -> None:
        assert self.client_mac is not None
        response = tcp_frame(
            self.client_mac,
            GUEST_IP,
            self.client_port,
            self.server_next,
            self.client_next,
            TCP_ACK | TCP_PSH,
            payload,
        )
        self.send(ethernet_broadcast(response))
        self.server_next += len(payload)


def monitor_command(command: str, wait: float = 0.5) -> None:
    sock = None
    for _ in range(50):
        try:
            sock = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
            sock.connect(str(MONITOR_SOCK))
            break
        except (FileNotFoundError, ConnectionRefusedError):
            sock = None
            time.sleep(0.1)
    if sock is None:
        raise SystemExit("could not connect to the QEMU monitor socket")
    sock.settimeout(5)
    time.sleep(0.2)
    sock.recv(65536)
    sock.sendall((command + "\n").encode())
    time.sleep(wait)
    sock.close()


def read_vga_text() -> str:
    if VGA_DUMP.exists():
        VGA_DUMP.unlink()
    monitor_command(f'pmemsave 0xb8000 4000 "{VGA_DUMP}"')
    raw = VGA_DUMP.read_bytes()
    chars = bytes(raw[offset] for offset in range(0, len(raw), 2))
    rows = [chars[row : row + 80].decode("ascii", "replace") for row in range(0, 2000, 80)]
    return "\n".join(row.rstrip() for row in rows)


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} does not exist; run `make image` first.")
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-telnet` first.")
    for path in (MONITOR_SOCK, VGA_DUMP):
        if path.exists():
            path.unlink()

    qemu = subprocess.Popen(
        [
            "qemu-system-i386",
            "-drive",
            f"format=raw,file={IMAGE}",
            "-snapshot",
            "-serial",
            "stdio",
            "-monitor",
            f"unix:{MONITOR_SOCK},server=on,wait=off",
            "-no-reboot",
            "-no-shutdown",
            "-netdev",
            f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
            "-device",
            f"rtl8139,netdev=net0,mac={send_tape.format_mac(GUEST_MAC)}",
            "-display",
            "none",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    assert qemu.stdout is not None
    output = bytearray()
    selector = selectors.DefaultSelector()
    selector.register(qemu.stdout, selectors.EVENT_READ, "qemu")
    mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
    mcast.sock.setblocking(False)
    server = RawTelnetServer(mcast)
    selector.register(mcast.sock, selectors.EVENT_READ, "network")

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
                if key.data == "network":
                    try:
                        server.handle(mcast.sock.recv(2048))
                    except BlockingIOError:
                        pass
                    continue
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(
                        "QEMU serial stream ended early:\n"
                        + output.decode("utf-8", "replace")
                    )
                output.extend(chunk)
        return bytes(output[start:])

    def send_term(tag: int, payload: bytes = b"") -> None:
        mcast.send(term_frame(GUEST_MAC, TERM_CLIENT_MAC, tag, payload))

    try:
        read_until(b"Waiting for a remote terminal client to connect", 30)
        send_term(TERM_TAG_HELLO)
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        start = len(output)
        send_term(TERM_TAG_DATA, b"tape load net\n")
        time.sleep(0.5)
        send_tape.send_tape(mcast, GUEST_MAC, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until(b"Tape loaded", 30, start)
        read_until(b"fos> ", 5, start)

        start = len(output)
        send_term(
            TERM_TAG_DATA,
            b"tape run static 192.0.2.10 255.255.255.0 0.0.0.0 192.0.2.20 2323\n",
        )
        read_until(b"TELNET CONNECTING", 10, start)
        read_until(b"TELNET CONNECTED", 15, start)
        read_until(b"ready\r\n", 10, start)
        if ANSI_METADATA not in output[start:]:
            raise SystemExit("Telnet server ANSI sequence did not reach raw output")
        vga_text = read_vga_text()
        if ANSI_METADATA[1:].decode() in vga_text:
            raise SystemExit(
                "ANSI terminal metadata leaked into the local VGA console:\n" + vga_text
            )
        if "Unix telnetd" not in vga_text:
            raise SystemExit("Telnet server text did not reach the local VGA console")
        if "π".encode() not in output[start:]:
            raise SystemExit(
                "server UTF-8 banner did not reach the tape output: "
                + repr(bytes(output[start:]))
            )
        print("PASS: Telnet tape completed TCP handshake and preserved UTF-8 server bytes.")

        start = len(output)
        send_term(TERM_TAG_DATA, b"hello \xc3\xa9\r\n")
        read_until(b"echo: ", 10, start)
        read_until("π".encode(), 10, start)
        if b"hello \xc3\xa9\r\n" not in server.received:
            raise SystemExit(
                f"Telnet server did not receive the UTF-8 terminal input: {server.received!r}"
            )
        if not server.negotiation_seen:
            raise SystemExit("Telnet tape did not answer the server's IAC negotiation")
        print("PASS: Telnet tape forwarded UTF-8 input and answered Telnet negotiation.")

        start = len(output)
        send_term(TERM_TAG_DATA, b"\x1b")
        read_until(b"TELNET ABORTED", 10, start)
        read_until(b"Tape returned; back at the fos shell.", 10, start)
        start = len(output)
        send_term(TERM_TAG_DATA, b"tape run dhcp 192.0.2.20 2323\n")
        read_until(b"DHCP lease acquired for 192.0.2.10.", 15, start)
        read_until(b"TELNET CONNECTED", 15, start)
        if server.dhcp_requests < 2:
            raise SystemExit("Telnet DHCP mode did not complete DISCOVER/OFFER/REQUEST/ACK")
        print("PASS: Telnet tape acquired a DHCP lease and connected to the server.")
        start = len(output)
        send_term(TERM_TAG_DATA, b"\x1b")
        read_until(b"TELNET ABORTED", 10, start)
        read_until(b"Tape returned; back at the fos shell.", 10, start)
        start = len(output)
        send_term(TERM_TAG_DATA, b"about\n")
        read_until(b"fos is a small educational 32-bit x86 operating system.", 10, start)
        print("PASS: Telnet tape aborted cleanly and the remote shell remained usable.")
        send_term(TERM_TAG_BYE)
    finally:
        for path in (MONITOR_SOCK, VGA_DUMP):
            if path.exists():
                path.unlink()
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
