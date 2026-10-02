#!/usr/bin/env python3
"""Copy a file to or from a fos machine's disk, with the same `HOST[:diskN]/NAME`
syntax fos's own `copy` command uses.

    foscopy.py notes.txt    myhost1:disk1/notes.txt     # PUT  (host -> fos)
    foscopy.py myhost1:disk2/start  ./start-copy          # GET  (fos -> host)
    foscopy.py router.cfg   ac:a7:f1:b3:08:5c/            # PUT, name = source name
    foscopy.py --mac 3 router.cfg  :disk1/router.cfg      # peer from --mac

This is the host-side half of NETFS (kernel/src/shell.rs, `NETFS_TAG_*`;
NETWORKING.md): the same GET/PUT frames fos's `copy`/`tape load` exchange
between two fos machines, so no tape-buffer juggling and no second cabled NIC
is needed (compare `devloop.py put`). Both directions carry the whole file's
RFC 1071 checksum, verified before anything is installed, and the machine
answers with a specific error (not found, directory full, busy, ...) rather
than silence. The machine serves NETFS at its idle prompt and while sitting in
`term net`; it does not serve it inside `chat`, `nic discover` or a running
tape, and a NIC on the `settings` file's `deny-term-net` list never serves it.

HOST is a hostname as `nic peers` shows it (fos appends the NIC number:
`myhost1`), a full MAC address, or a 1-based number in the shared address book
(see peers.py). `:diskN` defaults to disk1, the boot disk. A source or
destination counts as remote when it has the form HOST[:diskN]/NAME and no
local directory called HOST exists. Exactly one side must be remote. Names are
FOSFS names: up to 16 of a-z 0-9 . _ - (the file is never renamed for you; a
trailing slash on a PUT destination reuses the source's file name).

Sending raw frames needs CAP_NET_RAW (see AGENTS.md: the setcap'd
`python3-netraw` copy of Python). `--mcast GROUP:PORT` uses a UDP multicast
group instead, which is how the QEMU test reaches a guest without privileges.
There is no retransmission anywhere in NETFS: a gap aborts the transfer and
this tool reports it; retry, optionally with a larger --delay.
"""

import os
import re
import socket
import struct
import sys
import time
from argparse import ArgumentParser
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from send_tape import (  # noqa: E402
    BROADCAST_MAC,
    CHAT_ETHERTYPE,
    CHAT_TAG_DISCOVER_REPLY,
    CHAT_TAG_DISCOVER_REQUEST,
    MAC_PATTERN,
    default_interface,
    format_mac,
    interface_mac,
    load_known_peers,
    pad,
    parse_mac,
    print_known_peers,
    remember_known_peer,
    resolve_peer,
    save_known_peers,
)

# Must stay byte-for-byte in sync with kernel/src/shell.rs's NETFS_TAG_*,
# NETFS_ERROR_*, NETFS_NAME_CAPACITY and TAPE_CHUNK_LEN.
TAG_GET_REQUEST = 0x30
TAG_GET_START = 0x31
TAG_GET_DATA = 0x32
TAG_GET_STOP = 0x33
TAG_GET_ERROR = 0x34
TAG_PUT_REQUEST = 0x35
TAG_PUT_DATA = 0x36
TAG_PUT_STOP = 0x37
TAG_PUT_ACK = 0x38
TAG_PUT_ERROR = 0x39

ERROR_NAMES = {
    1: "file not found",
    2: "that disk is not available on the machine",
    3: "file too large for the machine's transfer buffer (4 MiB), or empty",
    4: "invalid file name",
    5: "the FOSFS directory is full",
    6: "the FOSFS data area is full",
    7: "disk error on the machine",
    8: "checksum mismatch (data was damaged in transit)",
    9: "busy: another transfer is in progress on that machine",
}
NAME_CAPACITY = 16
CHUNK_LEN = 1024
NAME_RE = re.compile(r"^[a-z0-9._-]{1,%d}$" % NAME_CAPACITY)  # fosfs::valid_name
REMOTE_RE = re.compile(r"^(?P<host>[^/]*?)(?::disk(?P<disk>\d+))?/(?P<name>[^/]*)$")


class Fatal(SystemExit):
    pass


def internet_checksum(data: bytes) -> int:
    """RFC 1071, identical to kernel/src/shell.rs's `internet_checksum`."""
    total = 0
    it = iter(data)
    for high in it:
        total += (high << 8) | next(it, 0)
    while total > 0xFFFF:
        total = (total & 0xFFFF) + (total >> 16)
    return (~total) & 0xFFFF


class Transport:
    """Sends and receives whole Ethernet frames. `RawTransport` is the real
    thing; `McastTransport` is the unprivileged test seam."""

    source: bytes

    def send(self, frame: bytes) -> None:
        raise NotImplementedError

    def receive(self, timeout: float) -> bytes | None:
        raise NotImplementedError


class RawTransport(Transport):
    def __init__(self, iface: str):
        self.sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0003))
        try:
            self.sock.bind((iface, 0))
        except PermissionError:
            raise Fatal(
                "permission denied opening a raw socket; sending raw Ethernet frames needs\n"
                "CAP_NET_RAW -- run this with the setcap'd python3-netraw (see AGENTS.md) or sudo."
            )
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
        self.source = interface_mac(iface)

    def send(self, frame: bytes) -> None:
        self.sock.send(frame)

    def receive(self, timeout: float) -> bytes | None:
        self.sock.settimeout(timeout)
        try:
            return self.sock.recv(2048)
        except socket.timeout:
            return None


class McastTransport(Transport):
    """A UDP multicast group carrying one Ethernet frame per datagram, which is
    what QEMU's `-netdev socket,mcast=` speaks."""

    def __init__(self, spec: str, source: bytes):
        group, port = spec.rsplit(":", 1)
        self.addr = (group, int(port))
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("", int(port)))
        mreq = struct.pack("4sl", socket.inet_aton(group), socket.INADDR_ANY)
        self.sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, mreq)
        self.sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
        self.source = source

    def send(self, frame: bytes) -> None:
        self.sock.sendto(frame, self.addr)

    def receive(self, timeout: float) -> bytes | None:
        self.sock.settimeout(timeout)
        try:
            return self.sock.recv(2048)
        except socket.timeout:
            return None


def frame(dst: bytes, src: bytes, tag: int, payload: bytes = b"") -> bytes:
    return pad(bytearray(dst + src + CHAT_ETHERTYPE.to_bytes(2, "big") + bytes([tag]) + payload))


def split_remote(text: str) -> tuple[str, int, str] | None:
    """`(host, disk, name)` if `text` is HOST[:diskN]/NAME, else None. A local
    directory named HOST wins, so `./x` and `build/out.bin` stay local."""
    match = REMOTE_RE.match(text)
    if not match or text.startswith(("/", ".")):
        return None
    host = match.group("host")
    if host and os.path.isdir(host):
        return None
    return host, int(match.group("disk") or 1), match.group("name")


def discover_hosts(link: Transport, timeout: float) -> dict[bytes, str]:
    """MAC -> hostname for every discovery reply within `timeout` seconds."""
    link.send(frame(BROADCAST_MAC, link.source, CHAT_TAG_DISCOVER_REQUEST))
    found: dict[bytes, str] = {}
    deadline = time.monotonic() + timeout
    while (left := deadline - time.monotonic()) > 0:
        packet = link.receive(min(left, 0.1))
        if not packet or len(packet) < 15:
            continue
        if packet[12:14] != CHAT_ETHERTYPE.to_bytes(2, "big") or packet[14] != CHAT_TAG_DISCOVER_REPLY:
            continue
        mac = packet[6:12]
        if mac != link.source:
            found[mac] = packet[15:].rstrip(b"\0").decode("ascii", "replace")
    return found


def resolve_host(link: Transport, host: str, default_mac: str | None, timeout: float) -> bytes:
    peers = load_known_peers()
    if not host:
        if default_mac is None:
            raise Fatal("no host given: use HOST/NAME, or pass --mac")
        host = default_mac
    if MAC_PATTERN.match(host):
        return parse_mac(host)
    if host.isdigit():
        return resolve_peer(host, peers)
    print(f"Looking for host '{host}' on the network ({timeout}s)...", file=sys.stderr)
    found = discover_hosts(link, timeout)
    if any(remember_known_peer(peers, mac) for mac in found):
        save_known_peers(peers)
    matches = [mac for mac, name in found.items() if name.lower() == host.lower()]
    if len(matches) == 1:
        return matches[0]
    if not matches:
        known = ", ".join(f"{name or '?'} ({format_mac(mac)})" for mac, name in found.items()) or "nothing"
        raise Fatal(f"no host named '{host}' answered discovery; heard: {known}.\n"
                    "(a fos hostname has its NIC number appended, e.g. myhost1; a MAC also works)")
    raise Fatal(f"'{host}' answered from {len(matches)} machines; use a full MAC address")


def chat_body_from(packet: bytes, peer: bytes) -> bytes | None:
    """The NETFS payload (tag + body) if `packet` is chat traffic from `peer`."""
    if len(packet) < 15 or packet[12:14] != CHAT_ETHERTYPE.to_bytes(2, "big") or packet[6:12] != peer:
        return None
    return packet[14:]


def put(link: Transport, peer: bytes, disk: int, name: str, data: bytes, delay: float, timeout: float) -> None:
    checksum = internet_checksum(data)
    link.send(frame(peer, link.source, TAG_PUT_REQUEST,
                    bytes([disk]) + len(data).to_bytes(4, "little") + checksum.to_bytes(2, "little") + name.encode()))
    # The machine only answers a request to refuse it (busy, bad name, no such
    # disk, too large); acceptance is silent. Give a refusal a moment to arrive
    # before streaming, so we fail fast instead of sending into the void.
    deadline = time.monotonic() + 0.5
    while (left := deadline - time.monotonic()) > 0:
        packet = link.receive(left)
        body = packet and chat_body_from(packet, peer)
        if body and body[0] == TAG_PUT_ERROR:
            raise Fatal(f"the machine refused the transfer: {ERROR_NAMES.get(body[1], body[1])}")
    chunks = max(1, (len(data) + CHUNK_LEN - 1) // CHUNK_LEN)
    for seq, offset in enumerate(range(0, len(data), CHUNK_LEN)):
        link.send(frame(peer, link.source, TAG_PUT_DATA, seq.to_bytes(4, "little") + data[offset:offset + CHUNK_LEN]))
        if delay:
            time.sleep(delay)
    link.send(frame(peer, link.source, TAG_PUT_STOP))
    print(f"Sent {len(data)} byte(s) in {chunks} frame(s) (checksum 0x{checksum:04x}); waiting for the reply...",
          file=sys.stderr)
    deadline = time.monotonic() + timeout
    while (left := deadline - time.monotonic()) > 0:
        packet = link.receive(min(left, 0.2))
        body = packet and chat_body_from(packet, peer)
        if not body:
            continue
        if body[0] == TAG_PUT_ACK:
            return
        if body[0] == TAG_PUT_ERROR:
            raise Fatal(f"the machine rejected the file: {ERROR_NAMES.get(body[1], body[1])}")
    raise Fatal("timed out waiting for the machine's reply. It is probably not at its idle prompt or in "
                "`term net` (NETFS is not served inside chat, nic discover or a tape), the NIC is on "
                "deny-term-net, or the transfer lost a frame (try a larger --delay).")


def get(link: Transport, peer: bytes, disk: int, name: str, timeout: float) -> bytes:
    link.send(frame(peer, link.source, TAG_GET_REQUEST, bytes([disk]) + name.encode()))
    total = None
    checksum = 0
    buffer = bytearray()
    expected = 0
    deadline = time.monotonic() + timeout
    while (left := deadline - time.monotonic()) > 0:
        packet = link.receive(min(left, 0.2))
        body = packet and chat_body_from(packet, peer)
        if not body:
            continue
        tag, payload = body[0], body[1:]
        if tag == TAG_GET_ERROR:
            raise Fatal(f"the machine refused the request: {ERROR_NAMES.get(payload[0], payload[0])}")
        if tag == TAG_GET_START and total is None:
            total = int.from_bytes(payload[0:4], "little")
            checksum = int.from_bytes(payload[4:6], "little")
        elif tag == TAG_GET_DATA and total is not None:
            seq = int.from_bytes(payload[0:4], "little")
            if seq != expected:
                raise Fatal(f"lost a frame (got chunk {seq}, expected {expected}); NETFS does not retransmit, retry")
            buffer += payload[4:4 + min(CHUNK_LEN, total - len(buffer))]
            expected += 1
        elif tag == TAG_GET_STOP and total is not None:
            if len(buffer) != total:
                raise Fatal(f"transfer ended early ({len(buffer)} of {total} bytes); retry")
            if internet_checksum(bytes(buffer)) != checksum:
                raise Fatal("checksum mismatch; the data was damaged in transit, retry")
            return bytes(buffer)
        else:
            continue
        deadline = time.monotonic() + timeout  # progress: restart the idle clock
    raise Fatal("timed out waiting for the file. Is the machine at its idle prompt or in `term net`?")


def main() -> None:
    parser = ArgumentParser(description=__doc__.splitlines()[0], epilog="Run with --help for the full syntax.")
    parser.add_argument("source", help="local file, or HOST[:diskN]/NAME to fetch")
    parser.add_argument("destination", help="HOST[:diskN]/NAME to store to, or a local file/directory")
    parser.add_argument("--iface", help="host interface (default: auto-detected, but pass it explicitly)")
    parser.add_argument("--mac", help="machine's MAC or peer number, used when HOST is empty (':disk1/name')")
    parser.add_argument("--mcast", metavar="GROUP:PORT", help="use a UDP multicast group instead of a raw socket (tests)")
    parser.add_argument("--source-mac", default="02:00:00:f0:5c:01", help="our MAC when using --mcast")
    parser.add_argument("--discover-timeout", type=float, default=2.0)
    parser.add_argument("--delay", type=float, default=0.003,
                        help="seconds between PUT data frames (default 0.003, the real-hardware-proven pace)")
    parser.add_argument("--timeout", type=float, default=10.0, help="seconds of silence before giving up")
    args = parser.parse_args()

    src_remote = split_remote(args.source)
    dst_remote = split_remote(args.destination)
    if bool(src_remote) == bool(dst_remote):
        raise Fatal("exactly one of SOURCE and DESTINATION must be remote (HOST[:diskN]/NAME);\n"
                    "a local path containing a slash needs a leading ./ (or an existing directory)")

    if src_remote:
        host, disk, name = src_remote
        local = Path(args.destination)
        if local.is_dir():
            local = local / name
        data = b""
    else:
        host, disk, name = dst_remote
        local = Path(args.source)
        if not local.is_file():
            raise Fatal(f"{local}: no such file")
        data = local.read_bytes()
        if not name:
            name = local.name
        if not data:
            raise Fatal("refusing to copy an empty file (fos rejects zero-length files)")
    if not NAME_RE.match(name):
        raise Fatal(f"'{name}' is not a legal FOSFS name (up to {NAME_CAPACITY} of a-z 0-9 . _ -)")
    if disk < 1:
        raise Fatal("disk numbers start at 1")

    if args.mcast:
        link: Transport = McastTransport(args.mcast, parse_mac(args.source_mac))
    else:
        link = RawTransport(args.iface or default_interface())
    peer = resolve_host(link, host, args.mac, args.discover_timeout)
    where = f"{format_mac(peer)} disk{disk}/{name}"

    if src_remote:
        print(f"Fetching {where}...", file=sys.stderr)
        received = get(link, peer, disk, name, args.timeout)
        local.write_bytes(received)
        print(f"Copied {len(received)} byte(s) from {where} to {local}.")
    else:
        print(f"Sending {len(data)} byte(s) of {local} to {where}...", file=sys.stderr)
        put(link, peer, disk, name, data, args.delay, args.timeout)
        print(f"Copied {len(data)} byte(s) from {local} to {where}.")


if __name__ == "__main__":
    main()
