#!/usr/bin/env python3
"""Tape ABI 32 `vfs_*`: stat, folder walk by index and ranged read through the `fscheck vfs` tape, on an ext4 disk made by
mkfs.ext4 -d (owners, modes, a UTF-8 name, a folder bigger than one block) and, for the fallback, on a FOSFS v4 disk. Run with
`make test-vfs`."""

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_fosfs4 import boot, foscopy  # noqa: E402

MANY = 400


def make_ext4(tmp):
    mkfs = shutil.which("mkfs.ext4") or "/usr/sbin/mkfs.ext4"
    if not Path(mkfs).exists():
        return None
    source = tmp / "src"
    (source / "docs").mkdir(parents=True)
    (source / "many").mkdir()
    (source / "hello.txt").write_text("hello from ext4\n" * 400)
    (source / "docs" / "räksmörgås.txt").write_text("hej\n")
    for index in range(MANY):
        (source / "many" / f"file-{index:04}-with-a-longish-name.txt").write_text("x")
    os.chmod(source / "hello.txt", 0o640)
    image = tmp / "ext.img"
    image.write_bytes(bytes(48 << 20))
    subprocess.run([mkfs, "-q", "-F", "-L", "linuxvol", "-d", str(source), str(image)], check=True, capture_output=True)
    return image, os.getuid(), os.getgid()


def scenario(uid, gid, ext):
    def scenario(run):
        def vfs(path):
            out = run(f"run fscheck vfs {path}")
            marker = out.find("fscheck: ")
            return out[marker:] if marker >= 0 else out

        if ext:
            out = vfs("disk3/hello.txt")
            check(f"kind 1 mode 640 size 6400 uid {uid} gid {gid}" in out, "ext4 stat of a file", out[:300])
            check("read 40 bytes [hello from ext4\nhello from ext4\nhello fr]" in out, "ext4 ranged read", out[:300])
            out = vfs("disk3")
            check("kind 2" in out and "[hello.txt] kind 1 size 6400" in out and "[docs] kind 2" in out, "ext4 root listing", out[:500])
            check("[.]" not in out and "[..]" not in out, "no . or .. entries", out[:500])
            out = vfs("disk3/docs")
            check("[räksmörgås.txt] kind 1 size 4" in out and "1 entries" in out, "UTF-8 name in a folder", out[:300])
            out = vfs("disk3/many")
            check(f"{MANY} entries" in out, f"all {MANY} entries are walked once each", out[-300:])
            check("error -1" in vfs("disk3/none.txt"), "a missing ext4 file is FS_NOT_FOUND", "")
        out = vfs("disk2/data.txt")
        check("kind 1 mode 644 size 40" in out and "read 40 bytes" in out, "FOSFS file through the fallback", out[:300])
        out = vfs("disk2")
        check("[data.txt] kind 1 size 40" in out and "1 entries" in out, "FOSFS listing through the fallback", out[:300])

    return scenario


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp / "data.txt").write_bytes(bytes(range(65, 65 + 40)))
        made = make_ext4(tmp)
        if made is None:
            print("SKIP ext4: no mkfs.ext4")
        a = tmp / "a.img"
        a.write_bytes(bytes(64 << 20))
        disks = [a, made[0]] if made else [a]

        def both(run):
            run("format disk2 volv4 v4")
            run("format disk2 confirm")
            result = foscopy(["data.txt", ":disk2/data.txt"], tmp)
            check(result.returncode == 0, "push the test file", result.stdout + result.stderr)
            scenario(made[1] if made else 0, made[2] if made else 0, bool(made))(run)

        boot(tmp, disks, both)
    print("vfs: all checks passed")


if __name__ == "__main__":
    main()
