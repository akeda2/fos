#!/usr/bin/env python3
"""A file as an AoE export on the server kernel (`make test-mf-aoe-file`, docs/block-sharing.md "A file as an export").
The host is the initiator over the QEMU multicast-socket network, as in test_mf_aoe.py.

- A FOSFS file on the boot volume (1 MiB and 300 bytes): exported read-only by its bare name, it takes slot 100; IDENTIFY
  says 2048 sectors (the partial last sector is left out), reads match the host's file, a write is aborted.
- An ext4 file (mkfs.ext4 -d) exported `rw confirm` as slot 7: reads match, a write lands in the file. A tape
  (`fscheck vfs`) read the start before the write, so the kept mount had it cached; it reads the new data after, and the
  guest's `sha256` equals the host's digest of the changed contents.
- A sparse ext4 file: `rw` is refused because of its holes; read-only, the holes read as zeros.
- The guard: a FOSFS file on a fresh volume is exported, removed here, and a new file written; first fit puts it on the
  exported file's sectors, so the write is refused, the export still serves the old data and `aoe` counts the refusal.
QEMU only (AHCI, e1000)."""
import hashlib
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_mf_aoe import ROOT, Guest, Initiator, ata_status  # noqa: E402

failures = []
FRESH = b"written by the initiator over the old start of the file. "


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}: {detail}")


def build_image(tmp, files):
    settings = tmp / "settings"
    settings.write_text("aoe-shelf 5\n")
    out = tmp / "boot.img"
    extra = [arg for name, path in files for arg in ("--file", f"{name}={path}")]
    subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", os.environ["LOADER_EFI"], "--kernel", os.environ["KERNEL_MF_ELF"],
                    "--output", str(out), "--bios-stage1", os.environ["BIOS_STAGE1"], "--bios-stage2", os.environ["BIOS_STAGE2"],
                    "--file", f"settings={settings}", *extra], check=True, capture_output=True)
    return out


def sectors_of(host, slot):
    reply = host.ata(slot, 0xEC)
    if not reply or len(reply) < 36 + 512 or ata_status(reply) != (0x50, 0):
        return None
    words = struct.unpack("<256H", reply[36:36 + 512])
    return words[100] | words[101] << 16 | words[102] << 32


def read(host, slot, lba, count=2):
    """`count` sectors from `lba`, two a frame (MTU 1500); None if any frame failed."""
    data = b""
    for at in range(lba, lba + count, 2):
        step = min(2, lba + count - at)
        reply = host.ata(slot, 0x24, at, step)
        if reply is None or ata_status(reply) != (0x50, 0):
            return None
        data += reply[36:36 + step * 512]
    return data


def main():
    mkfs = shutil.which("mkfs.ext4") or "/usr/sbin/mkfs.ext4"
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image_data = os.urandom((1 << 20) + 300)
        (tmp / "img.bin").write_bytes(image_data)
        boot = build_image(tmp, [("img.bin", tmp / "img.bin"), ("fscheck", ROOT / "build/fosfor/tapes/fscheck")])

        have_ext = Path(mkfs).exists()
        ext = tmp / "ext.img"
        disk_data = b"\xff" * 64 + os.urandom((2 << 20) - 64)
        sparse_head, sparse_tail = os.urandom(4096), os.urandom(4096)
        if have_ext:
            (tmp / "src").mkdir()
            (tmp / "src" / "disk.img").write_bytes(disk_data)
            with open(tmp / "src" / "sparse.img", "wb") as sparse:
                sparse.write(sparse_head)
                sparse.seek(1 << 20)
                sparse.write(sparse_tail)
            ext.write_bytes(bytes(32 << 20))
            subprocess.run([mkfs, "-q", "-F", "-L", "fileexp", "-d", str(tmp / "src"), str(ext)], check=True, capture_output=True)
        else:
            ext.write_bytes(bytes(8 << 20))
            print("SKIP: no mkfs.ext4, so no ext4 checks")
        blank = tmp / "blank.img"
        blank.write_bytes(bytes(16 << 20))

        guest = Guest(boot, [ext, blank])
        host = Initiator()
        try:
            check(guest.wait_for("fos> ", 150), "the guest booted to the prompt", guest.log[-800:].decode("utf8", "replace"))

            # --- a FOSFS file on the boot volume, read-only ---
            ok, text = guest.command("aoe export img.bin", "aoe:")
            check(ok and "exported read-only as shelf 5 slot 100" in text, "`aoe export img.bin` takes slot 100, read-only", text)
            check("The last 300 byte(s)" in text, "and says the partial last sector is not served", text)
            check(sectors_of(host, 100) == 2048, "IDENTIFY gives the file's 2048 whole sectors", sectors_of(host, 100))
            good = all(read(host, 100, lba) == image_data[lba * 512:(lba + 2) * 512] for lba in (0, 2, 1000, 2046))
            check(good, "reads at the start, middle and end match the file")
            reply = host.ata(100, 0x24, 2048, 1)
            check(reply is not None and ata_status(reply) == (0x51, 4), "a read past the last whole sector is aborted", reply)
            reply = host.ata(100, 0x34, 10, 1, bytes(512), write=True)
            check(reply is not None and ata_status(reply) == (0x51, 4), "a write to the read-only file export is aborted", reply)
            ok, text = guest.command("sha256 img.bin", "img.bin")
            check(ok and hashlib.sha256(image_data).hexdigest() in text, "and the file is unchanged", text)

            # --- an ext4 file, rw ---
            if have_ext:
                ok, text = guest.command("aoe export disk2/disk.img slot 7 rw", "confirm")
                check(ok and "READ-WRITE" not in text, "`rw` without `confirm` is refused", text)
                ok, text = guest.command("aoe export disk2/disk.img slot 7 rw confirm", "aoe:")
                check(ok and "READ-WRITE as shelf 5 slot 7" in text, "an ext4 file is exported rw as slot 7", text)
                check(sectors_of(host, 7) == 4096, "IDENTIFY gives its 4096 sectors", sectors_of(host, 7))
                good = all(read(host, 7, lba) == disk_data[lba * 512:(lba + 2) * 512] for lba in (0, 8, 2000, 4094))
                check(good, "reads match the ext4 file")
                ok, text = guest.command("sha256 @fileexp/disk.img", "  @fileexp/disk.img", 60)
                check(ok and hashlib.sha256(disk_data).hexdigest() in text, "the guest's sha256 of it before the write", text)
                # A tape's vfs_read keeps the ext volume mounted with its read cache (the shell remounts every time): read
                # the start once so the old block is cached, then write it over AoE.
                ok, text = guest.command("run fscheck vfs @fileexp/disk.img", "fscheck: read", 60)
                check(ok and "read 40 bytes [?]" in text, "a tape reads the start of the file (binary, shown as ?)", text)
                payload = FRESH + os.urandom(1024 - len(FRESH))
                reply = host.ata(7, 0x34, 0, 2, payload, write=True)
                check(reply is not None and ata_status(reply) == (0x50, 0), "a two-sector write is accepted", reply)
                reply = host.ata(7, 0xEA)
                check(reply is not None and ata_status(reply) == (0x50, 0), "FLUSH CACHE EXT is accepted", reply)
                check(read(host, 7, 0) == payload, "it reads back through AoE")
                changed = payload + disk_data[2 * 512:]
                ok, text = guest.command("run fscheck vfs @fileexp/disk.img", "fscheck: read", 60)
                check(ok and f"read 40 bytes [{FRESH[:40].decode()}]" in text, "the tape then reads the new data (no stale kept mount)", text)
                ok, text = guest.command("sha256 @fileexp/disk.img", "  @fileexp/disk.img", 60)
                check(ok and hashlib.sha256(changed).hexdigest() in text, "and the file holds the write (sha256)", text)
                # A new export in the table entry the rw export used starts with its own counters (they once carried over).
                ok, text = guest.command("aoe unexport 7", "no longer exported")
                ok, text = guest.command("aoe export disk2/disk.img slot 7", "aoe:")
                check(ok and "read-only as shelf 5 slot 7" in text, "re-exported read-only as slot 7", text)
                ok, text = guest.command("aoe", "file disk2/disk.img")
                section = text.split("file disk2/disk.img", 1)[-1].split("  file ", 1)[0]
                check(ok and "0 writes, 0 errors" in section and "disk commands: 0 writes, 0 frames gathered" in section,
                      "and its counters start at 0, not at the rw export's", section)
                ok, text = guest.command("aoe export 2 rw confirm", "aoe:")
                check(ok and "cannot be written as well" in text, "disk 2 cannot also be exported rw whole", text)

                ok, text = guest.command("aoe export disk2/sparse.img rw confirm", "aoe:")
                check(ok and "holes" in text and "slot" not in text.split("aoe:")[1].split("\n")[0], "`rw` of a sparse file is refused", text)
                ok, text = guest.command("aoe export disk2/sparse.img", "aoe:")
                check(ok and "read-only as shelf 5 slot 100" not in text and "slot 101" in text and "are holes" in text,
                      "read-only it is exported as slot 101 and says how much is holes", text)
                head, middle, tail = read(host, 101, 0, 8), read(host, 101, 1000), read(host, 101, 2048, 8)
                check(head == sparse_head and middle == bytes(1024) and tail == sparse_tail, "the hole reads as zeros between the data",
                      (head == sparse_head, middle == bytes(1024), tail == sparse_tail))

            # --- the guard: removing an exported file and writing over its sectors ---
            guest.command("format disk3 guard v4", "confirm")
            ok, text = guest.command("format disk3 confirm", "FOSFS")
            check(ok, "a fresh FOSFS volume on disk 3", text)
            ok, text = guest.command("cp img.bin disk3/a.bin", "Copied", 60)
            check(ok, "copy the test file onto it", text)
            ok, text = guest.command("aoe export disk3/a.bin slot 9", "aoe:")
            check(ok and "slot 9" in text, "export it as slot 9", text)
            ok, text = guest.command("rm disk3/a.bin", "fos>")
            ok, text = guest.command("write disk3/b.txt first fit lands on the old sectors", "fos>")
            check("refused a write to disk 3" in text, "the new file's write onto the exported sectors is refused", text)
            check(read(host, 9, 0) == image_data[:1024], "the export still serves the old data")
            ok, text = guest.command("aoe", "other write(s) to its sectors refused")
            check(ok, "`aoe` counts the refused write", text)
            ok, text = guest.command("aoe unexport 9", "no longer exported")
            check(ok, "aoe unexport 9", text)
            ok, text = guest.command("write disk3/c.txt now it may", "fos>")
            ok, text = guest.command("cat disk3/c.txt", "now it may")
            check(ok, "after unexport the sectors take writes again", text)

            ok, text = guest.command("aoe", "file img.bin")
            check(ok and "slot 100" in text, "`aoe` lists the file exports", text)
            ok, text = guest.command("aoe unexport 100", "no longer exported")
            check(ok and read(host, 100, 0) is None, "an unexported file no longer answers", text)
        finally:
            guest.kill()
    if failures:
        print(f"{len(failures)} check(s) failed:")
        for failure in failures:
            print("  " + str(failure)[:400])
        return 1
    print("PASS: mf-aoe-file")
    return 0


if __name__ == "__main__":
    sys.exit(main())
