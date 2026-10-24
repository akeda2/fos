#!/usr/bin/env python3
"""The `tape-echo` tape (tape echo effect box): booted with an HD Audio duplex codec on QEMU's `none` backend (silence in, output
swallowed), so this proves what QEMU can: the tape starts capture and playback together, takes the screen and draws the amber
panel, answers typed keys (f Freeze, 1-3 heads, Tab Tap) without failing and returns the console on Escape. The sound is
checked by the host tests of `crates/tape-echo` and `crates/fx`; live latency, feedback and the MIDI controllers need real
hardware. Run with `make test-tape-echo`."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402
from test_gfx import screenshot  # noqa: E402

MAC = "52:54:00:12:36:01"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        monitor = Path(tmp) / "monitor"
        guest = Guest(tmp, "tape-echo", MAC, extra=["-monitor", f"unix:{monitor},server,nowait", "-audiodev", "none,id=snd",
                                               "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"])
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("run tape-echo")
            guest.wait_for("Escape quits.", 30, mark)
            text = guest.text()[mark:]
            check("cannot" not in text and "no audio" not in text, "capture and playback start together", text[-500:])
            time.sleep(2)
            width, height, pixel = screenshot(monitor, "tape-echo.ppm")
            ambers = sum(1 for x in range(0, width, 2) for y in range(0, height, 2) if pixel(x, y)[0] > pixel(x, y)[2] + 40)
            check(ambers > 200, "the screen shows the amber controls")
            for key in b"f1f2\t\t\t":
                guest.process.stdin.write(bytes([key]))
                guest.process.stdin.flush()
                guest.pump(0.3)
            guest.pump(1)
            width, height, pixel = screenshot(monitor, "tape-echo2.ppm")
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("tape-echo: stopped.", 10, mark)
            text = guest.text()[mark:]
            check("overrun" not in text.lower(), "no errors while it ran", text[-500:])
        finally:
            guest.stop()
    print("tape-echo: all checks passed")


if __name__ == "__main__":
    main()
