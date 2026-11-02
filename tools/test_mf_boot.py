#!/usr/bin/env python3
"""Boots the server kernel image (build/fosfor-mf/fosfor.img, `make image-mf`) under QEMU and checks slices 1-4 of
docs/big-kernel.md: the loader hands over a valid BootInfo, the kernel narrates over COM1, reports a sane memory map,
builds its frame allocator, page tables (direct map, W^X image) and heap, brings up the interrupts and APIC timer, starts every
other CPU (slice 4) on a stack from its own NUMA node and reaches the end of boot, with 1, 2 and 8 CPUs. It also checks the flavour marker in the ELF (`.fos_flavour` says "mf") and
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
FINAL = "slice 8 reached the end of boot"


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
        check("Fosfor OS server kernel (mf, slice 8)" in text, f"-smp {cpus}: the server kernel announces itself", text)
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
        check("Lower half: empty apart from the kernel image, no identity alias" in text, f"-smp {cpus}: the identity alias is gone (drivers use the direct map)", text)
        check("Heap: self-test passed" in text, f"-smp {cpus}: the heap passes its self-test (including growth)", text)
        check("a breakpoint came back through the IDT" in text, f"-smp {cpus}: the GDT/TSS/IDT work (int3 handled and resumed)", text)
        acpi = re.search(r"ACPI: revision \d, local APIC at 0x[0-9a-f]+, (\d+) CPU\(s\), (\d+) I/O APIC", text)
        check(acpi is not None and int(acpi.group(1)) == cpus and int(acpi.group(2)) >= 1, f"-smp {cpus}: the MADT lists {cpus} CPU(s)", text)
        check(re.search(r"I/O APIC: \d+ pins, all masked", text) is not None, f"-smp {cpus}: the I/O APIC is found and masked", text)
        ticks = re.search(r"APIC timer: 100 Hz tick, (\d+) ticks in 200 ms", text)
        check(ticks is not None and 15 <= int(ticks.group(1)) <= 25, f"-smp {cpus}: the calibrated APIC timer ticks at 100 Hz", text)
        check(f"SMP: {cpus} CPU(s) online" in text and "did not start" not in text, f"-smp {cpus}: every processor starts and comes online", text)
        check(len(re.findall(r"^CPU \d+: APIC id \d+, NUMA node \d+", text, re.M)) == cpus, f"-smp {cpus}: one line per CPU", text)
        for line in re.findall(r"^CPU (\d+): (\d+) timer ticks since it came up", text, re.M):
            check(10 <= int(line[1]) <= 80, f"-smp {cpus}: CPU {line[0]}'s own timer ticks at about 100 Hz ({line[1]} ticks)", text)
        pci = re.search(r"PCI: (\d+) function\(s\) found", text)
        check(pci is not None and int(pci.group(1)) >= 4 and re.search(r"^  00:1f\.2 8086:2922 SATA controller", text, re.M) is not None,
              f"-smp {cpus}: the PCI scan finds the machine's devices, the SATA controller among them", text)
        check(re.search(r"^disk1: \d+ MiB, .* \(SATA 0:0\)", text, re.M) is not None, f"-smp {cpus}: kernel-core's AHCI driver finds the boot disk", text)
        check("Boot disk: disk1" in text, f"-smp {cpus}: the payload partition is read through the block layer and the boot disk identified", text)
        check(re.search(r"FOSFS: mounted on disk1 \(\d+ MiB, \d+ file", text) is not None, f"-smp {cpus}: FOSFS mounts on the boot disk", text)
        sched = re.search(r"Scheduler: 3 kernel threads took 15 interleaved steps in (\d+) context switches; 1 thread\(s\) left, 0 stack frame", text)
        check(sched is not None and int(sched.group(1)) >= 15 and "self-test FAILED" not in text, f"-smp {cpus}: kernel threads interleave, exit and give their stacks back", text)
        tlb = re.search(r"TLB: (\d+) shootdown\(s\) so far .* all (\d+) CPU", text)
        check(tlb is not None and int(tlb.group(1)) >= 3 and int(tlb.group(2)) == cpus, f"-smp {cpus}: exited threads' stacks are flushed from every CPU's TLB before reuse", text)
        pre = re.search(r"Scheduler: preemption ok \(\d+ and \d+ spins.*\), sleep ok \(\d+ ticks for 20\), mutex ok \(50 of 50 increments\)", text)
        check(pre is not None, f"-smp {cpus}: the tick preempts threads that never yield; sleep and the sleeping mutex work", text)
        if cpus > 1:
            wake = re.search(r"Scheduler: wake IPI ok \(.* started after (\d+) us on average", text)
            check(wake is not None, f"-smp {cpus}: an idle CPU is woken by IPI to run a new thread (not at its next tick)", text)
        smp_run = re.search(r"Scheduler: SMP ok \((\d+) unpinned threads ran on (\d+) of (\d+) CPU\(s\)\)", text)
        check(smp_run is not None and int(smp_run.group(2)) == cpus == int(smp_run.group(3)), f"-smp {cpus}: unpinned threads run on every CPU", text)
        check("Hello from ring 3!" in text, f"-smp {cpus}: a ring 3 process makes system calls", text)
        check(re.search(r"Ring 3: hello ok, bad pointer ok, page fault ok, stack guard ok, #GP ok, preempted spinner ok, ELF ok, SSE(\+AVX)? state ok, 0 frames not returned\.", text) is not None,
              f"-smp {cpus}: ring 3 processes: syscalls, pointer checks, faults and stack overflows kill only the process, user code is preempted, an ELF is loaded and relocated, SSE/AVX state survives switches, memory returns", text)
        micros = re.search(r"Clock: .* \(microseconds (\d+)\)", text)
        check(micros is not None and int(micros.group(1)) > 150_000, f"-smp {cpus}: the monotonic microsecond clock runs", text)
        check("PANIC" not in text and "CPU exception" not in text, f"-smp {cpus}: no panic", text)
    text = boot(4, numa=True)
    check(re.search(r"SRAT: 2 NUMA node\(s\), \d+ memory range\(s\), 4 CPU affinity", text) is not None, "-numa: the SRAT shows two nodes and four CPUs", text)
    check("PANIC" not in text and "CPU exception" not in text, "-numa: no panic", text)
    check("SMP: 4 CPU(s) online" in text, "-numa: all four processors start", text)
    nodes = {int(a): int(b) for a, b in re.findall(r"^CPU (\d+): APIC id \d+, NUMA node (\d+)", text, re.M)}
    check(nodes == {0: 0, 1: 0, 2: 1, 3: 1}, f"-numa: CPUs are placed on their SRAT nodes ({nodes})", text)
    memory = re.findall(r"^NUMA node (\d+): (\d+) MiB of memory, (\d+) MiB free", text, re.M)
    check(len(memory) == 2 and all(int(m[1]) >= 200 and int(m[2]) >= 100 for m in memory), "-numa: per-node memory and free counts are reported", text)
    flavour = section(TARGET / "fosfor-kernel-mf", ".fos_flavour")
    check(flavour == b"mf", f"the server kernel's ELF says mf (got {flavour!r})")
    embedded = section(TARGET / "fosfor-kernel", ".fos_flavour")
    check(embedded == b"embedded", f"the embedded kernel's ELF says embedded (got {embedded!r})")


main()
