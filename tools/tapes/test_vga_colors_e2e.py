#!/usr/bin/env python3
"""One-off end-to-end test: send tools/tapes/vga-colors's built tape to a
running fos QEMU instance, run it, and check two things a purely serial-
stream test (like test_primes_e2e.py/test_hello_rs_e2e.py) cannot: that
its colored text actually lands in VGA text memory (physical 0xb8000,
read via the QEMU monitor's `pmemsave`, the same technique
test_primes_vga_e2e.py uses), and that a simulated local "give up"
keypress (QEMU monitor `sendkey f12`) actually stops its otherwise-
infinite loop and returns control to the fos shell -- proving the
`should_abort` half of the two-callback tape ABI (see `shell::tape_run`'s
doc comment and `keyboard::tape_should_abort`) really works, not just
compiles.

Drives `tape load net`/`tape run` through one continuous `term net`
session established right after boot, the same way test_hello_rs_e2e.py
does (see that file's own header comment for why: `term_net`,
kernel/src/shell.rs, now loops back to waiting for a new client instead
of falling through to the local shell on a remote disconnect, so there is
no "connect, then disconnect to reach the local prompt" trick left to use
instead). The tape's bytes themselves come from a second, independent
MAC, mirroring an ordinary `send_tape.py` run from another machine.

This test also confirms the specific guarantee this tape exists to
demonstrate: pressing "give up" while this tape's loop is running stops
only the tape, never the `term net` session driving it -- checked here by
issuing one more command over the very same session afterward and
confirming it still gets a reply, instead of reconnecting. It checks this
twice: once via a simulated *local* keypress (QEMU monitor `sendkey
f12`), and once via a simulated *remote* one (a raw 0x1b byte sent inside
an ordinary `TERM_TAG_DATA` frame, exactly what `tools/term_net.py
--interactive` now forwards for a real Escape keypress once its own
stdin is in cbreak mode) -- proving `shell::poll_remote_tape_abort`
really does let a remote peer stop a currently running tape even though
`term_net`'s own session loop (and so its own `nic.receive`) is paused
for as long as that tape runs.

Not part of `make test` -- manual verification script, in the same spirit
as test_primes_vga_e2e.py (which this borrows its monitor/pmemsave
approach from) and test_hello_rs_e2e.py (which this borrows its
term_net-session approach from). Needs no root, same as those, for the
same reason (QEMU's socket,mcast= netdev is plain UDP).
"""

import re
import socket as socketlib
import selectors
import struct
import subprocess
import sys
import time
import os
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.51"
MCAST_PORT = 4251
IMAGE = Path(__file__).resolve().parent.parent.parent / "build" / "fos.img"
TAPE = Path(__file__).resolve().parent.parent.parent / "build" / "tapes" / "vga-colors.bin"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef0e")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef0f")
MONITOR_SOCK = Path("/tmp/fos-test-vga-colors-monitor.sock")
VGA_DUMP = Path("/tmp/fos-test-vga-colors-dump.bin")
CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

VGA_WIDTH = 80
VGA_HEIGHT = 25


def monitor_command(command: str, wait: float = 0.5) -> None:
    """Sends one line to the QEMU human monitor over its Unix socket and
    waits for it to be processed. Doesn't bother parsing the monitor's own
    (ANSI-decorated) reply text -- callers instead check the side effect
    directly."""
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
    sock.recv(65536)  # discard the monitor's startup banner/prompt
    sock.sendall((command + "\n").encode())
    time.sleep(wait)
    sock.close()


def read_vga_text_and_colors():
    """Dumps the live VGA text framebuffer and decodes it into a plain
    25-row/80-column string (attribute bytes dropped, trailing spaces
    stripped), plus a parallel list of each row's set of distinct
    foreground-color nibbles actually written (background/blink bits
    masked off), for rows that have any non-space text."""
    if VGA_DUMP.exists():
        VGA_DUMP.unlink()
    monitor_command(f'pmemsave 0xb8000 {VGA_WIDTH * VGA_HEIGHT * 2} "{VGA_DUMP}"', wait=1.0)
    raw = VGA_DUMP.read_bytes()
    chars = bytes(raw[i] for i in range(0, len(raw), 2))
    attrs = bytes(raw[i + 1] for i in range(0, len(raw), 2))
    rows_text = [chars[r * VGA_WIDTH:(r + 1) * VGA_WIDTH].decode("ascii", "replace") for r in range(VGA_HEIGHT)]
    rows_colors = []
    for r in range(VGA_HEIGHT):
        row_chars = chars[r * VGA_WIDTH:(r + 1) * VGA_WIDTH]
        row_attrs = attrs[r * VGA_WIDTH:(r + 1) * VGA_WIDTH]
        colors = {attr & 0x0F for byte, attr in zip(row_chars, row_attrs) if byte != ord(" ")}
        rows_colors.append(colors)
    text = "\n".join(row.rstrip() for row in rows_text)
    return text, rows_colors


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-vga-colors` first.")
    if MONITOR_SOCK.exists():
        MONITOR_SOCK.unlink()

    qemu = subprocess.Popen(
        [
            "qemu-system-i386",
            "-drive", f"format=raw,file={IMAGE}",
            "-serial", "stdio",
            "-monitor", f"unix:{MONITOR_SOCK},server,nowait",
            "-no-reboot", "-no-shutdown",
            "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
            "-device", "rtl8139,netdev=net0",
            "-display", "none",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    output = bytearray()
    selector = selectors.DefaultSelector()
    selector.register(qemu.stdout, selectors.EVENT_READ)

    def read_until(marker: bytes, timeout: float) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in output:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(f"timed out waiting for {marker!r}; output so far:\n{output.decode('utf-8', 'replace')}")
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(f"QEMU serial stream ended early:\n{output.decode('utf-8', 'replace')}")
                output.extend(chunk)
        return bytes(output)

    try:
        read_until(b"Waiting for a remote terminal client to connect", 30)
        del output[:]
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"nic\n"))
        read_until(b"fos> ", 10)
        match = re.search(rb"RTL8139 found: MAC ([0-9a-fA-F:]{17})", bytes(output))
        if not match:
            raise SystemExit(f"couldn't find fos's MAC address in:\n{output.decode('utf-8', 'replace')}")
        fos_mac = send_tape.parse_mac(match.group(1).decode())
        print(f"fos MAC: {send_tape.format_mac(fos_mac)}", file=sys.stderr)
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)  # give the shell a moment to enter its receive-wait loop

        data = TAPE.read_bytes()
        print(f"sending {len(data)}-byte tape from an independent sender...", file=sys.stderr)
        send_tape.send_tape(mcast, fos_mac, TAPE_SENDER_MAC, data, delay=0.003)

        read_until(b"fos> ", 30)
        if b"Tape loaded" not in output:
            raise SystemExit(f"tape did not load cleanly:\n{output.decode('utf-8', 'replace')}")
        print("tape loaded OK", file=sys.stderr)
        del output[:]

        # Start the tape's infinite animation loop.
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until(b"watch the screen", 10)
        print("tape running; letting it animate for a couple of frames...", file=sys.stderr)
        time.sleep(1.0)

        # Confirm the animation is genuinely painting VGA text memory
        # directly, in more than one color, *before* touching the "give
        # up" key at all.
        text, colors = read_vga_text_and_colors()
        print("---- VGA text framebuffer while the tape is still looping ----", file=sys.stderr)
        print(text, file=sys.stderr)
        print("----------------------------------------------------------------", file=sys.stderr)
        assert "VGA color demo" in text, "tape's own banner text not visible in VGA text memory"
        assert "press F12" in text, "tape's own instructions not visible in VGA text memory"
        nonempty_rows_with_color = [c for c in colors if c]
        assert len(nonempty_rows_with_color) >= 10, (
            f"expected at least 10 colored text rows, saw {len(nonempty_rows_with_color)}: {colors}"
        )
        all_colors_seen = set().union(*colors) if colors else set()
        assert len(all_colors_seen) >= 5, f"expected several distinct foreground colors, saw only {all_colors_seen}"

        # This is the actual point of this test: pressing "give up"
        # (simulated here via the QEMU monitor's `sendkey`, since nothing
        # over term_net can reach the *local* keyboard) must stop the
        # tape's own loop -- and must NOT also disconnect the term_net
        # session driving it (see keyboard::tape_should_abort's doc
        # comment for exactly why that's guaranteed).
        print("sending a simulated local F12 keypress via the QEMU monitor...", file=sys.stderr)
        monitor_command("sendkey f12", wait=0.5)
        read_until(b"VGA color demo stopped.", 10)
        read_until(b"Tape returned; back at the fos shell.", 10)
        print("tape stopped cleanly after the simulated local escape key", file=sys.stderr)
        del output[:]

        # The real assertion: the term_net session itself must still be
        # alive -- prove it by sending one more command over it and
        # getting a reply, rather than needing to reconnect.
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(b"fos is a small educational 32-bit x86 operating system.", 10)
        print(
            "PASS: vga-colors tape painted VGA text memory directly in several "
            "colors, and a local 'give up' keypress stopped the tape's own loop "
            "without disconnecting the term_net session driving it.",
            file=sys.stderr,
        )
        del output[:]

        # Second half: the *remote* equivalent. term_net's own session
        # loop (and so its own nic.receive) is paused for as long as
        # tape_run blocks, so a genuine remote client has no way to poll
        # for this -- shell::poll_remote_tape_abort exists precisely to
        # let a nested command like this notice a REMOTE_ABORT_BYTE
        # (0x1b) arriving inside an ordinary TERM_TAG_DATA frame anyway.
        # tools/term_net.py's own --interactive mode sends exactly this
        # byte for a plain Escape keypress once its stdin is in cbreak
        # mode; this test sends the same byte directly over the raw
        # socket instead, since it has no real terminal to type at.
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until(b"watch the screen", 10)
        print("tape running again; sending a simulated remote Escape byte (0x1b)...", file=sys.stderr)
        time.sleep(1.0)
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, bytes([0x1B])))
        read_until(b"VGA color demo stopped.", 10)
        read_until(b"Tape returned; back at the fos shell.", 10)
        print("tape stopped cleanly after the simulated remote escape byte", file=sys.stderr)
        del output[:]

        # And the same survival check again: a remote-triggered abort
        # must not have ended the session either.
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(b"fos is a small educational 32-bit x86 operating system.", 10)
        print(
            "PASS: a simulated remote Escape byte stopped the tape's own loop "
            "too, again without disconnecting the term_net session driving it.",
            file=sys.stderr,
        )

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
