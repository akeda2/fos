#!/usr/bin/env python3
"""The shell on FAT32 and exFAT disks, and the clock (v2).

`ls`, `copy` and `rm` work on a FAT disk only when it is named (`diskN`); a bare name never reaches one. The files
are stamped with the clock, which `date set` can change (the RTC is local time). The volumes are checked with `fsck`
afterwards. Run with `make test-fat-shell`."""

import datetime
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        fat32, exfat = tmp / "fat32.img", tmp / "exfat.img"
        for path, command in ((fat32, ["mkfs.vfat", "-F", "32", "-s", "1"]), (exfat, ["mkfs.exfat"])):
            path.write_bytes(bytes(96 << 20))
            subprocess.run([*command, str(path)], check=True, capture_output=True)
        extra = ["-device", "ich9-ahci,id=ahci",
                 "-drive", f"if=none,id=f1,format=raw,file={fat32}", "-device", "ide-hd,drive=f1,bus=ahci.0",
                 "-drive", f"if=none,id=f2,format=raw,file={exfat}", "-device", "ide-hd,drive=f2,bus=ahci.1"]
        guest = Guest(tmp, "fatshell", "52:54:00:12:34:e4", extra=extra)

        def run(line, wait=10):
            mark = guest.send(line)
            guest.wait_for("fos> ", wait, mark + len(line))
            return guest.text()[mark:]

        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            clock = run("date")
            year = datetime.datetime.now(datetime.timezone.utc).year
            check(f"{year}-" in clock, "`date` shows the RTC (QEMU's, from the host)", clock)
            check("Set" not in clock and "not set" not in clock, "and the clock counts as set", clock)
            answer = run("date set 2030-01-02 03:04:05")
            check("The clock is 2030-01-02 03:04:05" in answer, "`date set` sets it", answer)
            check("2030-01-02 03:04:0" in run("date") or "2030-01-02 03:04:1" in run("date"), "and it keeps counting from there", "")
            check("not a date" in run("date set 2030-02-31 00:00"), "an impossible date is refused", "")
            run("write note.txt hello from fosfs")
            for disk in ("disk2", "disk3"):
                text = run(f"ls {disk}")
                check("is empty" in text, f"`ls {disk}` on a fresh FAT volume says it is empty", text)
                text = run(f"copy disk1 note.txt {disk} Note.txt")
                check("Copied note.txt" in text, f"`copy` from FOSFS to {disk}", text)
                text = run(f"ls {disk}")
                check("Note.txt" in text and "16 bytes" in text and "2030-01-02 03:04" in text, f"`ls {disk}` shows the name, size and the time from the clock", text)
                text = run(f"copy {disk} Note.txt disk1")
                check("not a FOSFS file name" in text, "a FAT name FOSFS cannot hold needs a new name", text)
                text = run(f"copy {disk}/Note.txt disk1/back.txt")
                check("Copied Note.txt" in text, f"`copy {disk}/NAME disk1/NEW` brings it back", text)
                check("hello from fosfs" in run("cat back.txt"), "with the same contents", "")
                run("rm back.txt")
                text = run(f"copy {disk} Note.txt {disk} Copy.txt")
                check("Copied Note.txt" in text and "Copy.txt" in run(f"ls {disk}"), f"a copy inside {disk}", text)
                text = run(f"write {disk} Hand.txt typed on the shell")
                check(f"Wrote 18 bytes to {disk}/Hand.txt" in text, f"`write {disk} NAME TEXT`", text)
                check("typed on the shell" in run(f"cat {disk} Hand.txt"), f"`cat {disk} NAME` shows it", "")
                text = run(f"write {disk}/Slash.txt more   spaced   text")
                check(f"Wrote 20 bytes to {disk}/Slash.txt" in text and "more   spaced   text" in run(f"cat {disk}/Slash.txt"), f"`write {disk}/NAME TEXT` keeps the text as typed", text)
                check("no such file" in run(f"cat {disk} Nope.txt"), "cat of a missing file says so", "")
                text = run(f"rm {disk} Note.txt")
                check(f"Removed {disk}/Note.txt" in text and "Note.txt" not in run(f"ls {disk}"), f"`rm {disk} NAME`", text)
                check("no such file" in run(f"rm {disk} Note.txt"), "removing it twice says so", "")
            # A bare name is FOSFS only: a file that exists only on a FAT disk is not found by `cat NAME`.
            bare = run("cat Copy.txt")
            check("no such file" in bare or "cannot be used" in bare, "a bare name never reaches a FAT disk", bare)
            # The boot disk's EFI System Partition is `esp`: its root directory, like a FAT disk (the loader stays under EFI/BOOT).
            text = run("write esp/hello.txt typed on the esp")
            check("Wrote 16 bytes to esp/hello.txt" in text and "typed on the esp" in run("cat esp/hello.txt"), "`write esp/NAME TEXT` and `cat esp/NAME` on the EFI System Partition", text)
            text = run("copy disk1 note.txt esp Note.txt")
            check("Copied note.txt" in text and "Note.txt" in run("ls esp") and "EFI" in run("ls esp"), "`copy` to `esp`; `ls esp` lists it next to EFI", text)
            check("Removed esp/hello.txt" in run("rm esp/hello.txt") and "hello.txt" not in run("ls esp"), "`rm esp/NAME`", "")
            run("rm esp Note.txt")
        finally:
            guest.process.kill()
            guest.process.wait()
        for command, image in ((["fsck.vfat", "-n"], fat32), (["fsck.exfat", "-n"], exfat)):
            result = subprocess.run([*command, str(image)], capture_output=True, text=True)
            check(result.returncode == 0, f"{command[0]} finds {image.name} clean", result.stdout + result.stderr)
    print("fat shell: all checks passed")


if __name__ == "__main__":
    main()
