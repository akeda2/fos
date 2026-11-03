#!/usr/bin/env python3
"""USB on the server kernel (`make test-mf-usb`): an xHCI controller with a USB keyboard comes up, and keys typed on it
(QEMU monitor `sendkey`) reach the shell, a USB stick (usb-storage) is a disk whose file a tape reads, at -smp 1, 2 and 8. QEMU has no USB-MIDI or touch-digitizer model, so
`midi_*` and `touch_*` for tapes are only verified on hardware."""
import os
import select
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_USB", ROOT / "build" / "fosfor-mf" / "fosfor-usb.img"))
STICK = Path(os.environ.get("FOSFOR_USB_STICK", ROOT / "build" / "fosfor-mf" / "usb-stick.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())


def run(smp):
    with tempfile.TemporaryDirectory() as tmp:
        sock = f"{tmp}/mon.sock"
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "512", "-smp", str(smp), "-display", "none",
             "-serial", "stdio", "-monitor", f"unix:{sock},server,nowait", "-no-reboot",
             "-device", "qemu-xhci", "-device", "usb-kbd",
             "-drive", f"if=none,id=stick,format=raw,file={STICK},snapshot=on", "-device", "usb-storage,drive=stick",
             "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={IMAGE},snapshot=on"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        log = b""

        def wait_for(text, timeout, start=0):
            nonlocal log
            end = time.time() + timeout
            while time.time() < end:
                if text.encode() in log[start:]:
                    return True
                if select.select([qemu.stdout], [], [], 0.2)[0]:
                    log += os.read(qemu.stdout.fileno(), 4096)
            return text.encode() in log[start:]

        try:
            if not wait_for("fos> ", 150):
                return "no shell prompt:\n" + log[-800:].decode("utf8", "replace")
            if not wait_for("USB keyboard 0627:0001", 1):
                return "the USB keyboard was not found:\n" + log[-800:].decode("utf8", "replace")
            monitor = socket.socket(socket.AF_UNIX)
            monitor.connect(sock)
            mark = len(log)
            for key in ["n", "i", "c", "ret"]:
                monitor.send(f"sendkey {key}\n".encode())
                wait_for("\0", 0.3)
            if not wait_for("nic1: ", 10, mark):
                return "keys typed on the USB keyboard did not reach the shell:\n" + log[mark:].decode("utf8", "replace")
            # The stick is disk2: its FOSFS volume is reached by name, through the USB mass-storage driver.
            mark = len(log)
            qemu.stdin.write(b"tape fscheck disk2/note.txt 10 5\n")
            qemu.stdin.flush()
            if not wait_for("fscheck: 5 bytes [abcde]", 15, mark):
                return "a file on the USB stick was not read:\n" + log[mark - 400:].decode("utf8", "replace")
            return None
        finally:
            qemu.kill()
            qemu.wait()


failed = False
for smp in (1, 2, 8):
    problem = run(smp)
    print(f"-smp {smp}: {'ok' if problem is None else 'FAIL'}")
    if problem:
        print(problem)
        failed = True
sys.exit(1 if failed else 0)
