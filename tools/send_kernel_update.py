#!/usr/bin/env python3
"""Push a new kernel image to a fos host running in debug mode with the
`kernel-update` option enabled (see `kernel/src/debug.rs`,
`kernel/src/shell.rs`'s `serve_kup_put_*` functions, and AGENTS.md's
"KUP: a network kernel-update service" section).

This is the host-side half of the KUP (kernel update) protocol: it shares
`CHAT_ETHERTYPE` with `chat`/`nic discover`/`tape`/`term net`/NETFS, and
mirrors NETFS's own PUT flow almost exactly -- a `KUP_TAG_PUT_REQUEST`
frame declaring the image's exact length and its RFC 1071 Internet
checksum, a `TAPE_TAG_DATA`-shaped burst of sequence-numbered
`KUP_TAG_PUT_DATA` chunks, then `KUP_TAG_PUT_STOP`, followed by either a
`KUP_TAG_PUT_ACK` (the receiving machine will reboot into the new kernel
shortly afterward, on a fresh, unconfirmed A/B slot) or a one-byte
`KUP_TAG_PUT_ERROR` explaining why it refused (see `KUP_ERROR_*` below).

Peer discovery/addressing and the on-disk peer address book are reused
directly from `send_tape.py` (`--mac`, a peer number, or automatic
single-reply discovery) -- this tool intentionally does not duplicate any
of that logic.

Sending raw frames requires `CAP_NET_RAW` (in practice: run this as root,
e.g. with `sudo`, or see AGENTS.md's `setcap`-on-one-binary workaround for
a sandboxed dev environment).

A machine with `kernel-update` not enabled in its `debug` file does not
answer KUP frames at all -- this tool will simply wait for the ACK/ERROR
timeout and then report failure, which is indistinguishable, on the wire,
from "no such machine".
"""

import socket
import sys
import time
from argparse import ArgumentParser
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from send_tape import (  # noqa: E402
    CHAT_ETHERTYPE,
    default_interface,
    format_mac,
    interface_mac,
    load_known_peers,
    pad,
    parse_mac,
    print_known_peers,
    read_mac_file,
    resolve_peer,
    save_known_peers,
)
from send_tape import discover as discover_peers  # noqa: E402
from send_tape import remember_known_peer  # noqa: E402

# Continues the tag numbering kernel/src/shell.rs assigns right after
# NETFS's own tags (0x2f-0x39); must stay byte-for-byte in sync with the
# KUP_TAG_* constants there.
KUP_TAG_PUT_REQUEST = 0x3A
KUP_TAG_PUT_DATA = 0x3B
KUP_TAG_PUT_STOP = 0x3C
KUP_TAG_PUT_ACK = 0x3D
KUP_TAG_PUT_ERROR = 0x3E

KUP_ERROR_NAMES = {
    1: "TOO_LARGE (the image is larger than one A/B slot, or the shared staging buffer)",
    2: "CHECKSUM_MISMATCH (the reassembled image's checksum didn't match the declared one)",
    3: "DISK_ERROR (writing the inactive slot, or reading slot metadata, failed)",
    4: "BUSY (another kernel update is already in progress on that machine)",
}

# Must match kernel/src/shell.rs's TAPE_CHUNK_LEN, which KUP data chunks
# reuse exactly -- the receiver derives nothing from this tool's own
# choice of chunk size beyond agreeing where one chunk ends and the next
# begins for sequence numbering.
CHUNK_LEN = 1024


def internet_checksum(data: bytes) -> int:
    """The RFC 1071 16-bit Internet checksum, matching
    kernel/src/shell.rs's `internet_checksum` exactly (used identically by
    NETFS): sum 16-bit big-endian words with end-around carry, then invert."""
    total = 0
    it = iter(data)
    for high in it:
        low = next(it, 0)
        total += (high << 8) | low
    while total > 0xFFFF:
        total = (total & 0xFFFF) + (total >> 16)
    return (~total) & 0xFFFF


def send_kernel_update(
    raw: socket.socket,
    destination: bytes,
    source: bytes,
    data: bytes,
    delay: float,
    timeout: float,
) -> None:
    checksum = internet_checksum(data)
    request = bytearray(
        destination
        + source
        + CHAT_ETHERTYPE.to_bytes(2, "big")
        + bytes([KUP_TAG_PUT_REQUEST])
        + len(data).to_bytes(4, "little")
        + checksum.to_bytes(2, "little")
    )
    raw.send(pad(request))

    total_chunks = max(1, (len(data) + CHUNK_LEN - 1) // CHUNK_LEN)
    for seq, offset in enumerate(range(0, len(data), CHUNK_LEN)):
        chunk = data[offset : offset + CHUNK_LEN]
        frame = bytearray(
            destination
            + source
            + CHAT_ETHERTYPE.to_bytes(2, "big")
            + bytes([KUP_TAG_PUT_DATA])
            + seq.to_bytes(4, "little")
            + chunk
        )
        raw.send(pad(frame))
        print(f"  sent chunk {seq + 1}/{total_chunks} ({len(chunk)} bytes)", file=sys.stderr)
        if delay:
            time.sleep(delay)

    stop = bytearray(destination + source + CHAT_ETHERTYPE.to_bytes(2, "big") + bytes([KUP_TAG_PUT_STOP]))
    raw.send(pad(stop))

    print(f"Sent {len(data)} byte(s) (checksum 0x{checksum:04x}); waiting for a reply...", file=sys.stderr)
    raw.settimeout(0.2)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            packet = raw.recv(2048)
        except socket.timeout:
            continue
        if len(packet) < 15:
            continue
        if packet[12:14] != CHAT_ETHERTYPE.to_bytes(2, "big"):
            continue
        if packet[6:12] != destination:
            continue
        tag = packet[14]
        if tag == KUP_TAG_PUT_ACK:
            print(
                "ACK: the remote machine accepted the update and will reboot into it shortly.",
            )
            return
        if tag == KUP_TAG_PUT_ERROR:
            code = packet[15] if len(packet) > 15 else 0
            reason = KUP_ERROR_NAMES.get(code, f"unknown code {code}")
            raise SystemExit(f"the remote machine rejected the update: {reason}")
    raise SystemExit(
        "timed out waiting for an ACK/ERROR reply. Either the destination isn't running "
        "in debug mode with 'kernel-update' enabled (KUP frames are silently ignored "
        "when the option is off), or the update is still being written -- a full-size "
        "kernel image can take a little while at the default --delay."
    )


def main() -> None:
    parser = ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("kernel", help="Path to the new kernel.bin image to push.")
    parser.add_argument("--iface", help="Network interface to send from (default: auto-detected).")
    parser.add_argument("--mac", help="Destination MAC address, or a peer number from tools/peers.py; skips discovery.")
    parser.add_argument(
        "--mac-file",
        help="File containing the destination MAC address; skips discovery.",
    )
    parser.add_argument(
        "--discover-timeout",
        type=float,
        default=2.0,
        help="Seconds to wait for discovery replies before giving up (default: 2.0).",
    )
    parser.add_argument(
        "--delay",
        type=float,
        default=0.003,
        help=(
            "Seconds to sleep between data frames (default: 0.003, matching "
            "send_tape.py's own real-hardware-confirmed pacing -- a kernel image is "
            "typically far larger than a tape and needs the same protection against "
            "outrunning the polling-only RTL8139 receive path)."
        ),
    )
    parser.add_argument(
        "--ack-timeout",
        type=float,
        default=10.0,
        help="Seconds to wait for an ACK/ERROR reply after sending (default: 10.0).",
    )
    args = parser.parse_args()

    kernel_path = Path(args.kernel)
    data = kernel_path.read_bytes()
    if not data:
        raise SystemExit(f"{kernel_path} is empty; nothing to send.")

    iface = args.iface or default_interface()
    source = interface_mac(iface)

    with socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0003)) as raw:
        try:
            raw.bind((iface, 0))
        except PermissionError:
            raise SystemExit(
                "permission denied opening a raw socket; sending raw Ethernet frames needs\n"
                "CAP_NET_RAW -- try running this again with sudo."
            )

        known_peers = load_known_peers()

        if args.mac:
            destination = resolve_peer(args.mac, known_peers)
        elif args.mac_file:
            destination = read_mac_file(Path(args.mac_file))
        else:
            print(f"Discovering a fos host on {iface} ({args.discover_timeout}s)...", file=sys.stderr)
            live_peers = discover_peers(raw, iface, source, args.discover_timeout)
            if any(remember_known_peer(known_peers, mac) for mac in live_peers):
                save_known_peers(known_peers)
            if len(live_peers) == 1:
                destination = live_peers[0]
                print(f"Found one peer: {format_mac(destination)}", file=sys.stderr)
            elif not live_peers:
                raise SystemExit(
                    "no fos host replied to the discovery probe. Pass --mac AA:BB:CC:DD:EE:FF "
                    "(or a peer number) explicitly."
                )
            else:
                print_known_peers(known_peers)
                raise SystemExit(
                    f"found {len(live_peers)} peers this probe; pass --mac with a full MAC "
                    "address or one of the numbers listed above to pick one."
                )

        print(
            f"Pushing {len(data)} byte(s) from {kernel_path} to {format_mac(destination)} on {iface}...",
            file=sys.stderr,
        )
        send_kernel_update(raw, destination, source, data, args.delay, args.ack_timeout)


if __name__ == "__main__":
    main()
