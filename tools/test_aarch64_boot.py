#!/usr/bin/env python3
"""aarch64 boot test: the aarch64 UEFI loader (AAVMF firmware, QEMU `virt`) reads the kernel from the payload slot, loads
and relocates it, exits boot services, and the aarch64 kernel prints its BootInfo on the PL011. Needs qemu-system-aarch64
and the AAVMF firmware (apt: qemu-system-arm qemu-efi-aarch64). Run `make test-aarch64-boot`."""
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CODE = os.environ.get("AAVMF_CODE", "/usr/share/AAVMF/AAVMF_CODE.no-secboot.fd")
VARS = os.environ.get("AAVMF_VARS", "/usr/share/AAVMF/AAVMF_VARS.fd")
IMAGE = Path(os.environ.get("FOSFOR_AARCH64_IMAGE", ROOT / "build/fosfor-aarch64/fosfor.img"))


def check(condition, message, text=""):
    if not condition:
        raise SystemExit(f"FAIL: {message}\n--- output ---\n{text[-3500:]}")
    print(f"PASS: {message}")


def boot(image: Path, screendump: Path | None = None, commands: tuple[str, ...] = (), usb_keys: tuple[str, ...] = (), during: tuple | None = None, steps: tuple[tuple[str, str], ...] = (), sd: bool = False, fat: bool = False, usb: bool = False, big: bool = False) -> str:
    """Boots `image` under QEMU virt + AAVMF + ramfb and returns the serial text. The kernel parks, so the run ends
    when the parking message shows (or after 60 s); a PPM screenshot goes to `screendump` when given."""
    if not shutil.which("qemu-system-aarch64") or not Path(CODE).exists():
        raise SystemExit("qemu-system-aarch64 or the AAVMF firmware is missing (apt install qemu-system-arm qemu-efi-aarch64)")
    with tempfile.TemporaryDirectory() as scratch:
        variables = Path(scratch) / "vars.fd"
        shutil.copyfile(VARS, variables)
        log, monitor = Path(scratch) / "serial.txt", Path(scratch) / "mon.sock"
        if sd:
            # QEMU wants an SD card image whose size is a power of two: pad a copy.
            padded = Path(scratch) / "sd.img"
            shutil.copyfile(image, padded)
            size = padded.stat().st_size
            os.truncate(padded, 1 << (size - 1).bit_length())
        # The firmware cannot boot from an SD card, so it boots the virtio disk; the SD card is a second copy (disk2).
        if big:
            # A "stick" much bigger than the image, so `grow` has room (the copy is thrown away with the scratch directory).
            image = Path(scratch) / "big.img"
            shutil.copyfile(IMAGE, image)
            os.truncate(image, 256 << 20)
        disk = ["-drive", f"file={image},format=raw,if=none,id=boot,snapshot=on", "-device", "virtio-blk-pci,drive=boot"]
        if sd:
            disk += ["-drive", f"file={padded},format=raw,if=none,id=card,snapshot=on", "-device", "sdhci-pci", "-device", "sd-card,drive=card"]
        stick = []
        if fat or usb:
            # A PC-formatted FAT32 volume with no partition table (a "superfloppy"), as a third disk.
            fat_image = Path(scratch) / "fat.img"
            with open(fat_image, "wb") as handle:
                handle.truncate(64 << 20)
            subprocess.run(["mkfs.vfat", "-F", "32", "-s", "1", str(fat_image)], check=True, capture_output=True)
            # `usb` puts the same volume on a USB stick (xHCI, Bulk-Only Transport) instead of a second virtio disk.
            disk += ["-drive", f"file={fat_image},format=raw,if=none,id=fat"]
            if usb:
                stick = ["-device", "usb-storage,drive=fat,bus=xhci.0"]
            else:
                disk += ["-device", "virtio-blk-pci,drive=fat"]
        with open(log, "wb") as out:
            qemu = subprocess.Popen(
                ["qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72", "-m", "1G", "-display", "none", "-device", "ramfb",
                 "-serial", "stdio", "-no-reboot", "-monitor", f"unix:{monitor},server,nowait",
                 "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"if=pflash,format=raw,file={variables}",
                 *disk, "-device", "qemu-xhci,id=xhci", "-device", "usb-kbd", *stick],
                stdin=subprocess.PIPE, stdout=out, stderr=subprocess.STDOUT)
            try:
                def wait_for(count: int) -> None:
                    """Waits until the serial text holds `count` shell prompts (or the kernel parked after a fault)."""
                    deadline = time.time() + 60
                    while time.time() < deadline:
                        seen = log.read_bytes().lower()
                        if seen.count(b"fos> ") >= count or b"parking the cpu" in seen or qemu.poll() is not None:
                            return
                        time.sleep(0.3)

                wait_for(1)
                for number, command in enumerate(commands, start=2):
                    qemu.stdin.write(command.encode() + b"\r")
                    qemu.stdin.flush()
                    wait_for(number)
                for text, marker in steps:
                    # Interactive input for a running tape: typed a byte at a time (the PL011 FIFO is 16 deep), then wait for `marker`.
                    start = len(log.read_bytes())
                    for byte in text.encode():
                        qemu.stdin.write(bytes([byte]))
                        qemu.stdin.flush()
                        time.sleep(0.03)
                    deadline = time.time() + 20
                    while time.time() < deadline and marker.encode() not in log.read_bytes()[start:]:
                        time.sleep(0.2)
                if during is not None:
                    # A command whose screen is only there while it runs: type it, wait for its marker, grab the screen.
                    command, marker, shot, *after = during  # `after`: text typed once the screen is grabbed (Esc ends a tape)
                    qemu.stdin.write(command.encode() + b"\r")
                    qemu.stdin.flush()
                    deadline = time.time() + 30
                    while time.time() < deadline and marker.encode() not in log.read_bytes():
                        time.sleep(0.2)
                    time.sleep(0.5)
                    with socket.socket(socket.AF_UNIX) as sock:
                        sock.settimeout(10)
                        sock.connect(str(monitor))
                        sock.recv(4096)
                        sock.sendall(f"screendump {shot}\n".encode())
                        time.sleep(1.5)
                    if after:
                        qemu.stdin.write(after[0].encode())
                        qemu.stdin.flush()
                    wait_for(len(commands) + 2)
                if usb_keys and monitor.exists():
                    # QEMU's usb-kbd: keys typed through the monitor reach the shell as USB HID reports.
                    with socket.socket(socket.AF_UNIX) as sock:
                        sock.settimeout(10)
                        sock.connect(str(monitor))
                        sock.recv(4096)
                        for key in usb_keys:
                            sock.sendall(f"sendkey {key}\n".encode())
                            time.sleep(0.4)
                    wait_for(len(commands) + 2)
                time.sleep(1)  # let the console repaint reach the screen
                if screendump is not None and monitor.exists():
                    with socket.socket(socket.AF_UNIX) as sock:
                        sock.settimeout(10)
                        sock.connect(str(monitor))
                        sock.recv(4096)
                        sock.sendall(f"screendump {screendump}\n".encode())
                        time.sleep(1.5)
            finally:
                qemu.kill()
                qemu.wait()
        output = log.read_bytes()
    return re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", output.decode("utf-8", "replace")).replace("\r", "")


def ppm_reader(path: Path):
    """(pixel(x, y), (width, height)) for a binary PPM."""
    data = path.read_bytes()
    head = re.match(rb"P6\s+(\d+)\s+(\d+)\s+(\d+)\s", data)
    width, height = int(head.group(1)), int(head.group(2))
    body = data[head.end():]
    return (lambda x, y: tuple(body[(y * width + x) * 3:(y * width + x) * 3 + 3])), (width, height)


def lit_pixels(path: Path) -> tuple[int, int]:
    """(width, height) and how many pixels of a binary PPM are not black."""
    data = path.read_bytes()
    parts = data.split(b"\n", 3)
    width, height = map(int, parts[1].split())
    pixels = parts[3]
    return width * height, sum(1 for k in range(0, len(pixels), 3) if pixels[k:k + 3] != b"\0\0\0")


def main():
    shot = Path(tempfile.mkdtemp()) / "screen.ppm"
    text = boot(IMAGE, shot, ("version", "hexdump 0x40100000 16", "nic", "nic promisc 1 on", "usb", "usb rescan", "disks", "ls", "cat hostname", "mem", "run hello one two", "run nothere", "keymap sv"), ("e", "c", "h", "o", "spc", "minus", "ret"))
    check("Fosfor OS loader" in text, "the aarch64 loader runs under AAVMF", text)
    check("Kernel image read" in text and "Loading its ELF64 segments" in text, "the loader reads the kernel from the payload slot", text)
    check("Fosfor OS kernel (aarch64): running from the UEFI loader." in text, "the kernel runs after ExitBootServices (instruction cache synced)", text)
    match = re.search(r"BootInfo v5: (\d+) memory-map entries, kernel at (0x[0-9a-f]+) \((\d+) KiB\), slot 0, ACPI RSDP (0x[0-9a-f]+)", text)
    check(match is not None and int(match.group(1)) > 10 and match.group(4) != "0x0", "the kernel reads a valid BootInfo (memory map and ACPI RSDP)", text)
    timer = re.search(r"Generic timer: (\d+) Hz, ticks \d+; a 100 ms wait took (\d+) us", text)
    check(timer is not None and 90_000 <= int(timer.group(2)) <= 300_000, "the generic timer counts: a 100 ms busy wait measures about 100 ms", text)
    check("Page tables loaded" in text and re.search(r"Physical memory: \d+ MiB managed", text) is not None, "the kernel builds and loads its own page tables (MMU, W^X, device map)", text)
    irq = re.search(r"(\d+) timer interrupts", text)
    check(irq is not None and 8 <= int(irq.group(1)) <= 12, "the GIC delivers the 100 Hz timer interrupt and the CPU returns from it", text)
    check("Boot disk: disk1" in text and "virtio-blk" in text, "PCI (ECAM) finds virtio-blk; the boot disk is located by the payload partition GUID", text)
    check(re.search(r"FOSFS: mounted on disk1 \(\d+ MiB, \d+ file", text) is not None, "FOSFS mounts on the boot disk (GPT, block layer, virtio-blk driver)", text)
    check("Console: PL011 at 0x9000000 (ACPI SPCR)" in text and "GICv2 at 0x8000000/0x8010000" in text, "the UART (SPCR) and the GIC (MADT) bases come from the ACPI tables", text)
    check("Framebuffer console: on" in text, "the loader hands over a GOP framebuffer and the kernel starts its console on it", text)
    total, lit = lit_pixels(shot) if shot.exists() else (0, 0)
    check(total > 0 and lit > 2000, f"the screen shows the kernel's text ({lit} lit pixels of {total})", text)
    ed_text = boot(IMAGE, None, (), (), None, (("run ed note\r", "ed> "), ("a\r", ". "), ("one\r", ". "), ("two\r", ". "), (".\r", "ed> "), ("x\r", "written note"), ("cat note\r", "two\n")))
    calc_text = boot(IMAGE, None, ("run calc 3 4 + 6 *",))
    check(re.search(r"^= 42$", calc_text, re.M) is not None, "calc evaluates its arguments (3 4 + 6 * = 42)", calc_text)
    check("written note" in ed_text, "ed appends lines from the serial keyboard and writes the file through fs_write", ed_text)
    check(re.search(r"^one$", ed_text, re.M) is not None and re.search(r"^two$", ed_text, re.M) is not None, "the file ed wrote reads back with `cat`", ed_text)
    sd_text = boot(IMAGE, None, (), (), None, (("disks\r", "fos> "), ("ls disk2\r", "fos> "), ("cp readme.txt disk2/copied\r", "Copied"), ("cat disk2/copied\r", "fos> "), ("cat readme.txt\r", "fos> "), ("run ed disk2/tapenote\r", "ed> "), ("a\r", ". "), ("sdline\r", ". "), (".\r", "ed> "), ("x\r", "written"), ("cat disk2/tapenote\r", "sdline\n"), ("ls\r", "fos> "), ("fsck disk2\r", "consistent"), ("cp readme.txt disk2/copied\r", "Copied"), ("fsck disk2\r", "fsck: ")), sd=True)
    check(re.search(r"disk2: \d+ MiB, SD card", sd_text) is not None, "the SDHCI controller and SD card (sdhci.rs, PIO) are initialised and registered as a disk", sd_text)
    check(re.search(r"hello\s+\d+", sd_text) is not None or "hello" in sd_text.split("ls disk2", 1)[-1], "the SD card's FOSFS volume lists its files (sector reads)", sd_text)
    check("Copied readme.txt (73 bytes) to disk2/copied." in sd_text and sd_text.count("Welcome to FOSFS on Fosfor OS.") >= 2, "a file written to the SD card (`cp`, CMD24/25) reads back from disk2/", sd_text)
    check("written disk2/tapenote" in sd_text and re.search(r"^sdline$", sd_text, re.M) is not None, "a tape writes to another volume (`fs_write` with diskN/NAME) and the shell reads it back", sd_text)
    check("tapenote" not in sd_text.rsplit("fos> ls", 1)[-1].split("fsck disk2", 1)[0], "that file did not land on the boot volume", sd_text)
    fat_text = boot(IMAGE, None, (), (), None, (("ls disk2\r", "fos> "), ("run ed disk2/fatnote.txt\r", "ed> "), ("a\r", ". "), ("fatline\r", ". "), (".\r", "ed> "), ("x\r", "written"), ("ls disk2\r", "fos> "), ("cat disk2/fatnote.txt\r", "fatline\n")), fat=True)
    check("fatnote.txt" in fat_text.rsplit("fos> ls disk2", 1)[-1] and "written disk2/fatnote.txt" in fat_text and re.search(r"^fatline$", fat_text, re.M) is not None, "a FAT32 disk (no FOSFS) is written by a tape (`fs_write`), listed and read by the shell", fat_text)
    usb_text = boot(IMAGE, None, (), (), None, (("usb\r", "fos> "), ("disks\r", "fos> "), ("run ed disk2/stick.txt\r", "ed> "), ("a\r", ". "), ("stickline\r", ". "), (".\r", "ed> "), ("x\r", "written"), ("ls disk2\r", "fos> "), ("cat disk2/stick.txt\r", "stickline\n")), usb=True)
    check("USB storage" in usb_text and "disk2" in usb_text and "written disk2/stick.txt" in usb_text and re.search(r"^stickline$", usb_text, re.M) is not None, "a USB stick (xHCI, Bulk-Only) becomes a disk; a tape writes a file to it and the shell reads it back", usb_text)
    places = re.findall(r"^copied\s+\d+ bytes at sector (\d+)", sd_text, re.M)
    check(len(places) == 2 and places[0] != places[1] and "directory consistent" in sd_text, "replacing a file writes the new copy to a different extent (copy-on-write) and fsck finds the directory consistent", sd_text)
    gfx_shot = shot.with_name("gfx.ppm")
    gfx_text = boot(IMAGE, None, (), (), ("run gfxcheck secs=3", "gfxcheck: drawn", gfx_shot))
    pixel, size = ppm_reader(gfx_shot)
    width, height = size
    black = (0, 0, 0)
    check("gfxcheck: screen" in gfx_text and "gfxcheck: drawn" in gfx_text, "a tape reads the screen size and draws through gfx_*", gfx_text)
    check((pixel(10, 10), pixel(100, 10), pixel(170, 10)) == ((255, 0, 0), (0, 255, 0), (0, 0, 255)), "gfx_fill: red, green and blue bars have the right colours", str((pixel(10, 10), pixel(100, 10), pixel(170, 10))))
    check(pixel(100, 102) == (100, 0, 0) and pixel(200, 103) == (200, 0, 0), "gfx_blit: a ramp has the right red values", str((pixel(100, 102), pixel(200, 103))))
    check(pixel(4, 71) == (0, 255, 0) and pixel(8, 71) == black, "gfx_blit: a wider stride draws only the block's width", str((pixel(4, 71), pixel(8, 71))))
    check(pixel(width - 1, height - 1) == (255, 0, 255) and pixel(300, 300) == black, "a block past the corner is clipped and the rest of the screen is cleared", "")
    check("gfxcheck: done" in gfx_text, "the tape ends and the shell returns (gfx_close)", gfx_text)
    two_text = boot(IMAGE, None, ("date set 2026-10-20 11:52:30",), (), None, (("run clock ntp=off + clock ntp=off\r", "ntp=off\r\n"), ("\t", ""), ("\x1b", "clock: stopped."), ("\x1b", "Both tapes returned")))
    check(two_text.count("clock: stopped.") == 2 and "Both tapes returned; back at the fos shell." in two_text, "`run A + B` runs two tapes at once; Tab moves the keyboard, Esc ends the focused one and the shell returns when both have", two_text)
    start = time.time()
    off_text = boot(IMAGE, None, ("shutdown",))
    check("Powering off." in off_text and time.time() - start < 40, "`shutdown` powers the machine off (PSCI SYSTEM_OFF ends QEMU)", off_text)
    cmd_text = boot(IMAGE, None, ("write scratch.txt hello shell", "cat scratch.txt", "rm scratch.txt", "rm scratch.txt", "sleep 1", "debug", "memory", "irqstat"))
    check("Wrote 11 bytes to scratch.txt." in cmd_text and re.search(r"^hello shell$", cmd_text, re.M) is not None and "Removed scratch.txt." in cmd_text and "rm: scratch.txt: no such file" in cmd_text, "`write` and `rm` create, read back and remove a file, as on x86", cmd_text)
    check("Slept 1 s." in cmd_text and "kernel-update" in cmd_text and "slot sizes (sectors)" in cmd_text and "Managed:" in cmd_text, "`sleep`, `debug` (kernel-update and slots) and the x86 names `memory`/`irqstat` work", cmd_text)
    grow_text = boot(IMAGE, None, ("write keep.txt hello grown", "grow disk1", "grow disk1", "cat keep.txt", "help calc", "help hello"), big=True)
    check("grown from" in grow_text and "files are unchanged" in grow_text and "already fills the disk" in grow_text and "hello grown" in grow_text, "`grow disk1` grows the FOSFS volume into the free space, a second one says so, and the files are kept", grow_text)
    check("help: hello has no help screen" in grow_text or "usage" in grow_text.lower() or "calc" in grow_text.split("help calc", 1)[-1], "`help NAME` shows a tape's help or says it has none without running it", grow_text)
    esp_text = boot(IMAGE, None, ("write esp/config.txt arm_64bit=1", "ls esp", "cat esp/config.txt", "cp esp/config.txt copy.txt", "cat copy.txt", "cp readme.txt esp/readme.txt", "cat esp/readme.txt", "rm esp/config.txt", "ls esp"))
    check("Wrote 11 bytes to esp/config.txt." in esp_text and re.search(r"^arm_64bit=1$", esp_text, re.M) is not None and "Removed esp/config.txt." in esp_text, "the boot disk's EFI System Partition is reachable as `esp/NAME` (write, cat, rm)", esp_text)
    path_text = boot(IMAGE, None, ("disk1/hello", "ed esp/Cfg.txt"))
    check("hello" in path_text.lower() and "unknown command" not in path_text, "`disk1/NAME` runs a tape without `run`", path_text)
    check("Usage: tape run ed" not in path_text and "cannot inspect" not in path_text, "`ed esp/NAME` accepts the ESP selector and upper-case FAT names", path_text)
    check("Copied esp/config.txt (11 bytes) to copy.txt." in esp_text and "Copied readme.txt (73 bytes) to esp/readme.txt." in esp_text and "Welcome to FOSFS on Fosfor OS." in esp_text, "`cp` moves files between FOSFS and the ESP in both directions", esp_text)
    service_text = boot(IMAGE, None, ("date set 2026-10-20 11:52:30",), (), None, (("run clock ntp=off &0\r", "Service clock started"), ("ps\r", "no NIC"), ("echo shell-still-works\r", "shell-still-works"), ("run clock ntp=off &3\r", "no NIC 3"), ("kill clock\r", "Service clock ended."), ("ps\r", "No services running")))
    check("Service clock started" in service_text and "Service clock ended." in service_text and "shell-still-works" in service_text, "`run NAME &0` runs a tape as a service while the shell stays usable; `ps` lists it and `kill NAME` ends it", service_text)
    start_text = boot(IMAGE, None, (), (), None, (("run ed start\r", "ed> "), ("a\r", ". "), ("# comment\r", ". "), ("echo from-start-file\r", ". "), (".\r", "ed> "), ("x\r", "written start"), ("start\r", "fos> ")))
    check("start file: running 1 command(s)." in start_text and "start> echo from-start-file" in start_text and re.search(r"^from-start-file$", start_text, re.M) is not None, "`start` runs the boot volume's start file (comments skipped)", start_text)
    clock_shot = shot.with_name("clock.ppm")
    clock_text = boot(IMAGE, None, ("date set 2026-10-20 11:52:30",), (), ("run clock ntp=off", "run clock", clock_shot, "\x1b"))
    total, lit = lit_pixels(clock_shot)[0], lit_pixels(clock_shot)[1]
    check("The clock is 2026-10-20 11:52:30" in clock_text and "clock: stopped." in clock_text and "clock: the machine" not in clock_text, "the software clock is set with `date set`; the clock tape starts from it and Esc ends it", clock_text)
    check(lit > total // 50, "the clock tape draws its face on the screen", f"{lit} of {total} pixels lit")
    check("No network interface." in text.split("fos> nic", 1)[-1] and "nic promisc: no such interface" in text, "`nic` and `nic promisc` answer without a Genet (QEMU)", text)
    check("USB keyboard(s)" in text and "new device(s)" in text, "`usb` and `usb rescan` answer", text)
    check("0x0040100000: " in text and "|" in text.split("hexdump", 1)[-1], "`hexdump` dumps mapped memory", text)
    check("Fosfor OS (aarch64 port)" in text and "virtio-blk" in text.split("version", 1)[-1], "the shell reads the serial console and runs `version` and `disks`", text)
    check(re.search(r"^readme\.txt +73 bytes", text, re.M) is not None and re.search(r"Free:\s+\d+ MiB \(\d+ frames\)\s+fos> ", text) is not None, "the shell lists FOSFS files and answers `mem`", text)
    check("hello from a v2 tape" in text and "arguments: [one two]" in text and re.search(r"files on the boot volume: [1-9]\d*", text) is not None and re.search(r"primes up to 100000: 9592 \(in \d+ us\)", text) is not None and "Tape returned; back at the fos shell." in text, "a hard-float aarch64 tape loads from FOSFS, runs with arguments, lists files (fs_list), allocates, uses the clock and returns", text)
    check("run: nothere: no such file." in text, "a missing tape is refused politely", text)
    check(re.search(r"^\+$", text, re.M) is not None, "keys typed on the USB keyboard reach the shell, and `keymap sv` makes the US minus key type '+'", text)
    check("PANIC" not in text and "FATAL" not in text and "EXCEPTION" not in text, "no panic, no loader fatal, no exception", text)
    check("MSI: GICv2m frame." in text and re.search(r"xHCI \S+: MSI(-X)? on vector \S+ \(the self-test interrupt arrived\)", text) is not None, "the xHCI controller delivers its events by MSI through the GICv2m frame and the GIC", text)
    check("USB: 1 xHCI controller(s)." in text and re.search(r"USB keyboard 0627:0001 on port \d+ .*: ready\.", text) is not None, "the xHCI controller comes up and enumerates the USB keyboard", text)
    print("aarch64 boot: all checks passed")


if __name__ == "__main__":
    sys.exit(main())
