#!/usr/bin/env python3
"""`format` in the v2 shell.

Four extra disks next to the boot disk: one with a labelled FOSFS volume (a second image,
other partitions included), one blank, one holding junk where a partition table would be, and
one with a GPT that lists only a Linux partition. Checks:
 * the boot disk is refused;
 * `format diskN` only announces (nothing changes), a `confirm` after any other command is
   refused, and `format diskN confirm` does the work;
 * a disk with a FOSFS partition has just that volume erased, relabelled on request, and its
   other partitions are byte-for-byte untouched;
 * a blank disk is partitioned and formatted, and a host-side parse finds a valid GPT (both
   header CRCs, both entry-array CRCs) with a FOSFS partition and an empty volume;
 * disks with foreign contents are refused without `force`, accepted with it."""

import os
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import uuid
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import (BROADCAST, CODE_CANDIDATES, IMAGE, ROOT, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)
from test_stick_guids import gpt_problems  # noqa: E402

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fos-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fos-v2-kernel"
SECTOR = 512
LINUX_TYPE = bytes.fromhex("af3dc60f838472478e793d69d8477de4")  # 0fc63daf-8483-4772-8e79-3d69d8477de4, on-disk order


def linux_only_disk(size: int) -> bytes:
    """A disk whose GPT lists one Linux partition and nothing else."""
    image = bytearray(size)
    total = size // SECTOR
    entries = bytearray(128 * 128)
    entries[0:16] = LINUX_TYPE
    entries[16:32] = bytes(range(1, 17))
    struct.pack_into("<QQ", entries, 32, 2048, total - 34)

    def header(current, backup, entries_lba):
        sector = bytearray(SECTOR)
        sector[0:8] = b"EFI PART"
        struct.pack_into("<IIIIQQQQ", sector, 8, 0x10000, 92, 0, 0, current, backup, 34, total - 34)
        sector[56:72] = bytes(range(32, 48))
        struct.pack_into("<QIII", sector, 72, entries_lba, 128, 128, zlib.crc32(bytes(entries)) & 0xFFFFFFFF)
        struct.pack_into("<I", sector, 16, zlib.crc32(bytes(sector[:92])) & 0xFFFFFFFF)
        return sector

    image[SECTOR:2 * SECTOR] = header(1, total - 1, 2)
    image[2 * SECTOR:2 * SECTOR + len(entries)] = entries
    image[(total - 33) * SECTOR:(total - 33) * SECTOR + len(entries)] = entries
    image[(total - 1) * SECTOR:] = header(total - 1, 1, total - 33)
    return bytes(image)


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        boot = tmp / "boot.img"
        shutil.copy(IMAGE, boot)
        marker = tmp / "only2"
        marker.write_text("on the second disk\n")
        second = tmp / "second.img"
        subprocess.run([sys.executable, str(ROOT / "v2/tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
                        "--output", str(second), "--file", f"only2={marker}", "--label", "backup",
                        "--payload-guid", "f05b2a00-0000-4f05-8000-0000000000aa"], check=True, capture_output=True)
        blank, junk, linux = tmp / "blank.img", tmp / "junk.img", tmp / "linux.img"
        blank.write_bytes(bytes(16 << 20))
        junk.write_bytes(b"\x01junk where a partition table would be" + bytes((16 << 20) - 38))
        linux.write_bytes(linux_only_disk(16 << 20))
        before = second.read_bytes()
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-device", "ich9-ahci,id=ahci"]
        for number, (path, boot_flag) in enumerate(((second, ""), (boot, ",bootindex=0"), (blank, ""), (junk, ""), (linux, ""))):
            command += ["-drive", f"if=none,id=d{number},format=raw,file={path}", "-device", f"ide-hd,drive=d{number},bus=ahci.{number}{boot_flag}"]
        command += ["-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:7e"]
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
                return wire.until("fos> ", 20)

            # disk1 boot, disk2 second (labelled), disk3 blank, disk4 junk, disk5 Linux-only GPT
            out = run("format disk1")
            check("boot disk" in out, "the boot disk is refused", out + clean(bytes(raw))[-1500:])
            check("no such disk" in run("format disk9"), "a disk that does not exist is refused", "")

            plan = run("format disk3")
            check("repartitioned as one FOSFS partition" in plan and "format disk3 confirm" in plan, "`format diskN` announces what it would do", plan)
            check("no FOSFS" in run("ls disk3"), "and changes nothing yet", "")
            run("uptime")
            check("nothing is waiting" in run("format disk3 confirm"), "a confirmation after any other command is refused", "")
            check("nothing is waiting" in run("format disk4 confirm"), "and one for a different disk too", "")
            run("format disk3 scratch")
            done = run("format disk3 confirm")
            check("wrote a new partition table" in done and "formatted as @scratch" in done, "`confirm` partitions and formats the blank disk", done)
            check("FOSFS is empty" in run("ls disk3"), "the new volume is empty and mounts", "")
            run("write disk3 note.txt hello blank disk")
            check("hello blank disk" in run("cat @scratch note.txt"), "and it holds files, found by its label", "")
            check("nothing is waiting" in run("format disk3 confirm"), "a confirmation can be used only once", "")

            refused = run("format disk4")
            check("add `force`" in refused, "junk where a partition table belongs is refused without force", refused)
            check("whatever is in its first sectors" in run("format disk4 force"), "and announced with force", "")
            check("formatted" in run("format disk4 confirm"), "then formatted on confirmation", "")
            check("FOSFS is empty" in run("ls disk4"), "into a usable empty volume", "")

            check("add `force`" in run("format disk5"), "a GPT listing only foreign partitions is refused without force", "")
            run("format disk5 force")
            check("wrote a new partition table" in run("format disk5 confirm"), "and replaced with force", "")

            plan = run("format disk2 fresh")
            check("(32 MiB, 2 file(s)) will be erased; any other partition is left alone" in plan, "a disk with a FOSFS partition announces erasing just that volume", plan)
            run("format disk2 confirm")
            check("FOSFS is empty" in run("ls disk2") and "disk2: @fresh" in run("label"), "which is then empty and relabelled", "")
            wire.send(guest, TAG_DATA, b"shutdown\n")
            for _ in range(100):
                if process.poll() is not None:
                    break
                time.sleep(0.2)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()

        # What the guest left on the disk images, parsed on the host.
        for name, path in (("blank", blank), ("junk", junk), ("linux-only", linux)):
            image = path.read_bytes()
            problems, _ = gpt_problems(image)
            check(not problems, f"the {name} disk now has a valid GPT in both copies (CRCs)", repr(problems))
            check(image[SECTOR * 2:SECTOR * 2 + 16] == uuid.UUID("0f6f7362-0000-4f53-8000-000000000004").bytes_le
                  and not any(image[SECTOR * 2 + 128:SECTOR * 2 + 256]), "with a FOSFS partition as its only entry", image[1024:1040].hex())
        after = second.read_bytes()
        untouched = sum(1 for at in range(0, 54 * 1024 * 1024, 1 << 20) if before[at:at + (1 << 20)] == after[at:at + (1 << 20)])
        check(untouched == 54, "reformatting the data volume left the ESP and payload partitions untouched", f"{untouched}/54 MiB blocks identical")
    print("v2 format: all checks passed")


if __name__ == "__main__":
    main()
