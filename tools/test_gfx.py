#!/usr/bin/env python3
"""Tape graphics (`gfx_*`, ABI 17): `gfxcheck` takes the screen, draws colour bars, a ramp blitted with a stride and a block that
runs off the screen's corner, and the test reads the picture back from a QEMU screendump (the colours prove the pixel-format
conversion, the gap after a wide-stride row proves the stride, the corner pixel proves clipping). When the tape ends the console
must be back: the bars gone and text drawn again. Run with `make test-gfx`."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import ROOT, TAG_DATA, check, monitor, read_ppm  # noqa: E402
from test_debug_mode import Machine  # noqa: E402

GFXCHECK = ROOT / "build/cargo-target/x86_64-fos-tape/release/gfxcheck"


def screenshot(path, name):
    shot = Path(path).parent / name
    monitor(str(path), f"screendump {shot}")
    for _ in range(50):
        if shot.exists() and shot.stat().st_size > 1000:
            break
        time.sleep(0.1)
    time.sleep(0.3)
    return read_ppm(shot)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {}, {"gfxcheck": GFXCHECK})
        machine.monitor = Path(tmp) / "monitor"
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"run gfxcheck secs=12\n")
            shown = machine.wait_for("gfxcheck: drawn", since=start, timeout=30)
            check("a fill before gfx_open is refused" in shown, "a fill before gfx_open is refused", shown)
            width, height, pixel = screenshot(machine.monitor, "during.ppm")
            black, white = (0, 0, 0), (255, 255, 255)
            check(pixel(10, 10) == (255, 0, 0), "a red fill is red", str(pixel(10, 10)))
            check(pixel(100, 10) == (0, 255, 0), "a green fill is green", str(pixel(100, 10)))
            check(pixel(170, 10) == (0, 0, 255), "a blue fill is blue", str(pixel(170, 10)))
            check(pixel(100, 102) == (100, 0, 0) and pixel(200, 103) == (200, 0, 0), "a blitted ramp has the right red at x=100 and x=200", str((pixel(100, 102), pixel(200, 103))))
            check(pixel(4, 71) == (0, 255, 0) and pixel(8, 71) == black and pixel(12, 71) != white, "a blit with a wider stride draws only its width", str((pixel(4, 71), pixel(8, 71), pixel(12, 71))))
            check(pixel(width - 1, height - 1) == (255, 0, 255) and pixel(width - 11, height - 11) == black, "a block past the corner is clipped at the screen's edge", str((pixel(width - 1, height - 1), pixel(width - 11, height - 11))))
            check(pixel(300, 300) == black and pixel(5, 200) == black, "the rest of the screen was cleared (no console text under the picture)", "")
            # Console text still goes to serial while the screen is the tape's.
            done = machine.wait_for("Tape returned", since=start, timeout=40)
            check("gfxcheck: done" in done, "the tape ran to its end", done)
            time.sleep(0.5)
            _, _, after = screenshot(machine.monitor, "after.ppm")
            check(after(10, 10) != (255, 0, 0) and after(100, 102) == black, "the picture is gone when the tape ends", str((after(10, 10), after(100, 102))))
            lit = sum(1 for y in range(0, 160, 2) for x in range(0, 640, 2) if after(x, y) != black)
            check(lit > 200, f"the console's text is drawn again ({lit} lit sample pixels)", "")
        finally:
            machine.stop()
    print("gfx: all checks passed")


if __name__ == "__main__":
    main()
