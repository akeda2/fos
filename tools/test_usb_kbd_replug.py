#!/usr/bin/env python3
"""A USB keyboard that is unplugged and plugged back in keeps working, over and over (xHCI and EHCI).

Before: the kernel kept the old keyboard's table entry, so every replug added another one (the table of four was full after
the fourth, and the keyboard then stayed dead), `usb` counted ghosts, and on xHCI the new device's slot number matched the
old entry, whose report buffer had been freed. Now `usb rescan` releases the old slot and entry (xHCI) or queue head (EHCI)
before it enumerates the port again.

The only input device is the USB keyboard (`-machine q35,i8042=off`); commands come over the serial port, the keyboard's keys
through the QEMU monitor's `sendkey`. Five replugs (more than the four table entries) must each leave exactly one keyboard
and keys that still reach the shell. Run with `make test-usb-kbd-replug`."""
import os
import select
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, check, clean, find, monitor_output  # noqa: E402
import shutil  # noqa: E402

CYCLES = 5


class Machine:
    def __init__(self, tmp, controller):
        self.monitor_path = str(Path(tmp) / "monitor")
        code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        image = Path(tmp) / "boot.img"
        shutil.copy(IMAGE, image)
        self.process = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35,i8042=off", "-m", "512", "-display", "none", "-serial", "stdio",
             "-monitor", f"unix:{self.monitor_path},server,nowait",
             "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
             "-drive", f"format=raw,file={image}", "-device", f"{controller},id=hc", "-device", "usb-kbd,bus=hc.0,id=kbd0,port=1"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.raw = bytearray()

    def text(self):
        return clean(bytes(self.raw))

    def wait_for(self, marker, timeout=60, after=0):
        deadline = time.monotonic() + timeout
        while marker not in self.text()[after:]:
            if time.monotonic() > deadline or self.process.poll() is not None:
                raise SystemExit(f"FAIL: never saw {marker!r}\n{self.text()[after:][-2500:]}")
            ready, _, _ = select.select([self.process.stdout], [], [], 0.2)
            if ready:
                self.raw.extend(os.read(self.process.stdout.fileno(), 4096))

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([self.process.stdout], [], [], 0.1)
            if ready:
                self.raw.extend(os.read(self.process.stdout.fileno(), 4096))

    def run(self, command, timeout=60):
        mark = len(self.text())
        self.process.stdin.write(command.encode() + b"\n")
        self.process.stdin.flush()
        self.wait_for("fos> ", timeout, mark + len(command))
        return self.text()[mark:]

    def type_keys(self, keys):
        for key in keys:
            monitor_output(self.monitor_path, f"sendkey {key}")
            self.pump(0.15)

    def close(self):
        self.process.kill()
        self.process.wait()


def scenario(controller):
    label = "EHCI" if "ehci" in controller else "xHCI"
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, controller)
        try:
            machine.wait_for("fos> ", 90)
            check("1 USB keyboard(s)" in machine.run("usb"), f"{label}: one keyboard at boot", machine.text()[-800:])
            for cycle in range(1, CYCLES + 1):
                removed = monitor_output(machine.monitor_path, f"device_del kbd{cycle - 1}")
                check("Error" not in removed, f"{label} replug {cycle}: QEMU unplugs the keyboard", removed)
                machine.pump(1.5)
                added = monitor_output(machine.monitor_path, f"device_add usb-kbd,bus=hc.0,id=kbd{cycle},port=1")
                check("Error" not in added, f"{label} replug {cycle}: QEMU plugs a keyboard back in", added)
                machine.pump(1.5)
                machine.run("usb rescan")
                listing = machine.run("usb")
                check("1 USB keyboard(s)" in listing, f"{label} replug {cycle}: exactly one keyboard (no ghosts)", listing)
                letter = "abcdefghij"[cycle]
                machine.type_keys([letter, letter, "ret"])
                machine.wait_for(f"Unknown command: {letter}{letter}", 20)
                check(True, f"{label} replug {cycle}: keys typed on the new keyboard reach the shell")
            alive = machine.process.poll() is None and "CPU EXCEPTION" not in machine.text()
            check(alive, f"{label}: the kernel is still healthy", machine.text()[-1500:])
        finally:
            machine.close()


def main():
    for controller in ("qemu-xhci", "usb-ehci"):
        scenario(controller)
    print("usb keyboard replug: all checks passed")


if __name__ == "__main__":
    main()
