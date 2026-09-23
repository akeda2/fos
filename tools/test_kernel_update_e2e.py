#!/usr/bin/env python3
"""End-to-end test for KUP (the network kernel-update service; see
`kernel/src/debug.rs`, `kernel/src/shell.rs`'s `serve_kup_put_*`
functions, and `tools/send_kernel_update.py`).

Drives one QEMU guest built with a `debug` file enabling `kernel-update`,
joins its `socket,mcast=` netdev directly from the host as a third virtual
peer (mirroring `tools/test_term_net_e2e.py`'s own `McastSocket` approach,
since a QEMU multicast netdev is a plain UDP socket -- no CAP_NET_RAW, no
tap device, and no real interoperability with `tools/send_kernel_update.py`
itself, which needs a real `AF_PACKET` interface), and exercises:

  - A good push of the currently-built `build/kernel.bin` into the
    inactive slot: confirms the ACK, confirms the machine actually resets
    (a real QEMU reboot -- this script does NOT pass `-no-reboot`), and
    confirms (via the hidden `debug slots` command) that the new slot
    became `active` once the second boot reaches its own milestone.
  - A bad-checksum push: confirms a `KUP_TAG_PUT_ERROR`
    (`CHECKSUM_MISMATCH`) reply, and confirms the machine does NOT reset
    and its slot metadata is unchanged.
  - `kernel-update` left disabled (no `debug` file at all, a second QEMU
    guest): confirms KUP frames are simply never answered.

Not part of `make test` (like every other real-Ethernet-round-trip
end-to-end script here) -- a manual regression test for this feature.
"""

import os
import selectors
import socket as socketlib
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STAGE1 = ROOT / "build" / "stage1.bin"
STAGE2 = ROOT / "build" / "stage2.bin"
KERNEL = ROOT / "build" / "kernel.bin"
BUILD_IMAGE = ROOT / "tools" / "build_image.py"

MCAST_GROUP = "230.42.42.75"
MCAST_PORT = 4275
HOST_MAC = bytes.fromhex("02deadbeef0f")
GUEST_MAC = bytes.fromhex("525400123701")
BROADCAST_MAC = b"\xff" * 6
CHAT_ETHERTYPE = (0x88, 0xB6)

KUP_TAG_PUT_REQUEST = 0x3A
KUP_TAG_PUT_DATA = 0x3B
KUP_TAG_PUT_STOP = 0x3C
KUP_TAG_PUT_ACK = 0x3D
KUP_TAG_PUT_ERROR = 0x3E
KUP_ERROR_CHECKSUM_MISMATCH = 2

CHUNK_LEN = 1024


def internet_checksum(data: bytes) -> int:
    total = 0
    it = iter(data)
    for high in it:
        low = next(it, 0)
        total += (high << 8) | low
    while total > 0xFFFF:
        total = (total & 0xFFFF) + (total >> 16)
    return (~total) & 0xFFFF


class McastSocket:
    def __init__(self, group: str, port: int):
        self.sock = socketlib.socket(socketlib.AF_INET, socketlib.SOCK_DGRAM, socketlib.IPPROTO_UDP)
        self.sock.setsockopt(socketlib.SOL_SOCKET, socketlib.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", port))
        mreq = struct.pack("4sl", socketlib.inet_aton(group), socketlib.INADDR_ANY)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_ADD_MEMBERSHIP, mreq)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_MULTICAST_TTL, 1)
        self.group = group
        self.port = port

    def send(self, frame: bytes) -> None:
        self.sock.sendto(frame, (self.group, self.port))

    def receive(self, timeout: float) -> bytes | None:
        self.sock.settimeout(timeout)
        try:
            return self.sock.recv(2048)
        except TimeoutError:
            return None


def pad(frame: bytes) -> bytes:
    frame = bytearray(frame)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def kup_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    return pad(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)


def send_kup_update(mcast: McastSocket, data: bytes, checksum_override: int | None = None) -> None:
    checksum = checksum_override if checksum_override is not None else internet_checksum(data)
    request_payload = len(data).to_bytes(4, "little") + checksum.to_bytes(2, "little")
    mcast.send(kup_frame(BROADCAST_MAC, HOST_MAC, KUP_TAG_PUT_REQUEST, request_payload))
    for seq, offset in enumerate(range(0, len(data), CHUNK_LEN)):
        chunk = data[offset : offset + CHUNK_LEN]
        payload = seq.to_bytes(4, "little") + chunk
        mcast.send(kup_frame(BROADCAST_MAC, HOST_MAC, KUP_TAG_PUT_DATA, payload))
        time.sleep(0.003)
    mcast.send(kup_frame(BROADCAST_MAC, HOST_MAC, KUP_TAG_PUT_STOP))


def wait_for_kup_reply(mcast: McastSocket, timeout: float) -> tuple[int, int] | None:
    """Returns (tag, code) once a KUP_TAG_PUT_ACK/ERROR datagram addressed
    to HOST_MAC arrives, or None on timeout. `code` is 0 for an ACK."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        remaining = deadline - time.monotonic()
        packet = mcast.receive(max(remaining, 0.01))
        if packet is None:
            continue
        if len(packet) < 15:
            continue
        if packet[12:14] != bytes(CHAT_ETHERTYPE):
            continue
        if packet[0:6] != HOST_MAC:
            continue
        tag = packet[14]
        if tag == KUP_TAG_PUT_ACK:
            return (tag, 0)
        if tag == KUP_TAG_PUT_ERROR:
            code = packet[15] if len(packet) > 15 else 0
            return (tag, code)
    return None


def build_guest_image(image_path: Path, debug_file: Path | None) -> None:
    args = [
        sys.executable, str(BUILD_IMAGE),
        "--stage1", str(STAGE1),
        "--stage2", str(STAGE2),
        "--kernel", str(KERNEL),
        "--output", str(image_path),
    ]
    if debug_file is not None:
        args += ["--file", "debug", str(debug_file)]
    subprocess.run(args, check=True)


class Guest:
    def __init__(self, image_path: Path, monitor_sock: Path):
        self.monitor_sock = monitor_sock
        self.output = bytearray()
        self.selector = selectors.DefaultSelector()
        self.process = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive", f"format=raw,file={image_path}",
                "-serial", "stdio",
                "-monitor", f"unix:{monitor_sock},server,nowait",
                "-no-shutdown",
                "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
                "-device", f"rtl8139,netdev=net0,mac={GUEST_MAC.hex(':')}",
                "-display", "none",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.selector.register(self.process.stdout, selectors.EVENT_READ)

    def read_until(self, marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in self.output[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r}; output since checkpoint:\n"
                    f"{self.output[start:].decode('utf-8', 'replace')}"
                )
            for key, _ in self.selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(f"guest serial stream ended early:\n{self.output.decode('utf-8', 'replace')}")
                self.output.extend(chunk)
        return bytes(self.output[start:])

    def monitor_connect(self) -> socketlib.socket:
        monitor = None
        for _ in range(50):
            try:
                monitor = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
                monitor.connect(str(self.monitor_sock))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if monitor is not None:
                    monitor.close()
                monitor = None
                time.sleep(0.1)
        if monitor is None:
            raise SystemExit("could not connect to the guest's QEMU monitor")
        monitor.settimeout(5)
        monitor.recv(65536)
        return monitor

    def escape_to_idle_prompt(self) -> None:
        """Sends F12 over the monitor to leave the automatic boot-time
        `term net` wait and reach the ordinary idle prompt loop -- KUP is
        only ever served from there (see AGENTS.md's "Serving is scoped
        to the idle prompt loop only")."""
        start = len(self.output)
        self.read_until(b"Waiting for a remote terminal client to connect", 20, start)
        monitor = self.monitor_connect()
        monitor.sendall(b"sendkey f12\n")
        monitor.close()
        self.read_until(b"fos> ", 10, start)

    def run_command(self, command: str, timeout: float = 10) -> bytes:
        start = len(self.output)
        self.process.stdin.write((command + "\r\n").encode())
        self.process.stdin.flush()
        return self.read_until(b"fos> ", timeout, start)

    def terminate(self) -> None:
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def test_disabled_by_default(tmp_dir: Path) -> None:
    """No `debug` file at all: KUP frames must be silently ignored."""
    image_path = tmp_dir / "no-debug.img"
    build_guest_image(image_path, None)
    monitor_sock = tmp_dir / "no-debug-monitor.sock"
    guest = Guest(image_path, monitor_sock)
    try:
        guest.escape_to_idle_prompt()
        result = guest.run_command("debug status")
        if b"kernel-update=false" not in result:
            raise SystemExit(f"expected kernel-update=false by default:\n{result.decode('utf-8', 'replace')}")
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        send_kup_update(mcast, b"\x90" * 4096)
        reply = wait_for_kup_reply(mcast, 5)
        if reply is not None:
            raise SystemExit(f"expected no reply with kernel-update disabled, got {reply}")
        # Confirm the guest is still alive and unaffected.
        result = guest.run_command("debug slots")
        if b"active=0" not in result:
            raise SystemExit(f"guest's active slot changed despite kernel-update being disabled:\n{result!r}")
        print("PASS: KUP frames are silently ignored when 'kernel-update' is not in the debug file.")
    finally:
        guest.terminate()


def test_bad_checksum(tmp_dir: Path, debug_file: Path) -> None:
    image_path = tmp_dir / "bad-checksum.img"
    build_guest_image(image_path, debug_file)
    monitor_sock = tmp_dir / "bad-checksum-monitor.sock"
    guest = Guest(image_path, monitor_sock)
    try:
        guest.escape_to_idle_prompt()
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        data = b"\x90" * 4096
        send_kup_update(mcast, data, checksum_override=(internet_checksum(data) ^ 0xFFFF))
        reply = wait_for_kup_reply(mcast, 10)
        if reply != (KUP_TAG_PUT_ERROR, KUP_ERROR_CHECKSUM_MISMATCH):
            raise SystemExit(f"expected a CHECKSUM_MISMATCH error, got {reply}")
        result = guest.read_until(b"Checksum mismatch on incoming network kernel update", 5)
        result = guest.run_command("debug slots")
        if b"active=0" not in result or b"pending=none" not in result:
            raise SystemExit(f"a bad-checksum push modified slot metadata:\n{result.decode('utf-8', 'replace')}")
        print("PASS: a bad-checksum push is rejected and leaves slot metadata untouched.")
    finally:
        guest.terminate()


def test_good_update(tmp_dir: Path, debug_file: Path) -> None:
    image_path = tmp_dir / "good-update.img"
    build_guest_image(image_path, debug_file)
    monitor_sock = tmp_dir / "good-update-monitor.sock"
    guest = Guest(image_path, monitor_sock)
    try:
        guest.escape_to_idle_prompt()
        result = guest.run_command("debug slots")
        if b"active=0" not in result:
            raise SystemExit(f"expected a fresh image to boot active slot 0:\n{result.decode('utf-8', 'replace')}")

        data = KERNEL.read_bytes()
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        checkpoint = len(guest.output)
        send_kup_update(mcast, data)
        reply = wait_for_kup_reply(mcast, 30)
        if reply != (KUP_TAG_PUT_ACK, 0):
            raise SystemExit(f"expected a PUT_ACK reply, got {reply}")
        guest.read_until(b"Installed a", 5, checkpoint)
        guest.read_until(b"into slot 1; rebooting to try it", 2, checkpoint)
        print("PASS: kernel image accepted, installed into slot 1, ACK received.")

        # The machine performs a real reset (this test does not pass
        # -no-reboot); wait for the second boot cycle to reach the
        # automatic term_net wait again, then the idle prompt.
        guest.read_until(b"Kernel: protected-mode Rust entry reached.", 15, checkpoint)
        print("PASS: the machine actually reset and reached a second boot.")
        guest.escape_to_idle_prompt()

        result = guest.run_command("debug slots")
        if b"booted slot 1" not in result:
            raise SystemExit(f"expected the second boot to report booted slot 1:\n{result.decode('utf-8', 'replace')}")
        if b"active=1" not in result or b"pending=none" not in result:
            raise SystemExit(
                f"expected slot 1 to be promoted to active once the milestone was reached:\n"
                f"{result.decode('utf-8', 'replace')}"
            )
        print("PASS: slot 1 was promoted to active after reaching the shell prompt (confirm_boot).")
    finally:
        guest.terminate()


def main() -> None:
    for required in (STAGE1, STAGE2, KERNEL):
        if not required.exists():
            raise SystemExit(f"{required} does not exist; run `make image` first.")

    with tempfile.TemporaryDirectory(prefix="fos-kup-test-") as tmp_dir_name:
        tmp_dir = Path(tmp_dir_name)
        debug_file = tmp_dir / "debug"
        debug_file.write_text("reboot-on-crash\nkernel-update\n")

        test_disabled_by_default(tmp_dir)
        test_bad_checksum(tmp_dir, debug_file)
        test_good_update(tmp_dir, debug_file)

        print("PASS: KUP (network kernel-update service) works end to end.")


if __name__ == "__main__":
    main()
