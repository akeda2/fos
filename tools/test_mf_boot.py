#!/usr/bin/env python3
"""Boots the server kernel image (build/fosfor-mf/fosfor.img, `make image-mf`) under QEMU and checks slices 1 and 2 of
docs/big-kernel.md: the loader hands over a valid BootInfo, the kernel narrates over COM1, reports a sane memory map,
builds its frame allocator, page tables (direct map, W^X image) and heap and reaches the end of boot, with 1, 2 and 8 CPUs. It also checks the flavour marker in the ELF (`.fos_flavour` says "mf") and
that the embedded kernel's ELF says "embedded". `make test-mf-boot` runs it through UEFI and, with FOS_FIRMWARE=bios and the
qemu shim, through BIOS."""

import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF", ROOT / "build" / "fosfor-mf" / "fosfor.img"))
TARGET = ROOT / "build" / "cargo-target" / "x86_64-fos-kernel" / "release"
CODE_CANDIDATES = ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                   "/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd"]
VARS_CANDIDATES = ["/usr/share/OVMF/OVMF_VARS_4M.fd", "/usr/share/OVMF/OVMF_VARS.fd",
                   "/usr/share/edk2/ovmf/OVMF_VARS.fd", "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"]
BIOS = os.environ.get("FOS_FIRMWARE") == "bios"
FINAL = "slice 3 reached the end of boot"


def find(variable, candidates):
    explicit = os.environ.get(variable)
    for path in ([explicit] if explicit else []) + candidates:
        if path and Path(path).exists():
            return path
    raise SystemExit(f"no OVMF firmware image found (set {variable}); tried {candidates}")


def check(condition, message, text=""):
    if not condition:
        raise SystemExit(f"FAIL: {message}\n--- output ---\n{text[-3500:]}")
    print(f"PASS: {message}")


def clean(raw: bytes) -> str:
    return re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", raw.decode("utf-8", "replace")).replace("\r", "")


def section(path: Path, name: str):
    from_abi = subprocess.run(["objcopy", "-O", "binary", f"--only-section={name}", str(path), "/dev/stdout"], capture_output=True)
    return from_abi.stdout if from_abi.returncode == 0 else None


def boot(cpus: int, numa: bool = False) -> str:
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-smp", str(cpus), "-display", "none",
                   "-serial", "stdio", "-monitor", "none", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}",
                   "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={IMAGE},snapshot=on"]
        if numa:
            half = cpus // 2
            command += ["-object", "memory-backend-ram,id=m0,size=256M", "-object", "memory-backend-ram,id=m1,size=256M",
                        "-numa", f"node,nodeid=0,cpus=0-{half - 1},memdev=m0", "-numa", f"node,nodeid=1,cpus={half}-{cpus - 1},memdev=m1"]
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 90
        try:
            while FINAL not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: -smp {cpus}: the server kernel never reached the end of boot\n{clean(bytes(raw))[-3500:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))
            time.sleep(0.3)
            ready, _, _ = select.select([process.stdout], [], [], 0)
            if ready:
                raw.extend(os.read(process.stdout.fileno(), 4096))
        finally:
            process.kill()
            process.wait()
        return clean(bytes(raw))


def main():
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} missing; run `make image-mf` first")
    for cpus in (1, 2, 8):
        text = boot(cpus)
        check("Fosfor OS server kernel (mf, slice 3)" in text, f"-smp {cpus}: the server kernel announces itself", text)
        check("BootInfo: version" in text and "is not valid" not in text, f"-smp {cpus}: the loader's BootInfo is valid", text)
        match = re.search(r"Memory map: (\d+) regions, (\d+) MiB of RAM-like memory, (\d+) MiB conventional", text)
        check(match is not None and int(match.group(2)) >= 400 and int(match.group(3)) >= 100,
              f"-smp {cpus}: the memory map shows the guest's RAM", text)
        frames = re.search(r"Physical memory: (\d+) MiB managed in (\d+) frames, (\d+) MiB free", text)
        check(frames is not None and int(frames.group(1)) >= 300 and int(frames.group(3)) >= 300
              and int(frames.group(2)) >= int(frames.group(1)) * 256 - 1,
              f"-smp {cpus}: the frame allocator manages the guest's RAM", text)
        direct = re.search(r"direct map of (\d+) MiB at 0xffff800000000000", text)
        check(direct is not None and int(direct.group(1)) >= 400, f"-smp {cpus}: the direct map covers RAM in the upper half", text)
        check("W^X holds" in text, f"-smp {cpus}: the kernel image is W^X with unmapped guards and page 0", text)
        check("written and read back at 0xffff8000" in text, f"-smp {cpus}: a frame is reachable through the direct map", text)
        check("Heap: self-test passed" in text, f"-smp {cpus}: the heap passes its self-test (including growth)", text)
        check("a breakpoint came back through the IDT" in text, f"-smp {cpus}: the GDT/TSS/IDT work (int3 handled and resumed)", text)
        acpi = re.search(r"ACPI: revision \d, local APIC at 0x[0-9a-f]+, (\d+) CPU\(s\), (\d+) I/O APIC", text)
        check(acpi is not None and int(acpi.group(1)) == cpus and int(acpi.group(2)) >= 1, f"-smp {cpus}: the MADT lists {cpus} CPU(s)", text)
        check(re.search(r"I/O APIC: \d+ pins, all masked", text) is not None, f"-smp {cpus}: the I/O APIC is found and masked", text)
        ticks = re.search(r"APIC timer: 100 Hz tick, (\d+) ticks in 200 ms", text)
        check(ticks is not None and 15 <= int(ticks.group(1)) <= 25, f"-smp {cpus}: the calibrated APIC timer ticks at 100 Hz", text)
        check("PANIC" not in text, f"-smp {cpus}: no panic", text)
    text = boot(4, numa=True)
    check(re.search(r"SRAT: 2 NUMA node\(s\), \d+ memory range\(s\), 4 CPU affinity", text) is not None, "-numa: the SRAT shows two nodes and four CPUs", text)
    check("PANIC" not in text and "CPU exception" not in text, "-numa: no panic", text)
    flavour = section(TARGET / "fosfor-kernel-mf", ".fos_flavour")
    check(flavour == b"mf", f"the server kernel's ELF says mf (got {flavour!r})")
    embedded = section(TARGET / "fosfor-kernel", ".fos_flavour")
    check(embedded == b"embedded", f"the embedded kernel's ELF says embedded (got {embedded!r})")


main()
