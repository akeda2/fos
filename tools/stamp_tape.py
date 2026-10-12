#!/usr/bin/env python3
"""Adds the `.fos_build` stamp (see crates/tape-sync) to a linked tape: `stamp_tape.py IN OUT`.

The stamp is 16 bytes: the build time (u64, seconds since 1970), the tape ABI version the tape is built for (u32) and
four reserved zero bytes. The build time is the commit time of HEAD when the working tree is clean, so two machines
that build the same commit stamp the same value; with local changes it is the current time, so a dirty build counts
as newer than the commit it started from. `tapesync` compares these stamps to decide which machine has the newer tape.

The input is never modified (cargo owns it); OUT is a copy with the section added (GNU objcopy, as the BIOS boot chain
already needs binutils). The section is not loaded: the kernel's ELF loader only maps program headers.
"""
import os
import re
import struct
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def abi_version():
    text = open(os.path.join(ROOT, "abi", "src", "tape.rs")).read()
    return int(re.search(r"pub const TAPE_API_VERSION: u32 = (\d+);", text).group(1))


def build_time():
    forced = os.environ.get("FOS_BUILD_TIME")
    if forced:
        return int(forced)
    try:
        dirty = subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip()
        if not dirty:
            return int(subprocess.run(["git", "log", "-1", "--format=%ct"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip())
    except (OSError, subprocess.CalledProcessError, ValueError):
        pass
    return int(time.time())


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: stamp_tape.py IN OUT")
    source, destination = sys.argv[1:]
    stamp = struct.pack("<QI4x", build_time(), abi_version())
    with tempfile.NamedTemporaryFile(delete=False) as handle:
        handle.write(stamp)
    try:
        subprocess.run(
            ["objcopy", "--remove-section", ".fos_build", "--add-section", f".fos_build={handle.name}", "--set-section-flags", ".fos_build=noload,readonly", source, destination + ".tmp"],
            check=True,
        )
        os.replace(destination + ".tmp", destination)
    finally:
        os.unlink(handle.name)


if __name__ == "__main__":
    main()
