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


def main() -> None:
    text = base.boot(IMAGE, commands=("version", "cpuinfo", "uptime", "nic", "run hello", "io r8 0x60", "usb", "disks"), usb=True,
                     usb_keys=("c", "p", "u", "i", "n", "f", "o", "ret"))
    base.check("server kernel (mf, slice 8): running from the UEFI loader at EL" in text, "the mf kernel starts on aarch64", text)
    base.check("Physical memory:" in text, "the physical allocator is up", text)
    base.check("Page tables: loaded" in text, "the MMU is on with our page tables", text)
    base.check("Heap: self-test passed" in text, "the heap works", text)
    ticks = re.search(r"(\d+) timer interrupts", text)
    base.check(ticks is not None and 8 <= int(ticks.group(1)) <= 12, "the GIC delivers timer ticks", text)
    base.check("Scheduler: 3 kernel threads took 15 interleaved steps" in text and " 0 stack frame(s) not returned" in text, "kernel threads switch and their stacks are returned", text)
    base.check("preemption ok" in text and "sleep ok" in text and "mutex ok" in text, "timer preemption, sleep and mutex work", text)
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
    base.check("cannot start" in text or "no file" in text, "`run` refuses cleanly (no EL0 yet)", text)
    base.check("no I/O port space" in text, "`io` explains there are no ports", text)
    # A firmware that enters at EL2 (a Pi 4): the kernel leaves it for EL1, where EL0 programs can run.
    el2 = base.boot(IMAGE, commands=("cpuinfo", "uptime"), el2=True)
    base.check("from the UEFI loader at EL2" in el2 and "running at EL1" in el2, "entered at EL2, the kernel drops to EL1 and carries on", el2)
    base.check("preemption ok" in el2 and "timer interrupts (INTID 30)" in el2, "the timer and the scheduler work after the drop", el2)
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
