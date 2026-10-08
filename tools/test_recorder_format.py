#!/usr/bin/env python3
"""Formatting a blank disk as exFAT from the recorder's UI (v2): `D` shows a disk with no volume, a take on it is refused,
`X` asks for confirmation (any other key cancels), and after the format the disk takes a recording that `fsck.exfat`
accepts. Run with `make test-recorder-format`."""

import struct
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
        blank = tmp / "blank.img"
        blank.write_bytes(bytes(200 << 20))
        extra = ["-device", "ich9-ahci,id=ahci", "-drive", f"if=none,id=f1,format=raw,file={blank}", "-device", "ide-hd,drive=f1,bus=ahci.0"]
        guest = Guest(tmp, "recfmt", "52:54:00:12:34:e6", extra=extra)

        def key(data, wait=0.6):
            guest.process.stdin.write(data)
            guest.process.stdin.flush()
            guest.pump(wait)

        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("run recorder fm tone=440 notest pre=1")
            guest.wait_for("Record to: disk1  FOSFS", 30, mark)
            key(b"d")
            guest.wait_for("Record to: disk2  no volume", 10, mark)
            key(b" ")
            guest.wait_for("has no volume to record to", 10, mark)
            key(b"x")
            guest.wait_for("Volume label: FOS_", 10, mark)
            key(b"\x1b")  # Escape in the label prompt cancels
            guest.wait_for("Format cancelled", 10, mark)
            key(b"x")
            guest.wait_for("Volume label: FOS_", 10, mark)
            key(b"\x7f\x7f\x7f")
            key(b"My Disk")
            key(b"\r")
            guest.wait_for('ERASE disk2', 10, mark)
            guest.wait_for('as exFAT "My Disk"', 10, mark)
            key(b"n")  # not the disk's number: cancels
            guest.wait_for("Format cancelled", 10, mark)
            key(b"x")
            key(b"\x7f\x7f\x7f")
            key(b"My Disk")
            key(b"\r")
            guest.wait_for("Press 2 to confirm", 10, mark)
            key(b"2", 3.0)
            guest.wait_for("disk2 is now one exFAT volume", 60, mark)
            guest.wait_for("Record to: disk2  exFAT", 10, mark)
            guest.wait_for("longest run", 10, mark)
            key(b" ", 3.0)
            guest.wait_for("Recording disk2/fm001.wav", 10, mark)
            key(b" ", 1.0)
            guest.wait_for("Saved 1 file", 15, mark)
            key(b"q")
            guest.wait_for("file(s) written", 30, mark)
            check("(with errors)" not in guest.text()[mark:], "the take on the formatted disk had no errors", guest.text()[mark:][-1500:])
            mark = guest.send("run recorder fm2 tone=440 notest")
            guest.wait_for("Record to: disk2  exFAT", 30, mark)  # now the default: the first exFAT disk
            key(b"d")
            guest.wait_for("Record to: disk1  FOSFS", 10, mark)
            key(b"x")
            guest.wait_for("is the boot disk and is never formatted", 10, mark)
            key(b"q")
            guest.wait_for("file(s) written", 30, mark)
        finally:
            guest.process.kill()
            guest.process.wait()
        data = blank.read_bytes()
        first, last = struct.unpack_from("<QQ", data, 1024 + 32)
        volume = tmp / "volume.img"
        volume.write_bytes(data[first * 512:(last + 1) * 512])
        check("My Disk".encode("utf-16le") in volume.read_bytes(), "the label typed in the UI is in the volume", "")
        result = subprocess.run(["fsck.exfat", "-n", str(volume)], capture_output=True, text=True)
        check(result.returncode == 0 and "files 1" in result.stdout.replace(",", ""), "fsck.exfat finds the volume clean, with the one take on it", result.stdout + result.stderr)
    print("recorder format: all checks passed")


if __name__ == "__main__":
    main()
