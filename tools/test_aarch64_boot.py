#!/usr/bin/env python3
"""aarch64 boot test: the aarch64 UEFI loader (AAVMF firmware, QEMU `virt`) reads the kernel from the payload slot, loads
and relocates it, exits boot services, and the aarch64 kernel prints its BootInfo on the PL011. Needs qemu-system-aarch64
and the AAVMF firmware (apt: qemu-system-arm qemu-efi-aarch64). Run `make test-aarch64-boot`."""
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CODE = os.environ.get("AAVMF_CODE", "/usr/share/AAVMF/AAVMF_CODE.no-secboot.fd")
VARS = os.environ.get("AAVMF_VARS", "/usr/share/AAVMF/AAVMF_VARS.fd")
IMAGE = Path(os.environ.get("FOSFOR_AARCH64_IMAGE", ROOT / "build/fosfor-aarch64/fosfor.img"))


def check(condition, message, text=""):
    if not condition:
        raise SystemExit(f"FAIL: {message}\n--- output ---\n{text[-3500:]}")
    print(f"PASS: {message}")


def boot(image: Path) -> str:
    """Boots `image` under QEMU virt + AAVMF and returns the serial text (the kernel parks, so the timeout is the normal end)."""
    if not shutil.which("qemu-system-aarch64") or not Path(CODE).exists():
        raise SystemExit("qemu-system-aarch64 or the AAVMF firmware is missing (apt install qemu-system-arm qemu-efi-aarch64)")
    with tempfile.TemporaryDirectory() as scratch:
        variables = Path(scratch) / "vars.fd"
        shutil.copyfile(VARS, variables)
        try:
            run = subprocess.run(
                ["qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72", "-m", "1G", "-display", "none", "-serial", "stdio", "-no-reboot",
                 "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"if=pflash,format=raw,file={variables}",
                 "-drive", f"file={image},format=raw,if=virtio,snapshot=on"],
                capture_output=True, timeout=45)
            output = run.stdout
        except subprocess.TimeoutExpired as expired:
            output = expired.stdout or b""  # the kernel parks; the timeout is the normal end
    return re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", output.decode("utf-8", "replace")).replace("\r", "")


def main():
    text = boot(IMAGE)
    check("Fosfor OS loader" in text, "the aarch64 loader runs under AAVMF", text)
    check("Kernel image read" in text and "Loading its ELF64 segments" in text, "the loader reads the kernel from the payload slot", text)
    check("Fosfor OS kernel (aarch64): running from the UEFI loader." in text, "the kernel runs after ExitBootServices (instruction cache synced)", text)
    match = re.search(r"BootInfo v5: (\d+) memory-map entries, kernel at (0x[0-9a-f]+) \((\d+) KiB\), slot 0, ACPI RSDP (0x[0-9a-f]+)", text)
    check(match is not None and int(match.group(1)) > 10 and match.group(4) != "0x0", "the kernel reads a valid BootInfo (memory map and ACPI RSDP)", text)
    timer = re.search(r"Generic timer: (\d+) Hz, ticks \d+; a 100 ms wait took (\d+) us", text)
    check(timer is not None and 90_000 <= int(timer.group(2)) <= 300_000, "the generic timer counts: a 100 ms busy wait measures about 100 ms", text)
    check("Page tables loaded" in text and re.search(r"Physical memory: \d+ MiB managed", text) is not None, "the kernel builds and loads its own page tables (MMU, W^X, device map)", text)
    check("PANIC" not in text and "FATAL" not in text and "EXCEPTION" not in text, "no panic, no loader fatal, no exception", text)
    print("aarch64 boot: all checks passed")


if __name__ == "__main__":
    sys.exit(main())
