#!/usr/bin/env python3
"""Verify that CR0.WP and the kernel-text PTEs reject a supervisor write."""

from argparse import ArgumentParser
from pathlib import Path
import os
import selectors
import subprocess
import time

MARKER = b"kernel text (read-only)): protection violation on supervisor write"


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--image", type=Path, required=True)
    args = parser.parse_args()

    qemu = subprocess.Popen(
        [
            "qemu-system-i386",
            "-drive",
            f"format=raw,file={args.image}",
            "-serial",
            "stdio",
            "-monitor",
            "none",
            "-display",
            "none",
            "-nic",
            "none",
            "-no-reboot",
            "-no-shutdown",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    assert qemu.stdout is not None
    output = bytearray()
    selector = selectors.DefaultSelector()
    selector.register(qemu.stdout, selectors.EVENT_READ)

    try:
        deadline = time.monotonic() + 20
        while MARKER not in output:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"did not observe a kernel-text write-protection fault:\n"
                    f"{output.decode(errors='replace')}"
                )
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(
                        f"QEMU serial stream ended early:\n{output.decode(errors='replace')}"
                    )
                output.extend(chunk)
        print("PASS: kernel text rejected a supervisor write through CR0.WP.")
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
