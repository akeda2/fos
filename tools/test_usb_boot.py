#!/usr/bin/env python3
"""Boot the v2 image *from a USB stick* (and from an NVMe disk) and use it as the root
filesystem (milestone M4).

The image is attached as a `usb-storage` device on `qemu-xhci` (then on `usb-ehci`) with
no SATA disk at all, so OVMF boots the loader from USB and the kernel has to find its
own boot disk through its USB host-controller and Mass Storage drivers: boot-disk
discovery by the payload partition GUID, a FOSFS mount, a file read, a write and a
read-back over the term net session. No disk image outside the build directory is
touched: each boot works on a throw-away copy."""

import shutil
import subprocess
import sys
import tempfile
import time
import os
import select
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import (BROADCAST, CODE_CANDIDATES, IMAGE, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)


def boot_from_usb(controller_args, label):
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        stick = Path(tmp) / "stick.img"
        shutil.copy(IMAGE, stick)
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"if=none,id=stick,format=raw,file={stick}", *controller_args,
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:78"]
        wire = Wire()
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 120

        def pump(until):
            while until not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL ({label}): never saw {until!r}\n{clean(bytes(raw))[-3500:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))

        try:
            pump("M3 complete")
            wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            guest = None
            for _ in range(30):
                for source, tag, payload in wire.frames(0.2):
                    if tag == TAG_DISCOVER_REPLY:
                        guest = source
                if guest:
                    break
            wire.send(guest, TAG_HELLO)
            wire.until("fos> ", 10)
            outputs = {}
            for name in ("disks", "ls", "cat readme.txt", "write usb.txt written to a usb stick", "cat usb.txt"):
                wire.send(guest, TAG_DATA, name.encode() + b"\n")
                outputs[name] = wire.until("fos> ", 15)
            wire.send(guest, TAG_DATA, b"shutdown\n")
            for _ in range(100):
                if process.poll() is not None:
                    break
                time.sleep(0.2)
            text = clean(bytes(raw))
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
    return text, outputs


def main():
    for label, arguments in (("xHCI", ["-device", "qemu-xhci,id=xhci", "-device", "usb-storage,bus=xhci.0,drive=stick,bootindex=0"]),
                             ("EHCI", ["-device", "usb-ehci,id=ehci", "-device", "usb-storage,bus=ehci.0,drive=stick,bootindex=0"]),
                             ("NVMe", ["-device", "nvme,drive=stick,serial=fosnvme,bootindex=0"])):
        text, outputs = boot_from_usb(arguments, label)
        if label == "NVMe":
            check("(NVMe 0)" in text, "NVMe: the controller is found, identified and registered as a disk", text)
        else:
            check(f"USB storage" in text and f"disk1" in text, f"{label}: the USB stick is found and registered as a disk", text)
        check("Boot disk: disk1" in text, f"{label}: boot-disk discovery finds the disk by the loader's partition GUID", text)
        check("FOSFS: mounted on disk1" in text, f"{label}: FOSFS mounts from the disk", text)
        check("Welcome to FOSFS on fos v2" in outputs["cat readme.txt"], f"{label}: a file reads back", outputs["cat readme.txt"])
        check("written to a usb stick" in outputs["cat usb.txt"], f"{label}: a written file reads back (write, flush, read)", outputs["cat usb.txt"])
    print("v2 usb boot: all checks passed")


if __name__ == "__main__":
    main()
