#!/usr/bin/env python3
"""Server kernel (mf) on aarch64, first slice: the aarch64 UEFI loader (QEMU virt + AAVMF) starts it, and it reaches
the end of boot and the serial shell: PCI, the virtio disk and its FOSFS volume, physical memory, the MMU page tables, the heap self-test, GIC timer ticks and the scheduler self-tests.
Run `make test-mf-aarch64-boot`."""
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_aarch64_boot as base  # noqa: E402

IMAGE = Path(os.environ.get("FOSFOR_IMAGE", base.ROOT / "build/fosfor-mf-aarch64/fosfor.img"))


EL0_LINE = re.compile(r"EL0: hello ok, bad pointer ok, page fault ok, stack guard ok, undefined instruction ok, preempted spinner ok, "
                      r"ELF ok, big image ok, NEON state ok, 0 frames not returned\.")


def main() -> None:
    text = base.boot(IMAGE, commands=("version", "cpuinfo", "uptime", "nic", "run hello", "run thrcheck", "run netcheck", "fscheck readme.txt 0 7", "run httpd &0", "io r8 0x60", "usb", "disks"), usb=True, cpus=4,
                     usb_keys=("c", "p", "u", "i", "n", "f", "o", "ret"))
    base.check("server kernel (mf, slice 8): running from the UEFI loader at EL" in text, "the mf kernel starts on aarch64", text)
    base.check("Physical memory:" in text, "the physical allocator is up", text)
    base.check("Page tables: loaded" in text, "the MMU is on with our page tables", text)
    base.check("Heap: self-test passed" in text, "the heap works", text)
    ticks = re.search(r"(\d+) timer interrupts", text)
    base.check(ticks is not None and 8 <= int(ticks.group(1)) <= 12, "the GIC delivers timer ticks", text)
    base.check("Scheduler: 3 kernel threads took 15 interleaved steps" in text and " 0 stack frame(s) not returned" in text, "kernel threads switch and their stacks are returned", text)
    base.check("preemption ok" in text and "sleep ok" in text and "mutex ok" in text, "timer preemption, sleep and mutex work", text)
    base.check("SMP: 4 CPU(s) online (3 other started, 0 failed)." in text and "wake IPI ok" in text, "PSCI starts three more CPUs at EL1; threads run on all of them and the wake IPI works", text)
    base.check(EL0_LINE.search(text) is not None, "EL0 processes: system calls, faults contained, preemption, an ELF, NEON state across switches", text)
    base.check("reached the end of boot" in text, "boot reaches its end", text)
    base.check("PCI: config space at" in text and "MSI: GICv2m frame" in text, "PCI (ECAM) and MSI are up", text)
    base.check("00:02.0 1af4:1001" in text, "PCI lists the virtio disk", text)
    base.check("disk1: " in text and "(virtio 0)" in text, "the virtio disk is found", text)
    base.check("FOSFS: mounted on disk1" in text, "the boot disk's FOSFS volume is mounted", text)
    base.check("USB: 1 xHCI, 0 EHCI and 0 OHCI controller(s), 2 usable device(s)" in text, "xHCI finds the keyboard and the stick", text)
    base.check("USB keyboard 0627:0001" in text and "1 USB keyboard(s)" in text, "`usb` lists the keyboard", text)
    base.check(re.search(r"^disk2: .*USB", text, re.M) is not None or "USB storage 46f4:0001" in text, "the USB stick is a disk", text)
    base.check(text.count("features    fp asimd") >= 2, "keys typed on the USB keyboard reach the shell", text)
    base.check("fos> " in text, "the shell prompt appears on the serial console", text)
    base.check("server kernel (mf)" in text.split("fos> ", 1)[-1], "`version` answers", text)
    base.check("implementer" in text and "features" in text, "`cpuinfo` reads the ID registers", text)
    base.check(re.search(r"\d+ s$", text, re.M) is not None, "`uptime` answers", text)
    base.check("virtio-net at 00:01.0: MAC" in text, "the virtio-net card is brought up", text)
    base.check("nic1: virtio-net" in text, "`nic` lists it", text)
    shell = text.split("fos> ", 1)[-1]
    base.check("thrcheck: PASS threads share memory" in shell, "tape threads, futexes and a kill with sleeping and spinning threads work at EL0", text)
    base.check("netcheck: 4/4 replies." in shell, "the netcheck tape gets a DHCP lease and pings through the tape network calls", text)
    base.check("fscheck: 7 bytes [Welcome]" in shell, "fs_read_at reads a byte range from a tape", text)
    base.check("httpd: NIC 1 at 10.0.2.15:80" in shell, "httpd starts as a service with its worker threads", text)
    base.check("no file `hello`" not in shell and "first file: readme.txt" in shell, "the boot volume's tapes are found from the shell thread (DMA from a stack buffer)", text)
    base.check("hello from a v2 tape" in shell and "primes up to 100000: 9592" in shell and "Process 12 exited with code 0." in shell,
               "an aarch64 tape runs at EL0 through the tape ABI (files, alloc, clock) and exits", text)
    base.check("no I/O port space" in text, "`io` explains there are no ports", text)
    # A firmware that enters at EL2 (a Pi 4): the kernel leaves it for EL1, where EL0 programs can run.
    el2 = base.boot(IMAGE, commands=("cpuinfo", "uptime"), el2=True, cpus=4)
    base.check("from the UEFI loader at EL2" in el2 and "running at EL1" in el2, "entered at EL2, the kernel drops to EL1 and carries on", el2)
    base.check("preemption ok" in el2 and "timer interrupts (INTID 30)" in el2, "the timer and the scheduler work after the drop", el2)
    base.check("SMP: 4 CPU(s) online (3 other started, 0 failed)." in el2 and "Scheduler: SMP ok (8 unpinned threads ran on 4 of 4" in el2, "PSCI starts three more CPUs after the drop from EL2 and they run threads", el2)
    base.check(EL0_LINE.search(el2) is not None, "EL0 processes run after the drop from EL2", el2)
    sd = base.boot(IMAGE, steps=(("disks\r", "fos> "),), sd=True)
    base.check(re.search(r"disk2: \d+ MiB, SD card", sd) is not None, "the SDHCI controller and its card are registered as a disk", sd)


def network() -> None:
    """The guest's virtio-net on a multicast socket network: discovery, then a `term net` session (receive and transmit)."""
    from test_boot import BROADCAST, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST, TAG_HELLO, TAG_DATA
    seen = {}

    def talk(wire) -> None:
        guest = None
        for _ in range(60):
            wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            for source, tag, payload in wire.frames(0.25):
                if tag == TAG_DISCOVER_REPLY:
                    guest, seen["name"] = source, payload.rstrip(b"\0").decode("latin-1")
            if guest:
                break
        base.check(guest is not None, "the guest answers a discovery probe over virtio-net", "")
        wire.send(guest, TAG_HELLO)
        wire.until("fos> ", 10)
        wire.send(guest, TAG_DATA, b"version\n")
        seen["version"] = wire.until("fos> ", 10)

    text = base.boot(IMAGE, wire=talk)
    base.check(seen.get("name", "").startswith("fos-") or seen.get("name"), "the guest announces its name", text)
    base.check("server kernel (mf)" in seen.get("version", ""), "a `term net` session runs `version` over virtio-net", seen.get("version", ""))


if __name__ == "__main__":
    main()
    network()
