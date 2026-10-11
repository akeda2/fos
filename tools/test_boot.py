#!/usr/bin/env python3
"""Boots the v2 image under QEMU + OVMF and checks the whole chain (milestones M0,
M1, M2 and M3): the loader narrates the firmware and finds the payload partition, reads
the kernel from its A/B slot, loads it as a position-independent ELF and leaves the
firmware; the 64-bit kernel then reports its state over serial, and draws on the
framebuffer, which the test verifies from a QEMU screendump (three colour bars
drawn in logical red, green and blue, so a wrong pixel format would show). M2 adds
the physical allocator, the kernel's own page tables, the IDT, the local APIC timer
(checked against the independent ACPI PM timer) and a clean power-off, which the
test requires: QEMU must exit by itself. M3 adds PCI, the e1000 driver and the
network terminal: the test joins the guest's QEMU multicast network, discovers it,
opens a `term net` session (the v1 wire protocol), runs shell commands and finally
sends `shutdown` over the wire.

OVMF mirrors the firmware console to the serial port, so loader and kernel
narration arrive on the same `-serial` stream (escape sequences stripped before
matching). Firmware images come from the usual places or FOS_OVMF_CODE /
FOS_OVMF_VARS. Run with `make test`."""

import os
import re
import select
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE", ROOT / "build" / "fosfor" / "fosfor.img"))
CODE_CANDIDATES = ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                   "/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd"]
VARS_CANDIDATES = ["/usr/share/OVMF/OVMF_VARS_4M.fd", "/usr/share/OVMF/OVMF_VARS.fd",
                   "/usr/share/edk2/ovmf/OVMF_VARS.fd", "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"]
FINAL_MARKER = "M3 complete"
# `make test-bios-boot` runs this same test with a qemu shim that drops the OVMF drives (tools/qemu_bios_shim.py), so the
# image is booted through boot/bios instead of the UEFI loader: the loader's own narration and the framebuffer are
# different there, everything the kernel and the shell do must be the same.
BIOS = os.environ.get("FOS_FIRMWARE") == "bios"
# What each boot path prints once per boot, for the tests that count boots.
LOADER_BANNER = "Fosfor stage 2 (BIOS path)" if BIOS else "Fosfor OS loader (milestone M1)"
from testslot import port  # noqa: E402
MCAST_GROUP, MCAST_PORT = "230.42.42.44", port(4244)
HOST_MAC = bytes.fromhex("02deadbeef03")
BROADCAST = b"\xff" * 6
ETHERTYPE = b"\x88\xb6"
TAG_DISCOVER_REQUEST, TAG_DISCOVER_REPLY, TAG_HELLO, TAG_DATA, TAG_BYE = 0x02, 0x03, 0x20, 0x21, 0x22


class Wire:
    """The host's end of the guest's QEMU `socket,mcast=` network: one raw Ethernet
    frame per UDP datagram, so no privileges are needed."""

    def __init__(self):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", MCAST_PORT))
        membership = struct.pack("4sl", socket.inet_aton(MCAST_GROUP), socket.INADDR_ANY)
        self.sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, membership)
        self.sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)

    def send(self, destination, tag, payload=b""):
        frame = destination + HOST_MAC + ETHERTYPE + bytes([tag]) + payload
        self.sock.sendto(frame.ljust(60, b"\0"), (MCAST_GROUP, MCAST_PORT))

    def frames(self, timeout):
        """Yields (source, tag, payload) of fos frames addressed to us or broadcast, reading
        for up to `timeout` seconds. (The multicast group echoes our own datagrams back, so
        a single read can be our own frame: keep reading until the time is up.)"""
        end = time.monotonic() + timeout
        while True:
            remaining = end - time.monotonic()
            if remaining <= 0:
                return
            self.sock.settimeout(remaining)
            try:
                packet = self.sock.recv(2048)
            except socket.timeout:
                return
            if len(packet) >= 15 and packet[6:12] != HOST_MAC and packet[12:14] == ETHERTYPE and packet[:6] in (HOST_MAC, BROADCAST):
                yield packet[6:12], packet[14], packet[15:]

    def until(self, marker, timeout, expect_tag=TAG_DATA):
        """Collects DATA payload until `marker` appears in it; returns it as text."""
        deadline = time.monotonic() + timeout
        collected = bytearray()
        while marker.encode() not in collected:
            if time.monotonic() > deadline:
                raise SystemExit(f"FAIL: terminal output never contained {marker!r}; got:\n{collected.decode('utf-8', 'replace')}")
            for _, tag, payload in self.frames(0.2):
                if tag == expect_tag:
                    collected.extend(payload.rstrip(b"\0") if expect_tag != TAG_DATA else payload)
        return collected.decode("utf-8", "replace")



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


def monitor(path: str, command: str):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(5)
        for _ in range(100):
            try:
                sock.connect(path)
                break
            except OSError:
                time.sleep(0.05)
        sock.sendall(command.encode() + b"\n")
        time.sleep(0.5)


def monitor_output(path: str, command: str) -> str:
    """Sends one human-monitor command and returns what the monitor printed in answer."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(2)
        for _ in range(100):
            try:
                sock.connect(path)
                break
            except OSError:
                time.sleep(0.05)
        sock.sendall(command.encode() + b"\n")
        answer = bytearray()
        end = time.monotonic() + 3
        while time.monotonic() < end:
            try:
                chunk = sock.recv(4096)
            except socket.timeout:
                break
            if not chunk:
                break
            answer.extend(chunk)
            if answer.rstrip().endswith(b"(qemu)") and command.encode() in answer:
                break
        return answer.decode("utf-8", "replace")


def read_ppm(path: Path):
    data = path.read_bytes()
    match = re.match(rb"P6\s+(\d+)\s+(\d+)\s+(\d+)\s", data)
    width, height = int(match.group(1)), int(match.group(2))
    pixels = data[match.end():]

    def pixel(x, y):
        offset = (y * width + x) * 3
        return tuple(pixels[offset:offset + 3])

    return width, height, pixel


def main():
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} missing; run `make image` first")
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        monitor_path = str(Path(tmp) / "monitor")
        shot = Path(tmp) / "screen.ppm"
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none",
                   "-serial", "stdio", "-monitor", f"unix:{monitor_path},server,nowait", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}",
                   "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={IMAGE},snapshot=on",
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
                   "-device", "e1000,netdev=net0,mac=52:54:00:12:34:77"]
        wire = Wire()
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 90
        try:
            while FINAL_MARKER not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: the kernel never reported '{FINAL_MARKER}'\n{clean(bytes(raw))[-3500:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))
            time.sleep(0.3)
            text = clean(bytes(raw))
            pic = monitor_output(monitor_path, "info pic")
            monitor(monitor_path, f"screendump {shot}")
            for _ in range(50):
                if shot.exists() and shot.stat().st_size > 1000:
                    break
                time.sleep(0.1)
            time.sleep(0.3)
            width, height, pixel = read_ppm(shot) if shot.exists() else (0, 0, None)
            # Over the wire: discovery, a terminal session, commands, then shutdown.
            wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            reply = None
            for _ in range(30):
                for source, tag, payload in wire.frames(0.2):
                    if tag == TAG_DISCOVER_REPLY:
                        reply = (source, payload.rstrip(b"\0"))
                if reply:
                    break
            guest = reply[0] if reply else None
            wire.send(guest or BROADCAST, TAG_HELLO)
            session = wire.until("fos> ", 10)
            transcript = {}
            for name in ("help", "memory", "pci", "nic", "uptime", "disks", "ls", "cat readme.txt", "write hello.txt written over the wire", "cat hello.txt", "ls", "rm hello.txt", "cat hello.txt"):
                wire.send(guest, TAG_DATA, name.encode() + b"\n")
                output = wire.until("fos> ", 10)
                # A command may be run twice (`ls`, `cat hello.txt`): later runs are "name#2".
                transcript[name if name not in transcript else name + "#2"] = output
            wire.send(guest, TAG_DATA, b"shutdown\n")
            exit_deadline = time.monotonic() + 20
            while process.poll() is None and time.monotonic() < exit_deadline:
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))
            exited_by_itself = process.poll() == 0
            text = clean(bytes(raw))
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()

    # --- the loader (M0 + M1) ---
    if BIOS:
        check("Fosfor stage 2 (BIOS path): real mode, 16-bit." in text, "SeaBIOS boots the MBR and stage two", text)
        check("A20 line: ok" in text, "stage two enables and verifies A20", text)
        entries = re.findall(r"^    0x[0-9A-F]{16} \+ 0x[0-9A-F]{16} type \d\d", text, re.M)
        check(len(entries) >= 4, f"it reads the BIOS memory map ({len(entries)} E820 entries)", text)
        check(re.search(r"ACPI RSDP: [0-9A-F]{8}", text) is not None, "and finds the ACPI RSDP in BIOS memory", text)
        check(re.search(r"looking for the payload partition\.\.\. found at LBA 0x[0-9A-F]{8}, 0x[0-9A-F]{8}", text) is not None, "it finds the payload partition by its GPT type", text)
        check("Slot metadata: booting slot 0\r\n" in text or "Slot metadata: booting slot 0\n" in text, "reads the slot metadata and runs the A/B state machine", text)
        check("copying it above 1 MiB (unreal mode): ok" in text and "Entering long mode" in text, "reads the kernel slot, copies it above 1 MiB and enters long mode", text)
        kernel = re.search(r"BootInfo: version 5, kernel at (0x[0-9a-f]+)", text)
        check(kernel is not None, "places the kernel ELF at its base address", text)
    else:
        check("Fosfor OS loader (milestone M1): UEFI application running." in text, "OVMF boots BOOTX64.EFI from the ESP", text)
        check(re.search(r"UEFI specification \d+\.\d+", text) is not None, "the loader reports the UEFI version", text)
        entries = re.search(r"Memory map: (\d+) entries", text)
        check(entries is not None and int(entries.group(1)) > 10, "it reads the memory map", text)
        reusable = re.search(r"Reusable after ExitBootServices: (\d+) MiB", text)
        check(reusable is not None and 300 < int(reusable.group(1)) < 520, f"most of the 512 MiB guest is reusable ({reusable and reusable.group(1)} MiB)", text)
        check(re.search(r"Graphics: \d+x\d+ (RGB|BGR)", text) is not None, "it finds the GOP framebuffer", text)
        check("ACPI 2.0+ RSDP at" in text and "signature valid" in text, "and a valid ACPI 2.0+ RSDP", text)
        check(re.search(r"Payload partition: LBA \d+ \+ \d+ sectors", text) is not None, "it finds the payload partition by its GPT type", text)
        check(re.search(r"Slot metadata: booting slot 0 \(active 0, pending None", text) is not None, "reads the slot metadata", text)
        kernel = re.search(r"Kernel at (0x[0-9a-f]+), (\d+) KiB, entry (0x[0-9a-f]+)", text)
        check(kernel is not None, "loads the kernel ELF at an address of its own choosing", text)
        check("Exiting boot services" in text, "and exits boot services", text)


    # --- the kernel (M1) ---
    check("Fosfor OS kernel (milestone M4): running in 64-bit long mode." in text, "the kernel starts in long mode", text)
    check(re.search(r"BootInfo: version 5, kernel at " + kernel.group(1), text) is not None, "BootInfo arrives intact and names the same load address", text)
    managed = re.search(r"Physical memory: (\d+) MiB managed, (\d+) MiB free", text)
    check(managed is not None and 400 < int(managed.group(1)) <= 512 and int(managed.group(2)) > 300, f"the physical allocator manages the machine's RAM ({managed and managed.groups()} MiB)", text)
    check("Allocator self-test: ok" in text, "the allocator's bookkeeping, contiguity and below-4-GiB promises hold", text)
    check("GDT + TSS loaded" in text and "IDT loaded: 256 gates" in text, "the kernel installs its own GDT, TSS and IDT", text)
    check("NX and write-protect on" in text and "stack guard pages unmapped" in text, "and its own page tables with NX, read-only text and guard pages", text)
    check(re.search(r"Framebuffer console ready: \d+ columns x \d+ rows, mapped (write-combining|uncached)", text) is not None, "the framebuffer console comes up", text)
    if BIOS:
        check(re.search(r"Video: VBE 0x[0-9A-F]{4}, mode 0x[0-9A-F]{4}: \d+x\d+ 32 bpp, linear framebuffer at 0x[0-9A-F]{8}", text) is not None, "stage two sets a VBE linear-framebuffer mode and says which", text)
    check(re.search(r"ACPI (1\.0|2\.0\+): PM timer at port 0x[0-9a-f]+ \(\d+-bit\), local APIC at 0x[0-9a-f]+", text) is not None, "the kernel reads the ACPI FADT and MADT", text)
    check("ticking at 100 Hz" in text, "the local APIC timer is calibrated and running", text)
    check("1 breakpoint(s) delivered and handled" in text, "a breakpoint exception is delivered and handled", text)
    check("Legacy PIC: remapped to vectors 0x30-0x3f and masked; a spurious IRQ7 (1 injected) is ignored." in text,
          "a spurious legacy IRQ7 (vector 0x37) is ignored, not taken for a fault", text)
    # The controllers themselves, as QEMU models them: re-initialised away from the exception vectors and fully masked.
    check("pic0:" in pic and "irq_base=30" in pic and "pic1:" in pic and "irq_base=38" in pic and pic.count("imr=ff") == 2,
          "and the 8259 pair is remapped to 0x30/0x38 with every line masked (BIOS defaults: 0x08/0x70)", pic)
    milliseconds = re.search(r"100 ticks took (\d+) ms by the PM timer", text)
    check(milliseconds is not None and 900 <= int(milliseconds.group(1)) <= 1100, f"100 timer ticks take about a second by the independent PM timer ({milliseconds and milliseconds.group(1)} ms)", text)
    check(re.search(r"PCI: \d+ functions found", text) is not None, "the kernel scans PCI", text)
    check(re.search(r"e1000 \(\w+\) at [0-9a-f:.]+: MAC 52:54:00:12:34:77, link up", text) is not None, "and brings up the e1000 with its link", text)
    check(reply is not None and reply[1] == b"fos-123477" and guest == bytes.fromhex("525400123477"), f"a discovery probe over the wire is answered by 'fos-123477' ({reply})", text)
    check("Remote terminal connected" in text and "fos> " in session and "Type `help`" in session, "a term net session opens (HELLO answered with banner and prompt)", session)
    check("shutdown" in transcript["help"] and "dmesg" in transcript["help"], "`help` output arrives over the wire in DATA frames", transcript["help"])
    check(re.search(r"Managed:\s+\d+ MiB", transcript["memory"]) is not None, "`memory` reports the allocator", transcript["memory"])
    check("8086:100e network controller" in transcript["pci"], "`pci` lists the e1000 among the scanned functions", transcript["pci"])
    check("link up" in transcript["nic"] and "52:54:00:12:34:77" in transcript["nic"], "`nic` reports the interface", transcript["nic"])
    check(re.search(r"\d+ ticks at 100 Hz", transcript["uptime"]) is not None, "`uptime` reports the timer", transcript["uptime"])
    check(re.search(r"disk1: \d+ MiB, QEMU HARDDISK \(SATA 0:\d+\)", text) is not None, "the AHCI driver finds the SATA disk and identifies it", text)
    check("Boot disk: disk1" in text, "boot-disk discovery matches the payload partition GUID the loader passed", text)
    check(re.search(r"FOSFS: mounted on disk1 \(\d+ MiB, \d+ file\(s\)\)", text) is not None, "the FOSFS data partition on the boot disk is mounted", text)
    check("boot disk: disk1" in transcript["disks"], "`disks` shows the boot disk", transcript["disks"])
    check("readme.txt" in transcript["ls"], "`ls` lists the preloaded file", transcript["ls"])
    check("Welcome to FOSFS on Fosfor OS" in transcript["cat readme.txt"], "`cat` reads it from the disk through the AHCI driver", transcript["cat readme.txt"])
    check("Wrote 21 bytes to hello.txt" in transcript["write hello.txt written over the wire"] and "written over the wire" in transcript["cat hello.txt"], "`write` stores a file and `cat` reads it back (DMA write + flush)", transcript["cat hello.txt"])
    check("hello.txt" in transcript["ls#2"], "the new file shows in the directory", transcript["ls#2"])
    check("no such file" in transcript["cat hello.txt#2"], "`rm` removes it again", transcript["cat hello.txt#2"])
    check(exited_by_itself, "`shutdown` over the network powers the machine off through the UEFI runtime services", text)

    # --- the framebuffer, as QEMU displays it ---
    check(pixel is not None and width >= 640, f"QEMU produced a screendump ({width}x{height})", text)
    bar_y = height - 48 + 16
    check(pixel(32, bar_y) == (255, 0, 0), f"red bar is red {pixel(32, bar_y)}", text)
    check(pixel(112, bar_y) == (0, 255, 0), f"green bar is green {pixel(112, bar_y)}", text)
    check(pixel(192, bar_y) == (0, 0, 255), f"blue bar is blue {pixel(192, bar_y)}", text)
    check(pixel(width - 10, height // 2) == (0, 0, 0), f"the background fill is the console colour {pixel(width - 10, height // 2)}", text)
    lit = sum(1 for y in range(0, 16 * 12) for x in range(0, 8 * 60) if pixel(x, y) == (0xc0, 0xc0, 0xc0))
    check(lit > 400, f"text is drawn on screen ({lit} foreground pixels in the first rows)", text)
    print("boot via BIOS: all checks passed" if BIOS else "boot (M0-M3): all checks passed")


if __name__ == "__main__":
    main()
