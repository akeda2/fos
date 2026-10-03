#!/usr/bin/env python3
"""Boots the v2 loader under QEMU + OVMF and checks what it narrates (milestone
M0): the firmware identity, the memory map, the graphics mode, the ACPI root
pointer, and a clean power-off through the UEFI runtime services.

OVMF mirrors the firmware console to the serial port, so the narration arrives on
`-serial stdio` like every other test's output here (with escape sequences, which
are stripped before matching). The firmware images are looked up in the usual
places or in FOS_OVMF_CODE / FOS_OVMF_VARS.

Run with `make test-v2-loader`."""

import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
IMAGE = Path(os.environ.get("FOS_V2_IMAGE", ROOT / "build" / "v2" / "v2.img"))
CODE_CANDIDATES = ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                   "/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd"]
VARS_CANDIDATES = ["/usr/share/OVMF/OVMF_VARS_4M.fd", "/usr/share/OVMF/OVMF_VARS.fd",
                   "/usr/share/edk2/ovmf/OVMF_VARS.fd", "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"]


def find(variable, candidates):
    explicit = os.environ.get(variable)
    for path in ([explicit] if explicit else []) + candidates:
        if path and Path(path).exists():
            return path
    raise SystemExit(f"no OVMF firmware image found (set {variable}); tried {candidates}")


def check(condition, message, text):
    if not condition:
        raise SystemExit(f"FAIL: {message}\n--- loader output ---\n{text[-3000:]}")
    print(f"PASS: {message}")


def main():
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} missing; run `make v2-image` first")
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none",
                   "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}",
                   "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={IMAGE}"]
        try:
            run = subprocess.run(command, capture_output=True, timeout=120)
        except subprocess.TimeoutExpired as error:
            raise SystemExit(f"FAIL: the loader never powered the machine off\n{(error.stdout or b'').decode('utf-8', 'replace')[-2000:]}")
    text = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", run.stdout.decode("utf-8", "replace")).replace("\r", "")

    check("FOS v2 loader (milestone M0): UEFI application running." in text, "OVMF boots BOOTX64.EFI from the ESP", text)
    check(re.search(r"UEFI specification \d+\.\d+", text) is not None, "it reports the UEFI specification version", text)
    entries = re.search(r"Memory map: (\d+) entries of (\d+) bytes", text)
    check(entries is not None and int(entries.group(1)) > 10, "it reads the memory map", text)
    check(re.search(r"\d+ MiB  usable RAM", text) is not None, "and totals the usable RAM", text)
    reusable = re.search(r"Reusable after ExitBootServices: (\d+) MiB", text)
    check(reusable is not None and 300 < int(reusable.group(1)) < 520, f"which is most of the 512 MiB guest ({reusable and reusable.group(1)} MiB)", text)
    check(re.search(r"Graphics: \d+x\d+ (RGB|BGR)", text) is not None and "framebuffer at 0x" in text, "it finds the GOP framebuffer", text)
    check("ACPI 2.0+ RSDP at" in text and "signature valid" in text, "it finds a valid ACPI 2.0+ RSDP", text)
    check("M0 complete" in text and run.returncode == 0, "it powers off through the runtime services", text)
    print("v2 loader (M0): all checks passed")


if __name__ == "__main__":
    main()
