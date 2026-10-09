#!/usr/bin/env python3
"""A USB stick that is unplugged and plugged back in gets its disk back (EHCI and xHCI).

Before: the kernel kept the stick's old USB address, `usb rescan` skipped the "claimed" port, and the disk stayed dead until a
reboot (found on the AMD/nForce board when its boot stick was moved to another machine and returned).
Now: the controller's connect-status-change bit marks the port as changed, `usb rescan` enumerates it again, and a stick with
the same identity (INQUIRY data and size) as a disk that no longer answers takes over that disk's number.

Boots through SeaBIOS from a SATA disk with a second image on a USB stick behind QEMU's EHCI; the QEMU monitor removes and
re-adds the stick. Checks: the stick works; after removal it fails; after replug and `usb rescan` the same disk number works
again, lists the same files, and no extra disk appears. Run with `make test-usb-replug`."""
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_image  # noqa: E402
from test_boot import IMAGE, check, clean, monitor_output  # noqa: E402


class Machine:
    def __init__(self, tmp, boot, stick, controller="usb-ehci", boot_from_stick=False, behind_hub=False):
        self.monitor_path = str(Path(tmp) / "monitor")
        disks = [] if boot_from_stick else ["-drive", f"format=raw,file={boot}"]
        # the stick sits on an explicit root port so that a replug lands on the same port, as a person's does
        device = f"usb-storage,bus=hc.0,drive=stick,id=stickdev,port={'1.3' if behind_hub else '1'}" + (",bootindex=0" if boot_from_stick else "")
        self.process = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
             "-monitor", f"unix:{self.monitor_path},server,nowait", *disks,
             "-device", f"{controller},id=hc", "-drive", f"if=none,id=stick,format=raw,file={stick}",
             *(["-device", "usb-hub,bus=hc.0,port=1,id=hubdev"] if behind_hub else []), "-device", device],
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

    def run(self, command, timeout=30):
        """Types a command and returns its output (up to the next prompt)."""
        mark = len(self.text())
        self.process.stdin.write(command.encode() + b"\n")
        self.process.stdin.flush()
        self.wait_for("fos> ", timeout, mark + len(command))
        return self.text()[mark:]

    def close(self):
        self.process.kill()
        self.process.wait()


def scenario(controller, behind_hub=False):
    label = ("EHCI" if "ehci" in controller else "xHCI") + (" behind a hub" if behind_hub else "")
    port = "1.3" if behind_hub else "1"
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        boot = tmp / "boot.img"
        shutil.copy(IMAGE, boot)
        stick_image = bytearray(IMAGE.read_bytes())
        build_image.randomize_guids(stick_image)        # a different payload GUID: the kernel must not mistake it for the boot disk
        stick = tmp / "stick.img"
        stick.write_bytes(stick_image)
        machine = Machine(tmp, boot, stick, controller, behind_hub=behind_hub)
        try:
            machine.wait_for("fos> ", 90)
            disks = machine.run("disks")
            check("disk1:" in disks and "disk2:" in disks and f"USB {label.split()[0]}" in disks, f"{label}: the stick is disk2 next to the SATA boot disk", disks)
            before = machine.run("ls disk2")
            check("readme.txt" in before, "`ls disk2` lists the stick's files", before)

            removed = monitor_output(machine.monitor_path, "device_del stickdev")
            check("Error" not in removed, "QEMU unplugs the stick", removed)
            machine.pump(2)
            gone = machine.run("ls disk2")
            check("readme.txt" not in gone, "after the stick is removed the disk stops answering", gone)

            # QEMU drops the stick's backing drive along with the device; the same image file is the same stick.
            monitor_output(machine.monitor_path, f"drive_add 0 if=none,id=stick2,format=raw,file={stick}")
            added = monitor_output(machine.monitor_path, f"device_add usb-storage,bus=hc.0,drive=stick2,id=stickdev2,port={port}")
            check("Error" not in added, f"{label}: QEMU plugs the same stick back in", added)
            machine.pump(2)
            rescan = machine.run("usb rescan", 60)
            check("unplugged or replugged" in rescan or "is back" in machine.text(), "`usb rescan` notices that the port's device changed", rescan)
            check("is back; disk2 works again" in machine.text(), "and recognises the same stick: it takes over disk2", machine.text()[-1500:])
            after = machine.run("ls disk2")
            check("readme.txt" in after and after.split("ls disk2")[-1].strip() == before.split("ls disk2")[-1].strip(),
                  "`ls disk2` works again and lists the same files", after)
            disks = machine.run("disks")
            check("disk3" not in disks, "no extra disk was registered", disks)
        finally:
            machine.close()


def hub_replug_scenario():
    """The hub itself is unplugged, with a stick behind it, and plugged back in (the stick plugged into it again): everything
    behind the hub must fail at once, then the hub is enumerated afresh and the same stick takes its old disk number back."""
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        boot = tmp / "boot.img"
        shutil.copy(IMAGE, boot)
        stick_image = bytearray(IMAGE.read_bytes())
        build_image.randomize_guids(stick_image)
        stick = tmp / "stick.img"
        stick.write_bytes(stick_image)
        machine = Machine(tmp, boot, stick, "qemu-xhci", behind_hub=True)
        try:
            machine.wait_for("fos> ", 90)
            before = machine.run("ls disk2")
            check("readme.txt" in before, "hub replug: the stick behind the hub is disk2", before)
            monitor_output(machine.monitor_path, "device_del hubdev")
            machine.pump(2)
            started = time.monotonic()
            gone = machine.run("ls disk2", 30)
            check("readme.txt" not in gone and time.monotonic() - started < 10, f"after the hub is unplugged its disk fails at once ({time.monotonic() - started:.1f} s)", gone)
            check("cannot read the disk" in gone, "and `ls` says the disk cannot be read (not that it has no partition)", gone)
            added = monitor_output(machine.monitor_path, "device_add usb-hub,bus=hc.0,port=1,id=hubdev2")
            check("Error" not in added, "QEMU plugs a hub back in", added)
            machine.pump(2)
            monitor_output(machine.monitor_path, f"drive_add 0 if=none,id=stick2,format=raw,file={stick}")
            added = monitor_output(machine.monitor_path, "device_add usb-storage,bus=hc.0,drive=stick2,id=stickdev2,port=1.3")
            check("Error" not in added, "and the same stick into it", added)
            machine.pump(2)
            rescan = machine.run("usb rescan", 90)
            alive = machine.process.poll() is None and "CPU EXCEPTION" not in machine.text()
            check(alive, "the kernel survives `usb rescan` (no fault through a released slot)", machine.text()[-1500:])
            check("is back; disk2 works again" in machine.text(), "the hub is enumerated afresh and the stick takes disk2 back", rescan)
            after = machine.run("ls disk2")
            check("readme.txt" in after, "`ls disk2` works again", after)
            disks = machine.run("disks")
            check("disk3" not in disks, "and no extra disk was registered", disks)
        finally:
            machine.close()


def boot_disk_scenario():
    """The machine's own boot stick (on xHCI) is unplugged and plugged back in on the same port: it must come back as
    disk1, not as a second disk. (Found on the Skylake: the old handle named a slot number that was freed and, depending on
    the numbering, either aliased the new connection -- so the old disk looked healthy and the returning stick became a
    duplicate -- or pointed at a freed ring and crashed the kernel.)"""
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image = tmp / "boot.img"
        shutil.copy(IMAGE, image)
        machine = Machine(tmp, image, image, "qemu-xhci", boot_from_stick=True)
        try:
            machine.wait_for("fos> ", 90)
            disks = machine.run("disks")
            check("disk1:" in disks and "USB xHCI" in disks and "disk2" not in disks, "xHCI boot stick: it is disk1, the only disk", disks)
            monitor_output(machine.monitor_path, "device_del stickdev")
            machine.pump(2)
            monitor_output(machine.monitor_path, f"drive_add 0 if=none,id=stick2,format=raw,file={image}")
            added = monitor_output(machine.monitor_path, "device_add usb-storage,bus=hc.0,drive=stick2,id=stickdev2,port=1")
            check("Error" not in added, "xHCI boot stick: plugged back in on the same port", added)
            machine.pump(2)
            rescan = machine.run("usb rescan", 60)
            alive = machine.process.poll() is None and "CPU EXCEPTION" not in machine.text()
            check(alive, "the kernel survives `usb rescan` (no fault through a released slot)", machine.text()[-1500:])
            check("is back; disk1 works again" in machine.text(), "the returning boot stick takes over disk1", rescan)
            disks = machine.run("disks")
            check("disk2" not in disks, "and no duplicate disk2 appears", disks)
            files = machine.run("ls")
            check("readme.txt" in files, "`ls` on the boot volume works again", files)
        finally:
            machine.close()


def main():
    for controller in ("usb-ehci", "qemu-xhci"):
        scenario(controller)
    scenario("qemu-xhci", behind_hub=True)
    hub_replug_scenario()
    boot_disk_scenario()
    print("usb replug: all checks passed")


if __name__ == "__main__":
    main()
