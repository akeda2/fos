#!/usr/bin/env python3
"""The per-machine files (`hostname`, `start`, ...) are also found in the root of a FAT32/exFAT disk.

Boot 1 writes `hostname` and `start` onto an exFAT disk from the shell (the boot FOSFS volume has neither). Boot 2, a fresh
machine with the same exFAT disk, must read both from it: the announced name follows the file and the script runs.
Run with `make test-config-fat`."""

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
        exfat = tmp / "exfat.img"
        exfat.write_bytes(bytes(96 << 20))
        subprocess.run(["mkfs.exfat", str(exfat)], check=True, capture_output=True)
        extra = ["-device", "ich9-ahci,id=ahci", "-drive", f"if=none,id=f1,format=raw,file={exfat}", "-device", "ide-hd,drive=f1,bus=ahci.0"]

        first = Guest(tmp, "cfg-fat-1", "52:54:00:12:34:c7", extra=extra)
        try:
            first.wait_for("M3 complete")
            first.wait_for("fos> ")
            for line in ("write disk2 hostname fatlab", "write disk2 start uptime"):
                mark = first.send(line)
                first.wait_for("fos> ", 15, mark + len(line))
            text = first.text()
            check("hostname file" not in text, "boot 1 had no hostname file anywhere", text[-500:])
        finally:
            first.stop()

        second = Guest(tmp, "cfg-fat-2", "52:54:00:12:34:c8", extra=extra)
        try:
            second.wait_for("M3 complete")
            second.wait_for("fos> ")
            text = second.text()
            check("hostname: read from disk2 (FAT/exFAT)." in text, "boot 2 reads hostname from the exFAT disk", text[-1500:])
            check("hostname file: this machine announces itself as fatlab1." in text, "and announces itself under that name", text[-1500:])
            check("start file: running 1 command(s)." in text and "start> uptime" in text, "and runs the start script from it", text[-1500:])
            mark = second.send("nic peers")
            second.wait_for("fos> ", 10, mark)
        finally:
            second.stop()
    print("config on FAT: all checks passed")


if __name__ == "__main__":
    main()
