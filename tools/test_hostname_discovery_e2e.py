#!/usr/bin/env python3
"""End-to-end test for hostname-based NIC discovery.

Two QEMU guests, each built from its own tiny image carrying a distinct
`hostname` file (`alpha`/`beta`), share one multicast Ethernet segment. Both
are driven straight to the idle local prompt (no `term net` client involved)
so the always-on `ambient_discovery_poll()` loop -- not the dedicated
`nic discover` command -- is what exchanges and remembers each other's
hostname. `nic peers` on each side must then show the other guest's
per-interface name (`alpha1`/`beta1`), proving:

  - `hostname` is loaded from disk1 at boot and reported once.
  - The discovery frame payload carries that hostname to a peer.
  - The idle prompt loop replies to and remembers peers even though neither
    guest ever ran the `nic discover` command itself.

Not part of `make test`; a focused manual regression test for this feature,
mirroring `tapes/test_net_chat_e2e.py`'s two-guest structure.
"""

import argparse
import os
import selectors
import socket as socketlib
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
MCAST_GROUP = "230.42.42.57"
MCAST_PORT = 4257
GUEST_MACS = {
    "alpha": "52:54:00:12:35:01",
    "beta": "52:54:00:12:35:02",
}
HOSTNAME_FILES = {
    "alpha": ROOT / "tools" / "hostname-test-alpha.txt",
    "beta": ROOT / "tools" / "hostname-test-beta.txt",
}


def build_guest_image(name: str, image_path: Path) -> None:
    subprocess.run(
        [
            sys.executable,
            str(BUILD_IMAGE),
            "--stage1", str(STAGE1),
            "--stage2", str(STAGE2),
            "--kernel", str(KERNEL),
            "--output", str(image_path),
            "--file", "hostname", str(HOSTNAME_FILES[name]),
        ],
        check=True,
    )


def main(nic_model: str) -> None:
    for required in (STAGE1, STAGE2, KERNEL):
        if not required.exists():
            raise SystemExit(f"{required} does not exist; run `make image` first.")

    # QEMU calls its NE2000-compatible PCI model `ne2k_pci`, not `ne2000`.
    qemu_device = "ne2k_pci" if nic_model == "ne2000" else nic_model

    selector = selectors.DefaultSelector()
    guests: dict[str, subprocess.Popen[bytes]] = {}
    output: dict[str, bytearray] = {}
    monitor_socks: dict[str, Path] = {}

    def start_guest(name: str, image_path: Path, monitor_dir: Path) -> None:
        monitor_sock = monitor_dir / f"{name}-monitor.sock"
        monitor_socks[name] = monitor_sock
        guest = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive", f"format=raw,file={image_path}",
                "-snapshot",
                "-serial", "stdio",
                "-monitor", f"unix:{monitor_sock},server,nowait",
                "-no-reboot",
                "-no-shutdown",
                "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
                "-device", f"{qemu_device},netdev=net0,mac={GUEST_MACS[name]}",
                "-display", "none",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        guests[name] = guest
        output[name] = bytearray()
        selector.register(guest.stdout, selectors.EVENT_READ, name)

    def monitor_command(name: str, command: str) -> None:
        monitor_sock = monitor_socks[name]
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
            raise SystemExit(f"could not connect to the {name} guest's QEMU monitor")
        monitor.settimeout(5)
        monitor.recv(65536)
        monitor.sendall((command + "\n").encode())
        monitor.close()

    def monitor_send_text(name: str, text: str) -> None:
        monitor_sock = monitor_socks[name]
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
            raise SystemExit(f"could not connect to the {name} guest's QEMU monitor")
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

    with tempfile.TemporaryDirectory(prefix="fos-hostname-test-") as tmp_dir_name:
        tmp_dir = Path(tmp_dir_name)
        try:
            for name in ("alpha", "beta"):
                image_path = tmp_dir / f"{name}.img"
                build_guest_image(name, image_path)
                start_guest(name, image_path, tmp_dir)

            for name in ("alpha", "beta"):
                read_until(name, f"hostname: configured as '{name}'".encode(), 20)
                print(f"PASS: {name} guest loaded its own hostname file at boot.")

            for name in ("alpha", "beta"):
                read_until(name, b"Waiting for a remote terminal client to connect", 20)
                monitor_command(name, "sendkey f12")
                read_until(name, b"fos> ", 10)

            # Give both idle loops several ambient-discovery burst rounds
            # (~3s apart) to exchange probes/replies purely through
            # ambient_discovery_poll(), with neither guest ever running the
            # dedicated `nic discover` command.
            time.sleep(10)

            for name, peer in (("alpha", "beta"), ("beta", "alpha")):
                start = len(output[name])
                monitor_send_text(name, "nic peers\n")
                result = read_until(name, b"fos> ", 10, start)
                expected = f"({peer}1)".encode()
                if expected not in result:
                    raise SystemExit(
                        f"{name} guest's peer table did not show '{peer}1':\n"
                        f"{result.decode('utf-8', 'replace')}"
                    )
                print(f"PASS: {name} guest's ambient discovery learned peer '{peer}1'.")

            print("PASS: hostname-based ambient NIC discovery works end to end.")
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
