#!/usr/bin/env python3
"""The `diskN` selector and volume labels (v2 shell).

The guest boots from a SATA disk and has a second SATA disk with a FOSFS volume of its own
(built with one extra file). `ls disk2` must show that disk's files and not the boot disk's,
`cat disk2 NAME` must read from it, `write disk2 ...` must change only it, and a disk
without a FOSFS volume or a number that does not exist must be refused with a message --
never silently answered from the boot volume, which is what `ls disk3` used to do.

The boot disk is deliberately attached *behind* the second disk, so the drivers find it second;
the kernel must still number it disk1. The second disk carries the label `backup`, and
`@backup` must select it however the disks are numbered, while `label` sets, shows, refuses
duplicates and clears."""

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
        subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
                        "--output", str(second), "--file", f"only2={marker}", "--label", "backup",
                        "--payload-guid", "f05b2a00-0000-4f05-8000-0000000000aa"], check=True, capture_output=True)
        blank = tmp / "blank.img"
        blank.write_bytes(bytes(8 << 20))
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-device", "ich9-ahci,id=ahci",
                   "-drive", f"if=none,id=d2,format=raw,file={second}", "-device", "ide-hd,drive=d2,bus=ahci.0",
                   "-drive", f"if=none,id=d1,format=raw,file={boot}", "-device", "ide-hd,drive=d1,bus=ahci.1,bootindex=0",
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

            check("renumbering it to disk1" in clean(bytes(raw)), "the boot disk, found second by the drivers, is renumbered to disk1", clean(bytes(raw))[-2500:])
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

            listing = run("label")
            check("disk2: @backup" in listing and "disk1: (no label)" in listing, "`label` lists each volume's label", listing)
            check("only2" in run("ls @backup"), "`ls @backup` selects the labelled volume", "")
            check("lives on the second disk only" in run("cat @backup only2"), "`cat @backup NAME` reads from it", "")
            check("is now @boot" in run("label disk1 boot") and "disk1: @boot" in run("label disk1"), "`label disk1 NAME` sets a label", "")
            check("already carries" in run("label disk1 backup"), "a label already in use is refused", "")
            check("up to 16 lowercase" in run("label disk1 Bad/Name"), "an invalid label is refused", "")
            check("no FOSFS" in run("label disk3 x"), "a disk without a FOSFS volume cannot be labelled", "")
            check("no volume carries that label" in run("ls @nothing"), "an unknown label is refused, not answered from the boot volume", "")
            check("label cleared" in run("label @boot -") and "disk1: (no label)" in run("label"), "`label @NAME -` clears it", "")

            check("Copied only2" in run("copy disk2 only2 disk1"), "`copy disk2 NAME disk1` copies a file between volumes", "")
            check("lives on the second disk only" in run("cat only2"), "and the copy reads back on the destination", "")
            check("as moved2" in run("copy @backup only2 disk1 moved2") and "lives on the second" in run("cat moved2"), "`copy` takes @LABEL volumes and a new name", "")
            check("same file" in run("copy disk1 only2 disk1"), "copying a file onto itself is refused", "")
            check("no such file" in run("copy disk1 missing disk2"), "a missing source file is refused", "")
            check("not diskN or @LABEL" in run("copy boot only2 disk2"), "a bad volume word is refused", "")
            # The same files written as `diskN/NAME` (the form tapes use), in every command that takes a file.
            check("lives on the second disk only" in run("cat disk2/only2"), "`cat disk2/NAME` reads from the second disk", "")
            check("Copied only2" in run("copy disk2/only2 disk1/slash1") and "lives on the second" in run("cat slash1"), "`copy disk2/NAME disk1/NEWNAME` works", "")
            check("as slash2" in run("copy disk2/only2 disk1 slash2") and "lives on the second" in run("cat slash2"), "`copy disk2/NAME disk1 NEWNAME` works", "")
            check("Copied only2" in run("copy @backup/only2 disk1/slash3"), "`copy @LABEL/NAME` works too", "")
            check("Wrote" in run("write disk2/slashw hello") and "slashw" in run("ls disk2") and "slashw" not in run("ls"), "`write disk2/NAME` writes to that disk only", "")
            check("Removed slashw" in run("rm disk2/slashw"), "`rm disk2/NAME` removes it", "")
            check("no such file" in run("cat disk1/missing"), "a missing file in disk1/NAME is reported", "")
            check("no volume carries" in run("cat @nothere/x"), "an unknown label in @LABEL/NAME is reported", "")
            big = run("ls")
            size = [line.split()[1] for line in big.splitlines() if line.startswith("router")][0]
            check(int(size) > 65536, "(the router tape is larger than the shell's 64 KiB file buffer)", big)
            check("Copied router" in run("copy disk1 router @backup"), "a file larger than 64 KiB copies too", "")
            check(f"router" in run("ls @backup") and size in run("ls @backup"), "and arrives with the same length", "")
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
