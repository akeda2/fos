#!/usr/bin/env python3
"""`format diskN exfat [LABEL]` (v2): a blank disk becomes one GPT partition of exFAT that `fsck.exfat` accepts, the
shell can use, and a second format over it announces what it will erase. The boot disk is refused.
Run with `make test-format-exfat`."""

import struct
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402

BASIC_DATA = bytes([0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7])


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        blank = tmp / "blank.img"
        blank.write_bytes(bytes(200 << 20))
        extra = ["-device", "ich9-ahci,id=ahci", "-drive", f"if=none,id=f1,format=raw,file={blank}", "-device", "ide-hd,drive=f1,bus=ahci.0"]
        guest = Guest(tmp, "fmtexfat", "52:54:00:12:34:e5", extra=extra)

        def run(line, wait=60):
            mark = guest.send(line)
            guest.wait_for("fos> ", wait, mark + len(line))
            return guest.text()[mark:]

        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            check("boot disk" in run("format disk1 exfat") + run("format disk1 confirm"), "the boot disk is refused", "")
            text = run("format disk2 exfat Take Disk")
            check("It is blank" in text and "ALL of it will be erased" in text, "a blank disk is announced as such", text)
            check("formatted as exFAT" in run("format disk2 confirm"), "`format disk2 confirm` formats it", "")
            text = run("ls disk2")
            check("is empty" in text, "the new volume is empty", text)
            check("Wrote 2 bytes" in run("write disk2 hello.txt hi"), "files can be written to it", "")
            check("hi" in run("cat disk2 hello.txt"), "and read back", "")
            text = run("format disk2 exfat")
            check("exFAT volume with 1 file(s)" in text, "a second format says what it would erase", text)
            run("format disk2 confirm")
            check("is empty" in run("ls disk2"), "and a re-format empties it", "")
            run("write disk2 hello.txt hi")
            # Subfolders (step 3 of docs/fosfs-v4.md): made, filled, listed, read, and removed again.
            run("mkdir disk2/rec")
            run("mkdir disk2/rec/session1")
            run("write disk2/rec/session1/lp1.txt side one")
            run("write disk2/rec/note.txt a note")
            check("side one" in run("cat disk2/rec/session1/lp1.txt"), "a file in a nested exFAT folder reads back", "")
            top, rec = run("ls disk2"), run("ls disk2/rec")
            check("rec" in top and "lp1.txt" not in top, "ls shows a folder without its contents", top)
            check("session1" in rec and "note.txt" in rec and "hello" not in rec, "ls disk2/rec lists just that folder", rec)
            check("lp1.txt" in run("ls disk2/rec/session1"), "and a nested one", "")
            check("hello" in run("ls disk2") and "hi" in run("cat disk2/hello.txt"), "the root is untouched", "")
            check("not" in run("ls disk2/nothere").lower(), "ls of a missing folder says so", "")
            run("rm disk2/rec")
            check("rec" in run("ls disk2"), "a folder with contents is not removed", "")
            run("rm disk2/rec/session1/lp1.txt")
            run("rm disk2/rec/session1")
            check("session1" not in run("ls disk2/rec"), "an emptied folder is removed", "")
            run("mkdir disk2/rec/session2")
            run("write disk2/rec/session2/take.txt kept for the host check")
        finally:
            guest.process.kill()
            guest.process.wait()
        data = blank.read_bytes()
        check(data[512:520] == b"EFI PART", "the disk has a GPT", "")
        check(data[1024:1040] == BASIC_DATA, "with one basic-data partition", data[1024:1040].hex())
        first, last = struct.unpack_from("<QQ", data, 1024 + 32)
        check(first == 2048, "starting at 1 MiB", str(first))
        volume = tmp / "volume.img"
        volume.write_bytes(data[first * 512:(last + 1) * 512])
        result = subprocess.run(["fsck.exfat", "-n", str(volume)], capture_output=True, text=True)
        check(result.returncode == 0, "fsck.exfat finds the volume clean", result.stdout + result.stderr)
        dump = subprocess.run(["dump.exfat", str(volume)], capture_output=True, text=True).stdout
        check("4096" in dump or "cluster" in dump.lower(), "dump.exfat reads its boot sector", dump[:300])
    print("format exfat: all checks passed")


if __name__ == "__main__":
    main()
