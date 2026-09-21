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
    b"readme.txt  63 bytes, 1 sector(s)",
    b"router.cfg",
    b"primes.bin",
    b"hello-rs.bin",
    b"vga-colors.bin",
    b"bigsieve.bin",
    b"longmode.bin",
    b"long-rs.bin",
    b"calc.bin",
    b"beep.bin",
    b"morse.bin",
    b"net-chat.bin",
    b"ip-chat.bin",
    b"telnet.bin",
    b"bridge.bin",
    b"router.bin",
    b"net-dhcp.bin",
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
    b"Invalid name: use up to 16 lowercase letters, digits, '.', '_', or '-'.",
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
        "--legacy-unsafe-memory",
        action="store_true",
        help="expect the opt-in unpaged teaching image instead of the default paging foundation",
    )
    parser.add_argument(
        "--nic",
        choices=("none", "e1000", "dual-e1000", "rtl8139", "ne2000"),
        default="none",
        help="attach the selected QEMU NIC model (default: none)",
    )
    parser.add_argument(
        "--start-script-test",
        action="store_true",
        help="expect the test-only FOSFS start script to run before the prompt",
    )
    parser.add_argument(
        "--nic-watchdog-test",
        action="store_true",
        help="exercise the test-only foreground NIC reset latch (requires matching kernel feature)",
    )
    parser.add_argument(
        "--second-disk",
        action="store_true",
        help="attach a second, blank raw disk and exercise the diskN selector on the FOSFS commands",
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
        second_disk_path = None
        if args.second_disk:
            # A brand-new blank image has no partition table at all -- exactly
            # the state `format diskN` is meant to handle, since a secondary
            # disk is a flat FOSFS volume with no GPT (see AGENTS.md's "A
            # second (non-boot) disk" section). 4 MiB is comfortably above
            # FOSFS3's fixed 8-sector directory area with room for a few test
            # files.
            second_disk_path = os.path.join(monitor_dir, "disk2.img")
            with open(second_disk_path, "wb") as second_disk_file:
                second_disk_file.truncate(4 * 1024 * 1024)
            qemu_command.extend(["-drive", f"format=raw,file={second_disk_path}"])
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
            # Most `--nic` choices match QEMU's own `-device` name exactly;
            # "ne2000" is the one exception, since QEMU calls its NE2000-
            # compatible PCI model `ne2k_pci` rather than `ne2000`.
            qemu_device = "ne2k_pci" if args.nic == "ne2000" else args.nic
            qemu_command.extend(["-netdev", "user,id=net0", "-device", f"{qemu_device},netdev=net0"])

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
            if args.start_script_test:
                if b"start: running " not in output:
                    raise RuntimeError(
                        f"start script did not run before the prompt:\n{output.decode(errors='replace')}"
                    )
                if b"start line 3: about" not in output:
                    raise RuntimeError(
                        f"start command was not dispatched:\n{output.decode(errors='replace')}"
                    )
                # The interactive `start` command reruns the exact same
                # script on demand -- useful after F12-ing out of a
                # long-running tape it launched -- without a full machine
                # reset. Checked before the base command list's own bare
                # `format` below wipes disk1 (which would remove the start
                # file this same interactive rerun depends on).
                local_start = len(output)
                process.stdin.write(b"start\r")
                process.stdin.flush()
                wait_for(output, selector, BOOT_MARKER, 10, local_start)
                if b"start: running " not in output[local_start:]:
                    raise RuntimeError(
                        f"interactive 'start' did not rerun the script:\n{output.decode(errors='replace')}"
                    )
                if b"start line 3: about" not in output[local_start:]:
                    raise RuntimeError(
                        f"interactive 'start' did not dispatch the script's commands:\n{output.decode(errors='replace')}"
                    )
            else:
                # No start file exists anywhere in this scenario; the
                # interactive command should say so plainly rather than
                # silently doing nothing.
                local_start = len(output)
                process.stdin.write(b"start\r")
                process.stdin.flush()
                wait_for(output, selector, BOOT_MARKER, 10, local_start)
                if b"start: no start file found on disk1 or disk2." not in output[local_start:]:
                    raise RuntimeError(
                        f"interactive 'start' did not report a missing start file:\n{output.decode(errors='replace')}"
                    )
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
                b"tape load disk router.bin\r",
                b"tape load disk ip-chat.bin\r",
                b"tape load disk primes.bin\r",
                b"tape run\r",
                b"cat readme.txt\r",
                b"write note.txt HELLO\r",
                b"cat note.txt\r",
                b"rm note.txt\r",
                b"write ab@d.txt hi\r",
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
            if args.second_disk:
                commands.extend(
                    (
                        b"diskinfo\r",
                        b"diskinfo disk2\r",
                        b"format disk2\r",
                        b"ls disk2\r",
                        b"write disk2 config.txt hello-from-disk2\r",
                        b"ls disk2\r",
                        b"cat disk2 config.txt\r",
                        # `tape run @diskN/NAME` reads a tape's startup
                        # arguments from an attached secondary disk instead
                        # of only ever the boot disk. Exercised here while
                        # the primes tape the base command list already
                        # loaded (via its own earlier "tape run") is still
                        # resident, since disk1's own preloaded tapes were
                        # already wiped by that same base list's "format".
                        # primes.S ignores any startup arguments, so this
                        # only proves the file itself was read without
                        # error (no "not found"/usage message), not that
                        # its bytes changed behavior.
                        b"write disk2 expr.txt 3 4 +\r",
                        b"tape run @disk2/expr.txt\r",
                        b"tape run @disk9/expr.txt\r",
                        b"rm disk2 expr.txt\r",
                        # `tape load diskN NAME` loads straight from an
                        # attached secondary disk too, joining the existing
                        # `disk`/`net` kinds; `disk` alone still always
                        # means disk1. config.txt is plain text, not a real
                        # tape, so this only loads it (proving the disk2
                        # read path works) and never `tape run`s it.
                        b"tape load disk2 config.txt\r",
                        b"tape status\r",
                        b"tape load disk9 config.txt\r",
                        # copy: exercise both directions, an explicit rename,
                        # and confirm a loaded tape's state is invalidated
                        # since copy_file reuses the same shared scratch
                        # buffer as `tape load disk`/`tape load net`.
                        b"tape status\r",
                        b"copy disk2 config.txt disk1\r",
                        b"tape status\r",
                        b"cat disk1 config.txt\r",
                        b"write note2.txt hello-from-disk1\r",
                        b"copy disk1 note2.txt disk2 note2-copy.txt\r",
                        b"cat disk2 note2-copy.txt\r",
                        b"rm disk2 note2-copy.txt\r",
                        b"rm note2.txt\r",
                        b"rm disk1 config.txt\r",
                        b"rm disk2 config.txt\r",
                        b"ls disk2\r",
                        # The bare `format`/`ls` earlier in this same script
                        # already emptied disk1; re-checking it here proves
                        # disk2's own format/write/rm above left disk1
                        # completely untouched rather than merely unchecked.
                        b"ls\r",
                        b"cat readme.txt\r",
                    )
                )
            if args.nic in ("e1000", "dual-e1000", "ne2000"):
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
                if command == b"tape load disk router.bin\r":
                    # BIOS EDD reads temporarily enter real mode. Verify a
                    # real PS/2 key still reaches IRQ1 after loading this
                    # multi-sector tape, rather than merely proving serial
                    # shell input still works.
                    local_start = len(output)
                    send_monitor_command(monitor_path, "sendkey a")
                    send_monitor_command(monitor_path, "sendkey ret")
                    wait_for(output, selector, b"Unknown command.", 10, local_start)
                    wait_for(output, selector, BOOT_MARKER, 10, local_start)
            start = len(output)
            process.stdin.write(b"halt\r")
            process.stdin.flush()
            wait_for(output, selector, b"Halting CPU.", 10, start)
            expected = list(EXPECTED_COMMON)
            memory_marker = (
                b"Paging intentionally disabled: legacy unsafe physical-address mode;"
                if args.legacy_unsafe_memory
                else b"Paging enabled: supervisor-only identity map for the low physical range and PCI MMIO;"
            )
            expected.insert(expected.index(b"Physical memory: "), memory_marker)
            if not args.legacy_unsafe_memory:
                expected.insert(
                    expected.index(b"Physical memory: "),
                    b"Paging protection: kernel text/read-only data reject writes; PCI MMIO maps only when a driver registers its BAR.",
                )
            if args.nic == "e1000":
                expected.extend(
                    (
                        b"Intel e1000 found:",
                        b"NIC IRQ11:",
                        b"PCI IRQ11:",
                        b"Sent a 41-byte broadcast test frame",
                    )
                )
            elif args.nic == "dual-e1000":
                expected.extend(
                    (
                        b"NIC 1: Intel e1000 MAC 52:54:00:12:34:56",
                        b"NIC 2: Intel e1000 MAC 52:54:00:12:34:57",
                        b"Active interface is now 2 Intel e1000.",
                        b"Sent a 41-byte broadcast test frame",
                    )
                )
            elif args.nic == "ne2000":
                expected.extend(
                    (
                        b"NE2000-compatible found:",
                        b"Sent a 41-byte broadcast test frame",
                    )
                )
            elif args.nic_watchdog_test:
                expected.extend(
                    (
                        b"NIC watchdog: recovered a documented adapter fault in foreground",
                        b"NIC health: Rx 0 frame(s), Tx 0 frame(s), 1 recovery/recoveries, 0 failed",
                    )
                )
            else:
                expected.append(b"No supported Ethernet controller found on the PCI bus.")
            if args.second_disk:
                expected.extend(
                    (
                        b"disk2: found (BIOS drive 0x81, 8192 sectors).",
                        b"1 additional disk(s) attached: disk2..disk2.",
                        b"disk2: 8192 sectors; FOSFS partition: LBA 0 + 8192 sectors",
                        b"config.txt  16 bytes, 1 sector(s)",
                        b"hello-from-disk2",
                        b"Copied 16 byte(s) from disk2 'config.txt' to disk1 'config.txt'.",
                        b"Note: this reused the shared tape buffer, so the previously loaded tape is gone; 'tape load' again before 'tape run'.",
                        b"Copied 16 byte(s) from disk1 'note2.txt' to disk2 'note2-copy.txt'.",
                        b"hello-from-disk1",
                        b"Usage: tape run [ARGS...] or tape run router @CONFIG_FILE (or @diskN/CONFIG_FILE)",
                        b"Loaded 16 byte(s) from 'config.txt' at 0x100000. 'tape run' to execute it.",
                        b"Usage: tape load [net|disk NAME|diskN NAME]",
                    )
                )
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
