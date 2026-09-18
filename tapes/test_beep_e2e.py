#!/usr/bin/env python3
"""One-off end-to-end test for the Rust PC-speaker tape.

This follows the other `tapes/test_*_e2e.py` scripts: QEMU boots with an
emulated RTL8139, one UDP multicast socket carries the emulated Ethernet
frames, and the tape is loaded from a second independent sender. The shell
is driven through one continuous `term net` session.

The test cannot make a host speaker audible or capture QEMU's audio device,
so the tape emits one `Beep.` line after each accepted `b` event. It sends
one `b` through `term net`, one local `b` through QEMU's keyboard monitor,
then stops the tape with a local F12. The assertions prove both input paths
reached the tape, the PC-speaker programming path returned, and the same
remote shell session remained usable afterward.

Not part of `make test`; this is a manual tape verification script like the
other `tapes/test_*_e2e.py` harnesses.
"""

import os
import re
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

MCAST_GROUP = "230.42.42.54"
MCAST_PORT = 4254
IMAGE = Path(__file__).resolve().parent.parent / "build" / "fos.img"
TAPE = Path(__file__).resolve().parent.parent / "build" / "tapes" / "beep.bin"
MONITOR_SOCK = Path("/tmp/fos-test-beep-monitor.sock")
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef14")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef15")
CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main() -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-beep` first.")
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
                raise SystemExit(
                    f"timed out waiting for {marker!r}; output so far:\n"
                    f"{output.decode('utf-8', 'replace')}"
                )
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(
                        f"QEMU serial stream ended early:\n"
                        f"{output.decode('utf-8', 'replace')}"
                    )
                output.extend(chunk)
        return bytes(output)

    def monitor_command(command: str) -> None:
        monitor = None
        for _ in range(50):
            try:
                monitor = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
                monitor.connect(str(MONITOR_SOCK))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if monitor is not None:
                    monitor.close()
                monitor = None
                time.sleep(0.1)
        if monitor is None:
            raise SystemExit("could not connect to the QEMU monitor socket")
        monitor.settimeout(5)
        monitor.recv(65536)
        monitor.sendall((command + "\n").encode())
        time.sleep(0.5)
        monitor.close()

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
            raise SystemExit(
                f"couldn't find fos's MAC address in:\n"
                f"{output.decode('utf-8', 'replace')}"
            )
        fos_mac = send_tape.parse_mac(match.group(1).decode())
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        send_tape.send_tape(mcast, fos_mac, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until(b"Tape loaded", 30)
        read_until(b"fos> ", 5)
        del output[:]

        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until(b"PC speaker test:", 10)
        del output[:]

        # This exercises the remote-input half of read_key.
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"b"))
        read_until(b"Beep.", 5)

        # The QEMU monitor's keyboard injection exercises the local PS/2
        # input half of the same callback.
        monitor_command("sendkey b")
        read_until(b"Beep.", 5)

        # F12 is intentionally local: the remote session is paused inside
        # tape_run while the tape polls read_key.
        monitor_command("sendkey f12")
        read_until(b"PC speaker test stopped.", 10)
        read_until(b"Tape returned; back at the fos shell.", 10)
        text = output.decode("utf-8", "replace")
        assert text.count("Beep.") == 2, f"expected two beeps, got:\n{text}"

        del output[:]
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(b"fos is a small educational 32-bit x86 operating system.", 10)
        print("PASS: beep tape accepted remote and local b presses and returned cleanly.")
        mcast.send(term_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
