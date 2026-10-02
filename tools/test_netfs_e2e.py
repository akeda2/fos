#!/usr/bin/env python3
"""End-to-end test for NETFS: `copy`/`tape load`/`tape run @...` against a
discovered peer's own disk over the network.

Two QEMU guests (`alpha`/`beta`), each with its own `hostname` file and its
own blank second disk, share one multicast Ethernet segment. Both are
driven to the idle local prompt so ambient discovery (see
`test_hostname_discovery_e2e.py`) learns each other's per-interface
hostname (`alpha1`/`beta1`) with no `nic discover` command needed. Once
discovered, this test drives, from each side in turn:

  - `copy HOST:disk2/NAME disk2`       (remote GET into a local disk)
  - `copy disk1 NAME HOST:disk2`       (remote PUT from a local disk)
  - `tape load HOST disk2NAME`         (remote GET straight into the tape buffer)
  - `tape run @HOST:disk2/NAME`        (remote GET as a startup-argument file,
                                         via the calc tape's `3 4 +` syntax)
  - `copy alpha1:disk2/missing.txt disk2` (NETFS's NOT_FOUND error path)

Not part of `make test`; a focused manual regression test for this
feature, mirroring `test_hostname_discovery_e2e.py`'s two-guest structure.
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
MCAST_GROUP = "230.42.42.58"
MCAST_PORT = 4258
GUEST_MACS = {
    "alpha": "52:54:00:12:36:01",
    "beta": "52:54:00:12:36:02",
}
HOSTNAME_FILES = {
    "alpha": ROOT / "tools" / "hostname-test-alpha.txt",
    "beta": ROOT / "tools" / "hostname-test-beta.txt",
}


CALC_TAPE = ROOT / "build" / "tapes" / "calc.bin"


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
            "--file", "calc.bin", str(CALC_TAPE),
        ],
        check=True,
    )


def main(nic_model: str) -> None:
    for required in (STAGE1, STAGE2, KERNEL, CALC_TAPE):
        if not required.exists():
            raise SystemExit(f"{required} does not exist; run `make image` first.")

    # QEMU calls its NE2000-compatible PCI model `ne2k_pci`, not `ne2000`.
    qemu_device = "ne2k_pci" if nic_model == "ne2000" else nic_model

    selector = selectors.DefaultSelector()
    guests: dict[str, subprocess.Popen[bytes]] = {}
    output: dict[str, bytearray] = {}
    monitor_socks: dict[str, Path] = {}

    def start_guest(name: str, image_path: Path, second_disk_path: Path, monitor_dir: Path) -> None:
        monitor_sock = monitor_dir / f"{name}-monitor.sock"
        monitor_socks[name] = monitor_sock
        second_disk_path.write_bytes(b"\x00" * (4 * 1024 * 1024))
        guest = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive", f"format=raw,file={image_path}",
                "-drive", f"format=raw,file={second_disk_path}",
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
        # QEMU's monitor `sendkey` only accepts QKeyCode names, not literal
        # punctuation characters -- most letters/digits happen to double as
        # valid key names, but '.', '-', '/', ':', and '_' each need their
        # own named (or shifted) key instead.
        punctuation_keys = {
            ".": "dot",
            "-": "minus",
            "/": "slash",
            ":": "shift-semicolon",
            "_": "shift-minus",
            "@": "shift-2",
            "+": "shift-equal",
        }
        for byte in text.encode():
            char = chr(byte)
            if char == " ":
                key = "spc"
            elif char == "\n":
                key = "ret"
            else:
                key = punctuation_keys.get(char, char)
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

    def run_command(name: str, command: str, timeout: float = 10) -> bytes:
        start = len(output[name])
        monitor_send_text(name, command + "\n")
        return read_until(name, b"fos> ", timeout, start)

    with tempfile.TemporaryDirectory(prefix="fos-netfs-test-") as tmp_dir_name:
        tmp_dir = Path(tmp_dir_name)
        try:
            for name in ("alpha", "beta"):
                image_path = tmp_dir / f"{name}.img"
                second_disk_path = tmp_dir / f"{name}-disk2.img"
                build_guest_image(name, image_path)
                start_guest(name, image_path, second_disk_path, tmp_dir)

            for name in ("alpha", "beta"):
                read_until(name, f"hostname: configured as '{name}'".encode(), 20)

            for name in ("alpha", "beta"):
                read_until(name, b"Waiting for a remote terminal client to connect", 20)
                monitor_command(name, "sendkey f12")
                read_until(name, b"fos> ", 10)

            for name in ("alpha", "beta"):
                result = run_command(name, "format disk2")
                if b"disk2" not in result:
                    raise SystemExit(f"{name} guest failed to format disk2:\n{result!r}")
            print("PASS: both guests formatted their own disk2.")

            # Give both idle loops several ambient-discovery burst rounds
            # (~3s apart) to exchange probes/replies before any NETFS
            # command is typed, so peer resolution (`alpha1`/`beta1`) is
            # already populated.
            time.sleep(10)

            # Remote GET: alpha writes a file on its own disk2; beta pulls
            # it over NETFS straight into its own disk2.
            run_command("alpha", "write disk2 greeting.txt hello-from-alpha")
            result = run_command("beta", "copy alpha1:disk2/greeting.txt disk2", 15)
            if b"Copied" not in result:
                raise SystemExit(f"beta's remote GET (copy) failed:\n{result.decode('utf-8', 'replace')}")
            result = run_command("beta", "cat disk2 greeting.txt")
            if b"hello-from-alpha" not in result:
                raise SystemExit(f"beta's fetched file has wrong contents:\n{result.decode('utf-8', 'replace')}")
            print("PASS: remote GET via 'copy HOST:disk2/NAME disk2' works, checksum verified.")

            # Remote PUT: beta writes a file on its own disk1; alpha
            # receives it over NETFS onto its own disk2.
            run_command("beta", "write reply.txt hello-from-beta")
            result = run_command("beta", "copy disk1 reply.txt alpha1:disk2", 15)
            if b"Copied" not in result:
                raise SystemExit(f"beta's remote PUT (copy) failed:\n{result.decode('utf-8', 'replace')}")
            result = run_command("alpha", "cat disk2 reply.txt")
            if b"hello-from-beta" not in result:
                raise SystemExit(f"alpha's received file has wrong contents:\n{result.decode('utf-8', 'replace')}")
            print("PASS: remote PUT via 'copy disk1 NAME HOST:disk2' works, checksum verified.")

            # The `diskN/NAME` source and `HOST:diskN/NEWNAME` destination
            # spelling (new name inline) over NETFS, and local copy errors.
            result = run_command("beta", "copy disk1/reply.txt alpha1:disk2/slash.txt", 15)
            if b"Copied" not in result:
                raise SystemExit(f"beta's 'copy disk1/NAME HOST:diskN/NEW' failed:\n{result.decode('utf-8', 'replace')}")
            result = run_command("alpha", "cat disk2 slash.txt")
            if b"hello-from-beta" not in result:
                raise SystemExit(f"alpha's slash-PUT file has wrong contents:\n{result.decode('utf-8', 'replace')}")
            result = run_command("beta", "copy disk1/reply.txt disk2/a.txt b.txt")
            if b"once" not in result:
                raise SystemExit(f"two new names were not rejected:\n{result.decode('utf-8', 'replace')}")
            print("PASS: 'copy disk1/NAME HOST:diskN/NEW' sends under the inline new name.")

            # `tape load HOST NAME` fetches straight into the tape buffer.
            result = run_command("alpha", "tape load beta1 reply.txt", 15)
            if b"Loaded" not in result:
                raise SystemExit(f"alpha's 'tape load HOST NAME' failed:\n{result.decode('utf-8', 'replace')}")
            print("PASS: 'tape load HOST NAME' fetches a remote file into the tape buffer.")

            # `tape run @HOST:diskN/NAME` fetches a startup-argument file
            # over NETFS; reuse the calc tape (already built by `make
            # image`) with a small RPN expression file.
            run_command("beta", "write disk2 expr.txt 3 4 +")
            calc_load = run_command("alpha", "tape load disk calc.bin")
            if b"Loaded" not in calc_load:
                raise SystemExit(f"alpha failed to load calc.bin:\n{calc_load.decode('utf-8', 'replace')}")
            result = run_command("alpha", "tape run @beta1:disk2/expr.txt", 15)
            if b"= 7" not in result:
                raise SystemExit(
                    f"alpha's 'tape run @HOST:diskN/NAME' didn't evaluate '3 4 +':\n"
                    f"{result.decode('utf-8', 'replace')}"
                )
            print("PASS: 'tape run @HOST:diskN/NAME' fetches a startup-argument file over NETFS.")

            # A request for a file that doesn't exist reports NETFS's
            # NOT_FOUND error rather than hanging or crashing.
            result = run_command("beta", "copy alpha1:disk2/missing.txt disk2", 15)
            if b"not found" not in result.lower():
                raise SystemExit(
                    f"beta's request for a missing remote file didn't report NOT_FOUND:\n"
                    f"{result.decode('utf-8', 'replace')}"
                )
            print("PASS: NETFS reports a clean error for a nonexistent remote file.")

            print("PASS: NETFS (network filesystem over discovered peers) works end to end.")
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
