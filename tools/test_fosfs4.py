#!/usr/bin/env python3
"""FOSFS v4 (docs/fosfs-v4.md): `format diskN v4` makes a volume with 128-byte entries, a directory sized from
the volume, names of 48 characters in either case compared case-sensitively, and many more than 64 files; a v3
volume next to it keeps its 16 lowercase characters. A host-side parse of the image then agrees with the guest."""

import os
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import (BROADCAST, CODE_CANDIDATES, IMAGE, ROOT, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)

SECTOR = 512
LONG_NAME = "Mixed.Case_Name-with-forty-eight-characters.txt1"[:48]
FILES = 150


def v4_files(volume: bytes) -> dict:
    """The files of a FOSFS v4 volume (the partition's bytes): name -> contents."""
    assert volume[:8] == b"FOSFS4\0\0", volume[:8]
    version, entry_size, one, directory_sectors, data_start, recorded = struct.unpack_from("<HHIIQQ", volume, 8)
    assert (version, entry_size, one) == (4, 128, 1) and data_start == 1 + directory_sectors
    found = {}
    for at in range(SECTOR, data_start * SECTOR, entry_size):
        entry = volume[at:at + entry_size]
        if entry[0] != 1:
            continue
        start, length, _ = struct.unpack_from("<QQQ", entry, 4)
        found[entry[28:28 + entry[1]].decode()] = volume[start * SECTOR:start * SECTOR + length]
    return found


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        boot = tmp / "boot.img"
        shutil.copy(IMAGE, boot)
        four, three = tmp / "four.img", tmp / "three.img"
        four.write_bytes(bytes(64 << 20))
        three.write_bytes(bytes(64 << 20))
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-device", "ich9-ahci,id=ahci"]
        for number, (path, boot_flag) in enumerate(((boot, ",bootindex=0"), (four, ""), (three, ""))):
            command += ["-drive", f"if=none,id=d{number},format=raw,file={path}", "-device", f"ide-hd,drive=d{number},bus=ahci.{number}{boot_flag}"]
        command += ["-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:7f"]
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
                return wire.until("fos> ", 30)

            # disk1 boot, disk2 v4, disk3 v3
            run("format disk2 vol4 v4")
            done = run("format disk2 confirm")
            check("FOSFS v4" in done, "`format diskN v4` makes a v4 volume", done)
            run("format disk3 vol3")
            check("FOSFS v3" in run("format disk3 confirm"), "and without `v4` it stays v3", "")

            run(f"write disk2 {LONG_NAME} long name")
            check("long name" in run(f"cat disk2 {LONG_NAME}"), "a 48-character mixed-case name works on v4", "")
            run("write disk2 Readme upper")
            run("write disk2 readme lower")
            check("upper" in run("cat disk2 Readme") and "lower" in run("cat disk2 readme"), "names differ by case on v4", "")
            refused = run("write disk3 Readme x")
            check("upper" not in run("cat disk3 Readme"), "a v3 volume still refuses an uppercase name", refused)
            for number in range(FILES):
                run(f"write disk2 file{number:03d} content {number}")
            listing = run("ls disk2")
            check("file149" in listing and "file000" in listing, f"{FILES} files fit (v3 holds 64)", listing[-400:])
            check("content 77" in run("cat disk2 file077"), "and read back", "")
            run("rm disk2 file010")
            check("file010" not in run("ls disk2"), "a file can be removed", "")
            fsck = run("fsck disk2")
            check("FOSFS v4" in fsck and "directory consistent" in fsck, "fsck reports a consistent v4 directory", fsck[-400:])
            check("FOSFS v3" in run("fsck disk3"), "and v3 for the other volume", "")
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

        image = four.read_bytes()
        # The guest made a GPT with one FOSFS partition; find it from the entry array.
        first = struct.unpack_from("<Q", image, 2 * SECTOR + 32)[0]
        found = v4_files(image[first * SECTOR:])
        check(found.get(LONG_NAME) == b"long name" and found.get("Readme") == b"upper" and found.get("readme") == b"lower",
              "the host parse of the image finds the long and case-different names", repr(sorted(found)[:5]))
        check(len(found) == FILES + 3 - 1 and found.get("file149") == b"content 149" and "file010" not in found,
              "and every file, with the removed one gone", str(len(found)))
    print("fosfs4: all checks passed")


if __name__ == "__main__":
    main()
