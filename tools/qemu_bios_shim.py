#!/usr/bin/env python3
"""A `qemu-system-x86_64` stand-in that boots the tests' disk images through the BIOS path (boot/bios) instead of UEFI.

Put a directory holding a copy of this script named `qemu-system-x86_64` first on PATH (`make test-bios-NAME` does).
It removes the OVMF firmware drives (`-drive if=pflash,...`) from the command line, so QEMU falls back to its default
firmware, SeaBIOS, which boots the image's MBR; everything else (serial, monitor, NICs, USB, disks, -m) passes through
unchanged. The tests therefore check the same behaviour of the same kernel, booted by the other firmware."""

import os
import sys

arguments = sys.argv[1:]
kept = []
index = 0
while index < len(arguments):
    if arguments[index] == "-drive" and index + 1 < len(arguments) and "if=pflash" in arguments[index + 1]:
        index += 2
        continue
    kept.append(arguments[index])
    index += 1

# The real QEMU: the first one on PATH that is not this directory's copy.
here = os.path.dirname(os.path.abspath(__file__))
for directory in os.environ.get("PATH", "").split(os.pathsep):
    candidate = os.path.join(directory, "qemu-system-x86_64")
    if os.path.isfile(candidate) and os.access(candidate, os.X_OK) and os.path.abspath(directory) != here:
        os.execv(candidate, [candidate] + kept)
sys.exit("qemu_bios_shim: no real qemu-system-x86_64 found on PATH")
