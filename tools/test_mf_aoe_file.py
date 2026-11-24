#!/usr/bin/env python3
"""A file as an AoE export on the server kernel (`make test-mf-aoe-file`, docs/block-sharing.md "A file as an export").
The host is the initiator over the QEMU multicast-socket network, as in test_mf_aoe.py.

- A whole disk by its volume's label: `aoe-export @fileexp slot 30` in settings exports disk 2 at boot; the same disk is
  refused a second slot (by label or number), an unknown label is refused, and `aoe export @fileexp slot 31` works.
- One writer, from settings: `aoe-export @fileexp/disk.img slot 8 rw for HOST_MAC` exports the ext4 file at boot; a
  write from the host's MAC lands, one from another MAC is aborted and counted, reads from it are served. A settings
  `rw` without `for MAC` is ignored (nobody is there to confirm it).
- A FOSFS file on the boot volume (1 MiB and 300 bytes): exported read-only by its bare name, it takes slot 100; IDENTIFY
  says 2048 sectors (the partial last sector is left out), reads match the host's file, a write is aborted.
- An ext4 file (mkfs.ext4 -d) exported `rw confirm` as slot 7: reads match, a write lands in the file. A tape
  (`fscheck vfs`) read the start before the write, so the kept mount had it cached; it reads the new data after, and the
  guest's `sha256` equals the host's digest of the changed contents.
- A sparse ext4 file: `rw` is refused because of its holes; read-only, the holes read as zeros.
- `mkimage`: an 8 MiB image on the ext volume and a 4 MiB one on the boot volume (FOSFS), written in the background;
  each exported `rw for` the host without holes (IDENTIFY gives its size, it reads as zeros, a write lands), the FOSFS
  one in one run; an existing name, a size that is not whole MiB, one above the free space and a volume whose disk is
  exported rw whole are refused.
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
from test_mf_aoe import HOST_MAC, ROOT, Guest, Initiator, ata_status  # noqa: E402

failures = []
FRESH = b"written by the initiator over the old start of the file. "


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}: {detail}")


def build_image(tmp, files, settings_text="aoe-shelf 5\n"):
    settings = tmp / "settings"
    settings.write_text(settings_text)
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
        have_ext = Path(mkfs).exists()
        # A whole disk named by its volume's label, with a slot of its own (disk numbers can change between boots).
        host_mac = ":".join(f"{byte:02x}" for byte in HOST_MAC)
        settings_text = "aoe-shelf 5\naoe-export img.bin slot 40 rw\n"
        if have_ext:
            settings_text += f"aoe-export @fileexp slot 30\naoe-export @fileexp/disk.img slot 8 rw for {host_mac}\n"
        boot = build_image(tmp, [("img.bin", tmp / "img.bin"), ("fscheck", ROOT / "build/fosfor/tapes/fscheck")], settings_text)

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
            boot_log = guest.log.decode("utf8", "replace")

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

            # --- a whole disk by its volume's label ---
            if have_ext:
                check("aoe: @fileexp (disk 2) exported read-only as shelf 5 slot 30" in boot_log, "`aoe-export @fileexp slot 30` in settings exports disk 2 at boot")
                check(sectors_of(host, 30) == 65536, "slot 30 is the whole 32 MiB disk", sectors_of(host, 30))
                ok, text = guest.command("aoe export @fileexp", "aoe:")
                check(ok and "exported already" in text, "the same disk is not exported twice by label", text)
                ok, text = guest.command("aoe export 2 slot 31", "aoe:")
                check(ok and "exported already" in text, "nor by number under another slot", text)
                ok, text = guest.command("aoe export @nosuchlabel", "aoe:")
                check(ok and "no volume is labelled @nosuchlabel" in text, "an unknown label is refused", text)
                guest.command("aoe unexport 30", "no longer exported")
                ok, text = guest.command("aoe export @fileexp slot 31", "aoe:")
                check(ok and "@fileexp (disk 2) exported read-only as shelf 5 slot 31" in text, "`aoe export @fileexp slot 31` from the shell", text)
                check(sectors_of(host, 31) == 65536 and sectors_of(host, 30) is None, "slot 31 answers, slot 30 no longer does")
                guest.command("aoe unexport 31", "no longer exported")

            check("`aoe-export img.bin slot 40 rw` ignored" in boot_log and "needs `for MAC`" in boot_log,
                  "a settings `rw` without `for MAC` is ignored", boot_log[-1500:])
            check(sectors_of(host, 40) is None, "and nothing answers as slot 40")

            # --- one writer, from settings ---
            if have_ext:
                check("@fileexp/disk.img exported READ-WRITE as shelf 5 slot 8" in boot_log and f"Only {host_mac} may write it" in boot_log,
                      "`aoe-export @fileexp/disk.img slot 8 rw for MAC` exports the file at boot, one writer", boot_log[-1500:])
                other = bytes.fromhex("525400aabbcd")
                original = read(host, 8, 100)
                reply = host.ata(8, 0x34, 100, 2, bytes(1024), write=True, source=other)
                check(reply is not None and ata_status(reply) == (0x51, 4), "a write from another MAC is aborted", reply)
                check(read(host, 8, 100) == original, "and changed nothing")
                reply = host.ata(8, 0x34, 100, 2, bytes(1024), write=True)
                check(reply is not None and ata_status(reply) == (0x50, 0), "a write from the writer's MAC is accepted", reply)
                check(read(host, 8, 100) == bytes(1024), "and reads back")
                reply = host.ata(8, 0x34, 100, 2, original, write=True)
                check(reply is not None and ata_status(reply) == (0x50, 0) and read(host, 8, 100) == original,
                      "the old sectors are written back")
                ok, text = guest.command("aoe", "file @fileexp/disk.img")
                check(ok and f"only {host_mac} writes; 1 write(s) from others refused" in text, "`aoe` names the writer and counts the refusal", text)
                ok, text = guest.command("aoe unexport 8", "no longer exported")

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

            # --- mkimage: zero-filled images, exportable rw ---
            images = [("zero.img", 4, 13)] + ([("@fileexp/new.img", 8, 12)] if have_ext else [])
            for reference, mib, slot in images:
                ok, text = guest.command(f"mkimage {reference} {mib}M", "mkimage:")
                check(ok and "in the background" in text, f"`mkimage {reference} {mib}M` starts", text)
                ok = guest.wait_for(f"mkimage: {reference} done: {mib} MiB", 60)
                check(ok, "and finishes", guest.log[-600:].decode("utf8", "replace"))
                ok, text = guest.command(f"aoe export {reference} slot {slot} rw for {host_mac}", "aoe:")
                check(ok and f"READ-WRITE as shelf 5 slot {slot}" in text and "holes" not in text, "it is exported rw (no holes)", text)
                if not reference.startswith("@"):
                    check("in 1 run(s)" in text, "a FOSFS image is one run", text)
                else:
                    print("        ext image: " + next((line.strip() for line in text.splitlines() if "run(s)" in line), "?"))
                check(sectors_of(host, slot) == mib * 2048, f"IDENTIFY gives {mib * 2048} sectors", sectors_of(host, slot))
                check(read(host, slot, 0) == bytes(1024) and read(host, slot, mib * 2048 - 2) == bytes(1024), "it reads as zeros")
                reply = host.ata(slot, 0x34, 50, 2, FRESH + bytes(1024 - len(FRESH)), write=True)
                check(reply is not None and ata_status(reply) == (0x50, 0) and read(host, slot, 50)[:len(FRESH)] == FRESH,
                      "a write lands and reads back", reply)
                guest.command(f"aoe unexport {slot}", "no longer exported")
            ok, text = guest.command("mkimage zero.img 4M", "mkimage:")
            check(ok and "exists already" in text, "mkimage refuses an existing name", text)
            ok, text = guest.command("mkimage other.img 1500K", "mkimage:")
            check(ok and "whole MiB" in text, "and a size that is not whole MiB", text)
            if have_ext:
                ok, text = guest.command("mkimage @fileexp/huge.img 1G", "mkimage:")
                check(ok and "less free space" in text, "and one above the free space", text)
                # The file exports on disk 2 (slots 7 and 101) would keep it from being exported rw whole.
                guest.command("aoe unexport 7", "no longer exported")
                guest.command("aoe unexport 101", "no longer exported")
                ok, text = guest.command(f"aoe export @fileexp slot 30 rw for {host_mac}", "aoe:")
                check(ok and "READ-WRITE as shelf 5 slot 30" in text, "disk 2 exported rw whole", text)
                ok, text = guest.command("mkimage @fileexp/other.img 1M", "mkimage:")
                check(ok and "exported rw as a whole" in text, "and a volume whose disk is exported rw whole (two writers)", text)
                guest.command("aoe unexport 30", "no longer exported")

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
