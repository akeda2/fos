#!/usr/bin/env python3
"""SysRq and the trace ring on the server kernel (`make test-mf-sysrq`, kernel-mf/src/sysrq.rs, debugcpu.rs), at -smp 4:

 1. `trace` and `sysrq` in a `term net` session: every CPU with a backtrace, the threads, memory, recent trace events.
 2. A bare SysRq request (tag 0x24, no session) is answered with the dump as DATA frames ending in the end line.
 3. `debug hang now` (the service thread spins, interrupts on): SysRq is still answered, by the receive thread, with a line
    saying how long ago the service thread last went round.
 4. `debug deadhang now` (interrupts off as well): another CPU answers, and the hung CPU, stopped by an NMI, shows
    "interrupts OFF" with the shell's frames in its backtrace.
QEMU only: whether a real board delivers the NMI to a CPU with interrupts off is for hardware."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_debug_mode as base  # noqa: E402
from test_boot import ROOT, check  # noqa: E402
from test_mf_sessions import Client, READY, discover  # noqa: E402

base.KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf"
TAG_SYSRQ = 0x24
END = "--- end sysrq ---"


def sysrq(client, timeout=15):
    """A bare SysRq request; the dump, or None."""
    client.seen.clear()
    client.mark = 0
    client.send(TAG_SYSRQ)
    return client.until(END, timeout)


def boot(tmp):
    machine = base.Machine(tmp, {"debug": "kernel-update\n"})
    machine.extra = ["-smp", "4"]
    machine.cpu = "max"
    machine.start()
    machine.wait_for(READY)
    return machine, discover(machine)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        machine, guest = boot(tmp)
        try:
            shell, asker = Client(0xa1, guest), Client(0xc3, guest)
            check(shell.hello(), "a session gets a prompt", shell.seen.decode(errors="replace"))
            out = shell.run("trace on sched")
            check(out and "event(s) recorded; on:" in out and "sched" in out, "trace on sched", out or "")
            out = shell.run("trace 10")
            check(out and "sched switch" in out and "/t" in out, "trace prints context switches with CPU and thread", out or "")
            out = shell.run("sysrq", timeout=30)
            check(out and END in out and "cpu3:" in out and "bt:" in out, "sysrq in a session: every CPU with a backtrace", out or "")
            check("threads (" in out and "service" in out and "memory:" in out, "and the threads and memory", out)

            dump = sysrq(asker)
            check(dump is not None and "cpu0:" in dump and "cpu3:" in dump and "session 1" in dump,
                  "a bare SysRq request is answered over the wire", (dump or asker.seen.decode(errors="replace"))[-2500:])
            check("--- sysrq: up" in dump and "last trace events" in dump, "with the header and the trace events", dump)

            shell.send(0x21, b"debug hang now\n")
            time.sleep(3)
            dump = sysrq(asker)
            # The receive thread (net-rx) drains the NICs on their interrupts, so it answers while the service thread spins;
            # the timer interrupt answers only when the drain is stuck too (the deadhang below).
            check(dump is not None and ("answered from the timer interrupt" in dump or "service thread last went round" in dump),
                  "with the service thread spinning, SysRq is still answered and says the service thread is behind",
                  (dump or asker.seen.decode(errors="replace"))[-2500:])
            check("cpu" in dump and "(service)" in dump and "interrupts on" in dump, "and shows the spinning service thread", dump)
        finally:
            machine.stop()

        machine, guest = boot(tmp)
        try:
            shell, asker = Client(0xa2, guest), Client(0xc4, guest)
            check(shell.hello(), "after a fresh boot a session gets a prompt", shell.seen.decode(errors="replace"))
            shell.send(0x21, b"debug deadhang now\n")
            time.sleep(3)
            dump = sysrq(asker)
            check(dump is not None and "answered from the timer interrupt" in dump,
                  "with a CPU spinning with interrupts off, another CPU answers", (dump or asker.seen.decode(errors="replace"))[-2500:])
            hung = [line for line in dump.splitlines() if "(service)" in line and line.startswith("cpu")]
            check(hung and "interrupts OFF" in hung[0] and "kernel at k+" in hung[0],
                  "the hung CPU answered the NMI with its program counter, interrupts OFF", dump)
        finally:
            machine.stop()
    print("PASS: test-mf-sysrq")


if __name__ == "__main__":
    main()
