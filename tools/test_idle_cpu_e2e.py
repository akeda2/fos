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

Run via `make test-idle-cpu`.
"""

import os
import socket as socketlib
import sys
import tempfile
import time
from pathlib import Path

from test_scripts_settings_e2e import BUILD, GUEST_MACS, Guest, Mcast, build_image

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


def measure(guest: Guest) -> float:
    """Fraction of one host core QEMU used over SAMPLE_SECONDS. The guest's
    serial output is drained meanwhile so a full pipe cannot stall it."""
    pid = guest.process.pid
    start_cpu, start = cpu_seconds(pid), time.monotonic()
    while time.monotonic() - start < SAMPLE_SECONDS:
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


def main() -> None:
    with tempfile.TemporaryDirectory() as name:
        tmp = Path(name)
        image = tmp / "idle.img"
        build_image(image, {"vga-colors.bin": BUILD / "tapes" / "vga-colors.bin"})
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
        finally:
            guest.terminate()


if __name__ == "__main__":
    sys.exit(main())
