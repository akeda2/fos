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

Two lists below keep the summary honest instead of hiding problems:

  KNOWN_FAILING  Fail today for a reason that is not the test's fault (see
                 the note beside each). Reported as XFAIL and do not fail the
                 run; a pass is reported as XPASS so the entry gets removed.
  FLAKY          Retried once; a pass on retry is reported as FLAKY-PASS.

Deliberately not run (need a human, a host tap interface or real audio):
tools/test_realnet_e2e.py, tapes/test_audio_net_real_mic_e2e.py.
"""

import subprocess
import sys
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
    "make test-ne2000",
    "make test-ne2000-net",
    "make test-e1000-net",
    "make test-tape-bridge",
    "make test-tape-nettest",
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
    "tools/test_hostname_discovery_e2e.py",
    "tools/test_kernel_update_e2e.py",
    "tools/test_netfs_e2e.py",
    "tools/test_reset_e2e.py",
    "tools/test_term_net_e2e.py",
    "tools/test_term_net_tape_e2e.py",
]

KNOWN_FAILING = {
    # Every isochronous packet reports an error although playback timing is
    # right; not yet investigated (real driver bug or QEMU behavior change).
    "python3 tapes/test_uhci_play_e2e.py": "all isochronous packets report failed",
}

# Timing-sensitive against a finite polling receive ring (tape load pacing).
FLAKY = {"python3 tapes/test_audio_rtp_opus_send_e2e.py"}

GROUPS = {
    "most": MOST,
    "unit": UNIT,
    "e2e": E2E_MAKE + [f"python3 {s}" for s in E2E_SCRIPTS],
}
GROUPS["all"] = GROUPS["most"] + GROUPS["unit"] + GROUPS["e2e"]


def run(cmd: str) -> tuple[bool, float]:
    print(f"\n=== {cmd}", flush=True)
    start = time.monotonic()
    result = subprocess.run(cmd, shell=True, cwd=ROOT)
    return result.returncode == 0, time.monotonic() - start


def main() -> int:
    if len(sys.argv) != 2 or sys.argv[1] not in GROUPS:
        print(__doc__)
        return 2
    rows = []
    for cmd in GROUPS[sys.argv[1]]:
        ok, seconds = run(cmd)
        retried = False
        if not ok and cmd in FLAKY:
            print("--- flaky test failed; retrying once", flush=True)
            ok, extra = run(cmd)
            seconds += extra
            retried = True
        if ok:
            status = "FLAKY-PASS" if retried else ("XPASS" if cmd in KNOWN_FAILING else "PASS")
        else:
            status = "XFAIL" if cmd in KNOWN_FAILING else "FAIL"
        rows.append((status, seconds, cmd))

    print("\n" + "=" * 72)
    for status, seconds, cmd in rows:
        note = f"  ({KNOWN_FAILING[cmd]})" if cmd in KNOWN_FAILING and status != "PASS" else ""
        print(f"{status:<10} {seconds:6.0f}s  {cmd}{note}")
    failed = [r for r in rows if r[0] == "FAIL"]
    print(f"{len(rows)} run, {len(failed)} failed, "
          f"{sum(r[0] == 'XFAIL' for r in rows)} known-failing, "
          f"{sum(r[0] == 'XPASS' for r in rows)} unexpectedly passing")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
