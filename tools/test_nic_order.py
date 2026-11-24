#!/usr/bin/env python3
"""NIC numbering (`make test-nic-order`, `kernel_core::nicorder`): cards come up in PCI order whatever their driver, and
`nic N MAC` lines in `settings` pin a card to a number. An RTL8139 at 00:03.0 and an e1000 at 00:04.0 are numbered 1 and 2
(the old driver-by-driver order made the e1000 nic1); with `nic 1 <e1000 MAC>` they swap. `nic pin` prints the lines that
keep the current numbers, and a pin for an absent MAC or a bad line is reported, not fatal. Both kernels: the embedded one
(FOSFOR_IMAGE) and the server kernel (FOSFOR_IMAGE_MF)."""

import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import fosfs_image  # noqa: E402
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, clean, find  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
IMAGE_MF = Path(os.environ.get("FOSFOR_IMAGE_MF", ROOT / "build" / "fosfor-mf" / "fosfor.img"))
RTL = "52:54:00:00:81:39"
INTEL = "52:54:00:00:10:0e"
failures = []


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}\n{detail[-2500:]}")


def boot(tmp, name, image, settings):
    """Boots `image` with the two cards and `settings`; returns the serial text up to the prompt and after `nic pin`."""
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    vars_copy = Path(tmp) / f"{name}-vars.fd"
    shutil.copy(variables, vars_copy)
    disk = Path(tmp) / f"{name}.img"
    shutil.copy(image, disk)
    if settings:
        fosfs_image.add(disk, {"settings": settings})
    command = ["qemu-system-x86_64", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none", "-serial", "stdio",
               "-monitor", "none", "-no-reboot", "-nic", "none",
               "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
               "-drive", f"format=raw,file={disk}",
               # The RTL8139 sits before the e1000 on the bus; its driver is probed after the e1000's.
               "-netdev", "user,id=n1", "-device", f"rtl8139,netdev=n1,addr=0x3,mac={RTL}",
               "-netdev", "user,id=n2", "-device", f"e1000,netdev=n2,addr=0x4,mac={INTEL}"]
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    raw = bytearray()

    def wait_for(marker, since, timeout):
        end = time.monotonic() + timeout
        while marker not in clean(bytes(raw))[since:]:
            if time.monotonic() > end or process.poll() is not None:
                return False
            if select.select([process.stdout], [], [], 0.1)[0]:
                raw.extend(os.read(process.stdout.fileno(), 4096))
        return True

    try:
        if not wait_for("fos> ", 0, 180):
            check(False, f"{name}: shell prompt", clean(bytes(raw)))
            return ""
        time.sleep(1)
        mark = len(clean(bytes(raw)))
        process.stdin.write(b"nic pin\n")
        process.stdin.flush()
        wait_for("fos> ", mark + 1, 30)
        return clean(bytes(raw))
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()


def pinned(text):
    """The `nic N MAC` lines `nic pin` printed, as {number: mac}."""
    return {int(n): mac for n, mac in re.findall(r"^nic (\d+) ([0-9a-f:]{17})\s*$", text, re.M)}


def main():
    with tempfile.TemporaryDirectory() as tmp:
        for kernel, image in (("embedded", IMAGE), ("mf", IMAGE_MF)):
            text = boot(tmp, f"{kernel}-plain", image, None)
            check(pinned(text) == {1: RTL, 2: INTEL}, f"{kernel}: PCI order, the RTL8139 (00:03.0) is nic1", text)
            order = [m.start() for m in (re.search(r"rtl8139 at 00:03\.0", text), re.search(r"e1000 \(\w+\) at 00:04\.0", text)) if m]
            check(len(order) == 2 and order[0] < order[1], f"{kernel}: brought up in that order", text)

            settings = f"nic 1 {INTEL}\nnic 3 02:00:00:00:00:01\nnic x {RTL}\n"
            text = boot(tmp, f"{kernel}-pinned", image, settings)
            check(pinned(text) == {1: INTEL, 2: RTL}, f"{kernel}: `nic 1 <e1000 MAC>` makes the e1000 nic1", text)
            check("settings: nic 1 52:54:00:00:10:0e -- that card is nic1." in text, f"{kernel}: the pin is narrated", text)
            check("pin 3 ignored: no card with MAC 02:00:00:00:00:01" in text, f"{kernel}: a pin for an absent card is reported", text)
            check(f"`nic x {RTL}` ignored (nic N MAC)" in text, f"{kernel}: a bad line is reported", text)
    if failures:
        print("\n\n".join(failures))
        sys.exit(1)
    print("test-nic-order: all checks passed.")


if __name__ == "__main__":
    main()
