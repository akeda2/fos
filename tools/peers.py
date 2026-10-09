#!/usr/bin/env python3
"""Look up, discover, and manage the peer address book send_tape.py and
term_net.py share, so a small PEER_NUMBER shortcut keeps meaning the same
machine across separate invocations of either tool -- the same convenience
fos's own `chat`/`tape send NAME PEER_MAC|PEER_NUMBER` get from the
kernel's own `PEERS` table (`kernel/src/shell.rs`'s `remember_peer`/
`peer_by_index`/`print_peers`), just persisted on disk instead of only in
one long-running kernel's RAM.

The one real difference from the kernel's own table: that one lives only
as long as one boot stays up, so it starts empty every reboot; this one is
a small JSON file (see `send_tape.PEERS_CACHE_FILE`) that survives between
runs of either tool, so it can grow stale (a MAC that answered yesterday
might be a different machine, or offline, today) -- `--forget` exists for
exactly that reason, deliberately requiring a person to notice and reset
it rather than trying to guess when an entry has gone stale.

Examples:
  tools/peers.py                  # print the known peer list; no network traffic at all
  tools/peers.py --discover       # broadcast a probe, merge any replies in, then print
  tools/peers.py --forget         # empty the address book

Host names come from the machines' own announcements (every fos machine broadcasts its name at boot and every 10 s) and from
discovery replies; `--mac NAME` in the other tools accepts them. With --discover, names are heard during the whole wait.

Broadcasting with --discover requires CAP_NET_RAW, same as
send_tape.py/term_net.py (in practice: run this as root, e.g. with
`sudo`, unless the interpreter has been granted the capability directly,
e.g. via `setcap cap_net_raw+ep`); printing or forgetting the cached list
needs no network access at all.
"""

import socket
import sys
from argparse import ArgumentParser
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import send_tape  # noqa: E402 -- reuses the shared on-disk address book and discovery logic.


def main() -> None:
    parser = ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--discover", action="store_true", help="Broadcast a discovery probe first, merging any replies into the address book."
    )
    parser.add_argument("--iface", help="Network interface to broadcast from (default: auto-detected). Only used with --discover.")
    parser.add_argument(
        "--timeout", type=float, default=2.0, help="Seconds to wait for discovery replies (default: 2.0). Only used with --discover."
    )
    parser.add_argument("--forget", action="store_true", help="Empty the address book instead of printing or discovering.")
    args = parser.parse_args()

    if args.forget:
        send_tape.save_known_peers([])
        print(f"Cleared {send_tape.PEERS_CACHE_FILE}.")
        return

    known_peers = send_tape.load_known_peers()

    if args.discover:
        iface = args.iface or send_tape.default_interface()
        source = send_tape.interface_mac(iface)
        with socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0003)) as raw:
            try:
                raw.bind((iface, 0))
            except PermissionError:
                raise SystemExit(
                    "permission denied opening a raw socket; sending raw Ethernet frames needs\n"
                    "CAP_NET_RAW -- try running this again with sudo."
                )
            print(f"Discovering fos hosts on {iface} ({args.timeout}s)...", file=sys.stderr)
            live_peers = send_tape.discover(raw, iface, source, args.timeout)
        added = sum(send_tape.remember_known_peer(known_peers, mac) for mac in live_peers)
        if added:
            send_tape.save_known_peers(known_peers)
        print(f"Found {len(live_peers)} peer(s) this probe ({added} new).", file=sys.stderr)

    send_tape.print_known_peers(known_peers)


if __name__ == "__main__":
    main()
