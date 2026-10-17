#!/usr/bin/env python3
"""The `ltas` tape (hard-float, graphic) against QEMU's HD Audio codec. QEMU's `none` audio backend gives silence as input
and swallows the output, so this proves what QEMU can: the tape starts capture and playback, takes the screen and draws the
plot (background, grid, text, the curves at the floor), keeps analysing (frames counted), answers keys without failing,
returns the console on Escape and reports no capture overruns. Levels, the generator's signal and real input are checked
by the host tests of `crates/ltas` and `crates/noise` and on real hardware. Run with `make test-ltas`."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import ROOT, TAG_DATA, check  # noqa: E402
from test_debug_mode import Machine  # noqa: E402
from test_gfx import screenshot  # noqa: E402

LTAS = ROOT / "build/cargo-target/x86_64-fos-tape-fp/release/ltas"
BACKGROUND, GRID_MAJOR = (0x10, 0x14, 0x18), (0x3a, 0x44, 0x4d)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {}, {"ltas": LTAS})
        machine.monitor = Path(tmp) / "monitor"
        machine.extra = ["-audiodev", "none,id=snd", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"]
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"run ltas noise=pink level=-30 res=1/6\n")
            time.sleep(5)
            width, height, pixel = screenshot(machine.monitor, "ltas.ppm")
            check(width >= 640, f"QEMU produced a screendump ({width}x{height})", machine.text()[start:][-1500:])
            check(pixel(5, height - 5) == BACKGROUND, "the tape took the screen (background colour)", str(pixel(5, height - 5)))
            check(pixel(56, 60) == GRID_MAJOR, "the plot frame is drawn", str(pixel(56, 60)))
            lit = sum(1 for y in range(4, 36) for x in range(56, 600) if pixel(x, y) != BACKGROUND)
            check(lit > 300, f"the status line is drawn ({lit} lit pixels)", "")
            for key in (b"v", b"w", b"a", b"n", b"t", b"g", b"+", b"f", b"b", b"3", b"3", b"r", b" ", b" "):
                machine.wire.send(machine.guest, TAG_DATA, key)
                time.sleep(0.4)
            _, _, after = screenshot(machine.monitor, "ltas2.ppm")
            check(after(5, height - 5) == BACKGROUND and after(56, 60) == GRID_MAJOR, "the keys left the display intact", "")
            machine.wire.send(machine.guest, TAG_DATA, b"\x1b")
            text = machine.wait_for("frame(s) analysed", since=start, timeout=20)
            frames = int(text.split("ltas: ")[-1].split(" frame")[0])
            check(frames >= 3, f"the analyser ran ({frames} frames)", text[-1500:])
            check("capture overruns 0" in text, "no capture overruns", text[-500:])
            time.sleep(0.5)
            _, _, last = screenshot(machine.monitor, "ltas3.ppm")
            check(last(56, 60) != GRID_MAJOR, "the plot is gone when the tape ends", "")
        finally:
            machine.stop()
    print("ltas: all checks passed")


if __name__ == "__main__":
    main()
