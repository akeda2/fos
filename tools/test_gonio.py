#!/usr/bin/env python3
"""The `gonio` tape (goniometer) against QEMU's HD Audio codec, which gives silence as input, so the built-in stereo tone
(`src=test`) is the signal: mono (phase 0) must draw a vertical line and fill the correlation bar to the right, opposite
polarity (phase 180) a horizontal line and fill it to the left in red, a level meter must light, the keys must leave the
display intact, a bad `chan=` is refused and Escape returns the console. Run with `make test-gonio`."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import ROOT, TAG_DATA, check  # noqa: E402
from test_debug_mode import Machine  # noqa: E402
from test_gfx import screenshot  # noqa: E402

GONIO = ROOT / "build/cargo-target/x86_64-fos-tape-fp/release/gonio"
BACKGROUND, AXIS, FRAME = (0x10, 0x14, 0x18), (0x2c, 0x36, 0x3f), (0x4b, 0x59, 0x64)
GREEN, RED = (0x3c, 0xcf, 0x6a), (0xff, 0x4d, 0x4d)


def layout(width, height):
    """tapes/gonio/src/layout.rs: the scope square and the correlation bar."""
    size = max(min(height - 34 - 22 - 100 - 24, width - 400), 64) & ~1
    sx, sy = (width - size) // 2, 34 + 22
    return sx, sy, size, sy + size + 22


def trace_pixels(pixel, sx, sy, size):
    lit = []
    for y in range(sy + 22, sy + size - 22):
        for x in range(sx + 22, sx + size - 22):
            if pixel(x, y) not in (BACKGROUND, AXIS, FRAME):
                lit.append((x, y))
    return lit


def run(machine, line, wait):
    start = len(machine.text())
    machine.wire.send(machine.guest, TAG_DATA, line)
    time.sleep(wait)
    return start


def main():
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {}, {"gonio": GONIO})
        machine.monitor = Path(tmp) / "monitor"
        machine.extra = ["-audiodev", "none,id=snd", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"]
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"run gonio chan=3,4\n")
            text = machine.wait_for("gonio:", since=start, timeout=20)
            check("out of range" in text, "a pair beyond the device's channels is refused", text[-500:])
            start = run(machine, b"run gonio src=test phase=0 level=-12 law=linear\n", 5)
            width, height, pixel = screenshot(machine.monitor, "gonio.ppm")
            sx, sy, size, under = layout(width, height)
            cx, cy = sx + (size - 1) // 2, sy + (size - 1) // 2
            check(pixel(5, height - 5) == BACKGROUND, "the tape took the screen", str(pixel(5, height - 5)))
            lit = trace_pixels(pixel, sx, sy, size)
            check(len(lit) > 50, f"mono lights the scope ({len(lit)} pixels)", "")
            check(all(abs(x - cx) <= 2 for x, _ in lit), "mono is a vertical line", str(sorted({x for x, _ in lit})[:10]))
            linear_top = min(y for _, y in lit)
            machine.wire.send(machine.guest, TAG_DATA, b"l")
            time.sleep(2)
            _, _, pixel = screenshot(machine.monitor, "gonio_log.ppm")
            log_top = min(y for _, y in trace_pixels(pixel, sx, sy, size))
            check(log_top < linear_top - 20, f"the log law lifts a quiet signal (top {linear_top} -> {log_top})", "")
            machine.wire.send(machine.guest, TAG_DATA, b"l")
            time.sleep(2)
            _, _, pixel = screenshot(machine.monitor, "gonio.ppm")
            bar_y = under + 8
            check(pixel(sx + size * 3 // 4, bar_y) == GREEN, "the correlation bar is filled to the right", str(pixel(sx + size * 3 // 4, bar_y)))
            check(pixel(sx + size // 4, bar_y) != RED, "and not to the left", "")
            meter_x = sx + size + 28 + 56 + 5
            lit_bar = sum(1 for y in range(sy, sy + size) if pixel(meter_x, y) not in (BACKGROUND, AXIS))
            check(lit_bar > 20, f"the left level meter is lit ({lit_bar} rows)", "")
            for key in (b"p", b"p", b"p", b"p"):
                machine.wire.send(machine.guest, TAG_DATA, key)
                time.sleep(0.3)
            machine.wire.send(machine.guest, TAG_DATA, b"r")
            time.sleep(4)
            _, _, pixel = screenshot(machine.monitor, "gonio2.ppm")
            lit = trace_pixels(pixel, sx, sy, size)
            check(len(lit) > 50 and all(abs(y - cy) <= 2 for _, y in lit), "opposite polarity is a horizontal line", f"{len(lit)} {sorted({y for _, y in lit})[:10]}")
            check(pixel(sx + size // 4, bar_y) == RED, "the correlation bar is filled to the left in red", str(pixel(sx + size // 4, bar_y)))
            for key in (b"m", b"v", b"+", b"-", b"r", b" ", b" ", b",", b"w"):
                machine.wire.send(machine.guest, TAG_DATA, key)
                time.sleep(0.3)
            _, _, pixel = screenshot(machine.monitor, "gonio3.ppm")
            check(pixel(5, height - 5) == BACKGROUND and pixel(sx, sy) == FRAME, "the keys left the display intact", "")
            machine.wire.send(machine.guest, TAG_DATA, b"\x1b")
            text = machine.wait_for("frame(s) drawn", since=start, timeout=20)
            check("capture overruns 0" in text, "no capture overruns", text[-500:])
            machine.wait_for("Tape returned", since=start, timeout=10)
            start = len(machine.text())
            time.sleep(1)
            machine.wire.send(machine.guest, TAG_DATA, b"\n")  # the shell's escape decoder may still hold the Escape
            time.sleep(0.5)
            machine.wire.send(machine.guest, TAG_DATA, b"cat gonio.cfg\n")
            machine.pump(3)
            text = machine.text()[start:]
            check("scale=dbfs" in text and "style=lines" in text and "law=linear" in text, "w saved the settings in gonio.cfg", machine.text()[-800:])
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"run gonio @missing.cfg\n")
            text = machine.wait_for("gonio:", since=start, timeout=20)
            check("cannot find that configuration file" in text, "a missing @file is refused", text[-300:])
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"run gonio src=test\n")
            time.sleep(4)
            _, _, saved = screenshot(machine.monitor, "gonio5.ppm")
            check(saved(5, height - 5) == BACKGROUND, "the saved settings load (second run draws)", "")
            machine.wire.send(machine.guest, TAG_DATA, b"\x1b")
            machine.wait_for("Tape returned", since=start, timeout=20)
            time.sleep(0.5)
            _, _, last = screenshot(machine.monitor, "gonio4.ppm")
            check(last(sx, sy) != FRAME, "the scope is gone when the tape ends", "")
        finally:
            machine.stop()
    print("gonio: all checks passed")


if __name__ == "__main__":
    main()
