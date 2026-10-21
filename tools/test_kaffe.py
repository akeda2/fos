#!/usr/bin/env python3
"""The `kaffe` tape (coffee board): the machine's clock is set with `date set`, the tape draws two light-grey buttons and the red
clock; Enter stamps the time (yellow digits appear in the stamp line and `kaffe.cfg` holds `last=`), a second run still shows the
stamp (it was saved), `c` clears it (no yellow, empty file) and Escape returns the console.
Run with `make test-kaffe`."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402
from test_gfx import screenshot  # noqa: E402

MAC = "52:54:00:12:35:01"
RED, YELLOW, BUTTON = (0xff, 0x1a, 0x1a), (0xff, 0xd4, 0x00), (0xd3, 0xd3, 0xd3)


def buttons(width, height):
    """tapes/kaffe/src/main.rs `Layout::new`: the two button rectangles (x, y, w, h)."""
    margin = max(height // 30, 4)
    title_bottom = margin + 16 * max(height // 400, 1)
    h, y, gap = max(height // 7, 24), title_bottom + margin * 2, margin * 2
    done_w, clear_w = width * 11 // 20, width * 5 // 20
    left = (width - done_w - clear_w - gap) // 2
    return (left, y, done_w, h), (left + done_w + gap, y, clear_w, h)


def count(pixel, colour, x0, y0, x1, y1):
    return sum(1 for x in range(x0, x1, 2) for y in range(y0, y1, 2) if pixel(x, y) == colour)


def stop(guest, mark):
    guest.process.stdin.write(b"\x1b")
    guest.process.stdin.flush()
    guest.wait_for("kaffe: stopped.", 10, mark)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        monitor = Path(tmp) / "monitor"
        guest = Guest(tmp, "kaffe", MAC, extra=["-monitor", f"unix:{monitor},server,nowait"])
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            guest.send("rm kaffe.cfg")
            guest.send("date set 2026-10-21 09:15:00")
            mark = guest.send("run kaffe")
            guest.wait_for("kaffe: Enter or Space", 20, mark)
            time.sleep(2)
            width, height, pixel = screenshot(monitor, "kaffe.ppm")
            done, clear = buttons(width, height)
            check(pixel(done[0] + 4, done[1] + 4) == BUTTON and pixel(clear[0] + 4, clear[1] + 4) == BUTTON, "both buttons are light grey")
            check(count(pixel, RED, 0, done[1] + done[3], width, height) > 200, "the clock is drawn in red")
            check(count(pixel, YELLOW, 0, height // 2, width, height) == 0, "no stamp before the button is pressed")

            guest.process.stdin.write(b"\r")
            guest.process.stdin.flush()
            time.sleep(2)
            _, _, pixel = screenshot(monitor, "kaffe_stamp.ppm")
            check(count(pixel, YELLOW, 0, height // 2, width, height) > 20, "Enter stamps the time (yellow digits)")
            stop(guest, mark)
            mark = guest.send("cat kaffe.cfg")
            guest.wait_for("last=2026-10-21T09:", 10, mark)

            mark = guest.send("run kaffe")
            guest.wait_for("kaffe: Enter or Space", 20, mark)
            time.sleep(2)
            _, _, pixel = screenshot(monitor, "kaffe_again.ppm")
            check(count(pixel, YELLOW, 0, height // 2, width, height) > 20, "the saved stamp is still shown after a restart")
            guest.process.stdin.write(b"c")
            guest.process.stdin.flush()
            time.sleep(2)
            _, _, pixel = screenshot(monitor, "kaffe_clear.ppm")
            check(count(pixel, YELLOW, 0, height // 2, width, height) == 0, "c clears the stamp")
            stop(guest, mark)
            mark = guest.send("cat kaffe.cfg")
            time.sleep(1)
            check("last=" not in guest.text()[mark:], "the cleared stamp is saved too")
        finally:
            guest.stop()
    print("kaffe: all checks passed")


if __name__ == "__main__":
    main()
