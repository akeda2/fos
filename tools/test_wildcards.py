#!/usr/bin/env python3
"""Bash-style wildcards in `ls`, `cp`, `mv` and `rm` (docs/fosfs-v4.md "Wildcards in shell tools"), on the boot volume
(FOSFS v4, with folders) and on an exFAT disk (names compare ignoring case). Run through `make test-wildcards`."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_fosfs4 import boot  # noqa: E402


def scenario(run):
    run("mkdir rec")
    for name in ("a.flac", "b.flac", "c.txt"):
        run(f"write rec/{name} {name}")
    run("write top.flac top")

    out = run("ls rec/*.flac")
    check("rec/a.flac" in out and "rec/b.flac" in out and "c.txt" not in out, "ls rec/*.flac lists the two matches", out)
    out = run("ls re?/[ab].*")
    check("rec/a.flac" in out and "rec/b.flac" in out and "c.txt" not in out, "? and [ab] match within a folder", out)
    out = run("ls rec/[!a]*")
    check("rec/b.flac" in out and "rec/a.flac" not in out, "[!a] negates a set", out)
    out = run("ls */a.flac")
    check("rec/a.flac" in out, "a folder component may carry a pattern", out)
    check("no match for rec/*.wav" in run("ls rec/*.wav"), "no match is an error", "")

    out = run("cp rec/*.flac backup")
    check("Copied 2 of 2 file(s)." in out, "cp PATTERN DST makes the folder and copies the matches", out)
    check("b.flac" in run("cat backup/b.flac"), "the copies have their contents", "")
    check("no match" in run("cp rec/*.wav backup"), "cp with no match fails", "")

    out = run("mv backup/*.flac moved")
    check("Moved 2 of 2 file(s)." in out, "mv PATTERN DST moves the matches", out)
    check("no such file" in run("cat backup/a.flac"), "the sources are gone", "")
    check("a.flac" in run("cat moved/a.flac"), "and the destination has them", "")
    out = run("mv top.flac moved")
    check("Moved top.flac" in out and "top" in run("cat moved/top.flac"), "mv of one file into an existing folder keeps the name", out)

    out = run("rm moved/*.flac")
    check("matches 3 files" in out and "confirm" in out, "rm of several matches asks first", out)
    check("a.flac" in run("cat moved/a.flac"), "nothing was removed yet", "")
    out = run("rm moved/*.flac confirm")
    check("Removed 3 of 3." in out, "rm PATTERN confirm removes them", out)
    check("c.txt" in run("cat rec/c.txt"), "other files stay", "")
    check("no match" in run("rm moved/*.flac confirm"), "rm with no match fails", "")

    # In-place rename on FOSFS v4: folders and files move with no data copied, the contents follow.
    run("mkdir deep")
    run("write deep/x.txt xdata")
    out = run("mv deep renamed")
    check("renamed in place" in out, "mv of a folder renames it in place", out)
    check("xdata" in run("cat renamed/x.txt") and "no such file" in run("cat deep/x.txt"), "its files moved with it", "")
    out = run("mv renamed/x.txt rec/x2.txt")
    check("renamed in place" in out and "xdata" in run("cat rec/x2.txt"), "a file moves between folders in place", out)
    run("mkdir inner")
    out = run("mv renamed renamed/inner")
    check("renamed/inner" in out and "Moved" not in out.split("\n")[0], "a folder cannot move into itself", out)
    run("write rec/old.txt old")
    run("write rec/new.txt new")
    out = run("mv rec/new.txt rec/old.txt")
    check("renamed in place" not in out and "new" in run("cat rec/old.txt") and "no such file" in run("cat rec/new.txt"), "moving onto an existing file replaces it by copy", out)
    fsck = run("fsck")
    check("directory consistent" in fsck, "fsck is happy after the renames", fsck[-300:])

    # exFAT: names compare ignoring case, so a lower-case pattern finds upper-case files
    run("format disk2 exfat wild")
    run("format disk2 confirm")
    for name in ("REC1.TXT", "rec2.txt", "Other.bin"):
        run(f"write disk2/{name} x")
    out = run("ls disk2/REC*.txt")
    check("REC1.TXT" in out.upper() and "rec2.txt" in out.lower() and "Other" not in out, "exFAT matches ignore case", out)
    out = run("cp disk2/rec*.txt backup")
    check("Copied 2 of 2 file(s)." in out, "cp from exFAT to a FOSFS folder", out)
    out = run("rm disk2/rec*.txt confirm")
    check("Removed 2 of 2." in out and "Other" in run("ls disk2"), "rm on exFAT", out)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        disk = Path(tmp) / "exfat.img"
        disk.write_bytes(bytes(64 << 20))
        boot(Path(tmp), [disk], scenario)
    print("wildcards: all checks passed")


if __name__ == "__main__":
    main()
