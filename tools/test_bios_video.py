#!/usr/bin/env python3
"""BIOS path: the two ways to keep stage two from switching the video mode (a video BIOS that hangs in a VBE call).

 * Hold Shift: the QEMU monitor holds a Shift key down for several seconds from the start of the boot; stage two reads the
   BIOS keyboard state just before the VBE step, says it skipped the mode, and the kernel paints the BIOS's 80x25 text screen
   (and the serial port).
 * `debug video skip`: sets a flag in the slot-metadata sector (byte 16, bit 0) from the shell; after a reboot stage two
   honours it every time, until `debug video auto` clears it and the next boot has its VBE framebuffer again.
Boots through SeaBIOS (no OVMF), on a private copy of the image. Run with `make test-bios-video`."""
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import IMAGE, check, clean, monitor  # noqa: E402


class Machine:
    """One QEMU through SeaBIOS with the serial port on pipes; the monitor on a socket. It may reboot itself."""

    def __init__(self, tmp, image, extra=()):
        self.monitor_path = str(Path(tmp) / "monitor")
        self.process = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35", "-m", "256", "-display", "none", "-serial", "stdio",
             "-monitor", f"unix:{self.monitor_path},server,nowait", "-drive", f"format=raw,file={image}", *extra],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.raw = bytearray()

    def text(self):
        return clean(bytes(self.raw))

    def wait_for(self, marker, timeout=60, after=0):
        deadline = time.monotonic() + timeout
        while marker not in self.text()[after:]:
            if time.monotonic() > deadline or self.process.poll() is not None:
                raise SystemExit(f"FAIL: never saw {marker!r}\n{self.text()[after:][-2500:]}")
            ready, _, _ = select.select([self.process.stdout], [], [], 0.2)
            if ready:
                self.raw.extend(os.read(self.process.stdout.fileno(), 4096))
        return self.text()

    def send(self, line):
        mark = len(self.text())
        self.process.stdin.write(line.encode() + b"\n")
        self.process.stdin.flush()
        return mark

    def close(self):
        self.process.kill()
        self.process.wait()


def screen_text(machine, tmp):
    """What is on the BIOS text screen: the 80x25 character/attribute words at 0xB8000, read through the QEMU monitor."""
    path = Path(tmp) / "vga.bin"
    path.unlink(missing_ok=True)
    monitor(machine.monitor_path, f'pmemsave 0xb8000 4000 "{path}"')
    for _ in range(50):
        if path.exists() and path.stat().st_size >= 4000:
            break
        time.sleep(0.1)
    data = path.read_bytes()
    characters = data[0::2]
    return "\n".join(characters[row * 80:(row + 1) * 80].decode("latin-1").rstrip() for row in range(25))


def reboot(machine):
    """Sends `reboot`, waits for the *next* boot's stage two and then its prompt; returns that boot's serial text."""
    before = len(machine.text())
    machine.send("reboot")
    text = machine.wait_for("Fosfor stage 2", 90, before)
    start = text.index("Fosfor stage 2", before)
    return machine.wait_for("fos> ", 90, start)[start:]


def main():
    if not IMAGE.exists():
        sys.exit(f"{IMAGE} is missing; run `make image` first")
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)

        # --- a held Shift key ---
        machine = Machine(tmp, f"{IMAGE},snapshot=on")
        try:
            machine.wait_for("Fosfor stage 2")          # SeaBIOS is up and handling the keyboard; stage two is running
            monitor(machine.monitor_path, "sendkey shift 12000")
            text = machine.wait_for("fos> ")
            time.sleep(0.5)
            screen = screen_text(machine, tmp)
        finally:
            machine.close()
        check("Video: skipped (Shift is held" in text, "a held Shift makes stage two skip the video mode", text)
        check("Fosfor OS kernel" in screen and "fos>" in screen, "the screen itself shows the boot log and the prompt (VGA text memory)", screen)
        check("No framebuffer: using the BIOS's 80x25 VGA text screen" in text and "Framebuffer console ready" not in text, "and the kernel uses the BIOS text screen instead", text)

        # --- the persistent flag ---
        image = tmp / "copy.img"
        shutil.copy(IMAGE, image)
        machine = Machine(tmp, image)
        try:
            text = machine.wait_for("fos> ")
            check("Video: VBE" in text and "Framebuffer console ready" in text, "by default the VBE framebuffer is set", text)
            mark = machine.send("debug video")
            seen = machine.wait_for("automatically", 20, mark)[mark:]
            check("chosen automatically" in seen, "`debug video` reports the automatic default", seen)
            mark = machine.send("debug video skip")
            seen = machine.wait_for("video mode switch is skipped", 20, mark)[mark:]
            check("video mode switch is skipped" in seen, "`debug video skip` sets the flag", seen)
            mark = machine.send("debug slots")
            seen = machine.wait_for("BIOS video mode switch:", 20, mark)[mark:]
            machine.wait_for("watchdog:", 20, mark)      # the last line of the summary
            seen = machine.text()[mark:]
            check("BIOS video mode switch: skipped" in seen, "and `debug slots` shows it", seen)
            second = reboot(machine)
            check("Video: skipped (Shift is held, or `debug video skip` is set)" in second, "after a reboot stage two honours the flag", second[-1500:])
            check("No framebuffer: using the BIOS's 80x25 VGA text screen" in second, "and the kernel uses the BIOS text screen", second[-800:])
            machine.send("uptime")
            time.sleep(1.0)
            check("ticks at 100 Hz" in screen_text(machine, tmp), "a command typed afterwards shows its output on the screen too", "")
            mark = machine.send("debug video auto")
            machine.wait_for("chosen automatically", 20, mark)
            third = reboot(machine)
            check("Video: VBE" in third and "Framebuffer console ready" in third, "`debug video auto` brings the framebuffer back at the next boot", third[-1500:])
        finally:
            machine.close()

    # --- a video BIOS that gives no EDID: the operator tells stage two the monitor's size ---
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image = tmp / "hint.img"
        shutil.copy(IMAGE, image)
        machine = Machine(tmp, image, extra=("-vga", "none", "-device", "VGA,edid=off"))
        try:
            text = machine.wait_for("fos> ")
            check("native size (EDID)" not in text and "Video: VBE" in text, "without an EDID and without a hint, stage two keeps its old rule", text[:1500])
            mark = machine.send("debug video size 1366x768")
            seen = machine.wait_for("fits it best", 20, mark)[mark:]
            check("picks the VBE mode whose shape fits it best" in seen, "`debug video size 1366x768` stores the hint", seen)
            mark = machine.send("debug slots")
            seen = machine.wait_for("hint: 1366x768", 20, mark)[mark:]
            check("monitor size hint: set" in seen, "and `debug slots` shows it", seen)
            second = reboot(machine)
            check("using the operator's monitor size hint: 1366x768" in second, "after a reboot stage two uses the hint (the video BIOS gave no EDID)", second[:1800])
            check("mode 0x" in second and "1280x720 32 bpp" in second, "and takes the 16:9 mode closest to it (1280x720)", second[:1800])
            mark = machine.send("debug video size auto")
            machine.wait_for("Monitor size hint cleared", 20, mark)
            third = reboot(machine)
            check("monitor size hint" not in third and "1280x1024 32 bpp" in third, "`debug video size auto` returns to the old rule", third[:1800])
        finally:
            machine.close()
    print("bios video: all checks passed")


if __name__ == "__main__":
    main()
