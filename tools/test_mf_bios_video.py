#!/usr/bin/env python3
"""The server kernel's BIOS text-mode console (docs/big-kernel.md): with a Shift key held during a BIOS boot stage two skips the
VBE mode, and kernel-mf then draws its console into the 80x25 VGA text window instead of staying serial-only. Boots the mf image
through SeaBIOS; checks the kernel says so, the screen (read through the QEMU monitor at 0xB8000) shows the boot lines and a
command's output in light grey on black, and that a normal boot still gets its framebuffer. (The colour swizzle, ANSI red = VGA 4,
is not reachable from the serial shell, which drops ESC.) QEMU only: a real video BIOS and the hardware cursor are for hardware. `make test-mf-bios-video`."""
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_bios_video as bv  # noqa: E402
from test_boot import ROOT, check, monitor  # noqa: E402

IMAGE = ROOT / "build/fosfor-mf/fosfor.img"
PROMPT = "fos> "


def screen(machine, tmp):
    path = Path(tmp) / "vga.bin"
    path.unlink(missing_ok=True)
    monitor(machine.monitor_path, f'pmemsave 0xb8000 4000 "{path}"')
    for _ in range(50):
        if path.exists() and path.stat().st_size >= 4000:
            break
        time.sleep(0.1)
    return path.read_bytes()


def lines(data):
    return "\n".join(data[0::2][row * 80:(row + 1) * 80].decode("latin-1").rstrip() for row in range(25))


def caret_cells(machine, tmp):
    """Cells of the pixel screen (8x16) whose bottom two lines are all lit and the other lines all dark: a caret on a blank cell."""
    path = Path(tmp) / "screen.ppm"
    monitor(machine.monitor_path, f'screendump "{path}"')
    data = path.read_bytes()
    fields, at = [], 0
    while len(fields) < 4:
        while data[at:at + 1].isspace():
            at += 1
        end = at
        while not data[end:end + 1].isspace():
            end += 1
        fields.append(data[at:end])
        at = end
    width, height = int(fields[1]), int(fields[2])
    pixels = data[at + 1:]

    def lit(x, y):
        return any(pixels[(y * width + x) * 3:(y * width + x) * 3 + 3])

    found = 0
    for row in range(height // 16):
        for column in range(width // 8):
            lines = [any(lit(column * 8 + bit, row * 16 + line) for bit in range(8)) for line in range(16)]
            full = all(lit(column * 8 + bit, row * 16 + line) for line in (14, 15) for bit in range(8))
            if full and not any(lines[:14]):
                found += 1
    return found


def main():
    if not IMAGE.exists():
        sys.exit(f"{IMAGE} is missing; run `make image-mf` first")
    with tempfile.TemporaryDirectory() as tmp:
        machine = bv.Machine(tmp, f"{IMAGE},snapshot=on")
        try:
            machine.wait_for("Fosfor stage 2")
            bv.monitor(machine.monitor_path, "sendkey shift 12000")
            text = machine.wait_for(PROMPT, 90)
            check("using the BIOS's 80x25 VGA text screen" in text, "the kernel uses the BIOS text screen", text[-1500:])
            time.sleep(0.5)
            data = screen(machine, tmp)
            check("server kernel" in lines(data) or PROMPT.strip() in lines(data), "the screen shows kernel text", lines(data))
            mark = machine.send("echo hello-text-mode")
            machine.wait_for(PROMPT, 20, mark)
            time.sleep(0.3)
            data = screen(machine, tmp)
            check("hello-text-mode" in lines(data), "a command's output lands on the screen", lines(data))
            check(data[1::2][data[0::2].find(b"hello-text-mode")] == 0x07, "and the default text is light grey on black", lines(data))
        finally:
            machine.close()
        machine = bv.Machine(tmp, f"{IMAGE},snapshot=on")
        try:
            text = machine.wait_for(PROMPT, 90)
            check("Framebuffer console:" in text and "VGA text screen" not in text, "a normal boot still gets the pixel console", text[-1500:])
            time.sleep(0.5)
            check(caret_cells(machine, tmp) == 1, "the pixel console draws one caret, an underline in the next cell")
        finally:
            machine.close()
    print("test-mf-bios-video: all checks passed")


if __name__ == "__main__":
    main()
