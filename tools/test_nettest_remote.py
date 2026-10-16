#!/usr/bin/env python3
"""nettest on two machines: `serve=1` on one guest, `remote=1` on the other (master).

The master finds the server by itself (unicast HELLO to the announced machines), measures the clock offset,
runs a test in both directions and prints one merged report. Both guests share one virtual segment; the
guests' clocks both start at boot, so the measured offset is small. Run with `make test-nettest-remote`."""

import re
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402
from testslot import port  # noqa: E402

SERVER_MAC, MASTER_MAC = "52:54:00:12:36:01", "52:54:00:12:36:02"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        # NIC 1 of both guests is the control network; NIC 2 a second, separate one for the test.
        second = lambda mac: ["-netdev", f"socket,id=net1,mcast=230.42.42.78:{port(4278)}", "-device", f"e1000,netdev=net1,mac={mac}"]
        server = Guest(tmp, "server", SERVER_MAC, second("52:54:00:12:37:01"))
        master = Guest(tmp, "master", MASTER_MAC, second("52:54:00:12:37:02"))
        try:
            for guest in (server, master):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark = master.send("run nettest remote=1 serve=1")
            master.wait_for("use one", 20, mark)
            check(True, "serve= and remote= together are refused")
            server_mark = server.send("run nettest")
            server.wait_for("Serving on NIC 1", 30, server_mark)
            master_mark = master.send("run nettest remote=1")
            master.wait_for("Master on NIC 1", 30, master_mark)
            master.wait_for("server: ", 60, master_mark)
            master.wait_for("peer ready: clock offset", 30, master_mark)
            text = master.text()[master_mark:]
            check(SERVER_MAC in text, "the master found the server without being told", text[-800:])
            check(re.search(r"round trip \d+ us \(\d+ samples", text) is not None, "the clock exchange reports its round trip", text[-800:])
            mark = master.send("test both rate=4000 size=800 time=2")
            master.wait_for("== remote test with", 60, mark)
            master.wait_for("(one-way latency depends", 10, mark)
            report = master.text()[mark:]
            for direction in ("up   (here -> peer)", "down (peer -> here)"):
                match = re.search(re.escape(direction) + r": sent (\d+), got (\d+)", report)
                check(match is not None, f"the report has the {direction.split()[0]} line", report[-1200:])
                sent, got = int(match.group(1)), int(match.group(2))
                check(sent >= 100 and got >= sent * 0.9, f"{direction.split()[0]}: {got} of {sent} frames arrived")
            server.wait_for("master started a test: both ways", 5, server_mark)
            check("peer -> here" in report, "both directions are reported")
            check("% of rate per second:" in report, "the report shows delivery per second", report[-1200:])
            check(re.search(r"link here: \d+ Mbit/s (full|HALF) duplex", report) is not None, "the report names the master's link speed", report[-1200:])
            check(re.search(r"link peer: \d+ Mbit/s (full|HALF) duplex", report) is not None, "the report names the server's link speed", report[-1200:])
            raw_mark = len(master.raw)
            master.process.stdin.write(b"tesx\x7ft up rate=1000 time=1\n")
            master.process.stdin.flush()
            mark = len(master.text())
            master.wait_for("== remote test with", 30, mark)
            check(b"x\x08 \x08t" in bytes(master.raw[raw_mark:]), "backspace erases the character on screen", str(bytes(master.raw[raw_mark:])[:200]))
            check("peer NICs" in text and "52:54:00:12:37:01" in text, "the master lists the server's NICs", text[-800:])
            mark = master.send("test both rate=2000 time=2 via=2 here=2")
            master.wait_for("== remote test with", 60, mark)
            report = master.text()[mark:]
            check("test path: our NIC 2 <-> peer NIC 2" in report, "the report names the test path", report[-1200:])
            for direction in ("up   (here -> peer)", "down (peer -> here)"):
                match = re.search(re.escape(direction) + r": sent (\d+), got (\d+)", report)
                check(match is not None and int(match.group(2)) >= int(match.group(1)) * 0.9 > 0, f"{direction.split()[0]} crossed the second network", report[-1200:])
            mark = master.send("test up rate=1000 time=2 via=0 here=0")
            master.wait_for("== remote test with", 30, mark)
            mark = master.send("test up rate=1000 time=1 via=3 here=0")
            master.wait_for("no such NIC", 10, mark)
            check(True, "a NIC the peer does not have is refused")
            mark = master.send("test up rate=1000 time=1 via=0 here=0")
            master.wait_for("== remote test with", 30, mark)
            report = master.text()[mark:]
            check("down (peer" not in report, "an up-only test reports only the up stream", report[-800:])
            check("delay us: min" in report, "the report shows min and queueing delay", report[-800:])
            mark = master.send("test sweep both rate=1000 to=3000 step=1000 time=1")
            master.wait_for("sweep done:", 60, mark)
            report = master.text()[mark:]
            rows = re.findall(r"^\s*(\d+)\s+\d+\s+\d+\.\d%", report.replace("nettest> ", ""), re.M)
            check(rows[:3] == ["1000", "2000", "3000"], f"the sweep prints a row per step ({rows})", report[-800:])
            check("highest rate with at most 1% loss: up 3000 kbit/s  down 3000 kbit/s" in report, "the sweep names the loss-free rate", report[-800:])
            master.send("quit")
            master.wait_for("nettest stopped.", 10)
            mark = master.send("run nettest")
            master.wait_for("Serving on NIC 1", 20, mark)
            mark = master.send("remote 1")
            master.wait_for("peer ready: clock offset", 60, mark)
            mark = master.send("nics")
            master.wait_for("remote (master)", 10, mark)
            mark = master.send("remote off")
            master.wait_for("NIC is free", 10, mark)
            check(True, "'remote N' and 'remote off' work from the idle prompt")
            mark = master.send("test up")
            master.wait_for("type remote N", 10, mark)
            master.send("quit")
            master.wait_for("nettest stopped.", 10)
        finally:
            server.stop()
            master.stop()
    print("PASS: nettest remote")


if __name__ == "__main__":
    main()
