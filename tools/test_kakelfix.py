#!/usr/bin/env python3
"""The kakelfix tape's Mixer page on the loopback device's software mixer (`make test-kakelfix`).
Embedded kernel (writes made inline):
 - `kakelfix device=loop0 show` lists the identity mix (Out L <- Play L, Out R <- Play R at 0.0 dB) and the Dim switch
 - in the tape, `x` switches the selected strip off; a mouse click on the first fader sets it where the click is
   (half way up the travel is about -9 dB); a click on the Quit soft key ends the tape; `show` then reads the new gains
 - the screen shows the mixer (header text drawn, not a blank frame)
Server kernel (writes go to the worker thread): `x`, then `q` (the shell's Escape kills a foreground process there, so
`q` is what ends it through the writer's join), then `show`, at -smp 4.
QEMU's screen is 1280x800, so kakelfix's layout is at scale 1 (crates/kakelfix/src/layout.rs): soft key 8 (Quit) is at
x 1184..1272, y 620..676 (after two half-key gaps between the groups); strip 0's fader travel is x 12..57, y 152..744."""

import os
import re
import select
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import TAG_DATA, check, monitor  # noqa: E402
from test_debug_mode import Machine  # noqa: E402
from test_gfx import screenshot  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
TAPE = ROOT / "build/fosfor/tapes/kakelfix"
MF_IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_KAKELFIX", ROOT / "build/fosfor-mf/fosfor-kakelfix.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
QUIT = (1228, 648)
FADER_MIDDLE = (32, 448)


def send(machine, data, wait=0.6):
    machine.wire.send(machine.guest, TAG_DATA, data)
    machine.pump(wait)


def sends(text, output):
    """{source: level} of one output's line in `kakelfix show`."""
    m = re.search(rf"^\s*{output}:(.*)$", text, re.M)
    check(m, f"`show` has a line for {output}", text[-800:])
    return dict(re.findall(r"(Play [LR]) ([+-]?\d+\.\d|off|\?)", m.group(1)))


def show(machine):
    start = len(machine.text())
    send(machine, b"run kakelfix device=loop0 show\n", 2)
    return machine.wait_for("Dim:", since=start, timeout=15)


def steps(distance):
    """Counts per packet that move the cursor exactly `distance` (even) pixels: the kernel's pointer gain makes a packet of d
    counts travel 4d + d|d|/2 pixels (`usb_pointer::mouse_input`), so 10 counts go 90, 2 go 10 and 1 goes 4."""
    out = []
    while distance >= 98:
        out.append(10)
        distance -= 90
    while distance % 4:
        out.append(2)
        distance -= 10
    return out + [1] * (distance // 4)


def pointer_to(machine, x, y):
    """Parks the cursor in the top left corner (it is clamped), then walks it to (x, y)."""
    for _ in range(4):
        monitor(str(machine.monitor), "mouse_move -100 -100")
    for count in steps(x):
        monitor(str(machine.monitor), f"mouse_move {count} 0")
    for count in steps(y):
        monitor(str(machine.monitor), f"mouse_move 0 {count}")
    machine.pump(0.3)


def where(machine):
    start = len(machine.text())
    send(machine, b"touch\n", 1)
    m = re.search(r"at (\d+),(\d+),", machine.text()[start:])
    return m and (int(m.group(1)), int(m.group(2)))


def embedded():
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {}, {"kakelfix": TAPE})
        machine.monitor = Path(tmp) / "monitor"
        machine.start()
        machine.wait_for("M3 complete")
        machine.connect()
        try:
            text = show(machine)
            check("loop0" in text or "Loopback" in text or "loop" in text.lower(), "`show` names the loopback mixer", text[-600:])
            check(sends(text, "Out L") == {"Play L": "+0.0"}, "identity mix: Out L <- Play L at 0.0 dB", text[-600:])
            check(sends(text, "Out R") == {"Play R": "+0.0"}, "identity mix: Out R <- Play R at 0.0 dB", text[-600:])
            check(re.search(r"Dim: Off", text), "the Dim switch is listed, off", text[-600:])

            # Cursor placement is checked in the shell first: a click must land where the layout says.
            pointer_to(machine, *FADER_MIDDLE)
            at = where(machine)
            check(at == FADER_MIDDLE, f"the cursor is at the first fader's middle ({at})", machine.text()[-400:])

            start = len(machine.text())
            send(machine, b"run kakelfix device=loop0\n", 4)
            width, height, pixel = screenshot(machine.monitor, "kakelfix.ppm")
            check((width, height) == (1280, 800), f"the screen is 1280x800 ({width}x{height})")
            background = pixel(width // 2, height - 2)
            inked = sum(pixel(x, y) != background for x in range(8, 400, 2) for y in range(4, 20))
            check(inked > 50, f"the header is drawn ({inked} pixels)")
            # Strip 0 is the Play L/R pair into Out L/R: off with `x`, then a click half way up its fader.
            send(machine, b"x", 1)
            monitor(str(machine.monitor), "mouse_button 1")
            machine.pump(0.3)
            monitor(str(machine.monitor), "mouse_button 0")
            machine.pump(0.5)
            pointer_to(machine, *QUIT)
            monitor(str(machine.monitor), "mouse_button 1")
            machine.pump(0.3)
            monitor(str(machine.monitor), "mouse_button 0")
            machine.wait_for("fos>", since=start + 10, timeout=10)
            machine.pump(1)
            text = show(machine)
            left, right = sends(text, "Out L"), sends(text, "Out R")
            check(set(left) == {"Play L"} and set(right) == {"Play R"}, f"the pair stays a pair ({left}, {right})", text[-600:])
            check(float(left["Play L"]) < -5 and float(right["Play R"]) < -5, f"the fader click set about -9 dB ({left}, {right})", text[-600:])
        finally:
            machine.stop()


def server():
    if not MF_IMAGE.exists():
        raise SystemExit(f"{MF_IMAGE} missing; run `make test-kakelfix`")
    tmp = tempfile.TemporaryDirectory()
    socket_path = str(Path(tmp.name) / "monitor")
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "4", "-display", "none",
         "-nic", "user,model=e1000", "-serial", "stdio", "-monitor", f"unix:{socket_path},server,nowait", "-no-reboot",
         "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={MF_IMAGE},snapshot=on"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    log = b""

    def wait_for(text, timeout, start=0):
        nonlocal log
        end = time.time() + timeout
        while time.time() < end and text.encode() not in log[start:]:
            if select.select([qemu.stdout], [], [], 0.2)[0]:
                data = os.read(qemu.stdout.fileno(), 4096)
                if not data:
                    break
                log += data
        return text.encode() in log[start:]

    def type_text(data):
        qemu.stdin.write(data)
        qemu.stdin.flush()

    try:
        check(wait_for("fos> ", 150), "mf: shell prompt", log[-800:].decode("utf8", "replace"))
        boot_log = log.decode("utf8", "replace")
        check("PS/2: mouse on the second port" in boot_log, "mf: the PS/2 mouse (a laptop touchpad's port) is found", boot_log[-1500:])
        mark = first = len(log)
        type_text(b"touch monitor 3\n")
        wait_for("showing finger events", 5, mark)
        for command in ["mouse_move 20 10", "mouse_button 1", "mouse_button 0"]:
            monitor(socket_path, command)
        wait_for("fos> ", 10, mark)
        text = log[mark:].decode("utf8", "replace")
        check("finger 100 down/moved" in text and "finger 100 up" in text, "mf: PS/2 mouse moves and clicks reach the pointer queue",
              text[-800:])
        mark = len(log)
        type_text(b"tape kakelfix device=loop0\n")
        time.sleep(4)
        new = log[mark:].decode("utf8", "replace")
        check("no writer thread" not in new, "mf: the writer thread started", new[-600:])
        type_text(b"x")
        time.sleep(1)
        type_text(b"q")
        check(wait_for("exited with code 0", 20, mark), "mf: `q` ends kakelfix cleanly (the writer finished and joined)",
              log[mark:].decode("utf8", "replace")[-800:])
        wait_for("fos> ", 10, mark)
        mark = len(log)
        type_text(b"tape kakelfix device=loop0 show\n")
        check(wait_for("Dim:", 20, mark), "mf: `show` runs", log[mark:].decode("utf8", "replace")[-800:])
        text = log[mark:].decode("utf8", "replace")
        check(sends(text, "Out L") == {} and sends(text, "Out R") == {}, "mf: `x` from the tape reached the device through the worker", text[-600:])
        new = log[first:].decode("utf8", "replace")
        check("CPU exception" not in new and "killed by exception" not in new, "mf: no fault", new[-800:])
    finally:
        qemu.kill()
        qemu.wait()
        tmp.cleanup()


def main():
    embedded()
    print("kakelfix: embedded kernel checks passed")
    server()
    print("kakelfix: all checks passed")


if __name__ == "__main__":
    main()
