#!/usr/bin/env python3
"""NIC interrupts (MSI) on the v2 kernel.

QEMU's classic `e1000` has no MSI capability; `e1000e` (82574L) does, and the v2 driver
drives both. Checks:

 * e1000e: the driver claims an MSI vector, passes its self-test (a link-status interrupt
   raised through ICS arrives as a message), and `nic` then counts interrupts as frames
   come in -- the terminal session itself works over MSI-delivered frames;
 * classic e1000: stays polled and says why;
 * `no-msi` in the settings file keeps an MSI-capable NIC polled;
 * the point of it all: a tape idling in `wait_for_event` sleeps in `hlt` (low host CPU)
   when every NIC has interrupts, and still spins (high host CPU) when one is polled."""

import os
import re
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import MCAST_GROUP, MCAST_PORT, ROOT, TAG_DATA, check  # noqa: E402
from test_debug_mode import Machine  # noqa: E402


def cpu_seconds(pid):
    with open(f"/proc/{pid}/stat") as stream:
        fields = stream.read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


class ModelMachine(Machine):
    def __init__(self, tmp, files, model):
        super().__init__(tmp, files, {"calc": ROOT / "build/cargo-target/x86_64-fos-tape/release/calc"})
        self.model = model

    def start(self):
        import shutil
        import subprocess
        vars_copy = self.tmp / "vars.fd"
        shutil.copy(self.variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={self.code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={self.image}",
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", f"{self.model},netdev=net0,mac=52:54:00:12:34:7c"]
        self.process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.raw = bytearray()
        self.guest = None


def idle_cpu(machine):
    """Host CPU fraction QEMU uses over 4 s while an interactive tape sits waiting for input."""
    machine.wire.send(machine.guest, TAG_DATA, b"run calc\n")
    machine.wire.until("> ", 15)
    time.sleep(1.0)
    start_cpu, start_time = cpu_seconds(machine.process.pid), time.monotonic()
    time.sleep(4.0)
    fraction = (cpu_seconds(machine.process.pid) - start_cpu) / (time.monotonic() - start_time)
    machine.wire.send(machine.guest, TAG_DATA, b"\x1b")
    machine.wire.until("fos> ", 15)
    return fraction


def main():
    results = {}
    for label, model, files in (("e1000e", "e1000e", {}), ("e1000 (no MSI capability)", "e1000", {}), ("e1000e with no-msi", "e1000e", {"settings": "no-msi\n"}), ("e1000e with prefer-msix", "e1000e", {"settings": "prefer-msix\n"})):
        with tempfile.TemporaryDirectory() as tmp:
            machine = ModelMachine(tmp, files, model)
            machine.start()
            try:
                machine.wait_for("M3 complete")
                machine.connect()
                text = machine.text()
                nic = machine.run("nic")
                results[label] = (text, nic, idle_cpu(machine))
                if label == "e1000e":
                    after = machine.run("nic")
                    results[label] += (after,)
            finally:
                machine.stop()

    text, nic, cpu, after = results["e1000e"]
    check("e1000 (E1000e) at" in text and "interrupts: MSI on vector 0x50" in text, "e1000e: the driver claims an MSI vector and its self-test message arrives", text)
    taken = int(re.search(r"MSI vector 0x50: (\d+) taken", nic).group(1))
    check(taken >= 2, f"`nic` counts interrupts taken ({taken}: the self-test plus the session's own frames)", nic)
    later = int(re.search(r"MSI vector 0x50: (\d+) taken", after).group(1))
    check(later > taken, f"and the count keeps rising with traffic ({taken} -> {later})", after)

    text, nic, polled_classic = results["e1000 (no MSI capability)"]
    check("interrupts: legacy INTx, I/O APIC pin" in text and "interrupts: INTx pin" in nic, "classic e1000 (no MSI capability): falls back to legacy INTx through the I/O APIC, found by probing", text)
    taken = int(re.search(r"INTx pin \d+, vector 0x\w+: (\d+) taken", nic).group(1))
    check(taken >= 2, f"and its interrupts arrive on the probed pin ({taken} taken)", nic)
    check(polled_classic < 0.35, f"an idle tape on INTx sleeps in hlt too (QEMU host CPU {polled_classic:.0%})", "")

    text, nic, polled_forced = results["e1000e with no-msi"]
    check("interrupts: polled (settings: no-msi)" in text and "interrupts: polled" in nic, "`no-msi` keeps an MSI-capable NIC polled", text)

    text, nic, msix_cpu = results["e1000e with prefer-msix"]
    check("interrupts: MSI-X on vector 0x50" in text, "`prefer-msix`: the 82574 takes MSI-X (IVAR-mapped, self-test message arrives)", text)
    check(msix_cpu < 0.35 and "MSI-X vector 0x50" in nic, f"and an idle tape sleeps in hlt on it (QEMU host CPU {msix_cpu:.0%})", nic)

    check(cpu < 0.35, f"with MSI an idle tape sleeps in hlt (QEMU host CPU {cpu:.0%})", "")
    check(polled_forced > 0.6, f"with a polled NIC the idle tape still yields in a spin loop (QEMU host CPU {polled_forced:.0%})", "")

    # The forwarding tape on two MSI-driven NICs: frames wake the tape from hlt.
    import subprocess
    outcome = subprocess.run([sys.executable, str(Path(__file__).resolve().parent / "test_nettest.py")], env=dict(os.environ, FOS_NIC_MODEL="e1000e"), capture_output=True, text=True)
    check(outcome.returncode == 0 and "all checks passed" in outcome.stdout, "nettest forwards between two MSI-driven e1000e NICs (frames wake the tape from hlt)", outcome.stdout + outcome.stderr)
    print("v2 msi: all checks passed")


if __name__ == "__main__":
    main()
