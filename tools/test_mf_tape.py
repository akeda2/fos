#!/usr/bin/env python3
"""Runs real tapes on the server kernel (`make test-mf-tape`): build/fosfor-mf/fosfor-tapes.img has hello, fpcheck and gfxcheck on
FOSFS; the serial shell starts each with `tape NAME`, and the test checks their output and that the process exits 0, with 1, 2 and 8 CPUs.
Hard-float (AVX2) is checked under `-cpu max`, SSE2 under qemu64."""

import os
import select
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_TAPES", ROOT / "build" / "fosfor-mf" / "fosfor-tapes.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
CASES = [
    ("hello x y", ["arguments: [x y]", "primes up to 100000: 9592"]),
    ("fpcheck", ["fpcheck: ok"]),
    ("gfxcheck secs=1", ["gfxcheck: drawn"]),
]


def run(cpu, smp):
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-cpu", cpu, "-machine", "q35", "-m", "512", "-smp", str(smp), "-display", "none",
         "-serial", "stdio", "-monitor", "none", "-no-reboot",
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

    failures = []
    try:
        if not wait_for("fos> ", 150):
            failures.append("no shell prompt:\n" + log[-800:].decode("utf8", "replace"))
        else:
            for command, expected in CASES:
                mark = len(log)
                qemu.stdin.write((f"tape {command}\n").encode())
                qemu.stdin.flush()
                done = wait_for("exited with code 0.", 30, mark) and all(wait_for(e, 5, mark) for e in expected)
                new = log[mark:].decode("utf8", "replace")
                if not done or "exited with code 0." not in new:
                    failures.append(f"`tape {command}` did not finish cleanly:\n{new}")
                    break
        if not failures:
            # Escape reaches the foreground tape: gfxcheck would hold its picture for 10 s.
            mark = len(log)
            qemu.stdin.write(b"tape gfxcheck\n")
            qemu.stdin.flush()
            wait_for("gfxcheck: drawn", 10, mark)
            qemu.stdin.write(b"\x1b")
            qemu.stdin.flush()
            if not wait_for("fos> ", 5, mark + 40):
                failures.append("Escape did not end the foreground tape:\n" + log[mark:].decode("utf8", "replace"))
        text = log.decode("utf8", "replace")
        if "CPU exception" in text or "PANIC" in text:
            failures.append("kernel fault:\n" + text[-1500:])
    finally:
        qemu.kill()
    return failures


bad = False
for cpu, smp in [("qemu64", 1), ("max", 2), ("max", 8)]:
    problems = run(cpu, smp)
    print(f"-cpu {cpu} -smp {smp}: {'ok' if not problems else 'FAILED'}")
    for p in problems:
        print(p)
        bad = True
if bad:
    sys.exit(1)
print("PASS: real tapes run in ring 3 on the server kernel (hello, fpcheck, gfxcheck)")
