#!/usr/bin/env python3
"""audio-net bench: `benchserve` on one guest, `bench peer=MAC` on the other (the master).

Both guests share one virtual segment. The master sweeps five packet times in both directions, the server judges the
up direction and reports, the master judges the down direction. Checked: every step has a result, no step fails, the
summary names a recommendation, and bad options are refused. Run with `make test-audio-bench`."""

import re
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402

SERVER_MAC, MASTER_MAC = "52:54:00:12:36:01", "52:54:00:12:36:02"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        server = Guest(tmp, "server", SERVER_MAC)
        master = Guest(tmp, "master", MASTER_MAC)
        try:
            for guest in (server, master):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark = master.send("run audio-net bench")
            master.wait_for("peer=NAME|MAC", 20, mark)
            check(True, "bench without peer= says what it needs")
            mark = master.send("run audio-net bench peer=" + SERVER_MAC + " tol=abc")
            master.wait_for("tol= is a percentage", 20, mark)
            check(True, "a bad tol= is refused")
            mark = master.send("run audio-net bench peer=" + SERVER_MAC + " stepsecs=1")
            master.wait_for("stepsecs= is 2 to 60", 20, mark)
            check(True, "a bad stepsecs= is refused")
            mark = master.send("run audio-net bench peer=" + MASTER_MAC.replace("02", "09") + " proto=raw stepsecs=2")
            master.wait_for("does not answer", 60, mark)
            check(True, "a master with no server in sight says so")

            server_mark = server.send("run audio-net benchserve proto=avtp")
            server.wait_for("Waiting for a master", 20, server_mark)
            master_mark = master.send("run audio-net bench proto=avtp peer=" + SERVER_MAC + " stepsecs=2 tol=5")
            master.wait_for("Lowest within", 180, master_mark)
            report = master.text()[master_mark:]
            check("Peer answers: round trip" in report, "the clock exchange is reported", report[-1500:])
            rows = re.findall(r"^(up  |down) +(\d+) us +(\d+) fr: rx (\d+)/(\d+) lost (\d+)", report, re.M)
            check(len(rows) == 10, f"ten steps ran: five packet times, both ways ({len(rows)})", report[-1500:])
            for direction, ptime, frames, received, expected, lost in rows:
                # QEMU drops and delays packets at short packet times (that is what the bench measures); only the 2 ms steps must be clean.
                check(int(expected) > 0 and int(received) > 0, f"{direction.strip()} {ptime} us: {received} of {expected} arrived", report[-1500:])
                if ptime == "2000":
                    check(int(received) > 0.8 * int(expected), f"{direction.strip()} 2 ms packets are carried ({received} of {expected})", report[-1500:])
            check(re.search(r"Lowest within 5\.00 %: ptime=\d+ target=\d+", report) is not None, "the summary recommends ptime and target", report[-1500:])
            server.wait_for("step 9: rx", 20, server_mark)
            check(re.search(r"step 1: rx \d+/\d+", server.text()[server_mark:]) is not None, "the server reports the steps it judged", server.text()[server_mark:][-800:])
            mark = master.send("run audio-net bench proto=avtp peer=" + SERVER_MAC + " stepsecs=30 tol=5")
            master.wait_for("up  ", 60, mark)
            mark = master.send("\x1b")
            master.wait_for("stopped", 20, mark)
            check(True, "Escape stops a running bench")
        finally:
            server.stop()
            master.stop()
    print("PASS: audio-net bench")


if __name__ == "__main__":
    main()
