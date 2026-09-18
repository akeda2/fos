#!/usr/bin/env python3
"""One-off end-to-end test: send tools/tapes/primes.bin to a running fos
QEMU instance, run it, and check its summary by reading the *actual VGA
text framebuffer* (physical 0xb8000) via the QEMU monitor's `pmemsave`,
rather than QEMU's `-serial stdio` stream.

This distinction matters: primes.S's print_char mirrors every character
it prints to COM1 (serial_putc) unconditionally, *before* it even touches
VGA memory or the tracked cursor_pos. That made test_primes_e2e.py (which
only reads the serial stream) blind to a real, since-fixed bug where the
VGA side of the summary printing silently broke -- first because
cursor_pos was never bounds-checked and could scroll off-screen on real
hardware's differently-positioned starting cursor, and separately because
print_decimal briefly repurposed %ebx (this file's PIC load-base
register, required by every print_char call it makes) as a digit
counter, corrupting where digits landed in VGA memory specifically, while
their serial mirror bytes came out perfectly correct regardless. Anyone
touching primes.S's VGA/cursor code should run this test, not just
test_primes_e2e.py, since only this one actually checks what the tape's
real screen would show.

Not part of `make test` -- manual verification script, in the same spirit
as test_primes_e2e.py (which this reuses the sending logic from). Needs
no root, same as test_primes_e2e.py, for the same reason (QEMU's
socket,mcast= netdev is plain UDP). The QEMU monitor is exposed over a
throwaway local Unix socket rather than stdio, since -serial stdio is
already used for the shell's own text console.
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

MCAST_GROUP = "230.42.42.46"
MCAST_PORT = 4246
IMAGE = Path(__file__).resolve().parent.parent.parent / "build" / "fos.img"
TAPE = Path(__file__).resolve().parent.parent.parent / "build" / "tapes" / "primes.bin"
HOST_MAC = bytes.fromhex("02deadbeef05")
MONITOR_SOCK = Path("/tmp/fos-test-primes-vga-monitor.sock")
VGA_DUMP = Path("/tmp/fos-test-primes-vga-dump.bin")
CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_BYE = 0x22

VGA_WIDTH = 80
VGA_HEIGHT = 25


def monitor_command(command: str, wait: float = 0.5) -> None:
    """Sends one line to the QEMU human monitor over its Unix socket and
    waits for it to be processed. Doesn't bother parsing the monitor's own
    (ANSI-decorated) reply text -- callers instead check the side effect
    (e.g. the file pmemsave writes) directly."""
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


def read_vga_text() -> str:
    """Dumps the live VGA text framebuffer and decodes it into a plain
    25-row/80-column string, dropping the per-cell attribute bytes and any
    trailing spaces on each row."""
    if VGA_DUMP.exists():
        VGA_DUMP.unlink()
    monitor_command(f'pmemsave 0xb8000 {VGA_WIDTH * VGA_HEIGHT * 2} "{VGA_DUMP}"', wait=1.0)
    raw = VGA_DUMP.read_bytes()
    chars = bytes(raw[i] for i in range(0, len(raw), 2))
    rows = [chars[r * VGA_WIDTH:(r + 1) * VGA_WIDTH].decode("ascii", "replace") for r in range(VGA_HEIGHT)]
    return "\n".join(row.rstrip() for row in rows)


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-primes` first.")
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

    def send_line(line: str) -> None:
        qemu.stdin.write((line + "\n").encode())
        qemu.stdin.flush()

    def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
        frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
        if len(frame) < 60:
            frame.extend(b"\0" * (60 - len(frame)))
        return bytes(frame)

    try:
        # fos now runs `term net` automatically at boot (see shell::run/
        # AGENTS.md), waiting for a remote terminal client before it ever
        # shows the "fos> " prompt. This test doesn't want a term net
        # session, just the ordinary local prompt -- so it connects and
        # immediately disconnects one to skip past the automatic wait,
        # the same trick tools/tapes/test_primes_e2e.py uses.
        read_until(b"Waiting for a remote terminal client to connect", 30)
        del output[:]
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        mcast.send(term_frame(b"\xff" * 6, HOST_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)  # the initial in-session prompt
        del output[:]  # clear it so it can't be mistaken for the post-disconnect prompt below
        mcast.send(term_frame(b"\xff" * 6, HOST_MAC, TERM_TAG_BYE))
        read_until(b"Remote terminal disconnected", 10)
        read_until(b"fos> ", 10)
        del output[:]

        send_line("nic")
        read_until(b"fos> ", 10)
        match = re.search(rb"RTL8139 found: MAC ([0-9a-fA-F:]{17})", bytes(output))
        if not match:
            raise SystemExit(f"couldn't find fos's MAC address in:\n{output.decode('utf-8', 'replace')}")
        fos_mac = send_tape.parse_mac(match.group(1).decode())
        print(f"fos MAC: {send_tape.format_mac(fos_mac)}", file=sys.stderr)
        del output[:]

        send_line("tape load net")
        time.sleep(0.5)  # give the shell a moment to enter its receive-wait loop

        data = TAPE.read_bytes()
        print(f"sending {len(data)}-byte tape...", file=sys.stderr)
        send_tape.send_tape(mcast, fos_mac, HOST_MAC, data, delay=0.003)

        read_until(b"fos> ", 30)
        if b"Tape loaded" not in output:
            raise SystemExit(f"tape did not load cleanly:\n{output.decode('utf-8', 'replace')}")
        print("tape loaded OK", file=sys.stderr)
        del output[:]

        send_line("tape run")
        read_until(b"Tape returned; back at the fos shell.", 30)

        screen = read_vga_text()
        print("---- VGA text framebuffer ----", file=sys.stderr)
        print(screen, file=sys.stderr)
        print("-------------------------------", file=sys.stderr)

        assert "78498" in screen, "prime count '78498' not visible in VGA text memory"
        assert "999983" in screen, "largest prime '999983' not visible in VGA text memory"
        assert "Tape returned; back at the fos shell." in screen, "final shell line missing/corrupted in VGA text memory"
        print("PASS: primes tape's summary is genuinely visible in VGA text memory.", file=sys.stderr)
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()
        if MONITOR_SOCK.exists():
            MONITOR_SOCK.unlink()
        if VGA_DUMP.exists():
            VGA_DUMP.unlink()


if __name__ == "__main__":
    main()
