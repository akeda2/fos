#!/usr/bin/env python3
"""Tape ABI 34 `vfs_*` change calls: create, write, truncate, rename, remove and set-owner on an ext4 volume made by mkfs.ext4, driven by
`fscheck vfswrite`. The host then runs e2fsck on the image (the journal must be clean and the structures consistent) and reads the surviving files back with debugfs. Run with `make test-vfs-write`."""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_fosfs4 import boot  # noqa: E402


def tool(name):
    return shutil.which(name) or f"/usr/sbin/{name}"


def main():
    if not Path(tool("e2fsck")).exists():
        print("SKIP vfs-write: no e2fsck")
        return
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        boot_disk = tmp / "a.img"
        boot_disk.write_bytes(bytes(8 << 20))
        ext = tmp / "ext.img"
        ext.write_bytes(bytes(64 << 20))
        # A volume made by Linux's own mkfs (journal, checksums), not by the kernel: the writer has to keep it consistent.
        subprocess.run([tool("mkfs.ext4"), "-q", "-F", "-L", "vfswrite", str(ext)], check=True, capture_output=True)
        outputs = {}

        def scenario(run):
            outputs["write"] = run("run fscheck vfswrite disk3")
            outputs["ls"] = run("run fscheck vfs disk3")
            run("write ext3/shell.txt shell text")
            run("mkdir ext3/shdir")
            run("cp ext3/shell.txt ext3/shdir/copy.txt")
            outputs["shell"] = run("ls ext3/shdir")

        boot(tmp, [boot_disk, ext], scenario)
        out = outputs["write"]
        for step, wanted in [
            ("mkdir sub", "ok ()"),
            ("create sub/a.txt", "ok ()"),
            ("create sub/a.txt again", "error -6"),
            ("create nodir/x", "error -1"),
            ("write hello", "ok 5"),
            ("big writes", "120000 bytes"),
            ("stat a.txt", "size 120005 mode 640 uid 1000 gid 100"),
            ("truncate 50", "ok ()"),
            ("rename a to b", "ok ()"),
            ("stat a gone", "error -1"),
            ("stat b size", "ok 50"),
            ("remove sub (not empty)", "error -8"),
            ("remove b", "ok ()"),
            ("remove sub", "ok ()"),
            ("write keep.txt", "ok 9"),
            ("write keep.txt more", "ok 5"),
            ("sync", "ok ()"),
        ]:
            line = next((line for line in out.splitlines() if f"fscheck: {step} ->" in line), "")
            check(wanted in line, f"{step}: {wanted}", line or out[-600:])
        # 30000 bytes of (index % 251) start at offset 5: offset 29999+5-... the 8 bytes before offset 30005 are the
        # block's last bytes of round 0 (indexes 29996..29999) and round 1's first four (0..3), preceded by hello's tail.
        line = next((line for line in out.splitlines() if "read back" in line), "")
        expected = [(29_996 + n) % 251 for n in range(4)] + [0, 1, 2, 3]
        check(f"ok (8, {expected})" in line, "a read after the writes sees the bytes written", line)
        check("vfswrite done" in out, "the sequence ran to the end", out[-400:])
        listing = outputs["ls"]
        check("[keep.txt] kind 1 size 14 uid 1000" in listing and "[kdir] kind 2" in listing, "the survivors are listed", listing[-500:])

        check("copy.txt" in outputs["shell"], "the shell's mkdir, write and cp reach an extN volume", outputs["shell"])
        done = subprocess.run([tool("e2fsck"), "-fn", str(ext)], capture_output=True, text=True)
        check(done.returncode == 0, "e2fsck finds the volume consistent", done.stdout + done.stderr)
        shell_cat = subprocess.run([tool("debugfs"), "-R", "cat /shdir/copy.txt", str(ext)], capture_output=True).stdout
        check(shell_cat == b"shell text", "debugfs reads the file the shell copied", repr(shell_cat))
        cat = subprocess.run([tool("debugfs"), "-R", "cat /keep.txt", str(ext)], capture_output=True).stdout
        check(cat == b"kept text more", "debugfs reads the file back, the synced write included", repr(cat))
        stat = subprocess.run([tool("debugfs"), "-R", "stat /kdir", str(ext)], capture_output=True, text=True).stdout
        check("User:     7" in stat and "Group:     8" in stat and "Mode:  0750" in stat, "owner set, mode kept", stat[:400])
    print("vfs-write: all checks passed")


if __name__ == "__main__":
    main()
