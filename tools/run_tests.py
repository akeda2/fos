#!/usr/bin/env python3
"""Runs a named group of fos tests, keeps going after a failure, and prints
one summary table at the end.

    python3 tools/run_tests.py most|unit|e2e|all

Groups (see the Makefile's `test-most`, `test-unit`, `test-all` targets):

  most  `make test` plus the tests that guard boot-critical and
        unattended-recovery behavior (stack guard, page protection, debug
        mode / KUP, start script, scripts+settings, release loops).
  unit  Host-side `cargo test` suites of the tape crates that have them.
  e2e   Every other QEMU test: remaining make targets plus the tape/tool
        scripts that have no make target of their own.
  all   most + unit + e2e.

Plain `make test` stays the fast gate and is deliberately not changed.

Parallel runs (`-j N`, `make test-all TEST_JOBS=3`): the default is serial.
With N > 1 the `most` tests and anything that rebuilds or swaps the image
still run alone, one at a time, first. The remaining QEMU/cargo jobs then
run N at a time, with three protections:

  * each job runs the python/cargo commands from its Makefile recipe
    directly (a `make test-X` would rebuild build/fos.img under the others);
  * QEMU is started through tools/qemu-snapshot/, which adds `-snapshot`
    so concurrent guests can share the read-only image;
  * jobs whose scripts use the same multicast group (found by scanning the
    script sources for 230.x.y.z addresses) never overlap, because every
    guest and host script on one group would hear the others' frames.

Use at most about half the cores: tests with timing limits (tape pacing,
`test-idle-cpu`) get flaky on a loaded machine.

Two lists below keep the summary honest instead of hiding problems:

  KNOWN_FAILING  Fail today for a reason that is not the test's fault (see
                 the note beside each). Reported as XFAIL and do not fail the
                 run; a pass is reported as XPASS so the entry gets removed.
  FLAKY          Retried once; a pass on retry is reported as FLAKY-PASS.

Deliberately not run (need a human, a host tap interface or real audio):
tools/test_realnet_e2e.py, tapes/test_audio_net_real_mic_e2e.py.
"""

import argparse
import os
import re
import subprocess
import sys
import termios
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CARGO = "cargo +nightly-2026-09-13 test --manifest-path"

MOST = [
    "make test",
    "make test-stack-guard",
    "make test-page-protection",
    "make test-debug-mode",
    "make test-start",
    "make test-scripts-settings",
    "make test-release-loops",
]

UNIT = [
    "make test-tape-net",
    "make test-tape-ip-chat",
    "make test-tape-telnet",
    "make test-tape-audio-rtp",
    "make test-tape-opus-codec",
    f"{CARGO} tapes/ed/Cargo.toml --lib",
    f"{CARGO} tapes/router/Cargo.toml --lib",
    f"{CARGO} tapes/nettest/Cargo.toml --lib",
]

E2E_MAKE = [
    "make test-unsafe-memory",
    "make test-nic-watchdog",
    "make test-idle-cpu",
    "make test-e1000-net",
    "make test-tape-bridge",
    "make test-tape-nettest",
    "make test-tape-memtest",
    "make test-usb-hosts",
    "make test-xhci",
    "make test-ehci",
    "make test-ahci",
    "make test-v2",
    "make test-v2-faults",
    "make test-v2-keyboard",
    "make test-v2-usb-boot",
    "make test-v2-hub",
    "make test-v2-disks",
    "make test-v2-stick-guids",
    "make test-v2-format",
    "make test-v2-ed",
    "make test-v2-diag",
    "make test-v2-usb-audio",
    "make test-v2-audio-net",
    "make test-v2-audio-serve",
    "make test-v2-record",
    "make test-v2-lineedit",
    "make test-v2-scripts",
    "make test-v2-nic",
    "make test-v2-netfs",
    "make test-v2-config",
    "make test-v2-tapes",
    "make test-v2-nettest",
    "make test-v2-debug-mode",
    "make test-v2-msi",
    "make test-tape-nettest-router",
    "make test-tape-router",
    "make test-tape-router-arp",
    "make test-tape-net-dhcp",
    "make test-tape-ed",
    "make test-cpu64-lab",
    "make test-cpu64-show",
    "make test-audio-capture",
    "make test-audio-net",
    "make test-audio-multi",
    "make test-audio-rtp",
]

# Scripts that had no make target. Passing as of 2026-10-02 unless listed in
# KNOWN_FAILING.
E2E_SCRIPTS = [
    "tapes/test_audio_net_multi_e2e.py",
    "tapes/test_audio_rtp_duplex_e2e.py",
    "tapes/test_audio_rtp_opus_e2e.py",
    "tapes/test_audio_rtp_opus_send_e2e.py",
    "tapes/test_beep_e2e.py",
    "tapes/test_bigsieve_e2e.py",
    "tapes/test_calc_e2e.py",
    "tapes/test_hda_capture_e2e.py",
    "tapes/test_hda_e2e.py",
    "tapes/test_hda_multi_e2e.py",
    "tapes/test_hda_playback_e2e.py",
    "tapes/test_hello_rs_e2e.py",
    "tapes/test_ip_chat_e2e.py",
    "tapes/test_longmode_rs_e2e.py",
    "tapes/test_morse_e2e.py",
    "tapes/test_primes_e2e.py",
    "tapes/test_primes_vga_e2e.py",
    "tapes/test_telnet_e2e.py",
    "tapes/test_uhci_audio_facade_e2e.py",
    "tapes/test_uhci_e2e.py",
    "tapes/test_uhci_play_e2e.py",
    "tapes/test_usb_boot_promote_e2e.py",
    "tapes/test_usb_storage_disk_e2e.py",
    "tapes/test_usb_storage_e2e.py",
    "tapes/test_vga_colors_e2e.py",
    "tools/test_foscopy_e2e.py",
    "tools/test_hostname_discovery_e2e.py",
    "tools/test_kernel_update_e2e.py",
    "tools/test_netfs_e2e.py",
    "tools/test_reset_e2e.py",
    "tools/test_term_net_e2e.py",
    "tools/test_term_net_tape_e2e.py",
]

KNOWN_FAILING: dict[str, str] = {}

# Timing-sensitive against a finite polling receive ring (tape load pacing).
FLAKY = {"python3 tapes/test_audio_rtp_opus_send_e2e.py"}

# Not in any group on purpose: `make test-ne2000` and `make test-ne2000-net`
# (QEMU's ne2k_pci). NE2000 support is set aside (see DEFERRED.md); both still
# work as make targets.

# Jobs that rewrite or swap build/fos.img, measure host CPU use, or are the
# boot-critical gate itself: always run alone, even with -j.
EXCLUSIVE = set(MOST) | {
    "make test-unsafe-memory",
    "make test-nic-watchdog",
    "make test-idle-cpu",
}

GROUPS = {
    "most": MOST,
    "unit": UNIT,
    "e2e": E2E_MAKE + [f"python3 {s}" for s in E2E_SCRIPTS],
}
GROUPS["all"] = GROUPS["most"] + GROUPS["unit"] + GROUPS["e2e"]


def makefile_recipe(target: str) -> list[str] | None:
    """Commands of a Makefile target's recipe, or None if they use make
    variables (and so must be run through make itself)."""
    lines, found = [], False
    for line in (ROOT / "Makefile").read_text().splitlines():
        if re.match(rf"^{re.escape(target)}\s*:", line):
            found = True
            continue
        if found:
            if line.startswith("\t"):
                text = line.strip().lstrip("@-").strip()
                if text and not text.startswith("#"):
                    lines.append(text)
            elif line.strip() and not line.startswith("#"):
                break
    lines = [line.replace("$(IMAGE)", "build/fos.img") for line in lines]
    if not found or any("$(" in line for line in lines):
        return None
    return lines


class Job:
    def __init__(self, cmd: str, parallel: bool):
        self.cmd = cmd
        self.commands = [cmd]
        self.exclusive = cmd in EXCLUSIVE
        if parallel and not self.exclusive and cmd.startswith("make "):
            recipe = makefile_recipe(cmd.split()[1])
            if recipe:
                self.commands = recipe  # direct: no image rebuild under others
            else:
                self.exclusive = True  # needs make itself, which rebuilds the image
        # Shared resources used by the scripts this job runs: multicast
        # groups, and the test modules they import. Importing a sibling test
        # module shares its fixed TCP ports and QEMU monitor socket path, so
        # two jobs that import the same module (or one that *is* that
        # module) must not overlap.
        self.locks: set[str] = set()
        seen: set[str] = set()
        todo = [path for command in self.commands for path in re.findall(r"\S+\.py", command)]
        while todo:
            path = todo.pop()
            if path in seen:
                continue
            seen.add(path)
            try:
                text = (ROOT / path).read_text()
            except OSError:
                continue
            self.locks.add("module:" + Path(path).stem)
            self.locks.update(re.findall(r"\b23\d\.\d+\.\d+\.\d+\b", text))
            for module in re.findall(r"^\s*(?:from|import)\s+(test_\w+)", text, re.M):
                for directory in ("tapes", "tools"):
                    if (ROOT / directory / f"{module}.py").exists():
                        todo.append(f"{directory}/{module}.py")


def run_job(job: Job, parallel: bool) -> tuple[bool, float, str]:
    env = dict(os.environ)
    if parallel and not job.exclusive:
        env["PATH"] = f"{ROOT / 'tools' / 'qemu-snapshot'}:{env['PATH']}"
    start = time.monotonic()
    out = [f"\n=== {job.cmd}\n"]
    ok = True
    for command in job.commands:
        # stdin=/dev/null: several tests start QEMU with `-serial stdio` and
        # no stdin of their own. QEMU then puts the *terminal* it inherits
        # into raw mode and restores the state it saw at start-up; with QEMUs
        # overlapping (or one killed) the last restore can be the raw state,
        # leaving the user's shell without echo (`stty sane`).
        result = subprocess.run(command, shell=True, cwd=ROOT, env=env, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        out.append(result.stdout.decode("utf-8", "replace"))
        if result.returncode != 0:
            ok = False
            break
    return ok, time.monotonic() - start, "".join(out)


def run_with_policy(job: Job, parallel: bool) -> tuple[str, float, str]:
    ok, seconds, text = run_job(job, parallel)
    retried = False
    if not ok and job.cmd in FLAKY:
        text += "\n--- flaky test failed; retrying once\n"
        ok, extra, more = run_job(job, parallel)
        seconds += extra
        text += more
        retried = True
    if ok:
        status = "FLAKY-PASS" if retried else ("XPASS" if job.cmd in KNOWN_FAILING else "PASS")
    else:
        status = "XFAIL" if job.cmd in KNOWN_FAILING else "FAIL"
    return status, seconds, text


def schedule(jobs: list[Job], workers: int) -> dict[str, tuple[str, float]]:
    """Runs jobs, at most `workers` at once, never two with a shared lock;
    exclusive jobs run alone. Output is printed per job when it finishes."""
    results: dict[str, tuple[str, float]] = {}
    pending = list(jobs)
    running: dict[str, Job] = {}
    cond = threading.Condition()

    def worker(job: Job) -> None:
        status, seconds, text = run_with_policy(job, workers > 1)
        with cond:
            print(text, flush=True)
            print(f"--- {status} {seconds:.0f}s {job.cmd}", flush=True)
            results[job.cmd] = (status, seconds)
            del running[job.cmd]
            cond.notify_all()

    with cond:
        while pending or running:
            started = False
            for job in list(pending):
                busy = set().union(*(r.locks for r in running.values())) if running else set()
                if job.exclusive:
                    if running:
                        break  # barrier: nothing may start past an exclusive job
                elif any(r.exclusive for r in running.values()) or job.locks & busy:
                    continue
                if len(running) >= workers:
                    break
                pending.remove(job)
                running[job.cmd] = job
                threading.Thread(target=worker, args=(job,), daemon=True).start()
                started = True
                if job.exclusive:
                    break
            if not started:
                cond.wait()
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description="Run a named group of fos tests.")
    parser.add_argument("group", choices=sorted(GROUPS))
    parser.add_argument("-j", "--jobs", type=int, default=1, help="parallel jobs (default 1)")
    args = parser.parse_args()
    workers = max(1, args.jobs)
    # Belt and braces for the stdin note in run_job: whatever a test does to the
    # terminal, hand it back as we found it.
    saved_tty = None
    try:
        saved_tty = termios.tcgetattr(sys.stdin.fileno())
    except (termios.error, OSError, ValueError):
        pass
    try:
        return run_group(args, workers)
    finally:
        if saved_tty is not None:
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSANOW, saved_tty)


def run_group(args: argparse.Namespace, workers: int) -> int:
    cmds = GROUPS[args.group]
    # Exclusive jobs first (stable), so the parallel pool runs at the end.
    cmds = [c for c in cmds if c in EXCLUSIVE] + [c for c in cmds if c not in EXCLUSIVE]
    jobs = [Job(c, workers > 1) for c in cmds]
    started = time.monotonic()
    results = schedule(jobs, workers)

    print("\n" + "=" * 72)
    for job in jobs:
        status, seconds = results[job.cmd]
        note = f"  ({KNOWN_FAILING[job.cmd]})" if job.cmd in KNOWN_FAILING and status != "PASS" else ""
        print(f"{status:<10} {seconds:6.0f}s  {job.cmd}{note}")
    counts = lambda name: sum(r[0] == name for r in results.values())
    print(f"{len(jobs)} run in {time.monotonic() - started:.0f}s, {counts('FAIL')} failed, "
          f"{counts('XFAIL')} known-failing, {counts('XPASS')} unexpectedly passing")
    return 1 if counts("FAIL") else 0


if __name__ == "__main__":
    sys.exit(main())
