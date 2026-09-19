#!/usr/bin/env python3
"""Verify the kernel's selectable VGA text modes under QEMU.

The serial stream proves that the shell command completed, while QEMU's
monitor `screendump` header proves that the VGA timing actually changed:
80x25/80x50 use the normal 720x400 surface and the experimental 132-column
mode uses the wider 1056x400 surface.
"""

from argparse import ArgumentParser
from pathlib import Path
import os
import selectors
import socket
import subprocess
import sys
import tempfile
import time


def wait_for(output: bytearray, selector: selectors.BaseSelector, marker: bytes, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while marker not in output:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"did not observe {marker!r}; serial output:\n{output.decode(errors='replace')}")
        for key, _ in selector.select(remaining):
            chunk = os.read(key.fd, 4096)
            if not chunk:
                raise RuntimeError(f"QEMU serial stream ended early:\n{output.decode(errors='replace')}")
            output.extend(chunk)


def monitor_command(path: Path, command: str) -> None:
    deadline = time.monotonic() + 5
    while True:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as monitor:
                monitor.settimeout(2)
                monitor.connect(str(path))
                monitor.sendall(f"{command}\n".encode())
                monitor.recv(4096)
                return
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.02)


def read_ppm_dimensions(path: Path) -> tuple[int, int]:
    with path.open("rb") as dump:
        if dump.readline().strip() != b"P6":
            raise RuntimeError(f"{path} is not a QEMU PPM screen dump")
        dimensions = dump.readline().split()
        if len(dimensions) != 2:
            raise RuntimeError(f"invalid PPM dimensions in {path}")
        return int(dimensions[0]), int(dimensions[1])


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--qemu", default="qemu-system-i386")
    args = parser.parse_args()

    with tempfile.TemporaryDirectory(prefix="fos-vga-modes-") as temporary:
        temporary_path = Path(temporary)
        monitor = temporary_path / "monitor.sock"
        dump = temporary_path / "screen.ppm"
        process = subprocess.Popen(
            [
                args.qemu,
                "-drive",
                f"format=raw,file={args.image}",
                "-serial",
                "stdio",
                "-monitor",
                f"unix:{monitor},server=on,wait=off",
                "-display",
                "none",
                "-snapshot",
                "-no-reboot",
                "-no-shutdown",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        assert process.stdin is not None and process.stdout is not None
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        output = bytearray()
        try:
            wait_for(output, selector, b"Waiting for a remote terminal client to connect;", 10)
            monitor_command(monitor, "sendkey f12")
            wait_for(output, selector, b"fos> ", 10)
            expected = (
                (b"vga 80x25\r", b"VGA mode changed to 80x25", (720, 400)),
                (b"vga 80x50\r", b"VGA mode changed to 80x50", (720, 400)),
                (b"vga 132x25\r", b"VGA mode changed to 132x25", (1056, 400)),
            )
            for command, marker, dimensions in expected:
                process.stdin.write(command)
                process.stdin.flush()
                wait_for(output, selector, marker, 5)
                if dump.exists():
                    dump.unlink()
                monitor_command(monitor, f"screendump {dump}")
                deadline = time.monotonic() + 5
                while not dump.exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                if not dump.exists():
                    raise RuntimeError(f"QEMU did not create a screen dump for {command.strip()!r}")
                actual = read_ppm_dimensions(dump)
                if actual != dimensions:
                    raise RuntimeError(
                        f"{command.strip().decode()}: expected framebuffer {dimensions}, got {actual}"
                    )
        finally:
            process.terminate()
            process.wait(timeout=5)
            selector.close()
    print("PASS: 80x25, 80x50, and experimental 132x25 VGA modes")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, TimeoutError, ValueError, subprocess.SubprocessError) as error:
        print(f"VGA mode test failed: {error}", file=sys.stderr)
        raise SystemExit(1)
