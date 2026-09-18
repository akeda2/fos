#!/usr/bin/env python3
"""Drive a fos host's shell remotely, over Ethernet, via `term net`.

This is the host-side half of fos's `term net` command (see
`kernel/src/shell.rs`'s `term_net` and README.md's "Remote terminal
(`term net`)" section): unlike `tools/send_tape.py`/`send_frame.py`, which
each speak a small one-shot protocol of their own, `term net` hands the
*entire* live fos shell over the wire -- every command that works locally
(`nic`, `chat`, `tape`, `ls`, ...) works identically here, because the
fos kernel feeds incoming bytes through the exact same line editor and
`execute()` dispatcher a local keyboard would.

Wire protocol (shared with `kernel/src/shell.rs`'s `TERM_TAG_*` constants,
all riding `CHAT_ETHERTYPE` like `chat`/`tape`): `TERM_TAG_HELLO` (no
payload) opens a session, `TERM_TAG_DATA` (raw byte payload, either
direction) carries keystrokes one way and shell output the other, and
`TERM_TAG_BYE` (no payload) closes it from whichever side sends it first.

This tool's primary mode is non-interactive: give it one or more command
lines on the argument list (or on stdin, one per line, with --stdin), and
it sends each, waits for a quiet period on the wire (no new output for
--settle seconds) before printing what came back and moving to the next
one, then sends TERM_TAG_BYE and exits -- well suited to being driven by a
script rather than a human at a keyboard. Pass --interactive for a raw
terminal instead, reading from and echoing to this program's own stdin/
stdout, closer to a real remote login session: this program puts its own
terminal into cbreak mode (see `run_interactive`) so every keystroke is
forwarded the instant it is typed, with no waiting for Enter and no local
echo (the remote shell echoes typed characters itself, exactly as it
would for a local keyboard) -- this is what lets a plain Escape keypress
here reach the fos kernel as a single 0x1b byte, the same one
`kernel/src/shell.rs`'s `REMOTE_ABORT_BYTE` looks for to stop a
currently running blocking command like `tape run` (see that constant's
own doc comment) without ending this session -- F12 (and friends) has no
*session-ending* equivalent here by design, since a remote peer hanging
up is never supposed to look like a local escape key press to the fos
side (see `term_net`'s own doc comment in `kernel/src/shell.rs`); press
Ctrl-C or send fos's own `halt`/reboot to end the session itself, or use
--interactive's dedicated EOF-on-stdin handling).

Sending raw frames requires CAP_NET_RAW (in practice: run this as root,
e.g. with `sudo`, unless the interpreter has been granted the capability
directly, e.g. via `setcap cap_net_raw+ep`).
"""

import selectors
import socket
import sys
import termios
import time
import tty
from argparse import ArgumentParser
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import send_tape  # noqa: E402 -- reuses parse_mac/format_mac/discover/pad/interface helpers.

CHAT_ETHERTYPE = send_tape.CHAT_ETHERTYPE
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

# Must match kernel/src/shell.rs's TERM_DATA_CAPACITY: the payload of one
# TERM_TAG_DATA frame this tool sends is capped there, though (unlike
# tape's fixed-size chunks) nothing here requires matching it exactly --
# a longer line is simply split across more than one frame.
TERM_DATA_CAPACITY = 512


def send_control(raw: socket.socket, destination: bytes, source: bytes, tag: int) -> None:
    frame = bytearray(destination + source + CHAT_ETHERTYPE.to_bytes(2, "big") + bytes([tag]))
    raw.send(send_tape.pad(frame))


def send_data(raw: socket.socket, destination: bytes, source: bytes, data: bytes) -> None:
    for offset in range(0, len(data), TERM_DATA_CAPACITY):
        chunk = data[offset : offset + TERM_DATA_CAPACITY]
        frame = bytearray(destination + source + CHAT_ETHERTYPE.to_bytes(2, "big") + bytes([TERM_TAG_DATA]) + chunk)
        raw.send(send_tape.pad(frame))


def recv_data_frame(raw: socket.socket, source: bytes, peer: bytes, timeout: float) -> bytes | None:
    """Waits up to `timeout` seconds for one TERM_TAG_DATA frame from
    `peer` addressed to `source`, returning its (zero-trimmed, matching
    the trailing-padding convention `chat`/`tape` already use) payload, or
    None if nothing arrived in time. Also silently notices a TERM_TAG_BYE
    from `peer`, raising SystemExit so callers do not have to check for it
    themselves everywhere."""
    raw.settimeout(timeout)
    try:
        packet = raw.recv(2048)
    except socket.timeout:
        return None
    if len(packet) < 15 or packet[12:14] != CHAT_ETHERTYPE.to_bytes(2, "big") or packet[6:12] != peer:
        return None
    tag = packet[14]
    if tag == TERM_TAG_BYE:
        raise SystemExit("\nRemote fos shell closed the session (sent TERM_TAG_BYE).")
    if tag != TERM_TAG_DATA:
        return None
    payload = packet[15:]
    end = len(payload)
    while end > 0 and payload[end - 1] == 0:
        end -= 1
    return payload[:end]


def run_noninteractive(raw: socket.socket, source: bytes, peer: bytes, commands: list[str], settle: float, initial_wait: float) -> None:
    """Sends each of `commands` as one line, printing whatever text comes
    back before moving to the next: after sending, keeps reading
    TERM_TAG_DATA frames until `settle` seconds pass with none arriving,
    on the assumption that a quiet wire means the shell has finished
    reacting to that command (true for every existing fos command, none
    of which streams output indefinitely without more input)."""

    def drain_output(timeout_first: float) -> bool:
        """Returns whether any output was actually received."""
        timeout = timeout_first
        received_any = False
        while True:
            payload = recv_data_frame(raw, source, peer, timeout)
            if payload is None:
                return received_any
            received_any = True
            sys.stdout.buffer.write(payload)
            sys.stdout.flush()
            timeout = settle

    connected = drain_output(initial_wait)
    for command in commands:
        send_data(raw, peer, source, command.encode() + b"\n")
        if drain_output(settle):
            connected = True

    # Only say goodbye to a session that was actually established: if
    # nothing ever came back (no fos host was listening, or our own
    # TERM_TAG_HELLO never reached one), sending TERM_TAG_BYE regardless
    # would just leave a stray, unrelated control frame sitting unread in
    # some other fos host's Rx ring until the next time it happens to
    # poll for one -- exactly the kind of leftover frame that could
    # confuse a *later*, unrelated `term net` session on that host.
    if connected:
        send_control(raw, peer, source, TERM_TAG_BYE)
    else:
        print("No response from the remote shell; not sending TERM_TAG_BYE (no session was established).", file=sys.stderr)


def run_interactive(raw: socket.socket, source: bytes, peer: bytes, initial_wait: float) -> None:
    """A raw pass-through session: this program's own stdin is forwarded a
    byte at a time as it is typed, and anything the remote shell sends
    back is written straight to this program's stdout, with no local echo
    (the remote shell echoes typed characters itself, exactly as it would
    for a local keyboard) -- closer to a real terminal program than the
    request/response mode above, at the cost of needing a real interactive
    terminal to drive it from.

    Puts this program's own stdin into cbreak mode (see the `tty` module)
    when it is a real terminal: canonical line buffering and local echo
    are both off, but Ctrl-C/Ctrl-D still raise the usual
    KeyboardInterrupt/EOF this function already handles, since cbreak
    mode -- unlike full raw mode -- leaves signal-generating characters
    alone. Without this, every keystroke (including a lone Escape) would
    sit unsent in this program's own input buffer until Enter was
    pressed, which defeats `kernel/src/shell.rs`'s `REMOTE_ABORT_BYTE`
    check entirely: that check needs to see a single 0x1b byte arrive on
    its own, not batched together with whatever is typed next."""
    print(
        "Interactive session; Escape aborts a running remote command (like `tape run`) "
        "without ending this session; Ctrl-C or Ctrl-D ends the session itself (sends TERM_TAG_BYE).",
        file=sys.stderr,
    )
    selector = selectors.DefaultSelector()
    selector.register(sys.stdin, selectors.EVENT_READ)
    selector.register(raw, selectors.EVENT_READ)

    payload = recv_data_frame(raw, source, peer, initial_wait)
    connected = payload is not None
    if payload is not None:
        sys.stdout.buffer.write(payload)
        sys.stdout.flush()

    old_settings = None
    if sys.stdin.isatty():
        old_settings = termios.tcgetattr(sys.stdin.fileno())
        tty.setcbreak(sys.stdin.fileno())

    try:
        while True:
            for key, _ in selector.select():
                if key.fileobj is sys.stdin:
                    chunk = sys.stdin.buffer.read1(4096)
                    if not chunk:
                        return
                    send_data(raw, peer, source, chunk)
                    connected = True
                else:
                    raw.settimeout(0)
                    try:
                        packet = raw.recv(2048)
                    except (BlockingIOError, socket.timeout):
                        continue
                    if len(packet) < 15 or packet[12:14] != CHAT_ETHERTYPE.to_bytes(2, "big") or packet[6:12] != peer:
                        continue
                    connected = True
                    if packet[14] == TERM_TAG_BYE:
                        print("\nRemote fos shell closed the session.", file=sys.stderr)
                        return
                    if packet[14] == TERM_TAG_DATA:
                        body = packet[15:]
                        end = len(body)
                        while end > 0 and body[end - 1] == 0:
                            end -= 1
                        sys.stdout.buffer.write(body[:end])
                        sys.stdout.flush()
    except KeyboardInterrupt:
        pass
    finally:
        if old_settings is not None:
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSADRAIN, old_settings)
        # See run_noninteractive's identical guard: only say goodbye to a
        # session that was actually established, so a client that never
        # got a response (nobody was listening) does not leave a stray
        # TERM_TAG_BYE sitting unread in some host's Rx ring.
        if connected:
            send_control(raw, peer, source, TERM_TAG_BYE)


def main() -> None:
    parser = ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "commands", nargs="*", help="Shell command line(s) to run remotely, in order (ignored with --interactive)."
    )
    parser.add_argument("--stdin", action="store_true", help="Read additional command lines from stdin, one per line.")
    parser.add_argument("--interactive", action="store_true", help="Raw interactive session instead of request/response.")
    parser.add_argument("--iface", help="Network interface to send from (default: auto-detected).")
    parser.add_argument("--mac", help="Destination MAC address, or a peer number from tools/peers.py; skips discovery.")
    parser.add_argument(
        "--discover-timeout", type=float, default=1.0, help="Seconds to wait for discovery replies (default: 1.0)."
    )
    parser.add_argument(
        "--settle", type=float, default=0.5, help="Quiet period (seconds) that ends output collection per command (default: 0.5)."
    )
    parser.add_argument(
        "--initial-wait", type=float, default=2.0, help="Seconds to wait for the session banner/first prompt (default: 2.0)."
    )
    args = parser.parse_args()

    commands = list(args.commands)
    if args.stdin:
        commands.extend(line.rstrip("\n") for line in sys.stdin)
    if not args.interactive and not commands:
        raise SystemExit("no commands given; pass some on the command line, use --stdin, or pass --interactive.")

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

        known_peers = send_tape.load_known_peers()

        if args.mac:
            destination = send_tape.resolve_peer(args.mac, known_peers)
        else:
            print(f"Discovering a fos host on {iface} ({args.discover_timeout}s)...", file=sys.stderr)
            live_peers = send_tape.discover(raw, iface, source, args.discover_timeout)
            if any(send_tape.remember_known_peer(known_peers, mac) for mac in live_peers):
                send_tape.save_known_peers(known_peers)
            if len(live_peers) == 1:
                destination = live_peers[0]
                print(f"Found one peer: {send_tape.format_mac(destination)}", file=sys.stderr)
            elif not live_peers:
                raise SystemExit(
                    "no fos host replied to the discovery probe. Make sure one is running "
                    "'chat'/'nic discover'/'tape load net' (or already 'term net', which also "
                    "answers discovery until a client connects), or pass --mac AA:BB:CC:DD:EE:FF "
                    "(or a peer number) explicitly."
                )
            else:
                send_tape.print_known_peers(known_peers)
                raise SystemExit(
                    f"found {len(live_peers)} peers this probe; pass --mac with a full MAC address "
                    "or one of the numbers listed above to pick one."
                )

        print(f"Connecting to {send_tape.format_mac(destination)} on {iface}...", file=sys.stderr)
        send_control(raw, destination, source, TERM_TAG_HELLO)

        if args.interactive:
            run_interactive(raw, source, destination, args.initial_wait)
        else:
            run_noninteractive(raw, source, destination, commands, args.settle, args.initial_wait)


if __name__ == "__main__":
    main()
