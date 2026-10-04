#!/usr/bin/env python3
"""The `diskN` selector of `ls`, `cat`, `write` and `rm` (v2 shell).

The guest boots from a SATA disk and has a second SATA disk with a FOSFS volume of its own
(built with one extra file). `ls disk2` must show that disk's files and not the boot disk's,
`cat disk2 NAME` must read from it, `write disk2 ...` must change only it, and a disk
without a FOSFS volume or a number that does not exist must be refused with a message --
never silently answered from the boot volume, which is what `ls disk3` used to do."""

import shutil
import subprocess
import sys
import tempfile
import time
import os
import select
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import (BROADCAST, CODE_CANDIDATES, IMAGE, ROOT, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fos-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fos-v2-kernel"


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        boot = tmp / "boot.img"
        shutil.copy(IMAGE, boot)
        marker = tmp / "only2"
        marker.write_text("this file lives on the second disk only\n")
        second = tmp / "second.img"
        subprocess.run([sys.executable, str(ROOT / "v2/tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
                        "--output", str(second), "--file", f"only2={marker}"], check=True, capture_output=True)
        blank = tmp / "blank.img"
        blank.write_bytes(bytes(8 << 20))
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-device", "ich9-ahci,id=ahci",
                   "-drive", f"if=none,id=d1,format=raw,file={boot}", "-device", "ide-hd,drive=d1,bus=ahci.0,bootindex=0",
                   "-drive", f"if=none,id=d2,format=raw,file={second}", "-device", "ide-hd,drive=d2,bus=ahci.1",
                   "-drive", f"if=none,id=d3,format=raw,file={blank}", "-device", "ide-hd,drive=d3,bus=ahci.2",
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:79"]
        wire = Wire()
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 120
        try:
            while "M3 complete" not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: never booted\n{clean(bytes(raw))[-3000:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))
            wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            guest = None
            for _ in range(30):
                for source, tag, _ in wire.frames(0.2):
                    if tag == TAG_DISCOVER_REPLY:
                        guest = source
                if guest:
                    break
            wire.send(guest, TAG_HELLO)
            wire.until("fos> ", 10)

            def run(line):
                wire.send(guest, TAG_DATA, line.encode() + b"\n")
                return wire.until("fos> ", 15)

            disks = run("disks")
            check("disk1" in disks and "disk2" in disks and "disk3" in disks, "three disks are found", disks)
            boot_listing = run("ls")
            check("only2" not in boot_listing and "readme.txt" in boot_listing, "`ls` lists the boot volume (disk1)", boot_listing)
            second_listing = run("ls disk2")
            check("only2" in second_listing, "`ls disk2` lists the second disk's own files", second_listing)
            check("only2" in run("ls disk2") and "only2" not in run("ls disk1"), "and `ls disk1` is still the boot volume", "")
            check("lives on the second disk only" in run("cat disk2 only2"), "`cat disk2 NAME` reads from the second disk", "")
            check("no such file" in run("cat only2"), "and `cat NAME` still reads the boot volume", "")
            run("write disk2 note.txt written to disk two")
            check("note.txt" in run("ls disk2") and "note.txt" not in run("ls"), "`write disk2` changes only that disk", "")
            check("Removed note.txt" in run("rm disk2 note.txt") and "note.txt" not in run("ls disk2"), "`rm disk2` removes from that disk", "")
            blank_answer = run("ls disk3")
            check("no FOSFS data partition" in blank_answer and "readme.txt" not in blank_answer, "a disk without a FOSFS volume is refused, not answered from the boot volume", blank_answer)
            check("no such disk" in run("ls disk9"), "a disk that does not exist is refused", "")
            wire.send(guest, TAG_DATA, b"shutdown\n")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
    print("v2 disks: all checks passed")


if __name__ == "__main__":
    main()
