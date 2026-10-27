#!/usr/bin/env python3
"""`tapesync`: pulling newer tapes from another machine, authenticated with the shared `synckey`.

Four machines on the multicast seam (`tapesync HOST` and the no-argument form that asks every peer): alpha (the source) and bravo (the one that pulls) share a key, charlie has another
key, delta has none. Checks: identical machines have nothing to pull; a tape that is missing (`calc`) or older
(`hello`, restamped with an old build time on the host) is listed and then, with `apply`, installed and runs; a file that
is not a tape is never overwritten; a manifest signed with another key is refused; a machine without a key does not
answer; NETFS will not read or write the key file."""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import ROOT, check  # noqa: E402
from test_netfs import GROUP, PORT, foscopy  # noqa: E402

TAPES = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "build/cargo-target")) / "x86_64-fos-tape/release"


def run(guest, line, timeout=40):
    mark = guest.send(line)
    guest.wait_for("fos> ", timeout, mark)
    return guest.text()[mark:]


def main():
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        old_hello = work / "hello"
        subprocess.run([sys.executable, str(ROOT / "tools/stamp_tape.py"), str(TAPES / "hello"), str(old_hello)], check=True, env={**os.environ, "FOS_BUILD_TIME": "1000"})
        guests = {
            "alpha": Guest(tmp, "sync-a", "52:54:00:12:35:a1", files={"hostname": "alpha", "synckey": "correct horse\n"}),
            "bravo": Guest(tmp, "sync-b", "52:54:00:12:35:b2", files={"hostname": "bravo", "synckey": "correct horse\n"}),
            "charlie": Guest(tmp, "sync-c", "52:54:00:12:35:c3", files={"hostname": "charlie", "synckey": "battery staple\n"}),
            "delta": Guest(tmp, "sync-d", "52:54:00:12:35:d4", files={"hostname": "delta"}),
        }
        alpha, bravo, charlie, delta = guests.values()
        try:
            for guest in guests.values():
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")

            out = run(bravo, "tapesync alpha1")
            check("Nothing to pull" in out, "machines built from one image have nothing to pull", out[-600:])
            alpha.pump(2.5)

            out = run(bravo, "rm calc")
            result = foscopy(["--mac", "52:54:00:12:35:b2", str(old_hello), ":disk1/hello"], work)
            check(result.returncode == 0, "bravo loses calc and gets an old build of hello", result.stderr[-400:])
            out = run(bravo, "write ptp not a tape")
            alpha.pump(2.5)

            out = run(bravo, "tapesync alpha1")
            check("calc" in out and "new here" in out, "a missing tape is listed as new", out[-800:])
            check("hello" in out and "newer there" in out, "an older tape is listed as newer there", out[-800:])
            check("ptp" in out and "not a tape" in out, "a file that is not a tape is skipped", out[-800:])
            check("would be pulled" in out, "without `apply` nothing is changed", out[-800:])
            out = run(bravo, "ls")
            check("calc" not in out, "and calc is still missing", out[-600:])
            alpha.pump(2.5)

            out = run(bravo, "tapesync alpha1 apply", timeout=90)
            check("calc: installed" in out and "hello: installed" in out and "2 installed, 0 failed" in out, "apply installs both", out[-800:])
            out = run(bravo, "run calc 3 4 +")
            check("7" in out, "and the pulled calc runs", out[-400:])
            out = run(bravo, "cat ptp")
            check("not a tape" in out, "the plain file named ptp was left alone", out[-400:])
            alpha.pump(2.5)
            out = run(bravo, "rm ptp")
            out = run(bravo, "tapesync alpha1")
            check("ptp" in out and "new here" in out, "(with it gone, ptp is new)", out[-800:])
            check("calc" in out and "the same" in out, "calc and hello now match alpha's", out[-800:])
            alpha.pump(2.5)

            out = run(bravo, "tapesync charlie1")
            check("not signed with our key" in out, "a manifest signed with another key is refused", out[-400:])
            out = run(bravo, "tapesync delta1")
            check("not available there" in out, "a machine without a synckey does not answer", out[-400:])
            # Without a HOST every machine in the peer table is asked.
            out = run(bravo, "rm calc")
            alpha.pump(2.5)
            out = run(bravo, "tapesync", timeout=90)
            check("alpha1: " in out and "charlie1: the manifest is not signed" in out and "delta1: tape sync is not available" in out, "without a HOST every peer is asked and the unusable ones are named", out[-900:])
            check("calc" in out and "new here, from alpha1" in out and "would be pulled" in out, "the useful offer is listed with its source", out[-900:])
            alpha.pump(2.5)
            out = run(bravo, "tapesync apply", timeout=90)
            check("calc: installed" in out and "ptp: installed" in out and "2 installed, 0 failed" in out, "and `tapesync apply` installs both (ptp was removed above)", out[-900:])
            out = run(bravo, "run calc 3 4 +")
            check("7" in out, "which runs", out[-300:])
            alpha.pump(2.5)

            out = run(bravo, "tapesync alpha1 apply extra")
            check("Usage: tapesync" in out, "arguments are checked", out[-300:])

            result = foscopy(["--mac", "52:54:00:12:35:a1", ":disk1/synckey", str(work / "key")], work)
            check(result.returncode != 0 and not (work / "key").exists(), "NETFS will not read the key file", result.stderr[-300:] + result.stdout[-300:])
            (work / "evil").write_text("evil\n")
            result = foscopy(["--mac", "52:54:00:12:35:a1", str(work / "evil"), ":disk1/synckey"], work)
            check(result.returncode != 0, "nor replace it", result.stderr[-300:] + result.stdout[-300:])
            out = run(alpha, "cat synckey")
            check("correct horse" in out, "the key is unchanged", out[-300:])

            # The host-side client (fosadm's content check): the same signed manifest, read from the PC.
            def manifest(key, *names):
                (work / "host-key").write_text(key + "\n")
                return subprocess.run([sys.executable, str(ROOT / "tools/tape_manifest.py"), "--mcast", f"{GROUP}:{PORT}", "--mac", "52:54:00:12:35:a1",
                                       "--key", str(work / "host-key"), *names], capture_output=True, text=True, timeout=120)
            alpha.pump(2.5)
            result = manifest("correct horse", "hello", "calc")
            check(result.returncode == 0 and "2 of 2 identical" in result.stdout, "tape_manifest.py: the signed manifest matches the build", result.stdout[-500:] + result.stderr[-300:])
            alpha.pump(2.5)
            result = manifest("wrong key", "hello")
            check(result.returncode == 2 and "not signed with our key" in result.stderr, "tape_manifest.py: another key is refused", result.stdout[-300:] + result.stderr[-300:])
        finally:
            for guest in guests.values():
                guest.stop()
    print("tapesync: all checks passed")


if __name__ == "__main__":
    main()
