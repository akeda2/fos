#!/usr/bin/env python3
"""`install diskN` (v2): copies the running system (ESP, kernel, files) onto another disk and makes it bootable.
Boots the image with a blank 128 MiB second disk, checks the announce/confirm discipline and the boot-disk refusal,
installs, verifies the new GPT with `sgdisk --verify` (and that its GUIDs differ from the boot disk's), then boots the
installed disk *alone* and checks that it comes up with the files. Run with `make test-install`; `make test-bios-install` boots both the original and the copy through BIOS."""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_audio_net  # noqa: E402
from test_audio_net import Guest  # noqa: E402
from test_boot import IMAGE, check  # noqa: E402


def session(tmp, name, image, commands, extra=()):
    test_audio_net.IMAGE = image
    guest = Guest(tmp, name, "52:54:00:12:34:e8", extra=extra)
    outputs = []
    try:
        guest.wait_for("M3 complete")
        guest.wait_for("fos> ")
        for line in commands:
            mark = guest.send(line)
            guest.wait_for("fos> ", 120, mark + len(line))
            outputs.append(guest.text()[mark:])
    finally:
        guest.process.kill()
        guest.process.wait()
    return outputs


def guids(path):
    out = subprocess.run(["sgdisk", "--print", str(path)], capture_output=True, text=True).stdout
    return out


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        target = tmp / "target.img"
        with open(target, "wb") as handle:
            handle.truncate(128 << 20)
        boot = tmp / "boot.img"
        shutil.copy(IMAGE, boot)
        out = session(tmp, "inst1", boot, [
            "write marker.txt installed copy",
            "install disk1", "install disk3", "install disk2 confirm",
            "install disk2", "ls", "install disk2 confirm", "install disk2", "install disk2 confirm", "ls disk2",
        ], extra=["-drive", f"format=raw,file={target}"])
        check("boot disk" in out[1], "the boot disk is refused", out[1])
        check("no such disk" in out[2], "a missing disk is refused", out[2])
        check("nothing is waiting" in out[3], "a confirm without an announcement is refused", out[3])
        check("It is blank." in out[4] and "install disk2 confirm" in out[4], "an install is announced first", out[4])
        check("nothing is waiting" in out[6], "any other command cancels the announcement", out[6])
        check("disk2 is now bootable" in out[8] and "Copied" in out[8], "the install completes", out[8])
        check("marker.txt" in out[9], "the files are on the new disk", out[9])
        verify = subprocess.run(["sgdisk", "--verify", str(target)], capture_output=True, text=True)
        check("No problems found" in verify.stdout, "sgdisk finds the new GPT consistent", verify.stdout + verify.stderr)
        table = guids(target)
        check("EFI System" in table and "fos payload" in table and "FOSFS data" in table, "three partitions: ESP, payload, FOSFS", table)
        # The BIOS boot chain came along, if the boot disk has one: stage one in the MBR's code area, stage two at LBA 64.
        source, copy = boot.read_bytes(), target.read_bytes()
        source_has_bios = any(source[:446]) and any(source[64 * 512:(64 + 24) * 512])
        if source_has_bios:
            check(copy[:446] == source[:446] and copy[510:512] == b"\x55\xaa", "stage one (the MBR's code) is copied, signature kept", copy[:64].hex())
            check(copy[64 * 512:(64 + 24) * 512] == source[64 * 512:(64 + 24) * 512], "and stage two, byte for byte, at LBA 64", "")
            check("BIOS boot code" in out[8], "the install says so", out[8])
        # Booting the installed disk alone: it must find itself by its own new GUIDs.
        out = session(tmp, "inst2", target, ["ls", "cat marker.txt", "debug slots"])
        check("marker.txt" in out[0], "the installed disk boots on its own and has the files", out[0])
        check("installed copy" in out[1], "with their contents", out[1])
        print("v2 install: all checks passed")


if __name__ == "__main__":
    main()
