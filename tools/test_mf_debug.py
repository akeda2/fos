#!/usr/bin/env python3
"""Unattended operation on the server kernel (`make test-mf-debug`): `debug crash` with reboot-on-crash writes a crash record,
resets, and the next boot prints the record; `debug hang` (the service thread spins) is caught by the watchdog the same way, at
-smp 1 and 4; crash-on-boot ends in the crash-loop guard (five resets, then a halt). QEMU runs without -no-reboot: the point is
what happens across resets."""
import os
import select
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_DEBUG", ROOT / "build" / "fosfor-mf" / "fosfor-debug.img"))
LOOP_IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_DEBUG_LOOP", ROOT / "build" / "fosfor-mf" / "fosfor-debug-loop.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())


class Machine:
    def __init__(self, image, smp):
        self.log = b""
        self.qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "512", "-smp", str(smp), "-display", "none",
             "-serial", "stdio", "-monitor", "none",
             "-global", "ICH9-LPC.noreboot=false", "-watchdog-action", "reset",
             "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={image},snapshot=on"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    def wait_for(self, text, timeout, start=0):
        end = time.time() + timeout
        while time.time() < end:
            if text.encode() in self.log[start:]:
                return True
            if select.select([self.qemu.stdout], [], [], 0.2)[0]:
                self.log += os.read(self.qemu.stdout.fileno(), 4096)
        return text.encode() in self.log[start:]

    def send(self, text):
        self.qemu.stdin.write(text.encode())
        self.qemu.stdin.flush()

    def since(self, mark):
        return self.log[mark:].decode("utf8", "replace")

    def stop(self):
        self.qemu.kill()
        self.qemu.wait()


def crash_or_hang(smp, command, expect):
    machine = Machine(IMAGE, smp)
    try:
        if not machine.wait_for("fos> ", 150):
            return "no shell prompt:\n" + machine.since(0)[-800:]
        if not machine.wait_for("Watchdog armed: reset after 5 s", 1):
            return "the watchdog was not armed at the milestone"
        mark = len(machine.log)
        machine.send(command + " now\n")
        if not machine.wait_for(expect, 60, mark) or not machine.wait_for("resetting in 1 s", 20, mark):
            return f"`{command}` did not report and reset:\n" + machine.since(mark)[-1500:]
        if not machine.wait_for("fos> ", 150, mark + 1):
            return "no shell after the reset:\n" + machine.since(mark)[-1500:]
        text = machine.since(mark)
        if "Previous boot crashed (record found in RAM)" not in text or expect.split(":")[-1].strip().split()[0] not in text.split("Previous boot crashed")[1]:
            return "the next boot did not report the crash record:\n" + text[-1500:]
        return None
    finally:
        machine.stop()


def dead_hang():
    """`debug deadhang` spins with interrupts off on the boot CPU: the software watchdog's tick never comes, the chipset's TCO
    timer resets the machine (no crash record: nothing ran to write one)."""
    machine = Machine(IMAGE, 1)
    try:
        if not machine.wait_for("fos> ", 150):
            return "no shell prompt:\n" + machine.since(0)[-800:]
        if not machine.wait_for("Hardware watchdog: TCO timer running", 1):
            return "the hardware watchdog was not started:\n" + "\n".join(l for l in machine.since(0).splitlines() if "atchdog" in l)
        mark = len(machine.log)
        machine.send("debug deadhang now\n")
        if not machine.wait_for("entering the service loop", 120, mark):
            return "the hardware watchdog never reset the machine:\n" + machine.since(mark)[-1500:]
        if not machine.wait_for("fos> ", 150, mark + 1):
            return "no shell after the reset:\n" + machine.since(mark)[-1500:]
        return None
    finally:
        machine.stop()


def crash_loop():
    machine = Machine(LOOP_IMAGE, 1)
    try:
        if not machine.wait_for("not resetting again", 240):
            return "the crash-loop guard never stopped the resets:\n" + machine.since(0)[-1500:]
        text = machine.since(0)
        if text.count("CPU exception 6") < 5 or "Previous boot crashed (record found in RAM)" not in text:
            return f"expected five crashes and a record ({text.count('CPU exception 6')} crashes seen)"
        return None
    finally:
        machine.stop()


failed = False
for name, problem in [
    ("-smp 1 debug crash", lambda: crash_or_hang(1, "debug crash", "#UD")),
    ("-smp 4 debug crash", lambda: crash_or_hang(4, "debug crash", "#UD")),
    ("-smp 1 debug hang (watchdog)", lambda: crash_or_hang(1, "debug hang", "watchdog timeout")),
    ("-smp 4 debug hang (watchdog)", lambda: crash_or_hang(4, "debug hang", "watchdog timeout")),
    ("debug deadhang (hardware watchdog)", dead_hang),
    ("crash-on-boot (crash-loop guard)", crash_loop),
]:
    result = problem()
    print(f"{name}: {'ok' if result is None else 'FAIL'}")
    if result:
        print(result)
        failed = True
sys.exit(1 if failed else 0)
