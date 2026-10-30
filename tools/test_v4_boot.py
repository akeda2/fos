#!/usr/bin/env python3
"""A FOSFS v4 boot volume laid out in folders (docs/fosfs-v4.md, step 4): tapes in bin/, configuration in settings/.
Run through `make test-v4-boot`, which builds the image (FOSFOR_IMAGE). `run NAME` must find bin/NAME, the kernel must
read settings/debug at boot and a bare name in the root is still read as itself
(the v3 and FAT habit stays valid). A tape's config/ lookup is only covered by tape-rt's host tests."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_fosfs4 import boot  # noqa: E402


def scenario(run):
    listing = run("ls")
    check("bin" in listing and "settings" in listing, "the root has the bin and settings folders", listing)
    check("hello" in run("ls bin"), "ls bin lists the tapes", "")
    status = run("debug status")
    check("kernel-update true" in status and "watchdog: 60 s" in status, "settings/debug was read at boot", status)
    out = run("run hello")
    check("hello from a v2 tape" in out, "run hello starts bin/hello", out)
    run("write note.txt plain")
    check("plain" in run("cat note.txt"), "a bare file in the root is read as itself", "")
    run("write hello replaced")
    check("replaced" in run("cat bin/hello"), "a bare write replaces the tape where it lives, in bin/", "")
    check("hello " not in run("ls").replace("bin", ""), "and leaves no copy in the root", "")
    run("rm hello")
    check("no such file" in run("cat bin/hello"), "a bare rm removes it from bin/", "")


def main():
    with tempfile.TemporaryDirectory() as tmp:
        boot(Path(tmp), [], scenario)
    print("v4-boot: all checks passed")


if __name__ == "__main__":
    main()
