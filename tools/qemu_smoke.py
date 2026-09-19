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
import socket
import subprocess
import sys
import tempfile
import time

BOOT_MARKER = b"fos> "
EXPECTED_COMMON = (
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
    b"System: help clear vga about bootinfo memmap memory cpuinfo hexdump halt reset",
    b"Interrupts: ticks uptime irqstat pmem palloc pfree",
    b"Ethernet: nic (active status), nic list, nic use N, nic send (broadcast test frame),",
    b"fos is a small educational 32-bit x86 operating system.",
    b"This command does not accept arguments.",
    b"Unknown command. Type 'help' for commands.",
    b"VGA mode changed to 80x50 (80 columns x 50 rows).",
    b"VGA mode changed to 132x25 (experimental) (132 columns x 25 rows).",
    b"Warning: 132-column timing is experimental",
    b"VGA mode: 132x25 (experimental) (132 columns x 25 rows).",
    b"Available modes: 80x25 (default), 80x50, 132x25 (experimental).",
    b"Use 'vga d' to return to the default 80x25 mode.",
    b"VGA mode changed to 80x25 (80 columns x 25 rows).",
    b"Boot drive: 0x80;",
    b"Firmware memory map (",
    b"usable RAM)",
    b"reserved (not usable))",
    b"Halting CPU.",
    b"FOSFS partition: LBA 4096 + 28639 sectors",
    b"FOSFS3",
    b"README.TXT  63 bytes, 1 sector(s)",
    b"PRIMES.BIN",
    b"HELLO-RS.BIN",
    b"VGA-COLORS.BIN",
    b"BIGSIEVE.BIN",
    b"LONGMODE.BIN",
    b"LONG-RS.BIN",
    b"CALC.BIN",
    b"BEEP.BIN",
    b"MORSE.BIN",
    b"NET-CHAT.BIN",
    b"IP-CHAT.BIN",
    b"TELNET.BIN",
    b"BRIDGE.BIN",
    b"ROUTER.BIN",
    b"Loaded ",
    b"fos test tape: Sieve of Eratosthenes up to 1,000,000",
    b"primes found:   78498",
    b"largest prime:  999983",
    b"Tape returned; back at the fos shell.",
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
)


def wait_for(
    output: bytearray,
    selector: selectors.BaseSelector,
    marker: bytes,
    timeout: float,
    start: int = 0,
) -> None:
    """Blocks until `marker` has appeared anywhere in `output`.

    Reads from the QEMU serial pipe (registered on `selector`) as it
    arrives, appending to `output`, and raises `TimeoutError` if `marker`
    still hasn't shown up after `timeout` seconds, or `RuntimeError` if the
    stream closes first (e.g. the VM crashed or exited unexpectedly).
    """
    deadline = time.monotonic() + timeout
    while marker not in output[start:]:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"did not observe {marker!r}; serial output:\n{output.decode(errors='replace')}")
        for key, _ in selector.select(remaining):
            chunk = os.read(key.fd, 4096)
            if not chunk:
                raise RuntimeError(f"QEMU serial stream ended early:\n{output.decode(errors='replace')}")
            output.extend(chunk)


def send_monitor_command(path: str, command: str, timeout: float = 5) -> None:
    """Sends one human-monitor command, retrying until QEMU creates the socket."""
    deadline = time.monotonic() + timeout
    while True:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as monitor:
                monitor.settimeout(1)
                monitor.connect(path)
                monitor.sendall((command + "\n").encode())
                return
        except (FileNotFoundError, ConnectionRefusedError, TimeoutError):
            if time.monotonic() >= deadline:
                raise RuntimeError(f"could not connect to QEMU monitor at {path}")
            time.sleep(0.05)


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--qemu", default="qemu-system-i386")
    parser.add_argument(
        "--nic",
        choices=("none", "e1000", "dual-e1000", "rtl8139"),
        default="none",
        help="attach the selected QEMU NIC model (default: none)",
    )
    args = parser.parse_args()

    with tempfile.TemporaryDirectory(prefix="fos-qemu-monitor-") as monitor_dir:
        monitor_path = os.path.join(monitor_dir, "monitor.sock")
        qemu_command = [
            args.qemu,
            "-drive", f"format=raw,file={args.image}",
            "-serial", "stdio",
            "-monitor", f"unix:{monitor_path},server=on,wait=off",
            "-display", "none",
            "-snapshot",
            "-no-reboot",
            "-no-shutdown",
        ]
        if args.nic == "none":
            qemu_command.extend(["-nic", "none"])
        elif args.nic == "dual-e1000":
            qemu_command.extend(
                [
                    "-netdev",
                    "user,id=net0",
                    "-device",
                    "e1000,netdev=net0,mac=52:54:00:12:34:56",
                    "-netdev",
                    "user,id=net1",
                    "-device",
                    "e1000,netdev=net1,mac=52:54:00:12:34:57",
                ]
            )
        else:
            qemu_command.extend(["-netdev", "user,id=net0", "-device", f"{args.nic},netdev=net0"])

        process = subprocess.Popen(
            qemu_command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        assert process.stdin is not None and process.stdout is not None
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        output = bytearray()
        try:
            if args.nic == "none":
                wait_for(output, selector, BOOT_MARKER, 10)
            else:
                wait_for(output, selector, b"Waiting for a remote terminal client to connect;", 10)
                send_monitor_command(monitor_path, "sendkey f12")
            wait_for(output, selector, BOOT_MARKER, 10)
            commands = [
                b"helx\x08p\r",
                b"about\r",
                b"help extra\r",
                b"unknown\r",
                b"vga 80x50\r",
                b"vga 132x25\r",
                b"vga\r",
                b"vga d\r",
                b"clear\r",
                b"diskinfo\r",
                b"readsector 4096\r",
                b"cpuinfo\r",
                b"memory\r",
                b"ls\r",
                b"tape load disk IP-CHAT.BIN\r",
                b"tape load disk PRIMES.BIN\r",
                b"tape run\r",
                b"cat README.TXT\r",
                b"write NOTE.TXT HELLO\r",
                b"cat NOTE.TXT\r",
                b"rm NOTE.TXT\r",
                b"write AB@D.TXT hi\r",
                b"format\r",
                b"ls\r",
                b"ticks\r",
                b"uptime\r",
                b"irqstat\r",
                b"pmem\r",
                b"palloc\r",
                b"pfree 0\r",
                b"bootinfo\r",
                b"memmap\r",
            ]
            if args.nic in ("e1000", "dual-e1000"):
                commands.extend((b"nic\r", b"nic send\r"))
            else:
                commands.append(b"nic\r")
            if args.nic == "dual-e1000":
                commands[commands.index(b"nic\r") : commands.index(b"nic\r") + 1] = [
                    b"nic list\r",
                    b"nic use 2\r",
                    b"nic\r",
                    b"nic use 1\r",
                    b"nic send\r",
                ]
                commands.pop()
            for command in commands:
                start = len(output)
                process.stdin.write(command)
                process.stdin.flush()
                wait_for(output, selector, BOOT_MARKER, 10, start)
            start = len(output)
            process.stdin.write(b"halt\r")
            process.stdin.flush()
            wait_for(output, selector, b"Halting CPU.", 10, start)
            expected = list(EXPECTED_COMMON)
            if args.nic == "e1000":
                expected.extend((b"Intel e1000 found:", b"Sent a 41-byte broadcast test frame"))
            elif args.nic == "dual-e1000":
                expected.extend(
                    (
                        b"NIC 1: Intel e1000 MAC 52:54:00:12:34:56",
                        b"NIC 2: Intel e1000 MAC 52:54:00:12:34:57",
                        b"Active interface is now 2 Intel e1000.",
                        b"Sent a 41-byte broadcast test frame",
                    )
                )
            else:
                expected.append(b"No supported Ethernet controller found on the PCI bus.")
            for marker in expected:
                wait_for(output, selector, marker, 10)
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
