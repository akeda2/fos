#!/usr/bin/env python3
"""Send one raw Ethernet frame to a fos host running `nic sniff`.

`nic sniff` (see kernel/src/shell.rs and README.md's "Ethernet (`nic`)"
section) puts a fos machine's RTL8139 into promiscuous mode and prints
every frame it sees. This is the host-side counterpart: it builds one raw
Ethernet frame (destination MAC + this host's own source MAC + ethertype +
payload) and transmits it directly on a chosen network interface via a
Linux `AF_PACKET` raw socket -- no IP, no sockets library, just the same
"whole frame, no protocol above Ethernet" level fos's own driver speaks.

Sending raw frames requires `CAP_NET_RAW` (in practice: run this as root,
e.g. with `sudo`).

The destination MAC address is read from a small local file rather than a
required command-line argument, since it will typically be the same
physical fos machine's card every time during one session of testing: see
`--mac-file` (default `tools/mac`, one line of text like
`52:54:00:12:34:56`, deliberately `.gitignore`d since it identifies
whoever's test hardware is at hand, not something to check in).
"""

import re
import socket
import sys
from argparse import ArgumentParser
from pathlib import Path

# The same IEEE-reserved "experimental" ethertype fos's own `nic send`
# tags its test frames with (see kernel/src/shell.rs's TEST_ETHERTYPE), so
# frames from this tool are just as unmistakably test traffic and easy to
# pick out of a `nic sniff` capture.
DEFAULT_ETHERTYPE = 0x88B5

# Real Ethernet requires at least 60 bytes of destination+source+ethertype
# +payload (before the 4-byte FCS the hardware appends); fos's own
# Rtl8139::send zero-pads up to this length, and this tool matches it so a
# short payload behaves identically from either end.
MIN_FRAME_LEN = 60

DEFAULT_PAYLOAD = b"fos raw ethernet test frame from host"

MAC_PATTERN = re.compile(r"^([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}$")


def parse_mac(text: str) -> bytes:
    text = text.strip()
    if not MAC_PATTERN.match(text):
        raise SystemExit(f"'{text}' is not a MAC address in aa:bb:cc:dd:ee:ff form")
    return bytes(int(byte, 16) for byte in text.split(":"))


def read_destination_mac(mac_file: Path) -> bytes:
    if not mac_file.exists():
        raise SystemExit(
            f"{mac_file} does not exist. Create it with the target fos machine's MAC\n"
            "address on one line (as shown by fos's 'nic' command), e.g.:\n"
            f"  echo 52:54:00:12:34:56 > {mac_file}"
        )
    return parse_mac(mac_file.read_text())


def default_interface() -> str:
    """Picks the first non-loopback interface that has link-layer
    addressing at all (i.e. `AF_PACKET` reports a hardware address for
    it), preferring one that is currently up. There is no single
    "default route interface" concept at this raw a level, so this is a
    best-effort guess only -- pass --iface explicitly to skip the guess."""
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


def build_frame(destination: bytes, source: bytes, ethertype: int, payload: bytes) -> bytes:
    frame = bytearray(destination + source + ethertype.to_bytes(2, "big") + payload)
    if len(frame) < MIN_FRAME_LEN:
        frame.extend(b"\0" * (MIN_FRAME_LEN - len(frame)))
    return bytes(frame)


def main() -> None:
    parser = ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("text", nargs="?", help="Literal payload text (default: a fixed test string).")
    parser.add_argument("--file", type=Path, help="Send this file's raw bytes as the payload instead of TEXT.")
    parser.add_argument("--iface", help="Network interface to send from (default: auto-detected).")
    parser.add_argument(
        "--mac-file",
        type=Path,
        default=Path(__file__).resolve().parent / "mac",
        help="File containing the destination MAC address (default: tools/mac).",
    )
    parser.add_argument(
        "--ethertype",
        type=lambda value: int(value, 0),
        default=DEFAULT_ETHERTYPE,
        help=f"Ethertype field, decimal or 0x-prefixed hex (default: 0x{DEFAULT_ETHERTYPE:04x}).",
    )
    args = parser.parse_args()

    if args.text is not None and args.file is not None:
        raise SystemExit("pass either TEXT or --file, not both")
    if args.file is not None:
        payload = args.file.read_bytes()
    elif args.text is not None:
        payload = args.text.encode()
    else:
        payload = DEFAULT_PAYLOAD

    destination = read_destination_mac(args.mac_file)
    iface = args.iface or default_interface()
    source = interface_mac(iface)
    frame = build_frame(destination, source, args.ethertype, payload)

    try:
        with socket.socket(socket.AF_PACKET, socket.SOCK_RAW) as raw:
            raw.bind((iface, 0))
            raw.send(frame)
    except PermissionError:
        raise SystemExit(
            "permission denied opening a raw socket; sending raw Ethernet frames needs\n"
            "CAP_NET_RAW -- try running this again with sudo."
        )

    dest_text = ":".join(f"{b:02x}" for b in destination)
    source_text = ":".join(f"{b:02x}" for b in source)
    print(
        f"Sent {len(frame)}-byte frame on {iface}: dst {dest_text} src {source_text} "
        f"ethertype 0x{args.ethertype:04x}, payload {len(payload)} byte(s)."
    )


if __name__ == "__main__":
    main()
