#!/usr/bin/env python3
"""`run A | B`: two graphic tapes share one screen. `run kaffe ntp=off | clock ntp=off` puts kaffe (light-grey buttons) in the
left half and the clock (red dots) in the right one; with `split=v` they are stacked, kaffe on top; `rotate=r|l` turns the whole screen first. Escape ends kaffe, which
takes the clock with it, and the shell prompt returns.
Run with `make test-split`."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402
from test_gfx import screenshot  # noqa: E402

MAC = "52:54:00:12:35:01"
YELLOW, BUTTON = (0xff, 0xd4, 0x00), (0xd3, 0xd3, 0xd3)


def count(pixel, colour, x0, y0, x1, y1):
    return sum(1 for x in range(x0, x1, 3) for y in range(y0, y1, 3) if pixel(x, y) == colour)


def run_split(guest, monitor, command, name, halves):
    """Starts `command`, screenshots it and checks the grey buttons and the yellow digits fall in the two `halves`."""
    mark = guest.send(command)
    guest.wait_for("kaffe: Enter or Space", 20, mark)
    time.sleep(3)
    width, height, pixel = screenshot(monitor, name)
    (grey_box, red_box) = halves(width, height)
    check(count(pixel, BUTTON, *grey_box) > 100, f"{name}: kaffe's buttons are in its half")
    check(count(pixel, YELLOW, *red_box) > 100, f"{name}: the clock's yellow digits are in its half")
    check(count(pixel, YELLOW, *grey_box) == 0, f"{name}: no clock digits in kaffe's half")
    check(count(pixel, BUTTON, *red_box) == 0, f"{name}: no grey buttons in the clock's half")
    guest.process.stdin.write(b"\x1b")
    guest.process.stdin.flush()
    guest.wait_for("Service clock ended.", 20, mark)
    time.sleep(1)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        monitor = Path(tmp) / "monitor"
        guest = Guest(tmp, "kaffe", MAC, extra=["-monitor", f"unix:{monitor},server,nowait"])
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            guest.send("rm kaffe.cfg")
            guest.send("date set 2026-10-21 09:15:00")
            run_split(guest, monitor, "run kaffe ntp=off | clock ntp=off", "split-h.ppm",
                      lambda w, h: ((0, 0, w // 2, h), (w // 2, 0, w, h)))
            run_split(guest, monitor, "run kaffe ntp=off | clock ntp=off split=v", "split-v.ppm",
                      lambda w, h: ((0, 0, w, h // 2), (0, h // 2, w, h)))
            # A turned monitor: the logical screen is portrait, so `split=v` gives two squarish halves. Turned right, the
            # logical top (kaffe) is the framebuffer's right side; turned left it is the left side.
            run_split(guest, monitor, "run kaffe ntp=off | clock ntp=off split=v rotate=r", "split-r.ppm",
                      lambda w, h: ((w // 2, 0, w, h), (0, 0, w // 2, h)))
            run_split(guest, monitor, "run kaffe ntp=off | clock ntp=off split=v rotate=l", "split-l.ppm",
                      lambda w, h: ((0, 0, w // 2, h), (w // 2, 0, w, h)))
            print("split screen: all checks passed")
        finally:
            guest.stop()


if __name__ == "__main__":
    main()
