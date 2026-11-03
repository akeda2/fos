#!/usr/bin/env python3
"""The server kernel's shell gets the shared file commands (`crates/shell`: ls cat write rm cp mv cmp mkdir, docs/shell.md) through
`kernel-mf/src/shellhost.rs`, the same code the embedded kernel's shell runs. Checks: ls shows a file of the boot volume, cat reads it,
write/cat/cp/cmp/mv/rm/mkdir round-trip through the disk, help NAME comes from the shared table, and an unknown command is reported.
QEMU only."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_debug_mode as base  # noqa: E402
from test_boot import ROOT, check  # noqa: E402

base.KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf"
READY = "entering the service loop"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        machine = base.Machine(tmp, {"hostname": "mfshell\n"}, {})
        machine.start()
        try:
            machine.wait_for(READY)
            machine.connect()
            out = machine.run("ls")
            check("hostname" in out, "ls lists the boot volume", out[-400:])
            out = machine.run("cat hostname")
            check("mfshell" in out, "cat reads a file", out[-300:])
            out = machine.run("write note.txt hello from mf")
            out = machine.run("cat note.txt")
            check("hello from mf" in out, "write makes a file and cat reads it back", out[-300:])
            out = machine.run("cp note.txt copy.txt")
            out = machine.run("cmp note.txt copy.txt")
            check("identical" in out or "same" in out, "cp copies and cmp says they match", out[-300:])
            out = machine.run("mv copy.txt moved.txt")
            out = machine.run("ls")
            check("moved.txt" in out and "copy.txt" not in out, "mv renames", out[-400:])
            out = machine.run("rm moved.txt")
            out = machine.run("ls")
            check("moved.txt" not in out and "note.txt" in out, "rm removes", out[-400:])
            out = machine.run("cat nothere.txt")
            check("no such file" in out, "a missing file is reported", out[-300:])
            out = machine.run("help ls")
            check("ls [" in out, "help NAME shows the shared usage line", out[-300:])
            out = machine.run("help")
            check("ls cat write" in out, "help lists the file commands", out[-400:])
            out = machine.run("bogus")
            check("Unknown command" in out, "an unknown command is reported", out[-300:])
        finally:
            machine.stop()
    print("test-mf-shell: all checks passed")


if __name__ == "__main__":
    main()
