#!/usr/bin/env python3
"""Boot the image and validate the serial-visible educational milestones.

Drives a headless QEMU instance over its `-serial stdio` pipe, typing a
scripted sequence of shell commands and asserting that every message in
`EXPECTED` appears, in order, within a bounded timeout. This is the project's
sole automated test path (`cargo test` cannot run this crate's `no_std`
custom-target binary); see `make test` and the top-level AGENTS.md.
"""

from argparse import ArgumentParser
from pathlib import Path
import os
import selectors
import subprocess
import sys
import time

BOOT_MARKER = b"fos> "
EXPECTED = (
    # Stage one's messages go to the VGA text console only (see boot/stage1.S
    # for why it doesn't also initialize the serial UART); they are not
    # visible on this serial-only capture, so the first markers here are
    # stage two's, which mirrors everything to both VGA and COM1.
    b"FOS stage 2: real mode active.",
    b"Querying BIOS INT 15h EAX=E820h for the physical memory map...",
    b"Memory map collected.",
    b"Enabling the A20 address line (needed to address memory above 1 MiB)...",
    b"A20 enabled.",
    b"Loading the Rust kernel from disk to physical 0x10000...",
    b"Kernel loaded. BIOS services end now.",
    b"Masking the legacy 8259 PIC and installing a flat 32-bit GDT...",
    b"GDT loaded, CR0.PE set. Far-jumping into 32-bit protected mode",
    b"Kernel: protected-mode Rust entry reached.",
    b"Physical memory: ",
    b"Interrupts enabled: IDT loaded, PIC remapped to 0x20/0x28,",
    b"PS/2 keyboard initialized.",
    b"System: help clear about bootinfo memmap memory cpuinfo hexdump halt reset",
    b"Interrupts: ticks uptime irqstat pmem palloc pfree",
    b"Ethernet: nic (status), nic send (broadcast test frame), nic recv (poll for one frame),",
    b"fos is a small educational 32-bit x86 operating system.",
    b"This command does not accept arguments.",
    b"Unknown command. Type 'help' for commands.",
    b"Boot drive: 0x80;",
    b"Firmware memory map (",
    b"usable RAM)",
    b"reserved (not usable))",
    b"Halting CPU.",
    b"FOSFS partition: LBA 4096 + 4096 sectors",
    b"FOSFS1",
    b"README.TXT  63 bytes, 1 sector(s)",
    b"Welcome to FOSFS.",
    b"File written.",
    b"File removed.",
    b"FOSFS formatted.",
    b"FOSFS is empty.",
    b"Timer ticks: ",
    b"Uptime: ",
    b"IRQ0 (timer): ",
    b"Physical frames: ",
    b"Allocated frame at 0x",
    b"released (if it was allocated and in range).",
    b"Invalid name: use up to 16 uppercase letters, digits, '.', '_', or '-'.",
    b"No RTL8139 Ethernet controller found on the PCI bus.",
)


def wait_for(output: bytearray, selector: selectors.BaseSelector, marker: bytes, timeout: float) -> None:
    """Blocks until `marker` has appeared anywhere in `output`.

    Reads from the QEMU serial pipe (registered on `selector`) as it
    arrives, appending to `output`, and raises `TimeoutError` if `marker`
    still hasn't shown up after `timeout` seconds, or `RuntimeError` if the
    stream closes first (e.g. the VM crashed or exited unexpectedly).
    """
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
            b"write NOTE.TXT HELLO\rcat NOTE.TXT\rrm NOTE.TXT\r"
            b"write AB@D.TXT hi\rformat\rls\r"
            b"ticks\ruptime\rirqstat\rpmem\rpalloc\rpfree 0\r"
            b"bootinfo\rmemmap\rnic\rhalt\r"
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
