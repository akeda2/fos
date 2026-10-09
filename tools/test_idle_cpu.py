#!/usr/bin/env python3
"""An idle machine sleeps instead of spinning: waiting for input must not pin a host core.

fos polls its NICs and keys in several loops. Each sleeps in `hlt` when a pass found nothing to do (the shell's idle prompt,
`term net`, and the tapes through `tape_rt::idle()` / the `wait_for_event` callback), relying on the NIC's interrupt and the
100 Hz timer to wake it. The QEMU process's CPU time (from /proc, all threads) is a faithful proxy: a guest that executes `hlt`
lets the vCPU thread sleep, a spinning guest uses a whole host core. Measured over a few seconds in each situation:

  1. The shell's idle prompt                                  -> low
  2. `calc`, `ed` and `net-chat` waiting for a key              -> low
  3. A `term net` session open at the prompt                    -> low
  4. Control: `memtest` (a RAM tester that never sleeps)        -> HIGH, which proves the measurement sees a busy guest.

Ported from the legacy test of the same name (tools/test_idle_cpu_e2e.py on legacy-v1). Run with `make test-idle-cpu`."""
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import (BROADCAST, CODE_CANDIDATES, IMAGE, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)

LOW_LIMIT = 0.25   # fraction of one host core
HIGH_LIMIT = 0.60
SAMPLE_SECONDS = 4.0


def cpu_seconds(pid):
    with open(f"/proc/{pid}/stat") as stat:
        fields = stat.read().rsplit(")", 1)[1].split()   # the command name may contain spaces
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")   # utime + stime


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        image = Path(tmp) / "image.img"
        shutil.copy(IMAGE, image)
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={image}",
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:7a"]
        process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        wire = Wire()

        def pump(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                ready, _, _ = select.select([process.stdout], [], [], 0.1)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))

        def wait_for(marker, timeout=60):
            end = time.monotonic() + timeout
            while marker not in clean(bytes(raw)):
                if time.monotonic() > end or process.poll() is not None:
                    raise SystemExit(f"FAIL: never saw {marker!r}\n{clean(bytes(raw))[-2500:]}")
                pump(0.2)

        def type_line(text, marker, timeout=30):
            mark = len(clean(bytes(raw)))
            process.stdin.write(text.encode() + b"\n")
            process.stdin.flush()
            end = time.monotonic() + timeout
            while marker not in clean(bytes(raw))[mark:]:
                if time.monotonic() > end:
                    raise SystemExit(f"FAIL: never saw {marker!r} after {text!r}\n{clean(bytes(raw))[-2500:]}")
                pump(0.2)

        def measure(label, low):
            pump(1.5)   # let the screen redraw settle
            start_cpu, start = cpu_seconds(process.pid), time.monotonic()
            pump(SAMPLE_SECONDS)
            usage = (cpu_seconds(process.pid) - start_cpu) / (time.monotonic() - start)
            if low:
                check(usage <= LOW_LIMIT, f"{label}: {usage * 100:.0f}% of a core (limit {LOW_LIMIT * 100:.0f}%)")
            else:
                check(usage >= HIGH_LIMIT, f"control, {label}: {usage * 100:.0f}% of a core (the measurement can see a busy guest)")

        def leave_tape():
            mark = len(clean(bytes(raw)))
            process.stdin.write(b"\x1b")   # a lone Escape on the serial port stops a tape
            process.stdin.flush()
            end = time.monotonic() + 20
            while "Tape returned" not in clean(bytes(raw))[mark:]:
                if time.monotonic() > end:
                    raise SystemExit(f"FAIL: the tape did not return after Escape\n{clean(bytes(raw))[-1500:]}")
                pump(0.2)
            pump(0.5)

        try:
            wait_for("M3 complete")
            wait_for("fos> ")
            # The boot-time wait for a term net client is over once the shell has a prompt; the prompt itself must sleep.
            measure("the idle shell prompt", True)

            type_line("run calc", "> ")
            measure("calc waiting for a key", True)
            leave_tape()

            type_line("write k.txt hello", "Wrote 5 bytes")
            type_line("run ed k.txt", "ed> ")
            measure("ed waiting for a key", True)
            type_line("q", "fos> ")

            type_line("run net-chat", "Type a line")
            measure("net-chat waiting for a key", True)
            leave_tape()

            # A term net session open at the prompt (the client is a test harness: HELLO, then silence).
            wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            guest = None
            for _ in range(30):
                for source, tag, _ in wire.frames(0.2):
                    if tag == TAG_DISCOVER_REPLY:
                        guest = source
                if guest:
                    break
            wire.send(guest, TAG_HELLO)
            wire.until("fos> ", 10)
            measure("an open term net session", True)

            type_line("run memtest size=8 passes=0", "memtest", 20)
            measure("memtest running", False)
            leave_tape()
        finally:
            process.kill()
            process.wait()
    print("idle cpu: all checks passed")


if __name__ == "__main__":
    main()
