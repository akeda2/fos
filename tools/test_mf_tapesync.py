#!/usr/bin/env python3
"""The `tapesync` client on the server kernel (docs/big-kernel.md): kernel-mf pulls newer tapes from another machine, the client
code being the one the embedded kernel uses (`crates/kernel-core/src/tapesync_client.rs`). The far machine is an embedded-kernel
guest (alpha, sharing the key); a second one (charlie) has another key. Checks:

 1. `nic discover` and `nic peers` list the machines that answered;
 2. identical tapes: nothing to pull; a missing tape (`calc`) is listed as new and, without `apply`, not installed;
 3. `apply` installs it, byte for byte what alpha has (read back with foscopy), and a second run finds nothing to do;
 4. a manifest signed with another key is refused.
QEMU only: real NIC pacing on silicon and a transfer racing a running tape are for hardware."""

import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_audio_net as audio_net  # noqa: E402
import test_debug_mode as base  # noqa: E402
from test_boot import ROOT, check  # noqa: E402

base.KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf"
audio_net.GROUP, audio_net.PORT = base.MCAST_GROUP, base.MCAST_PORT
READY = "entering the service loop"
MAC = "52:54:00:12:34:7b"


def foscopy(machine, args, cwd):
    command = [sys.executable, str(ROOT / "tools/foscopy.py"), "--mcast", f"{base.MCAST_GROUP}:{base.MCAST_PORT}", "--delay", "0.002",
               "--timeout", "15", "--mac", MAC, *args]
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, cwd=cwd)
    while process.poll() is None:
        machine.pump(0.1)
    return process.returncode, process.stdout.read()


def main():
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        tapes = ROOT / "build/fosfor/tapes"
        machine = base.Machine(tmp, {"hostname": "mfhost\n", "synckey": "correct horse\n"}, {"hello": tapes / "hello"})
        machine.start()
        guests = []
        try:
            machine.wait_for(READY)
            machine.connect()
            # The far machines boot after the connection, so discovery during `connect` cannot pick one of them.
            alpha = audio_net.Guest(tmp, "sync-a", "52:54:00:12:35:a1", files={"hostname": "alpha", "synckey": "correct horse\n"})
            charlie = audio_net.Guest(tmp, "sync-c", "52:54:00:12:35:c3", files={"hostname": "charlie", "synckey": "battery staple\n"})
            guests = [alpha, charlie]
            for guest in guests:
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")

            out = machine.run("nic discover", 30)
            check("alpha1" in out and "charlie1" in out, "nic discover finds both machines", out[-600:])
            out = machine.run("nic peers")
            check("alpha1" in out and "charlie1" in out, "nic peers lists them", out[-600:])
            alpha.pump(2.5)

            out = machine.run("tapesync alpha1", 60)
            check("calc" in out and "new here" in out, "a tape alpha has and we lack is listed as new", out[-900:])
            check("would be pulled" in out, "without `apply` nothing is changed", out[-900:])
            code, out = foscopy(machine, [":disk1/calc", "calc.none"], work)
            check(code != 0, "and calc is still missing", out[-400:])
            alpha.pump(2.5)

            out = machine.run("tapesync alpha1 apply", 120)
            check("calc: installed" in out and "0 failed" in out, "apply installs it", out[-900:])
            code, out = foscopy(machine, [":disk1/calc", "calc.back"], work)
            check(code == 0 and (work / "calc.back").read_bytes() == (tapes / "calc").read_bytes(), "byte for byte what the build has", out[-400:])
            alpha.pump(2.5)
            out = machine.run("tapesync alpha1", 60)
            check("calc" in out and "the same" in out, "a second run finds calc up to date", out[-900:])
            alpha.pump(2.5)

            out = machine.run("tapesync charlie1", 60)
            check("alpha" not in out and ("not available" in out or "no answer" in out or "signature" in out.lower() or "key" in out.lower()), "a machine with another key is refused", out[-900:])
        finally:
            for guest in guests:
                guest.stop()
            machine.stop()
    print("test-mf-tapesync: all checks passed")


if __name__ == "__main__":
    main()
