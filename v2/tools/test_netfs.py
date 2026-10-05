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
        import random
        large = random.Random(7).randbytes(300_000)  # random data: a 32-bit checksum accumulator used to overflow here
        (work / "large.bin").write_bytes(large)
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

            result = foscopy(["--mac", mac_a, "large.bin", ":disk1/large.bin"], work)
            check(result.returncode == 0, "300,000 bytes of random data are stored (the checksum must not overflow)", result.stderr[-600:])
            result = foscopy(["--mac", mac_a, ":disk1/large.bin", "large.back"], work)
            check(result.returncode == 0 and (work / "large.back").read_bytes() == large, "and read back byte for byte", result.stderr[-600:])

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

            # Tapes between machines: `tape load HOST NAME`, `tape save HOST NAME`, `tape send NAME PEER`.
            mark = a.send("tape load bravo1 hello")
            a.wait_for("fos> ", 30, mark)
            check("Tape loaded:" in a.text()[mark:] and "from hello" in a.text()[mark:], "tape load HOST NAME fetches a tape from another machine", a.text()[mark:][-400:])
            mark = a.send("tape run")
            a.wait_for("fos> ", 30, mark)
            check("hello from a v2 tape" in a.text()[mark:], "and the fetched tape runs", a.text()[mark:][-400:])
            mark = a.send("tape save bravo1 hello-copy")
            a.wait_for("fos> ", 30, mark)
            check("Saved" in a.text()[mark:] and "bravo1 as hello-copy" in a.text()[mark:], "tape save HOST NAME stores the loaded tape on another machine", a.text()[mark:][-400:])
            mark = b.send("ls")
            b.pump(1.0)
            check("hello-copy" in b.text()[mark:], "which has it", b.text()[mark:][-400:])
            mark_b = b.send("tape load net")
            b.wait_for("Waiting for a tape", 30, mark_b)
            mark = a.send("tape send calc bravo1")
            a.wait_for("Sent ", 60, mark)
            b.wait_for("Tape loaded:", 60, mark_b)
            check("from network" in b.text()[mark_b:], "tape send NAME PEER delivers a tape to a machine in tape load net", b.text()[mark_b:][-400:])
            mark = b.send("tape run 3 4 +")
            b.wait_for("fos> ", 30, mark)
            check("7" in b.text()[mark:], "and the sent tape (calc) runs", b.text()[mark:][-300:])
        finally:
            a.stop()
            b.stop()
    print("v2 netfs: all checks passed")


if __name__ == "__main__":
    main()
