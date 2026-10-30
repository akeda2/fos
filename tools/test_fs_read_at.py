#!/usr/bin/env python3
"""Tape ABI 29 `fs_read_at`: a byte range of a file without loading the rest. The `fscheck` tape reads NAME OFFSET LENGTH and
prints it; the test compares that with the same slice of a 5000-byte text file on a FOSFS v4 volume (a start inside a sector,
across sector boundaries, the last bytes, past the end), on an exFAT disk (clusters) and through a folder. Run with
`make test-fs-read-at`."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_fosfs4 import boot, foscopy  # noqa: E402

DATA = bytes(ord("a") + (index * 7 + index // 26) % 26 for index in range(5000))


def scenario(work):
    def scenario(run):
        run("format disk2 volv4 v4")
        run("format disk2 confirm")
        run("format disk3 exfat stick")
        run("format disk3 confirm")
        run("mkdir disk2/sub")
        result = foscopy(["data.txt", ":disk2/data.txt"], work)
        check(result.returncode == 0, "push the test file", result.stdout + result.stderr)

        run("cp disk2/data.txt disk2/sub/data.txt")
        run("cp disk2/data.txt disk3/data.txt")

        def ranged(name, offset, length):
            out = run(f"run fscheck {name} {offset} {length}")
            marker = out.find("fscheck: ")
            return out[marker:] if marker >= 0 else out

        for name in ("disk2/data.txt", "disk2/sub/data.txt", "disk3/data.txt"):
            for offset, length in ((0, 40), (3, 40), (500, 30), (509, 30), (1020, 100), (4090, 100), (4999, 5), (0, 4096)):
                expected = DATA[offset:offset + length]
                got = ranged(name, offset, length)
                check(f"fscheck: {len(expected)} bytes [{expected.decode()}]" in got, f"{name}: {length} bytes at {offset}", got[:300])
            check("fscheck: 0 bytes []" in ranged(name, 5000, 10), f"{name}: at the end gives 0 bytes", "")
            check("fscheck: 0 bytes []" in ranged(name, 9000, 10), f"{name}: past the end gives 0 bytes", "")
        check("error -1" in ranged("disk2/none.txt", 0, 10), "a missing file is FS_NOT_FOUND", "")

    return scenario


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp / "data.txt").write_bytes(DATA)
        disks = []
        for name in ("a.img", "b.img"):
            disk = tmp / name
            disk.write_bytes(bytes(64 << 20))
            disks.append(disk)
        boot(tmp, disks, scenario(tmp))
    print("fs_read_at: all checks passed")


if __name__ == "__main__":
    main()
