#!/usr/bin/env python3
"""End-to-end interoperability test for the Rust network-chat tape.

Two QEMU guests share one multicast Ethernet segment. The first stays in the
ordinary shell `chat` command; the second loads and runs `net-chat.bin` under
its own `term net` client. The test proves the tape's kernel-managed NIC
callbacks exchange the existing shell chat frames in both directions, answer
the existing discovery protocol, accept remote keyboard input, and return to
a still-usable remote shell after Escape.

Not part of `make test`; this is the focused manual regression test required
for this tape and the extended tape network ABI. Use `--nic e1000` to run the
same two-guest test through the Intel 82540EM backend.
"""

import os
import re
import selectors
import socket as socketlib
import subprocess
import sys
import time
import argparse
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.56"
MCAST_PORT = 4256
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TAPE = ROOT / "build" / "tapes" / "net-chat.bin"
MONITOR_SOCKS = {
    "shell": Path("/tmp/fos-test-net-chat-shell-monitor.sock"),
    "tape": Path("/tmp/fos-test-net-chat-tape-monitor.sock"),
}
CHAT_CLIENT_MAC = bytes.fromhex("02deadbeef21")
TAPE_CLIENT_MAC = bytes.fromhex("02deadbeef22")
TAPE_SENDER_MAC = bytes.fromhex("02deadbeef23")
GUEST_MACS = {
    "shell": "52:54:00:12:34:51",
    "tape": "52:54:00:12:34:52",
}
CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22


def term_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main(nic_model: str) -> None:
    if not TAPE.exists():
        raise SystemExit(f"{TAPE} does not exist; run `make tape-net-chat` first.")

    # QEMU calls its NE2000-compatible PCI model `ne2k_pci`, not `ne2000`;
    # every other `--nic` choice already matches QEMU's own `-device` name.
    qemu_device = "ne2k_pci" if nic_model == "ne2000" else nic_model

    selector = selectors.DefaultSelector()
    guests: dict[str, subprocess.Popen[bytes]] = {}
    output: dict[str, bytearray] = {}

    def start_guest(name: str, with_monitor: bool = False) -> None:
        monitor_sock = MONITOR_SOCKS[name]
        if with_monitor and monitor_sock.exists():
            monitor_sock.unlink()
        monitor = (
            ["-monitor", f"unix:{monitor_sock},server,nowait"]
            if with_monitor
            else ["-monitor", "none"]
        )
        guest = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive",
                f"format=raw,file={IMAGE}",
                "-snapshot",
                "-serial",
                "stdio",
                *monitor,
                "-no-reboot",
                "-no-shutdown",
                "-netdev",
                f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
                "-device",
                f"{qemu_device},netdev=net0,mac={GUEST_MACS[name]}",
                "-display",
                "none",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        guests[name] = guest
        output[name] = bytearray()
        selector.register(guest.stdout, selectors.EVENT_READ, name)

    def monitor_send_text(guest_name: str, text: str) -> None:
        monitor_sock = MONITOR_SOCKS[guest_name]
        monitor = None
        for _ in range(50):
            try:
                monitor = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
                monitor.connect(str(monitor_sock))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if monitor is not None:
                    monitor.close()
                monitor = None
                time.sleep(0.1)
        if monitor is None:
            raise SystemExit(f"could not connect to the {guest_name} guest's QEMU monitor")
        monitor.settimeout(5)
        monitor.recv(65536)
        for byte in text.encode():
            key = "spc" if byte == b" "[0] else "ret" if byte == b"\n"[0] else chr(byte)
            monitor.sendall(f"sendkey {key}\n".encode())
            time.sleep(0.05)
        monitor.close()

    def read_until(name: str, marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in output[name][start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r} from {name}; output since checkpoint:\n"
                    f"{output[name][start:].decode('utf-8', 'replace')}"
                )
            for key, _ in selector.select(remaining):
                guest_name = key.data
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(
                        f"{guest_name} serial stream ended early:\n"
                        f"{output[guest_name].decode('utf-8', 'replace')}"
                    )
                output[guest_name].extend(chunk)
        return bytes(output[name][start:])

    def run_command(
        destination: bytes,
        source: bytes,
        command: bytes,
        guest_name: str,
        timeout: float = 10,
    ) -> bytes:
        start = len(output[guest_name])
        mcast.send(term_frame(destination, source, TERM_TAG_DATA, command + b"\n"))
        return read_until(guest_name, b"fos> ", timeout, start)

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

        # Bring up the shell-chat guest first so its broadcast HELLO cannot
        # accidentally connect both guests to one terminal client.
        start_guest("shell", with_monitor=True)
        read_until("shell", b"Waiting for a remote terminal client to connect", 30)
        mcast.send(term_frame(b"\xff" * 6, CHAT_CLIENT_MAC, TERM_TAG_HELLO))
        read_until("shell", b"Remote terminal connected", 10)
        read_until("shell", b"fos> ", 5)
        shell_nic = run_command(b"\xff" * 6, CHAT_CLIENT_MAC, b"nic", "shell")
        nic_name = b"Intel e1000" if nic_model == "e1000" else b"NE2000-compatible" if nic_model == "ne2000" else b"RTL8139"
        shell_match = re.search(
            rb"(" + re.escape(nic_name) + rb") found: MAC ([0-9a-fA-F:]{17})",
            shell_nic,
        )
        if not shell_match:
            raise SystemExit(f"could not find shell guest MAC:\n{shell_nic.decode('utf-8', 'replace')}")
        shell_mac = send_tape.parse_mac(shell_match.group(2).decode())

        shell_chat_start = len(output["shell"])
        mcast.send(term_frame(shell_mac, CHAT_CLIENT_MAC, TERM_TAG_DATA, b"chat\n"))
        read_until("shell", b"Broadcasting to everyone listening.", 10, shell_chat_start)

        # The tape guest now receives a distinct HELLO/session and can be
        # driven through read_key while the other guest remains in `chat`.
        start_guest("tape", with_monitor=True)
        read_until("tape", b"Waiting for a remote terminal client to connect", 30)
        mcast.send(term_frame(b"\xff" * 6, TAPE_CLIENT_MAC, TERM_TAG_HELLO))
        read_until("tape", b"Remote terminal connected", 10)
        read_until("tape", b"fos> ", 5)
        tape_nic = run_command(b"\xff" * 6, TAPE_CLIENT_MAC, b"nic", "tape")
        tape_match = re.search(
            rb"(" + re.escape(nic_name) + rb") found: MAC ([0-9a-fA-F:]{17})",
            tape_nic,
        )
        if not tape_match:
            raise SystemExit(f"could not find tape guest MAC:\n{tape_nic.decode('utf-8', 'replace')}")
        tape_mac = send_tape.parse_mac(tape_match.group(2).decode())

        tape_load_start = len(output["tape"])
        mcast.send(term_frame(tape_mac, TAPE_CLIENT_MAC, TERM_TAG_DATA, b"tape load net\n"))
        time.sleep(0.5)
        send_tape.send_tape(mcast, tape_mac, TAPE_SENDER_MAC, TAPE.read_bytes(), delay=0.003)
        read_until("tape", b"Tape loaded", 30, tape_load_start)
        read_until("tape", b"fos> ", 5, tape_load_start)

        tape_run_start = len(output["tape"])
        mcast.send(term_frame(tape_mac, TAPE_CLIENT_MAC, TERM_TAG_DATA, b"tape run\n"))
        read_until("tape", b"fos tape chat:", 10, tape_run_start)

        # These bytes become read_key events, then a CHAT_TAG_TEXT frame
        # from the tape guest. The ordinary shell `chat` must display it.
        shell_receive_start = len(output["shell"])
        mcast.send(term_frame(tape_mac, TAPE_CLIENT_MAC, TERM_TAG_DATA, b"from tape\n"))
        read_until("shell", b": from tape", 10, shell_receive_start)
        assert send_tape.format_mac(tape_mac).encode() in output["shell"][shell_receive_start:]
        print("PASS: Rust tape sent shell-compatible chat text to shell chat.")

        # A local keyboard drives the shell chat editor (that nested command
        # deliberately owns its own input loop), producing a frame the tape
        # receives/copies via its new callback and prints through putc.
        tape_receive_start = len(output["tape"])
        monitor_send_text("shell", "from shell\n")
        read_until("tape", b": from shell", 10, tape_receive_start)
        assert send_tape.format_mac(shell_mac).encode() in output["tape"][tape_receive_start:]
        print("PASS: Rust tape received shell-chat text through nic_receive.")

        # `read_key` checks the local PS/2 queue before remote term-net
        # data, so this monitor-injected line proves the tape accepts its
        # second documented input source even while a remote session drives
        # the surrounding shell.
        shell_local_receive_start = len(output["shell"])
        monitor_send_text("tape", "local tape\n")
        read_until("shell", b": local tape", 10, shell_local_receive_start)
        assert send_tape.format_mac(tape_mac).encode() in output["shell"][shell_local_receive_start:]
        print("PASS: Rust tape accepted local keyboard chat input.")

        # `/discover` is tape input too. The shell's existing chat command
        # replies to the exact probe, demonstrating shared control tags as
        # well as shared ordinary-text framing.
        discovery_start = len(output["tape"])
        mcast.send(term_frame(tape_mac, TAPE_CLIENT_MAC, TERM_TAG_DATA, b"/discover\n"))
        read_until("tape", b"Found peer", 10, discovery_start)
        read_until("tape", send_tape.format_mac(shell_mac).encode(), 5, discovery_start)
        print("PASS: Rust tape discovery interoperated with shell chat.")

        # Escape travels over term net, not by directly manipulating the
        # tape's raw NIC buffer. It must remain a read_key abort event.
        exit_start = len(output["tape"])
        mcast.send(term_frame(tape_mac, TAPE_CLIENT_MAC, TERM_TAG_DATA, b"\x1b"))
        read_until("tape", b"Left tape chat; back at the fos shell.", 10, exit_start)
        read_until("tape", b"Tape returned; back at the fos shell.", 10, exit_start)

        recovery_start = len(output["tape"])
        mcast.send(term_frame(tape_mac, TAPE_CLIENT_MAC, TERM_TAG_DATA, b"about\n"))
        read_until(
            "tape",
            b"fos is a small educational 32-bit x86 operating system.",
            10,
            recovery_start,
        )
        print("PASS: tape chat returned cleanly and the same remote shell remained usable.")
        mcast.send(term_frame(tape_mac, TAPE_CLIENT_MAC, TERM_TAG_BYE))
        mcast.send(term_frame(shell_mac, CHAT_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        for guest in guests.values():
            guest.terminate()
        for guest in guests.values():
            try:
                guest.wait(timeout=5)
            except subprocess.TimeoutExpired:
                guest.kill()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--nic", choices=("rtl8139", "e1000", "ne2000"), default="rtl8139")
    main(parser.parse_args().nic)
