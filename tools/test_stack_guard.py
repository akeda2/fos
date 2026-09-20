#!/usr/bin/env python3
"""Verify that an exhausted kernel stack reaches the emergency #DF task.

`make test-stack-guard` builds fos with the otherwise-disabled
`stack-guard-test` Cargo feature. The kernel deliberately pushes into the
absent page below its normal stack. A healthy image cannot push a #PF frame
there, so x86 switches through IDT vector 8's task gate to the dedicated TSS
and its emergency stack, which prints the marker asserted below.
"""

from argparse import ArgumentParser
from pathlib import Path
import os
import selectors
import subprocess
import time

MARKER = b"DOUBLE FAULT: switched to the emergency task stack."


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
                    f"did not reach the emergency double-fault task:\n"
                    f"{output.decode(errors='replace')}"
                )
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(
                        f"QEMU serial stream ended early:\n{output.decode(errors='replace')}"
                    )
                output.extend(chunk)
        print("PASS: stack guard transferred to the emergency double-fault task.")
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
