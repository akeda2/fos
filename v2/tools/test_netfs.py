#!/usr/bin/env python3
"""NETFS on v2: files between a host and a fos machine with `tools/foscopy.py` (the unchanged v1 host tool, over the
multicast seam), and between two fos machines with `copy HOST/NAME DST` and `copy SRC NAME HOST`. Round trips of a
multi-chunk file are compared byte for byte; refusals (no such file, bad name, no such disk) are reported."""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import GROUP, PORT, Guest  # noqa: E402
from test_boot import ROOT, check  # noqa: E402

FOSCOPY = ROOT / "tools/foscopy.py"


def foscopy(args, cwd):
    return subprocess.run([sys.executable, str(FOSCOPY), "--mcast", f"{GROUP}:{PORT}", "--delay", "0.002", "--timeout", "15", *args],
                          capture_output=True, text=True, cwd=cwd, timeout=120)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        big = bytes((index * 7 + index // 251) % 256 for index in range(37_000))
        (work / "big.bin").write_bytes(big)
        (work / "note.txt").write_text("hello from the host\n")
        a = Guest(tmp, "netfs-a", "52:54:00:12:34:f1", files={"hostname": "alpha", "alpha.txt": "file on alpha\n"})
        b = Guest(tmp, "netfs-b", "52:54:00:12:34:f2", files={"hostname": "bravo", "bravo.txt": "file on bravo\n"})
        try:
            for guest in (a, b):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mac_a = "52:54:00:12:34:f1"

            result = foscopy(["--mac", mac_a, "note.txt", ":disk1/note.txt"], work)
            check(result.returncode == 0, "foscopy stores a file on the machine", result.stderr[-600:])
            mark = a.send("cat note.txt")
            a.pump(1.0)
            check("hello from the host" in a.text()[mark:], "and the machine has it", a.text()[mark:])

            result = foscopy(["--mac", mac_a, "big.bin", ":disk1/big.bin"], work)
            check(result.returncode == 0, "a file of 37,000 bytes (37 chunks) is stored", result.stderr[-600:])
            result = foscopy(["--mac", mac_a, ":disk1/big.bin", "back.bin"], work)
            check(result.returncode == 0 and (work / "back.bin").read_bytes() == big, "and read back byte for byte", result.stderr[-600:])

            result = foscopy(["--mac", mac_a, ":disk1/nosuchfile", "x.bin"], work)
            check(result.returncode != 0 and "file not found" in result.stderr + result.stdout, "a missing file is reported as such", result.stderr[-400:])
            result = foscopy(["--mac", mac_a, "note.txt", ":disk9/note.txt"], work)
            check(result.returncode != 0 and "disk is not available" in result.stderr + result.stdout, "an unavailable disk is refused", result.stderr[-400:])

            # Machine to machine.
            mark = a.send("nic discover")
            a.wait_for("fos> ", 30, mark)
            check("bravo1" in a.text()[mark:], "alpha finds bravo by name (a host name gets its NIC number appended)", a.text()[mark:][-400:])
            mark = a.send("copy bravo1/bravo.txt disk1")
            a.wait_for("fos> ", 30, mark)
            check("Copied bravo.txt" in a.text()[mark:], "copy HOST/NAME DST fetches a file from another machine", a.text()[mark:][-400:])
            mark = a.send("cat bravo.txt")
            a.pump(1.0)
            check("file on bravo" in a.text()[mark:], "and its contents arrive intact", a.text()[mark:])
            mark = a.send("copy disk1 alpha.txt bravo1 alpha-copy.txt")
            a.wait_for("fos> ", 30, mark)
            check("Copied alpha.txt to bravo1" in a.text()[mark:], "copy SRC NAME HOST stores a file on another machine", a.text()[mark:][-400:])
            mark = b.send("cat alpha-copy.txt")
            b.pump(1.0)
            check("file on alpha" in b.text()[mark:], "which has it", b.text()[mark:])
            mark = a.send("copy bravo1/nothing.txt disk1")
            a.wait_for("fos> ", 30, mark)
            check("no such file" in a.text()[mark:], "a missing remote file is reported", a.text()[mark:][-300:])
        finally:
            a.stop()
            b.stop()
    print("v2 netfs: all checks passed")


if __name__ == "__main__":
    main()
