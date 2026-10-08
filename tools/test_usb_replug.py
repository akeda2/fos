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
    def __init__(self, tmp, boot, stick, controller="usb-ehci"):
        self.monitor_path = str(Path(tmp) / "monitor")
        self.process = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
             "-monitor", f"unix:{self.monitor_path},server,nowait", "-drive", f"format=raw,file={boot}",
             "-device", f"{controller},id=hc", "-drive", f"if=none,id=stick,format=raw,file={stick}",
             "-device", "usb-storage,bus=hc.0,drive=stick,id=stickdev"],
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


def scenario(controller):
    label = "EHCI" if "ehci" in controller else "xHCI"
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        boot = tmp / "boot.img"
        shutil.copy(IMAGE, boot)
        stick_image = bytearray(IMAGE.read_bytes())
        build_image.randomize_guids(stick_image)        # a different payload GUID: the kernel must not mistake it for the boot disk
        stick = tmp / "stick.img"
        stick.write_bytes(stick_image)
        machine = Machine(tmp, boot, stick, controller)
        try:
            machine.wait_for("fos> ", 90)
            disks = machine.run("disks")
            check("disk1:" in disks and "disk2:" in disks and f"USB {label}" in disks, f"{label}: the stick is disk2 next to the SATA boot disk", disks)
            before = machine.run("ls disk2")
            check("readme.txt" in before, "`ls disk2` lists the stick's files", before)

            removed = monitor_output(machine.monitor_path, "device_del stickdev")
            check("Error" not in removed, "QEMU unplugs the stick", removed)
            machine.pump(2)
            gone = machine.run("ls disk2")
            check("readme.txt" not in gone, "after the stick is removed the disk stops answering", gone)

            # QEMU drops the stick's backing drive along with the device; the same image file is the same stick.
            monitor_output(machine.monitor_path, f"drive_add 0 if=none,id=stick2,format=raw,file={stick}")
            added = monitor_output(machine.monitor_path, "device_add usb-storage,bus=hc.0,drive=stick2,id=stickdev2")
            check("Error" not in added, "QEMU plugs the same stick back in", added)
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


def main():
    for controller in ("usb-ehci", "qemu-xhci"):
        scenario(controller)
    print("usb replug: all checks passed")


if __name__ == "__main__":
    main()
