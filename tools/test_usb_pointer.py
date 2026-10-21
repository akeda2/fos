#!/usr/bin/env python3
"""A USB pointer device nothing claims is looked at: its HID report descriptor is read, dumped and parsed (xHCI).

QEMU's `usb-tablet` is an absolute pointer (X and Y 0..32767) and `usb-mouse` a relative one. The boot (or `usb rescan`) must
print each device's report descriptor and what `hid::parse_pointer` made of it (kernel/src/usb_pointer.rs), and the kernel must
stay healthy. This is the first step towards touch input for tapes; a real touch panel is read the same way. Run with
`make test-usb-pointer`."""
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, check, clean, find  # noqa: E402


def main():
    with tempfile.TemporaryDirectory() as tmp:
        code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
        vars_copy, image = Path(tmp) / "vars.fd", Path(tmp) / "boot.img"
        shutil.copy(variables, vars_copy)
        shutil.copy(IMAGE, image)
        process = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
             "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
             "-drive", f"format=raw,file={image}", "-device", "qemu-xhci,id=hc",
             "-device", "usb-tablet,bus=hc.0,port=1", "-device", "usb-mouse,bus=hc.0,port=2"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        try:
            deadline = time.monotonic() + 90
            while "fos> " not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: no prompt\n{clean(bytes(raw))[-2500:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))
            text = clean(bytes(raw))
            check(text.count("report descriptor,") == 2, "both pointer devices have their report descriptor dumped", text[-2500:])
            check("[0..32767]" in text, "the tablet's absolute X range 0..32767 is parsed", text[-2500:])
            check(" rel" in text, "the mouse's relative axes are recognised", text[-2500:])
            check("CPU EXCEPTION" not in text and process.poll() is None, "the kernel is still healthy", text[-1500:])
        finally:
            process.kill()
            process.wait()
    print("usb pointer: all checks passed")


if __name__ == "__main__":
    main()
