#!/usr/bin/env python3
"""The `midi` shell command on a machine without a USB MIDI device.

QEMU has no USB-MIDI device model, so the transfer path is only checked on real hardware (a class-compliant cable looped
from out to in: `midi send` comes back through `midi monitor`). What this test pins: with an xHCI controller and no MIDI device
`midi` and `midi monitor` say there is none and return to the prompt, `midi send` refuses a missing device, and a bad usage
line prints the usage. Run with `make test-midi`."""
import os
import selectors
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import CODE_CANDIDATES, ROOT, VARS_CANDIDATES, check, clean, find  # noqa: E402

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image = tmp / "image.img"
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
                        "--output", str(image)], check=True, capture_output=True)
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-no-reboot", "-serial", "stdio",
             "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
             "-device", "qemu-xhci", "-device", "usb-kbd",
             "-device", "ich9-ahci,id=ahci", "-drive", f"if=none,id=d1,format=raw,snapshot=on,file={image}",
             "-device", "ide-hd,drive=d1,bus=ahci.0,bootindex=0"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        selector = selectors.DefaultSelector()
        selector.register(qemu.stdout, selectors.EVENT_READ)
        output = bytearray()

        def pump(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                for key, _ in selector.select(0.02):
                    chunk = os.read(key.fd, 4096)
                    if chunk:
                        output.extend(chunk)

        def wait_for(text, seconds=30):
            end = time.monotonic() + seconds
            while text not in clean(bytes(output)):
                if time.monotonic() > end or qemu.poll() is not None:
                    raise SystemExit(f"FAIL: no {text!r}:\n{clean(bytes(output))[-2000:]}")
                pump(0.1)

        def run(line, expect):
            mark = len(clean(bytes(output)))
            qemu.stdin.write(line.encode() + b"\r")
            qemu.stdin.flush()
            end = time.monotonic() + 15
            while expect not in clean(bytes(output))[mark:]:
                if time.monotonic() > end:
                    raise SystemExit(f"FAIL: {line!r} gave no {expect!r}:\n{clean(bytes(output))[mark:]}")
                pump(0.1)
            wait_for("fos> ")
            return clean(bytes(output))[mark:]

        try:
            wait_for("fos> ", 60)
            pump(1.0)
            check("no USB MIDI device" in run("midi", "no USB MIDI device"), "midi lists nothing")
            check("no USB MIDI device" in run("midi monitor", "no USB MIDI device"), "monitor returns at once")
            check("midi send" in run("midi send 90 3c 64", "midi send"), "send needs a device")
            check("Usage: midi" in run("midi bogus", "Usage: midi"), "usage line")
            print("PASS: midi command without a device")
        finally:
            qemu.kill()
            qemu.wait()


if __name__ == "__main__":
    main()
