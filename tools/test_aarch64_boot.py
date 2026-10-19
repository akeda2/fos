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


def boot(image: Path, screendump: Path | None = None, commands: tuple[str, ...] = (), usb_keys: tuple[str, ...] = (), during: tuple[str, str, Path] | None = None, steps: tuple[tuple[str, str], ...] = ()) -> str:
    """Boots `image` under QEMU virt + AAVMF + ramfb and returns the serial text. The kernel parks, so the run ends
    when the parking message shows (or after 60 s); a PPM screenshot goes to `screendump` when given."""
    if not shutil.which("qemu-system-aarch64") or not Path(CODE).exists():
        raise SystemExit("qemu-system-aarch64 or the AAVMF firmware is missing (apt install qemu-system-arm qemu-efi-aarch64)")
    with tempfile.TemporaryDirectory() as scratch:
        variables = Path(scratch) / "vars.fd"
        shutil.copyfile(VARS, variables)
        log, monitor = Path(scratch) / "serial.txt", Path(scratch) / "mon.sock"
        with open(log, "wb") as out:
            qemu = subprocess.Popen(
                ["qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72", "-m", "1G", "-display", "none", "-device", "ramfb",
                 "-serial", "stdio", "-no-reboot", "-monitor", f"unix:{monitor},server,nowait",
                 "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"if=pflash,format=raw,file={variables}",
                 "-drive", f"file={image},format=raw,if=none,id=boot,snapshot=on", "-device", "virtio-blk-pci,drive=boot", "-device", "qemu-xhci", "-device", "usb-kbd"],
                stdin=subprocess.PIPE, stdout=out, stderr=subprocess.STDOUT)
            try:
                def wait_for(count: int) -> None:
                    """Waits until the serial text holds `count` shell prompts (or the kernel parked after a fault)."""
                    deadline = time.time() + 60
                    while time.time() < deadline:
                        seen = log.read_bytes().lower()
                        if seen.count(b"fos> ") >= count or b"parking the cpu" in seen:
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
                    command, marker, shot = during
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
    text = boot(IMAGE, shot, ("version", "disks", "ls", "cat hostname", "mem", "run hello one two", "run nothere", "keymap sv"), ("e", "c", "h", "o", "spc", "minus", "ret"))
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
    check("Fosfor OS (aarch64 port)" in text and "virtio-blk" in text.split("version", 1)[-1], "the shell reads the serial console and runs `version` and `disks`", text)
    check(re.search(r"\d+ file\(s\)\.", text) is not None and re.search(r"Physical memory: \d+ MiB managed, \d+ MiB free\.\s+fos> ", text) is not None, "the shell lists FOSFS files and answers `mem`", text)
    check("hello from a v2 tape" in text and "arguments: [one two]" in text and re.search(r"primes up to 100000: 9592 \(in \d+ us\)", text) is not None and "Tape returned; back at the fos shell." in text, "a hard-float aarch64 tape loads from FOSFS, runs with arguments, allocates, uses the clock and returns", text)
    check("run: nothere: no such file." in text, "a missing tape is refused politely", text)
    check(re.search(r"^\+$", text, re.M) is not None, "keys typed on the USB keyboard reach the shell, and `keymap sv` makes the US minus key type '+'", text)
    check("PANIC" not in text and "FATAL" not in text and "EXCEPTION" not in text, "no panic, no loader fatal, no exception", text)
    check("USB: 1 xHCI controller(s)." in text and re.search(r"USB keyboard 0627:0001 on port \d+ .*: ready\.", text) is not None, "the xHCI controller comes up and enumerates the USB keyboard", text)
    print("aarch64 boot: all checks passed")


if __name__ == "__main__":
    sys.exit(main())
