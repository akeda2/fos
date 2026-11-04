#!/usr/bin/env python3
"""Hostile tape-table calls on the server kernel (`make test-mf-abibad`): the `abibad` tape calls every table function with null,
tiny, unmapped, kernel-half, read-only and all-ones pointers and sizes. A pointer from a tape is a number to be validated; the
kernel must neither fault (SMAP) nor run out of memory. A process killed by a fault, or any kernel exception, fails the test."""
import os
import select
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_ABIBAD", ROOT / "build" / "fosfor-mf" / "fosfor-abibad.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())


def main():
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
         "-nic", "user,model=e1000", "-serial", "stdio", "-monitor", "none", "-no-reboot",
         "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={IMAGE},snapshot=on"],
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

    problems = []
    try:
        if not wait_for("fos> ", 150):
            problems.append("no shell prompt")
        else:
            mark = len(log)
            qemu.stdin.write(b"tape abibad server\n")
            qemu.stdin.flush()
            if not wait_for("the kernel is still here", 120, mark):
                problems.append("abibad did not finish")
            new = log[mark:].decode("utf8", "replace")
            if "CPU exception" in new or "killed by exception" in new or "KERNEL BUG" in new:
                problems.append("a fault")
            if problems:
                problems.append(new[-1500:])
    finally:
        qemu.kill()
    print(("FAIL: " if problems else "PASS: ") + "every tape-table function survives hostile arguments")
    for problem in problems:
        print(problem)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
