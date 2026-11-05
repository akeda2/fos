#!/usr/bin/env python3
"""Runs real tapes on the server kernel (`make test-mf-tape`): build/fosfor-mf/fosfor-tapes.img has hello, fpcheck, gfxcheck, netcheck (DHCP + ping on QEMU user networking), fscheck (files: ranges, two streams), ed (keyboard + fs_read/fs_write) and a text file on
FOSFS; the serial shell starts each with `tape NAME`, and the test checks their output and that the process exits 0, with 1, 2 and 8 CPUs.
Hard-float (AVX2) is checked under `-cpu max`, SSE2 under qemu64."""

import os
import re
import select
import subprocess
import sys
import socket
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(Path(__file__).resolve().parent))
import testslot  # noqa: E402

PEER_PORT = testslot.port(5873)
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_TAPES", ROOT / "build" / "fosfor-mf" / "fosfor-tapes.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
CASES = [
    ("hello x y", ["arguments: [x y]", "primes up to 100000: 9592"]),
    ("fpcheck", ["fpcheck: ok"]),
    ("gfxcheck secs=1", ["gfxcheck: drawn"]),
    ("fscheck note.txt 10 5", ["fscheck: 5 bytes [abcde]"]),
    ("fscheck note.txt 100 5", ["fscheck: 0 bytes []"]),
    ("fscheck streams s1.bin s2.bin", ["streams 0: 20000 bytes, 0 wrong", "streams 1: 19900 bytes, 0 wrong"]),
    ("fscheck remove gone.tmp", ["fscheck: remove ok"]),
    ("fscheck info", ["hostname [fos-", "disk1 kind ", "console ", "clock set"]),
    ("netcheck", ["lease: address", "4/4 replies."]),
    ("gpiopanel", ["gpiopanel: this machine has no GPIO pins"]),
    ("memtest size=8 passes=1", ["memtest: PASS"]),
    ("hello", ["first file: ", "files on the boot volume: "]),
]


def run(cpu, smp):
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-cpu", cpu, "-machine", "q35", "-m", "512", "-smp", str(smp), "-display", "none",
         "-nic", "user,model=e1000", "-serial", "stdio", "-serial", f"tcp:127.0.0.1:{PEER_PORT},server=on,wait=off", "-monitor", "none", "-no-reboot",
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
                done = wait_for("exited with code 0.", 40, mark) and all(wait_for(e, 5, mark) for e in expected)
                new = log[mark:].decode("utf8", "replace")
                if not done or "exited with code 0." not in new:
                    failures.append(f"`tape {command}` did not finish cleanly:\n{new}")
                    break
        if not failures:
            # serial_*: port 1 is the second UART (COM2), a TCP socket on the host.
            mark = len(log)
            peer = socket.create_connection(("127.0.0.1", PEER_PORT), timeout=5)
            qemu.stdin.write(b"tape run fscheck serial\n")
            qemu.stdin.flush()
            if wait_for("wrote 5", 20, mark):
                peer.settimeout(5)
                heard = peer.recv(64)
                peer.sendall(b"pong\n")
            else:
                heard = b""
            wait_for("exited with code 0.", 20, mark)
            new = log[mark:].decode("utf8", "replace")
            for expected in ["serial ports 1", "serial open 9 -> -1", "serial open 0 wrote 5", "serial got [pong]", "serial reopen 0"]:
                if expected not in new:
                    failures.append(f"serial: missing {expected!r}:\n{new}")
                    break
            if b"ping" not in heard:
                failures.append(f"serial: the peer heard {heard!r}, not ping")
            peer.close()
        if not failures:
            # memtest reports a failing word at its physical address (ABI 31 `phys_addr`), not the process's virtual one.
            mark = len(log)
            qemu.stdin.write(b"tape memtest size=8 passes=1 inject=100\n")
            qemu.stdin.flush()
            wait_for("exited with code 0.", 40, mark)
            new = log[mark:].decode("utf8", "replace")
            found = re.search(r"bad 1 MiB block at (0x[0-9a-f]+)", new)
            if found is None or int(found.group(1), 16) >= 512 << 20:
                failures.append("memtest did not report a physical address inside RAM:\n" + new)
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
        if not failures:
            # Keyboard input and fs_read/fs_write together: append a line with ed, save, and read it back in a second session.
            def type_to_ed(lines, expect):
                mark = len(log)
                qemu.stdin.write(lines.encode())
                qemu.stdin.flush()
                return wait_for(expect, 10, mark)
            ok = type_to_ed("tape ed note.txt\n", "ed> ")
            ok = ok and type_to_ed("a\nhello\n.\nw\n", "written note.txt")
            ok = ok and type_to_ed("q\n", "exited with code 0.")
            ok = ok and type_to_ed("tape ed note.txt\n", "ed> ")
            ok = ok and type_to_ed("p\n", "2: hello")
            type_to_ed("q\n", "exited with code 0.")
            # ESC [ D is a cursor key for the tape, not an Escape: ed must survive it (a lone ESC kills the tape after ~30 ms).
            ok2 = type_to_ed("tape ed note.txt\n", "ed> ")
            qemu.stdin.write(b"\x1b[D")
            qemu.stdin.flush()
            time.sleep(0.5)
            mark = len(log)
            qemu.stdin.write(b"\n")
            qemu.stdin.flush()
            ok2 = ok2 and wait_for("unknown command", 10, mark)  # ed read the key as input and answered; it was not killed
            ok2 = ok2 and type_to_ed("q\n", "exited with code 0.")
            if not ok2:
                failures.append("a cursor key (ESC [ D) killed or confused the foreground tape:\n" + log[-600:].decode("utf8", "replace"))
            if not ok:
                failures.append("ed could not save and reload a file:\n" + log[-600:].decode("utf8", "replace"))
        text = log.decode("utf8", "replace")
        if (cpu == "max") != ("SMAP: the kernel reaches user memory" in text):
            failures.append("SMAP should be on exactly when the CPU model has it (max yes, qemu64 no)")
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
