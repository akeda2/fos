#!/usr/bin/env python3
"""Checks that waiting for network input no longer pins the CPU.

fos polls its NICs in several loops. Each now sleeps with `sti; hlt` when an
iteration finds nothing to do (`shell::idle_wait`), relying on the NIC's own
receive interrupt and the 100 Hz PIT tick to wake it. QEMU's host-side CPU use
is a faithful proxy: a guest that executes `hlt` lets the vCPU thread sleep,
while a spinning guest uses a whole host core.

Measures the QEMU process's user+system CPU time (from /proc, all threads)
over a few seconds in each situation:

  1. The boot-time `term net` wait for a client      -> must be low.
  2. An active `term net` session (client connected) -> must be low.
  3. `nic discover`, which polls every NIC           -> must be low.
  4. Control: the `vga-colors` tape, which spins      -> must be HIGH, which
     proves the measurement can see a busy guest at all.
  5. The interactive tapes `net-chat`, `ip-chat`, `calc` and `ed` sitting at
     their prompts, and `telnet` waiting out its ARP timeout for a server
     that does not exist (they call `tape_rt::idle`)    -> must be low.
  6. The `bridge` and `router` tapes on a two-NIC guest, idle on the wire
     (they call the tape ABI's `wait_for_event` after an empty polling
     pass)                                              -> must be low.

Run via `make test-idle-cpu`.
"""

import os
import socket as socketlib
import sys
import tempfile
import time
from pathlib import Path

from test_scripts_settings_e2e import BUILD, GROUPS, GUEST_MACS, Guest, Mcast, build_image

GROUP = ("230.42.42.83", 4283)
LOW_LIMIT = 0.25  # fraction of one host core
HIGH_LIMIT = 0.60
SAMPLE_SECONDS = 4.0


def cpu_seconds(pid: int) -> float:
    with open(f"/proc/{pid}/stat") as stat:
        # The command name may contain spaces; fields resume after the last ')'.
        fields = stat.read().rsplit(")", 1)[1].split()
    ticks = int(fields[11]) + int(fields[12])  # utime + stime
    return ticks / os.sysconf("SC_CLK_TCK")


def measure(guest: Guest, seconds: float = SAMPLE_SECONDS) -> float:
    """Fraction of one host core QEMU used over `seconds`. The guest's
    serial output is drained meanwhile so a full pipe cannot stall it."""
    pid = guest.process.pid
    start_cpu, start = cpu_seconds(pid), time.monotonic()
    while time.monotonic() - start < seconds:
        for key, _ in guest.selector.select(0.2):
            guest.output.extend(os.read(key.fd, 4096))
    return (cpu_seconds(pid) - start_cpu) / (time.monotonic() - start)


def monitor_key(path: str, key: str) -> None:
    for _ in range(50):
        try:
            with socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM) as mon:
                mon.connect(path)
                mon.sendall(f"sendkey {key}\n".encode())
            return
        except (FileNotFoundError, ConnectionRefusedError):
            time.sleep(0.1)
    raise SystemExit("could not reach the QEMU monitor")


def check(label: str, usage: float, low: bool) -> None:
    percent = usage * 100
    if low and usage > LOW_LIMIT:
        raise SystemExit(f"FAIL: {label} used {percent:.0f}% of a core (limit {LOW_LIMIT * 100:.0f}%)")
    if not low and usage < HIGH_LIMIT:
        raise SystemExit(f"FAIL: control '{label}' used only {percent:.0f}% of a core;"
                         " the measurement cannot see a busy guest")
    print(f"PASS: {label}: {percent:.0f}% of a core")


def test_network_tapes(tmp: Path) -> None:
    image = tmp / "tapes.img"
    build_image(image, {
        "bridge.bin": BUILD / "tapes" / "bridge.bin",
        "router.bin": BUILD / "tapes" / "router.bin",
    })
    monitor = str(tmp / "tapes.mon")
    extra = ["-monitor", f"unix:{monitor},server,nowait"]
    for index, (group, port) in enumerate(GROUPS):
        extra += [
            "-netdev", f"socket,id=net{index},mcast={group}:{port}",
            "-device", f"rtl8139,netdev=net{index},mac={GUEST_MACS[index].hex(':')}",
        ]
    guest = Guest(image, extra)
    try:
        guest.read_until(b"Waiting for a remote terminal client to connect;", 20)
        monitor_key(monitor, "f12")
        guest.read_until(b"fos> ", 10)
        for label, command, ready in (
            ("bridge tape, idle", "bridge 1 2", b"learning Ethernet"),
            ("router tape, idle", "router 1 192.168.50.1/24 2 192.168.60.1/24", b"Running"),
        ):
            start = len(guest.output)
            guest.process.stdin.write(command.encode() + b"\r\n")
            guest.process.stdin.flush()
            guest.read_until(ready, 10, start)
            time.sleep(1.5)
            check(label, measure(guest), low=True)
            monitor_key(monitor, "f12")
            guest.read_until(b"Tape returned", 10, start)
    finally:
        guest.terminate()


def main() -> None:
    with tempfile.TemporaryDirectory() as name:
        tmp = Path(name)
        image = tmp / "idle.img"
        build_image(image, {
            f"{name}.bin": BUILD / "tapes" / f"{name}.bin"
            for name in ("vga-colors", "net-chat", "ip-chat", "calc", "ed", "telnet")
        })
        monitor = str(tmp / "idle.mon")
        guest = Guest(image, [
            "-monitor", f"unix:{monitor},server,nowait",
            "-netdev", f"socket,id=net0,mcast={GROUP[0]}:{GROUP[1]}",
            "-device", f"rtl8139,netdev=net0,mac={GUEST_MACS[0].hex(':')}",
        ])
        try:
            guest.read_until(b"Waiting for a remote terminal client to connect;", 20)
            time.sleep(1)  # let boot-time work settle
            check("term net waiting for a client", measure(guest), low=True)

            sender = Mcast(*GROUP)
            checkpoint = len(guest.output)
            deadline = time.monotonic() + 15
            while b"Remote terminal connected" not in guest.output[checkpoint:]:
                if time.monotonic() > deadline:
                    raise SystemExit("HELLO never opened a term net session")
                sender.hello(GUEST_MACS[0])
                for key, _ in guest.selector.select(0.3):
                    guest.output.extend(os.read(key.fd, 4096))
            time.sleep(1)
            check("active term net session", measure(guest), low=True)

            monitor_key(monitor, "f12")  # local escape: back to the prompt
            guest.read_until(b"fos> ", 10, checkpoint)

            start = len(guest.output)
            guest.process.stdin.write(b"nic discover\r\n")
            guest.process.stdin.flush()
            guest.read_until(b"Broadcasting a discovery probe", 10, start)
            time.sleep(1)
            check("nic discover", measure(guest), low=True)
            monitor_key(monitor, "f12")
            guest.read_until(b"fos> ", 10, start)

            start = len(guest.output)
            guest.process.stdin.write(b"vga-colors\r\n")
            guest.process.stdin.flush()
            guest.read_until(b"Running", 10, start)
            time.sleep(1)
            check("vga-colors tape (control, spins)", measure(guest), low=False)
            monitor_key(monitor, "f12")
            guest.read_until(b"Tape returned", 10, start)

            # `ed` edits an existing file, so make one through the shell and
            # wait for that command's own output rather than for a prompt.
            start = len(guest.output)
            guest.process.stdin.write(b"write scratch hello\r\n")
            guest.process.stdin.flush()
            guest.read_until(b"File written.", 10, start)
            guest.read_until(b"fos> ", 10, start + guest.output[start:].index(b"File written."))

            # (label, command, text that shows it is waiting, text after F12)
            for label, command, ready, done in (
                ("net-chat tape, idle", "net-chat", b"Type a line and press Enter", b"Left tape chat"),
                ("ip-chat tape, idle", "ip-chat static 10.9.0.2/24 10.9.0.1", b"IPv4 chat ready",
                 b"Left IPv4 chat"),
                ("calc tape, at its prompt", "calc", b"RPN calculator", b"Exiting the calculator"),
                ("ed tape, at its prompt", "ed scratch", b"Type h for help", b"Tape returned"),
            ):
                start = len(guest.output)
                guest.process.stdin.write(command.encode() + b"\r\n")
                guest.process.stdin.flush()
                guest.read_until(ready, 10, start)
                time.sleep(1)
                check(label, measure(guest), low=True)
                monitor_key(monitor, "f12")
                guest.read_until(done, 10, start)
                guest.read_until(b"fos> ", 10, start)

            # telnet's ARP wait lasts 3 s (ARP_TIMEOUT_TICKS), so sample 2 s
            # starting the moment the tape is running.
            start = len(guest.output)
            guest.process.stdin.write(b"telnet static 10.9.0.2/24 10.9.0.1 10.9.0.9 23\r\n")
            guest.process.stdin.flush()
            guest.read_until(b"Running", 10, start)
            check("telnet tape, waiting for ARP", measure(guest, 2.0), low=True)
            guest.read_until(b"Tape returned", 10, start)
        finally:
            guest.terminate()
        test_network_tapes(tmp)


if __name__ == "__main__":
    sys.exit(main())
