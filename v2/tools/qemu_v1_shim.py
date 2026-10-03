#!/usr/bin/env python3
"""A `qemu-system-i386` stand-in for running v1 end-to-end tests against the v2 kernel.

Put a directory holding a copy of this script named `qemu-system-i386` first on PATH (see
`make test-v2-router`). It rewrites the v1 command line into the v2 one: x86-64, the q35
machine, OVMF firmware (code read-only, a private copy of the variable store), and the
disk as given. `-snapshot` keeps the shared image unmodified. Everything else (serial,
monitor, netdevs, -m) passes through unchanged, so the v1 test sees a machine that behaves
the same on the wire and on the serial port."""

import os
import shutil
import sys
import tempfile

CODE = next(p for p in ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_CODE.fd") if os.path.exists(p))
VARS = next(p for p in ("/usr/share/OVMF/OVMF_VARS_4M.fd", "/usr/share/OVMF/OVMF_VARS.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd") if os.path.exists(p))

arguments = sys.argv[1:]
variables = os.path.join(tempfile.mkdtemp(prefix="fos-v2-vars-"), "vars.fd")
shutil.copy(VARS, variables)
command = ["qemu-system-x86_64", "-machine", "q35",
           "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}",
           "-drive", f"if=pflash,format=raw,file={variables}"]
# v2 images are bigger and use 512 MiB-class memory comfortably; keep -m as the test asked.
os.execvp(command[0], command + arguments)
