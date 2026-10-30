#!/usr/bin/env python3
"""FOSFS v4 (docs/fosfs-v4.md): `format diskN v4` makes a volume with 128-byte entries, a directory sized from
the volume, names of 48 characters in either case compared case-sensitively, folders, a CRC on every entry, a
backup superblock, timestamps, and files in up to three extents when the free space is fragmented; a v3 volume next
to it keeps its 16 lowercase characters. A host-side parse of the image then agrees with the guest, and a second boot
over a damaged disk (one entry's CRC broken, sector 0 wiped) still mounts it from the backup superblock."""

import os
import re
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import (BROADCAST, CODE_CANDIDATES, IMAGE, ROOT, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)

SECTOR = 512
LONG_NAME = "Mixed.Case_Name-with-forty-eight-characters.txt1"[:48]
FILES = 150
GUEST_MAC = "52:54:00:12:34:7f"
BLOCK = 1000 * SECTOR  # the files that fill the small disk
FOSCOPY = ROOT / "tools/foscopy.py"


def partition_start(image: bytes) -> int:
    """The first sector of the first GPT partition (the guest's `format` makes exactly one)."""
    return struct.unpack_from("<Q", image, 2 * SECTOR + 32)[0]


def v4_files(volume: bytes):
    """The files of a FOSFS v4 volume (the partition's bytes): (name -> contents, name -> (extents, mtime); key
    "damaged" counts entries skipped for a wrong CRC32). Reads the superblock from sector 0, or from the backup in
    sector 1 when sector 0 is not a superblock."""
    head = volume[:SECTOR] if volume[:8] == b"FOSFS4\0\0" else volume[SECTOR:2 * SECTOR]
    assert head[:8] == b"FOSFS4\0\0", head[:8]
    version, entry_size, one, directory_sectors, data_start, recorded = struct.unpack_from("<HHIIQQ", head, 8)
    assert (version, entry_size, one) == (4, 128, 1) and data_start == 2 + directory_sectors
    slots, damaged = {}, 0
    for number, at in enumerate(range(2 * SECTOR, data_start * SECTOR, entry_size), 1):
        entry = bytearray(volume[at:at + entry_size])
        if entry[0] not in (1, 2):
            continue
        crc = struct.unpack_from("<I", entry, 4)[0]
        entry[4:8] = bytes(4)
        if zlib.crc32(bytes(entry)) != crc:
            damaged += 1
            continue
        slots[number] = (entry[0], entry[40:40 + entry[1]].decode(), struct.unpack_from("<I", entry, 24)[0], entry)

    def path(number):
        _, name, parent, _ = slots[number]
        return name if parent == 0 else path(parent) + "/" + name

    found, meta = {}, {"damaged": damaged}
    for number, (kind, _, _, entry) in slots.items():
        if kind == 2:
            found[path(number) + "/"] = None
            continue
        length, sectors = struct.unpack_from("<QQ", entry, 8)
        mtime = struct.unpack_from("<Q", entry, 32)[0]
        data = b""
        for n in range(entry[2]):
            start, count = struct.unpack_from("<QI", entry, 88 + 12 * n)
            data += volume[start * SECTOR:(start + count) * SECTOR]
        found[path(number)] = data[:length]
        meta[path(number)] = (entry[2], mtime)
    return found, meta


def boot(tmp, disks, scenario):
    """Boots the image with `disks` after it and runs `scenario(run)`."""
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    boot_image = tmp / "boot.img"
    shutil.copy(IMAGE, boot_image)
    vars_copy = tmp / "vars.fd"
    shutil.copy(variables, vars_copy)
    command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
               "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
               "-device", "ich9-ahci,id=ahci"]
    for number, (path, boot_flag) in enumerate([(boot_image, ",bootindex=0")] + [(path, "") for path in disks]):
        command += ["-drive", f"if=none,id=d{number},format=raw,file={path}", "-device", f"ide-hd,drive=d{number},bus=ahci.{number}{boot_flag}"]
    command += ["-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", f"e1000,netdev=net0,mac={GUEST_MAC}"]
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

        scenario(run)
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


def foscopy(args, cwd):
    return subprocess.run([sys.executable, str(FOSCOPY), "--mcast", f"{MCAST_GROUP}:{MCAST_PORT}", "--delay", "0.002", "--timeout", "15",
                           "--mac", GUEST_MAC, *args], capture_output=True, text=True, cwd=cwd, timeout=180)


def first_scenario(work):
    contents = {name: os.urandom(BLOCK) for name in "abcdefgh"}
    big = os.urandom(2500 * SECTOR)

    def scenario(run):
        # disk1 boot, disk2 v4, disk3 v3, disk4 a small disk for the fragmentation case
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
        check(re.search(r"file000.*20\d\d-\d\d-\d\d \d\d:\d\d:\d\d", listing) is not None, "ls shows a v4 file's modification time", listing[:400])
        check("content 77" in run("cat disk2 file077"), "and read back", "")
        run("rm disk2 file010")
        check("file010" not in run("ls disk2"), "a file can be removed", "")
        fsck = run("fsck disk2")
        check("FOSFS v4" in fsck and "directory consistent" in fsck, "fsck reports a consistent v4 directory", fsck[-400:])
        check("FOSFS v3" in run("fsck disk3"), "and v3 for the other volume", "")
        check("only FOSFS v4" in run("mkdir disk3 bin"), "a v3 volume has no folders", "")
        run("mkdir disk2 bin")
        run("mkdir disk2 bin/tools")
        run("mkdir disk2 doc")
        check("missing" in run("mkdir disk2 nope/sub"), "a folder needs its parent", "")
        run("write disk2 bin/tools/phono deep file")
        run("write disk2 bin/phono tape")
        run("write disk2 phono root")
        check("deep file" in run("cat disk2 bin/tools/phono") and "tape" in run("cat disk2 bin/phono") and "root" in run("cat disk2 phono"),
              "the same name lives in the root and in two folders", "")
        top, bin_ = run("ls disk2"), run("ls disk2/bin")
        check("<dir>" in top and "doc" in top and "tools" not in top, "ls shows folders at the root without their contents", top[-300:])
        check("tools" in bin_ and "phono" in bin_ and "doc" not in bin_, "ls disk2/bin lists just that folder", bin_)
        check("is empty" in run("ls disk2/doc"), "an empty folder exists", "")
        check("no such" in run("ls disk2/nothere").lower() or "not" in run("ls disk2/nothere").lower(), "ls of a missing folder says so", "")
        check("must be empty" in run("rm disk2 bin"), "a folder with contents is not removed", "")
        run("rm disk2 bin/tools/phono")
        run("rm disk2 bin/tools")
        check("tools" not in run("ls disk2/bin"), "an emptied folder can be removed", "")
        run("mkdir disk2 bin/tools")
        run("write disk2 bin/tools/phono deep file")
        fsck = run("fsck disk2")
        check("directory consistent" in fsck, "fsck is happy with folders", fsck[-400:])

        # Fragmentation: eight files of 1000 sectors fill the 4 MiB disk, three alternate ones are removed, and a file
        # of 2500 sectors, bigger than any one hole, must still fit by taking the three holes.
        run("format disk4 small v4")
        run("format disk4 confirm")
        stored = []
        for name in "abcdefgh":
            (work / name).write_bytes(contents[name])
            result = foscopy([name, f":disk4/{name}"], work)
            if result.returncode != 0:
                check("full" in result.stderr + result.stdout, "the small disk fills up and says so", result.stderr[-400:])
                break
            stored.append(name)
        check(len(stored) >= 6, f"{len(stored)} files of 1000 sectors fit the small v4 disk", "")
        for name in stored[1:6:2]:
            run(f"rm disk4 {name}")
        (work / "big").write_bytes(big)
        result = foscopy(["big", ":disk4/big"], work)
        check(result.returncode == 0, "a file bigger than any free run is stored in several extents", result.stderr[-400:] + result.stdout[-400:])
        result = foscopy([":disk4/big", "big.back"], work)
        check(result.returncode == 0 and (work / "big.back").read_bytes() == big, "and reads back byte for byte", result.stderr[-400:])
        fsck = run("fsck disk4")
        check("3 extents" in fsck and "directory consistent" in fsck, "fsck names the extents and finds nothing wrong", fsck[-500:])
        for name in stored[0:6:2]:
            result = foscopy([f":disk4/{name}", f"{name}.back"], work)
            check(result.returncode == 0 and (work / f"{name}.back").read_bytes() == contents[name], f"the files around it ({name}) are untouched", result.stderr[-300:])

    return scenario


def second_scenario(run):
    listing = run("ls disk2")
    check("file149" in listing, "after damage: the volume with a wiped sector 0 still mounts from the backup superblock", listing[-300:])
    check("file000" not in listing, "and the entry with a broken CRC is not listed", listing[:300])
    fsck = run("fsck disk2")
    check("damaged" in fsck and "sector 1" in fsck, "fsck names the damaged entry and the superblock fallback", fsck[-500:])
    run("write disk2 after.txt written after the fallback")
    check("written after" in run("cat disk2 after.txt"), "the fallback volume can still be written", "")
    run("label disk2 keep")
    check("keep" in run("label disk2"), "relabelling writes a superblock that reads back", "")


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        work = tmp / "work"
        work.mkdir()
        four, three, small = tmp / "four.img", tmp / "three.img", tmp / "small.img"
        four.write_bytes(bytes(64 << 20))
        three.write_bytes(bytes(64 << 20))
        small.write_bytes(bytes(4 << 20))
        boot(tmp, [four, three, small], first_scenario(work))

        image = four.read_bytes()
        found, meta = v4_files(image[partition_start(image) * SECTOR:])
        check(found.get(LONG_NAME) == b"long name" and found.get("Readme") == b"upper" and found.get("readme") == b"lower",
              "the host parse of the image finds the long and case-different names", repr(sorted(found)[:5]))
        check(found.get("file149") == b"content 149" and "file010" not in found,
              "and every file, with the removed one gone", str(len(found)))
        check(found.get("bin/tools/phono") == b"deep file" and found.get("bin/phono") == b"tape" and found.get("phono") == b"root"
              and "bin/" in found and "doc/" in found and "bin/tools/" in found, "the host parse resolves folders through the parent field", repr(sorted(k for k in found if "/" in k or k == "phono")))
        check(meta["damaged"] == 0 and meta["file000"][1] > 1_700_000_000, "every entry's CRC is right and files carry a time (Unix seconds)", repr(meta["file000"]))

        image = small.read_bytes()
        found, meta = v4_files(image[partition_start(image) * SECTOR:])
        check(meta["damaged"] == 0 and meta["big"][0] == 3 and found["big"] == (work / "big").read_bytes(),
              "the host parse reads the three-extent file and its CRC", repr(meta.get("big")))

        # Damage: break one entry's CRC (change its length) and wipe sector 0 of the same volume.
        image = bytearray(four.read_bytes())
        start = partition_start(image)
        for number in range(256):
            at = (start + 2) * SECTOR + number * 128
            if image[at + 40:at + 47] == b"file000":
                image[at + 8] ^= 1
                break
        else:
            raise SystemExit("FAIL: file000 not found in the directory")
        image[start * SECTOR:(start + 1) * SECTOR] = bytes(SECTOR)
        four.write_bytes(image)
        boot(tmp, [four, three, small], second_scenario)
    print("fosfs4: all checks passed")


if __name__ == "__main__":
    main()
