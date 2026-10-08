#!/usr/bin/env python3
"""Local keyboard tests for the v2 kernel (milestone M3b).

1. A PS/2 keyboard: keys sent through the QEMU monitor (`sendkey`) arrive as scancodes
   on the emulated 8042, are routed by the I/O APIC to the kernel, decoded and fed to
   the shell, whose output appears on the serial port.
2. A USB keyboard (`usb-kbd` on `qemu-xhci`, with the PS/2 controller switched off so
   the keys can only arrive through the xHCI driver and its HID boot-keyboard
   handling).
3. A USB keyboard on `usb-ehci` (high speed; the split-transaction path for full/low
   speed devices behind a hub's transaction translator needs real hardware, since
   QEMU has no TT).
4. No keyboard at all (`-machine q35,i8042=off`: no 8042 on the board): the kernel
   must still boot to its network shell, reporting the missing controller and
   carrying on. A keyboard is never a boot dependency.

Run with `make test-keyboard`."""

import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, check, clean, find, monitor  # noqa: E402


def boot(machine, script, extra=()):
    """Boots, waits for the shell, runs script(send_monitor, wait_for) and returns the serial text."""
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        monitor_path = str(Path(tmp) / "monitor")
        command = ["qemu-system-x86_64", "-machine", machine, "-m", "512", "-display", "none", "-serial", "stdio",
                   "-monitor", f"unix:{monitor_path},server,nowait", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}",
                   "-drive", f"if=pflash,format=raw,file={vars_copy}", "-drive", f"format=raw,file={IMAGE}", *extra]
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()

        def wait_for(marker, timeout=60):
            deadline = time.monotonic() + timeout
            while marker not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: never saw {marker!r}\n{clean(bytes(raw))[-3000:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))

        try:
            wait_for("M3 complete")
            wait_for("fos> ")
            script(lambda text: monitor(monitor_path, text), wait_for)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
        return clean(bytes(raw))


def type_keys(send, keys):
    for key in keys:
        send(f"sendkey {key}")
        time.sleep(0.12)


def ps2_session(send, wait_for):
    type_keys(send, ["u", "p", "t", "i", "m", "e", "ret"])
    wait_for("ticks at 100 Hz")
    type_keys(send, ["shift-h", "ret"])
    wait_for("Unknown command: H")
    type_keys(send, ["x", "y", "backspace", "z", "ret"])
    wait_for("Unknown command: xz")


def erase_on_screen_session(shots):
    """Types two letters and erases both: the *screen* (not just the serial echo) must end up
    exactly as it was. The shell echoes Backspace as "\\x08 \\x08", which the framebuffer
    console once drew as '?', a blank and '?'."""
    def session(send, wait_for):
        time.sleep(1)
        send(f"screendump {shots[0]}")
        time.sleep(1)
        type_keys(send, ["x", "y"])
        time.sleep(0.5)
        send(f"screendump {shots[1]}")
        time.sleep(1)
        type_keys(send, ["backspace", "backspace"])
        time.sleep(0.5)
        send(f"screendump {shots[2]}")
        time.sleep(1)
    return session


def main():
    with tempfile.TemporaryDirectory() as tmp:
        shots = [Path(tmp) / f"screen{n}.ppm" for n in range(3)]
        boot("q35", erase_on_screen_session(shots))
        before, typed, erased = (shot.read_bytes() for shot in shots)
        check(before != typed, "typed letters appear on the screen", "")
        check(before == erased, "Backspace erases them on the screen too (the console honours the \\x08 echo)", "")

    text = boot("q35", ps2_session)
    check("PS/2 keyboard: 8042 ready" in text, "the PS/2 controller is initialised and its IRQ routed through the I/O APIC", text)
    check("ticks at 100 Hz" in text, "typed `uptime` runs the command", text)
    check("Unknown command: H" in text, "Shift produces upper case", text)
    check("Unknown command: xz" in text, "Backspace edits the line", text)

    text = boot("q35,i8042=off", ps2_session, ["-device", "qemu-xhci,id=xhci", "-device", "usb-kbd,bus=xhci.0"])
    check("USB: 1 xHCI and 0 EHCI controller(s), 1 usable device(s)" in text, "the xHCI driver finds the USB keyboard", text)
    check("USB keyboard 0627:0001" in text and "ready" in text, "and sets it up as a HID boot keyboard", text)
    check("ticks at 100 Hz" in text and "Unknown command: H" in text and "Unknown command: xz" in text, "keys typed on the USB keyboard reach the shell (letters, Shift, Backspace)", text)

    text = boot("q35,i8042=off", ps2_session, ["-device", "usb-ehci,id=ehci", "-device", "usb-kbd,bus=ehci.0"])
    check("USB: 0 xHCI and 1 EHCI controller(s), 1 usable device(s)" in text, "the EHCI driver finds a keyboard on a high-speed port", text)
    check("ticks at 100 Hz" in text and "Unknown command: H" in text and "Unknown command: xz" in text, "keys typed on the EHCI keyboard reach the shell (its interrupt endpoint polled through the async schedule)", text)

    text = boot("q35,i8042=off", lambda send, wait_for: None)
    check("no 8042 controller" in text, "with no PS/2 controller the kernel reports it", text)
    check("M3 complete" in text, "and still boots to its shell", text)
    print("keyboard: all checks passed")


if __name__ == "__main__":
    main()
