#!/usr/bin/env python3
"""The mount table (docs/fosfs-v4.md, step 2): `mount PATH @LABEL` makes a folder path of the boot volume live on
another FOSFS volume found by label. A write to PATH/NAME lands on that volume (making the folders on the way), a read
finds it there, `ls PATH` lists it, `mount fstab` fills the table from the boot volume's `fstab`, `umount` takes a mount
away, and a mount whose volume is absent is reported as such. The boot volume here is v3 (no folders), so the fallback
to a boot-volume folder is a v4-boot case this test does not reach."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_fosfs4 import SECTOR, boot, partition_start, v4_files  # noqa: E402


def scenario(run):
    run("format disk2 cfgvol v4")
    run("format disk2 confirm")
    check("No mounts" in run("mount"), "the table starts empty", "")
    done = run("mount config @cfgvol")
    check("config is now @cfgvol" in done and "not present" not in done, "mount PATH @LABEL accepts a present volume", done)
    check("config -> @cfgvol (disk2)" in run("mount"), "mount lists it with the disk it is on", "")
    run("write config/phono.cfg gain=3")
    check("gain=3" in run("cat config/phono.cfg"), "a file under the mount point reads back", "")
    check("gain=3" in run("cat @cfgvol/phono.cfg"), "and is on the mounted volume under the shortened name", "")
    run("write config/sub/deep.cfg deep")
    check("deep" in run("cat @cfgvol/sub/deep.cfg"), "a write makes the folders on the way", "")
    listing = run("ls config")
    check("phono.cfg" in listing and "sub" in listing, "ls of the mount point lists the mounted volume", listing)
    check("config" not in run("ls disk1"), "and nothing was made on the boot volume", "")
    check("is not a folder path" in run("mount a//b @cfgvol"), "a bad mount point is refused", "")
    check("not mounted" in run("umount nothere"), "umount of an unknown path says so", "")
    run("write fstab settings @cfgvol")
    done = run("mount fstab")
    check("1 mount(s)" in done, "mount fstab reads the boot volume's fstab", done)
    table = run("mount")
    check("settings -> @cfgvol" in table and "config ->" not in table, "and replaces the table", table)
    run("umount settings")
    check("No mounts" in run("mount"), "umount empties it", "")
    run("mount settings @nothere")
    check("not present" in run("mount"), "a mount whose volume is absent is reported", "")


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        disk = tmp / "cfg.img"
        disk.write_bytes(bytes(64 << 20))
        boot(tmp, [disk], scenario)
        image = disk.read_bytes()
        found, meta = v4_files(image[partition_start(image) * SECTOR:])
        check(found.get("phono.cfg") == b"gain=3" and found.get("sub/deep.cfg") == b"deep" and "sub/" in found and meta["damaged"] == 0,
              "the host parse finds the files on the mounted volume without the mount path", repr(sorted(found)))
    print("mounts: all checks passed")


if __name__ == "__main__":
    main()
