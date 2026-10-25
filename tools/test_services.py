#!/usr/bin/env python3
"""Background tapes: `run ptp master ... &1` keeps serving while the shell and a foreground tape are used.

The master guest starts the ptp master as a service on its only NIC and gets its prompt back. While the service runs
the shell answers (`echo`, `tasks`), a foreground tape runs and ends (`calc`) without disturbing it, and a slave guest
still locks on to the master's Announce/Sync and completes Delay_Req exchanges (the service owns the NIC, so the
frames reach it, not the foreground tape). `kill 1` ends the service; the slave then stops hearing the master.
Run with `make test-services`."""

import re
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402

MASTER_MAC, SLAVE_MAC = "52:54:00:12:36:01", "52:54:00:12:36:02"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        master = Guest(tmp, "master", MASTER_MAC)
        slave = Guest(tmp, "slave", SLAVE_MAC)
        try:
            for guest in (master, slave):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark = master.send("run ptp master ip=10.7.0.1/24 &1")
            master.wait_for("Service ptp started on NIC 1", 30, mark)
            master.wait_for("ptp master: NIC 1 10.7.0.1", 30, mark)
            master.wait_for("fos> ", 10, mark)
            check(True, "the service starts and the shell prompt comes back")

            mark = master.send("echo shell-alive")
            master.wait_for("shell-alive", 10, mark)
            mark = master.send("tasks")
            master.wait_for("NIC 1", 10, mark)
            text = master.text()[mark:]
            check(re.search(r"1\s+ptp\s+NIC 1\s+(idle|running)", text) is not None, "`tasks` lists the service", text)

            mark = master.send("run calc 3 4 + 6 *")
            master.wait_for("= 42", 20, mark)
            master.wait_for("Tape returned", 20, mark)
            check(True, "a foreground tape runs and ends while the service keeps its NIC")

            slave_mark = slave.send("run ptp ip=10.7.0.2/24 secs=14")
            slave.wait_for("ptp: master 52:54:00:ff:fe:12:36:01", 30, slave_mark)
            check(True, "a slave finds the service's master from its Announce")
            slave.wait_for("path delay", 40, slave_mark)
            text = slave.text()[slave_mark:]
            summary = re.search(r"ptp: (\d+) samples; announce (\d+) sync (\d+) follow-up (\d+) delay-resp (\d+) sync lost (\d+)", text)
            check(summary is not None, "the slave prints a summary", text[-1200:])
            samples, _, syncs, _, delay_resps, lost = map(int, summary.groups())
            check(samples >= 40 and syncs >= samples, f"Sync/Follow_Up pairs complete at the master's rate ({samples} samples)")
            check(delay_resps >= 8, f"the service answers Delay_Req messages ({delay_resps})")
            check(lost <= 2, f"almost no Sync is lost while the shell is in use ({lost})")

            # A foreground tape on the service's NIC gets no frames: it must not disturb the service.
            mark = master.send("run ptp ip=10.7.0.3/24 secs=3")
            master.wait_for("Tape returned", 30, mark)
            slave_mark = slave.send("run ptp ip=10.7.0.2/24 secs=8")
            slave.wait_for("path delay", 40, slave_mark)
            check(re.search(r"ptp: (\d+) samples", slave.text()[slave_mark:]) is not None and int(re.search(r"ptp: (\d+) samples", slave.text()[slave_mark:]).group(1)) >= 20, "the service keeps serving after a foreground tape ended on its NIC")

            mark = master.send("kill 1")
            master.wait_for("Service ptp ended.", 20, mark)
            mark = master.send("tasks")
            master.wait_for("No services running", 10, mark)
            check(True, "`kill 1` ends the service")
            mark = master.send("run ptp master ip=10.7.0.1/24 &1")
            master.wait_for("fos> ", 20, mark)
            mark = master.send("ps")
            master.wait_for("NIC 1", 10, mark)
            mark = master.send("kill nosuch")
            master.wait_for("no service", 10, mark)
            mark = master.send("kill ptp")
            master.wait_for("Service ptp ended.", 20, mark)
            mark = master.send("run ptp master ip=10.7.0.1/24 &1")
            master.wait_for("fos> ", 20, mark)
            mark = master.send("ps")
            master.wait_for("NIC 1", 10, mark)
            mark = master.send("kill all")
            master.wait_for("Service ptp ended.", 20, mark)
            mark = master.send("ps")
            master.wait_for("No services running", 10, mark)
            check(True, "`kill NAME` and `kill all` end services; `ps` lists them")
            slave_mark = slave.send("run ptp ip=10.7.0.2/24 secs=6")
            slave.wait_for("Tape returned", 30, slave_mark)
            check("ptp: master" not in slave.text()[slave_mark:], "the slave hears no master after the service ended")
            # Several NICs: a second guest has two and a service owns both.
            two = Guest(tmp, "two", "52:54:00:12:36:03", extra=["-netdev", "user,id=net1", "-device", "e1000,netdev=net1,mac=52:54:00:12:36:04"])
            try:
                two.wait_for("M3 complete")
                two.wait_for("fos> ")
                mark = two.send("run ptp master ip=10.7.0.9/24 &1,2")
                two.wait_for("Service ptp started on NIC 1,2", 30, mark)
                two.wait_for("fos> ", 10, mark)
                mark = two.send("ps")
                two.wait_for("NIC 1,2", 10, mark)
                check(True, "a service can own several NICs (`&1,2`), `ps` lists them")
                mark = two.send("run ptp master ip=10.7.0.8/24 &2")
                two.wait_for("a service already owns NIC 2", 10, mark)
                check(True, "a second service cannot take one of its NICs")
                mark = two.send("run ptp master ip=10.7.0.8/24 &3")
                two.wait_for("no NIC 3", 10, mark)
                check(True, "a NIC that does not exist is refused")
                mark = two.send("kill 1")
                two.wait_for("Service ptp ended.", 20, mark)
            finally:
                two.stop()
            print("PASS: services")
        finally:
            master.stop()
            slave.stop()


if __name__ == "__main__":
    main()
