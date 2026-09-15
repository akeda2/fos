#!/usr/bin/env python3
"""Boot the image and validate the serial-visible educational milestones."""

from argparse import ArgumentParser
from pathlib import Path
import os
import selectors
import subprocess
import sys
import time

BOOT_MARKER = b"fos> "
EXPECTED = (
    b"FOS stage 2: real mode active.",
    b"Memory map collected.",
    b"A20 enabled.",
    b"Kernel loaded. BIOS services end now.",
    b"Kernel: protected-mode Rust entry reached.",
    b"PS/2 keyboard initialized.",
    b"System: help clear about bootinfo memmap memory cpuinfo hexdump halt",
    b"fos is a small educational 32-bit x86 operating system.",
    b"This command does not accept arguments.",
    b"Unknown command. Type 'help' for commands.",
    b"Boot drive: 0x80;",
    b"Firmware memory map (",
    b"Halting CPU.",
    b"FOSFS partition: LBA 4096 + 4096 sectors",
    b"FOSFS1",
    b"README.TXT  63 bytes, 1 sector(s)",
    b"Welcome to FOSFS.",
    b"File written.",
    b"File removed.",
    b"FOSFS formatted.",
    b"FOSFS is empty.",
)


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


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--qemu", default="qemu-system-i386")
    args = parser.parse_args()

    process = subprocess.Popen(
        [args.qemu, "-drive", f"format=raw,file={args.image}", "-serial", "stdio",
         "-monitor", "none", "-display", "none", "-no-reboot", "-no-shutdown"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    assert process.stdin is not None and process.stdout is not None
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    output = bytearray()
    try:
        wait_for(output, selector, BOOT_MARKER, 10)
        process.stdin.write(
            b"helx\x08p\rabout\rhelp extra\runknown\rclear\rdiskinfo\r"
            b"readsector 4096\rcpuinfo\rmemory\rls\rcat README.TXT\r"
            b"write NOTE.TXT HELLO\rcat NOTE.TXT\rrm NOTE.TXT\rformat\rls\r"
            b"bootinfo\rmemmap\rhalt\r"
        )
        process.stdin.flush()
        for marker in EXPECTED:
            wait_for(output, selector, marker, 5)
    finally:
        process.terminate()
        process.wait(timeout=5)
        selector.close()
    print(output.decode(errors="replace"), end="")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, TimeoutError, subprocess.SubprocessError) as error:
        print(f"QEMU smoke test failed: {error}", file=sys.stderr)
        raise SystemExit(1)
