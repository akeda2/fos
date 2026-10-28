#!/usr/bin/env python3
"""Mouse support: a PS/2 mouse (QEMU's built-in one, what a laptop touchpad looks like to the kernel) and a USB boot mouse on
xHCI move one cursor on the screen and reach tapes as touch panel 6 with finger id 100 (`tape_rt::TouchEvent::is_mouse`).
Checks:
 - `touch` lists the mouse and `touch monitor` shows the hover moves, the press, the drag and the release with the
   screen coordinates (the cursor starts in the middle and is clamped to the screen)
 - the same through a USB mouse
 - end to end: in `miccal` the cursor is drawn and a click on the Quit button (bottom right corner) ends the tape.
Events come from QEMU's human monitor (`mouse_move`, `mouse_button`). Run with `make test-mouse`."""

import re
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import TAG_DATA, check, monitor  # noqa: E402
from test_debug_mode import Machine  # noqa: E402
from test_gfx import screenshot  # noqa: E402
from test_miccal import AUDIO, MICCAL  # noqa: E402


def send(machine, data, wait=0.5):
    machine.wire.send(machine.guest, TAG_DATA, data)
    machine.pump(wait)


def boot(tmp, extra=(), binaries=None, files=None):
    machine = Machine(tmp, files or {}, binaries or {})
    machine.monitor = Path(tmp) / "monitor"
    machine.extra = list(extra)
    machine.start()
    machine.wait_for("M3 complete")
    machine.connect()
    return machine


def mouse(machine, *commands):
    for command in commands:
        monitor(str(machine.monitor), command)


def events(text):
    """[(x, y, down)] of the `touch monitor` lines of finger 100."""
    return [(int(x), int(y), word == "down/moved") for word, x, y in re.findall(r"finger 100 (down/moved|up) at (\d+),(\d+)", text)]


def watch(label, extra):
    with tempfile.TemporaryDirectory() as tmp:
        machine = boot(tmp, extra)
        try:
            start = len(machine.text())
            send(machine, b"touch\n", 1)
            listing = machine.text()[start:]
            check("mouse cursor" in listing, f"{label}: `touch` lists the mouse", listing[-600:])
            start = len(machine.text())
            send(machine, b"touch monitor\n", 1)
            mouse(machine, "mouse_move 20 10", "mouse_button 1", "mouse_move 30 0", "mouse_button 0")
            machine.pump(1.0)
            seen = events(machine.text()[start:])
            check(len(seen) >= 3, f"{label}: hover, press/drag and release arrive ({seen})", machine.text()[start:][-800:])
            downs = [e for e in seen if e[2]]
            ups = [e for e in seen if not e[2]]
            check(len(downs) >= 1 and len(ups) >= 2, f"{label}: down events while the button is held, up events otherwise ({seen})", "")
            check(downs[-1][0] > ups[0][0], f"{label}: dragging with the button down moves the cursor right ({seen})", "")
            send(machine, b"\x1b", 1)
            mouse(machine, *["mouse_move 400 400"] * 4)
            start = len(machine.text())
            send(machine, b"touch\n", 1)
            listing = machine.text()[start:]
            m = re.search(r"at (\d+),(\d+),", listing)
            check(m and int(m.group(1)) <= 4096 and int(m.group(2)) <= 4096, f"{label}: the cursor stays on the screen ({m and m.groups()})", listing[-400:])
        finally:
            machine.stop()


def travel(settings):
    """How far one `mouse_move 20 0` takes the cursor from the centre with the given `settings` file."""
    with tempfile.TemporaryDirectory() as tmp:
        machine = boot(tmp, [], None, {"settings": settings} if settings else None)
        try:
            boot_log = machine.text()
            send(machine, b"touch monitor\n", 1)
            start = len(machine.text())
            mouse(machine, "mouse_move 20 0")
            machine.pump(1.0)
            seen = events(machine.text()[start:])
            check(seen, "a move arrives", machine.text()[start:][-400:])
            return seen[-1][0] - 640, boot_log
        finally:
            machine.stop()


def speed_setting():
    full, _ = travel("")
    half, log = travel("mouse-speed 50\n")
    check("mouse-speed 50%" in log, "settings: `mouse-speed 50` is announced", log[-600:])
    check(full > 0 and abs(half * 2 - full) <= 2, f"mouse-speed 50 halves the travel ({half} against {full})", "")
    _, log = travel("mouse-speed 5\n")
    check("mouse-speed 5` ignored" in log, "settings: an out-of-range mouse-speed is ignored", log[-600:])


def clicks_in_miccal():
    with tempfile.TemporaryDirectory() as tmp:
        machine = boot(tmp, AUDIO, {"miccal": MICCAL})
        try:
            start = len(machine.text())
            send(machine, b"run miccal device=loop0 outdev=loop0 method=reference source=pink name=mouse\n", 6)
            mouse(machine, "mouse_move 1 0")
            time.sleep(0.5)
            width, height, pixel = screenshot(machine.monitor, "mouse.ppm")
            # The arrow starts at the centre; look for the text-coloured arrow body in a window around it.
            cx, cy = width // 2, height // 2
            ink = (200, 208, 216)

            def arrow(pixel):
                return sum(pixel(x, y) == ink for x in range(cx - 4, cx + 24, 2) for y in range(cy - 4, cy + 40, 2))

            check(arrow(pixel) > 20, "the cursor arrow is drawn at the screen centre", str(arrow(pixel)))
            mouse(machine, *["mouse_move 300 300"] * 8)
            time.sleep(0.5)
            _, _, moved = screenshot(machine.monitor, "mouse2.ppm")
            check(arrow(moved) == 0, "the cursor left the centre when the mouse moved", str(arrow(moved)))
            # The corner pixel is in the gap beside the last button: step in.
            mouse(machine, "mouse_move -2 -2", "mouse_button 1", "mouse_button 0")
            text = machine.wait_for("frame(s) analysed", since=start, timeout=15)
            check("miccal:" in text, "a click on the Quit button (bottom right) ends the tape", text[-800:])
        finally:
            machine.stop()


def main():
    watch("PS/2", [])
    watch("USB", ["-device", "qemu-xhci,id=hc", "-device", "usb-mouse,bus=hc.0"])
    speed_setting()
    clicks_in_miccal()
    print("mouse: all checks passed")


if __name__ == "__main__":
    main()
