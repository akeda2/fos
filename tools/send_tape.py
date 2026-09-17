#!/usr/bin/env python3
"""Send a local file to a fos host running `tape load net`, as a "paper tape".

This is the host-side half of fos's `tape` feature (see `kernel/src/shell.rs`
and README.md's "Tape (`tape`)" section): it speaks the exact same
`CHAT_ETHERTYPE` wire protocol fos's own `tape send`/`tape load net` use --
one `TAPE_TAG_START` frame declaring the total length, then `TAPE_TAG_DATA`
frames each carrying an explicit sequence number and up to `TAPE_CHUNK_LEN`
bytes, then one `TAPE_TAG_STOP` frame -- built directly on an `AF_PACKET` raw
socket, no IP, no protocol above Ethernet, matching `tools/send_frame.py`'s
own approach.

Sending raw frames requires `CAP_NET_RAW` (in practice: run this as root,
e.g. with `sudo`).

Rather than requiring the destination fos machine's MAC address up front
(the way `send_frame.py` reads one from a small `--mac-file`), this tool can
find it itself: it broadcasts the same discovery probe fos's own
`nic discover` sends (`CHAT_TAG_DISCOVER_REQUEST`) and listens briefly for
replies. Exactly one reply picks the destination automatically; zero or
several require `--mac` to say which machine you mean. Pass `--mac` (or the
legacy `--mac-file`, kept for parity with `send_frame.py`) to skip discovery
entirely.
"""

import re
import socket
import sys
import time
from argparse import ArgumentParser
from pathlib import Path

# Shared with kernel/src/shell.rs: one dedicated ethertype carries chat,
# discovery, and tape traffic alike, distinguished by the one-byte tag that
# follows the Ethernet header (see CHAT_ETHERTYPE/TAPE_TAG_* there).
CHAT_ETHERTYPE = 0x88B6
CHAT_TAG_DISCOVER_REQUEST = 0x02
CHAT_TAG_DISCOVER_REPLY = 0x03
TAPE_TAG_START = 0x10
TAPE_TAG_DATA = 0x11
TAPE_TAG_STOP = 0x12

# Must match kernel/src/shell.rs's TAPE_CHUNK_LEN exactly: the receiver
# derives each chunk's expected length from its sequence number and the
# tape's declared total length, not from this tool's choice of chunk size,
# but both ends still need to agree on where one chunk ends and the next
# begins to reassemble the same sequence numbering.
TAPE_CHUNK_LEN = 1024

MIN_FRAME_LEN = 60
BROADCAST_MAC = b"\xff" * 6

MAC_PATTERN = re.compile(r"^([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}$")


def parse_mac(text: str) -> bytes:
    text = text.strip()
    if not MAC_PATTERN.match(text):
        raise SystemExit(f"'{text}' is not a MAC address in aa:bb:cc:dd:ee:ff form")
    return bytes(int(byte, 16) for byte in text.split(":"))


def format_mac(mac: bytes) -> str:
    return ":".join(f"{b:02x}" for b in mac)


def read_mac_file(mac_file: Path) -> bytes:
    if not mac_file.exists():
        raise SystemExit(f"{mac_file} does not exist.")
    return parse_mac(mac_file.read_text())


def default_interface() -> str:
    up_candidate = None
    any_candidate = None
    for name, is_up in _list_interfaces():
        if name == "lo":
            continue
        if is_up and up_candidate is None:
            up_candidate = name
        if any_candidate is None:
            any_candidate = name
    chosen = up_candidate or any_candidate
    if chosen is None:
        raise SystemExit("no non-loopback network interface found; pass --iface explicitly")
    return chosen


def _list_interfaces():
    sys_class_net = Path("/sys/class/net")
    for entry in sorted(sys_class_net.iterdir()):
        operstate_file = entry / "operstate"
        is_up = operstate_file.exists() and operstate_file.read_text().strip() == "up"
        yield entry.name, is_up


def interface_mac(iface: str) -> bytes:
    address_file = Path(f"/sys/class/net/{iface}/address")
    if not address_file.exists():
        raise SystemExit(f"interface '{iface}' not found (no {address_file})")
    return parse_mac(address_file.read_text())


def pad(frame: bytearray) -> bytes:
    if len(frame) < MIN_FRAME_LEN:
        frame.extend(b"\0" * (MIN_FRAME_LEN - len(frame)))
    return bytes(frame)


def discover(raw: socket.socket, iface: str, source: bytes, timeout: float) -> list[bytes]:
    """Broadcasts one CHAT_TAG_DISCOVER_REQUEST and collects every distinct
    CHAT_TAG_DISCOVER_REPLY seen within `timeout` seconds, the same window
    fos's own `nic discover` would keep listening through -- just much
    shorter, since this tool only needs one round, not an open-ended one."""
    frame = bytearray(BROADCAST_MAC + source + CHAT_ETHERTYPE.to_bytes(2, "big") + bytes([CHAT_TAG_DISCOVER_REQUEST]))
    raw.send(pad(frame))

    found: list[bytes] = []
    deadline = time.monotonic() + timeout
    raw.settimeout(0.1)
    while time.monotonic() < deadline:
        try:
            packet = raw.recv(2048)
        except socket.timeout:
            continue
        if len(packet) < 15:
            continue
        if packet[12:14] != CHAT_ETHERTYPE.to_bytes(2, "big") or packet[14] != CHAT_TAG_DISCOVER_REPLY:
            continue
        reply_source = packet[6:12]
        if reply_source == source or reply_source in found:
            continue
        found.append(reply_source)
    return found


def send_tape(raw: socket.socket, destination: bytes, source: bytes, data: bytes, delay: float) -> None:
    start = bytearray(destination + source + CHAT_ETHERTYPE.to_bytes(2, "big") + bytes([TAPE_TAG_START]) + len(data).to_bytes(4, "little"))
    raw.send(pad(start))

    total_chunks = max(1, (len(data) + TAPE_CHUNK_LEN - 1) // TAPE_CHUNK_LEN) if data else 0
    for seq, offset in enumerate(range(0, len(data), TAPE_CHUNK_LEN)):
        chunk = data[offset : offset + TAPE_CHUNK_LEN]
        frame = bytearray(
            destination + source + CHAT_ETHERTYPE.to_bytes(2, "big") + bytes([TAPE_TAG_DATA]) + seq.to_bytes(4, "little") + chunk
        )
        raw.send(pad(frame))
        print(f"  sent chunk {seq + 1}/{total_chunks} ({len(chunk)} bytes)", file=sys.stderr)
        if delay:
            time.sleep(delay)

    stop = bytearray(destination + source + CHAT_ETHERTYPE.to_bytes(2, "big") + bytes([TAPE_TAG_STOP]))
    raw.send(pad(stop))


def main() -> None:
    parser = ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("file", type=Path, help="File to send as the tape's contents.")
    parser.add_argument("--iface", help="Network interface to send from (default: auto-detected).")
    parser.add_argument("--mac", help="Destination MAC address; skips discovery.")
    parser.add_argument(
        "--mac-file",
        type=Path,
        help="File containing the destination MAC address (kept for parity with send_frame.py); skips discovery.",
    )
    parser.add_argument(
        "--discover-timeout",
        type=float,
        default=1.0,
        help="Seconds to wait for discovery replies before giving up (default: 1.0).",
    )
    parser.add_argument(
        "--delay",
        type=float,
        default=0.0,
        help=(
            "Seconds to sleep between data frames. fos's RTL8139 driver only polls "
            "for frames (no interrupt, and one receive ring no bigger than 32 KiB); "
            "on real hardware, sending a nontrivial tape with no delay at all can "
            "outrun it and lose data (fos reports this as an overflow-recovery count "
            "and/or a 'Gap in tape data' abort, never a silent hang). "
            "0.003 (3ms) has been confirmed reliable against real hardware for a "
            "~1 MB tape; QEMU needs the same pacing (default: 0, none)."
        ),
    )
    args = parser.parse_args()

    data = args.file.read_bytes()
    if not data:
        raise SystemExit(f"{args.file} is empty; nothing to send.")

    iface = args.iface or default_interface()
    source = interface_mac(iface)

    # ETH_P_ALL (0x0003), not CHAT_ETHERTYPE, so this socket can both send
    # tape/discovery frames and receive the discovery replies that answer
    # them; the ethertype/tag filtering below is done in Python instead.
    with socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0003)) as raw:
        try:
            raw.bind((iface, 0))
        except PermissionError:
            raise SystemExit(
                "permission denied opening a raw socket; sending raw Ethernet frames needs\n"
                "CAP_NET_RAW -- try running this again with sudo."
            )

        if args.mac:
            destination = parse_mac(args.mac)
        elif args.mac_file:
            destination = read_mac_file(args.mac_file)
        else:
            print(f"Discovering a fos host on {iface} ({args.discover_timeout}s)...", file=sys.stderr)
            peers = discover(raw, iface, source, args.discover_timeout)
            if len(peers) == 1:
                destination = peers[0]
                print(f"Found one peer: {format_mac(destination)}", file=sys.stderr)
            elif not peers:
                raise SystemExit(
                    "no fos host replied to the discovery probe. Make sure one is running "
                    "'tape load net' or 'nic discover'/'chat', or pass --mac AA:BB:CC:DD:EE:FF explicitly."
                )
            else:
                listed = ", ".join(format_mac(mac) for mac in peers)
                raise SystemExit(f"found {len(peers)} peers ({listed}); pass --mac to pick one.")

        print(f"Sending {len(data)} byte(s) from {args.file} to {format_mac(destination)} on {iface}...", file=sys.stderr)
        send_tape(raw, destination, source, data, args.delay)

    print(f"Sent {len(data)} byte(s) to {format_mac(destination)}.")


if __name__ == "__main__":
    main()
