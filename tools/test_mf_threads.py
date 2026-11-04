#!/usr/bin/env python3
"""Tape threads on the server kernel (`make test-mf-threads`): the `thrcheck` tape runs four counting threads, a futex wake-up,
concurrent alloc/free and ends while one thread sleeps forever and another spins. It must print PASS, the process must exit
cleanly, no thread of it may remain (`ps`), and a second run must work (the address space was given back)."""
import os
import select
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_THREADS", ROOT / "build" / "fosfor-mf" / "fosfor-threads.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())


def main():
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "4", "-display", "none",
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

    def type_line(line):
        qemu.stdin.write(line.encode() + b"\n")
        qemu.stdin.flush()

    problems = []
    try:
        if not wait_for("fos> ", 150):
            problems.append("no shell prompt")
        else:
            for run in (1, 2):
                mark = len(log)
                type_line("tape thrcheck")
                if not wait_for("thrcheck: PASS", 90, mark):
                    problems.append(f"run {run}: no PASS line")
                    break
                if not wait_for("exited with code 0", 30, mark):
                    problems.append(f"run {run}: the process did not exit (a sleeping or spinning thread kept it alive?)")
                    break
                if not wait_for("fos> ", 30, mark):
                    problems.append(f"run {run}: no prompt after the tape")
                    break
                new = log[mark:].decode("utf8", "replace")
                if "FAIL" in new or "CPU exception" in new or "killed by exception" in new:
                    problems.append(f"run {run}: a check failed or a fault")
                    break
            if not problems:
                mark = len(log)
                type_line("ps")
                wait_for("fos> ", 20, mark)
                if "thrcheck" in log[mark:].decode("utf8", "replace").split("ps", 1)[-1]:
                    problems.append("a thrcheck thread is still listed by ps")
            if problems:
                problems.append(log[-2000:].decode("utf8", "replace"))
    finally:
        qemu.kill()
    print(("FAIL: " if problems else "PASS: ") + "tape threads share memory, wake each other and stop with the tape")
    for problem in problems:
        print(problem)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
