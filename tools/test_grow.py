#!/usr/bin/env python3
"""`grow diskN` (v2): a stick flashed from the 96 MiB image has a 32 MiB FOSFS partition and unused space after it; `grow`
moves the backup GPT to the real end of the disk, extends the FOSFS partition and volume to fill it, and keeps the files.
Checked three ways: the shell's own report, `sgdisk --verify` (an independent GPT checker: CRCs, backup at the end, no
overlap), and a second boot from the grown image that still finds its files and its new size. Run with `make test-grow`."""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_audio_net  # noqa: E402
from test_audio_net import Guest  # noqa: E402
from test_boot import IMAGE, check  # noqa: E402


def session(tmp, name, image, commands):
    """Boots `image` (a copy of it), runs the shell `commands`, returns (outputs, the disk image the guest used)."""
    test_audio_net.IMAGE = image
    guest = Guest(tmp, name, "52:54:00:12:34:e7")
    outputs = []
    try:
        guest.wait_for("M3 complete")
        guest.wait_for("fos> ")
        for line in commands:
            mark = guest.send(line)
            guest.wait_for("fos> ", 30, mark + len(line))
            outputs.append(guest.text()[mark:])
    finally:
        guest.process.kill()
        guest.process.wait()
    return outputs, Path(tmp) / f"{name}.img"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        big = tmp / "big.img"
        shutil.copy(IMAGE, big)
        with open(big, "r+b") as handle:
            handle.truncate(256 << 20)  # a "stick" much bigger than the image: the new GPT end is far from the old one
        outputs, grown = session(tmp, "grow1", big, ["write keep.txt hello grown", "grow disk1", "grow disk1", "cat keep.txt", "write after.txt written after the grow", "ls"])
        check("Wrote" in outputs[0], "a file written before the grow", outputs[0])
        check("grown from 32 MiB to" in outputs[1] and "files are unchanged" in outputs[1], "`grow disk1` reports the new size", outputs[1])
        check("already fills the disk" in outputs[2], "a second `grow` says there is nothing to do", outputs[2])
        check("hello grown" in outputs[3], "the old file is intact", outputs[3])
        check("after.txt" in outputs[5] and "keep.txt" in outputs[5], "and the volume still takes new files", outputs[5])
        verify = subprocess.run(["sgdisk", "--verify", str(grown)], capture_output=True, text=True)
        check("No problems found" in verify.stdout, "sgdisk finds the grown GPT consistent (CRCs, backup at the disk's end)", verify.stdout + verify.stderr)
        listing = subprocess.run(["sgdisk", "--print", str(grown)], capture_output=True, text=True).stdout
        last_line = [line for line in listing.splitlines() if "FOSFS" in line][-1].split()
        end_sector = int(last_line[2])
        check(end_sector == (256 << 11) - 34, f"the FOSFS partition ends at the last usable sector ({end_sector})", listing)
        # A second boot from the grown disk: the volume mounts at its new size with the files in place.
        outputs, _ = session(tmp, "grow2", grown, ["ls", "grow disk1", "cat keep.txt"])
        check("keep.txt" in outputs[0] and "after.txt" in outputs[0], "the grown disk boots and its files are there", outputs[0])
        check("already fills the disk" in outputs[1], "and the volume knows its new size", outputs[1])
        check("hello grown" in outputs[2], "with the file's contents unchanged", outputs[2])
        verify = subprocess.run(["sgdisk", "--verify", str(tmp / "grow2.img")], capture_output=True, text=True)
        check("No problems found" in verify.stdout, "and the table is still consistent after the second boot", verify.stdout)
    print("grow: all checks passed")


if __name__ == "__main__":
    main()
