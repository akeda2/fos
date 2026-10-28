#!/usr/bin/env python3
"""`gpio` on a simulated pin bank (`gpio-sim`) and GPIO over the network (`gpio @HOST`), signed with the `synckey`.

alpha has pins (sim) and allows remote writes, bravo is a client with the same key, charlie has another key, delta has
none. Checks: local pin control and its refusals, remote read, remote write, a pin denied by `gpio-deny`, and that a wrong
or missing key gets no answer. Real pins can only be checked on the Pi."""

import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import GROUP, PORT, Guest  # noqa: E402
from test_boot import check  # noqa: E402


ROOT = Path(__file__).resolve().parent.parent


def run(guest, line, timeout=40):
    mark = guest.send(line)
    guest.wait_for("fos> ", timeout, mark)
    return guest.text()[mark:]


def main():
    with tempfile.TemporaryDirectory() as tmp:
        guests = {
            "alpha": Guest(tmp, "gpio-a", "52:54:00:12:36:a1", files={"hostname": "alpha", "synckey": "correct horse\n", "settings": "gpio-sim\ngpio-remote\ngpio-deny 5\n"}),
            "bravo": Guest(tmp, "gpio-b", "52:54:00:12:36:b2", files={"hostname": "bravo", "synckey": "correct horse\n"}),
            "charlie": Guest(tmp, "gpio-c", "52:54:00:12:36:c3", files={"hostname": "charlie", "synckey": "battery staple\n", "settings": "gpio-sim\ngpio-remote\n"}),
            "delta": Guest(tmp, "gpio-d", "52:54:00:12:36:d4", files={"hostname": "delta", "settings": "gpio-sim\ngpio-remote\n"}),
        }
        alpha, bravo, charlie, delta = guests.values()
        try:
            for guest in guests.values():
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")

            out = run(bravo, "gpio")
            check("no GPIO pins" in out, "a machine without pins says so", out[-300:])
            out = run(alpha, "gpio read 7")
            check("GPIO7" in out and "in" in out and "low" in out, "an input reads low without a pull", out[-300:])
            out = run(alpha, "gpio set 7 1")
            check("not allowed or not an output" in out, "set refuses a pin that is not an output", out[-300:])
            out = run(alpha, "gpio mode 7 out")
            check("Usage: gpio" in out, "an output needs a starting level", out[-300:])
            out = run(alpha, "gpio mode 7 out 1")
            out = run(alpha, "gpio read 7")
            check("out" in out and "high" in out, "mode out 1 drives the pin high", out[-300:])
            out = run(alpha, "gpio set 7 0")
            out = run(alpha, "gpio read 7")
            check("low" in out, "set drives it low", out[-300:])
            out = run(alpha, "gpio mode 14 out 1")
            check("gpio:" in out and "not allowed" in out.lower(), "the console UART pins are protected", out[-300:])
            out = run(alpha, "gpio mode 5 out 1")
            check("gpio:" in out, "a denied pin is refused", out[-300:])
            out = run(alpha, "gpio pull 9 up")
            out = run(alpha, "gpio read 9")
            check("pull up" in out and "high" in out, "a pull-up reads high", out[-300:])
            alpha.pump(2.5)

            out = run(bravo, "gpio @alpha1 read 9")
            check("GPIO9" in out and "pull up" in out, "bravo reads alpha's pin remotely", out[-400:])
            alpha.pump(2.5)
            out = run(bravo, "gpio @alpha1 mode 8 out 1")
            check("done" in out, "bravo changes a pin remotely", out[-400:])
            alpha.pump(2.5)
            out = run(alpha, "gpio read 8")
            check("out" in out and "high" in out, "and alpha shows it", out[-300:])
            out = run(bravo, "gpio @alpha1 mode 5 out 1")
            check("not allowed" in out, "a denied pin is refused remotely too", out[-300:])
            alpha.pump(2.5)

            out = run(bravo, "gpio @charlie1 read 9")
            check("not signed" in out or "no answer" in out, "a machine with another key gets no good answer", out[-400:])
            out = run(bravo, "gpio @delta1 read 9")
            check("no answer" in out, "a machine without a key does not answer", out[-400:])

            # The host-side client, tools/gpio.py, through the multicast seam.
            def host(key, *words):
                (Path(tmp) / "host-key").write_text(key + "\n")
                return subprocess.run([sys.executable, str(ROOT / "tools/gpio.py"), "--mcast", f"{GROUP}:{PORT}", "--mac", "52:54:00:12:36:a1",
                                       "--key", str(Path(tmp) / "host-key"), *words], capture_output=True, text=True, timeout=60)
            alpha.pump(2.5)
            result = host("correct horse", "read", "9")
            check(result.returncode == 0 and "GPIO9" in result.stdout and "high" in result.stdout, "gpio.py reads a pin", result.stdout[-300:] + result.stderr[-300:])
            alpha.pump(2.5)
            result = host("correct horse", "mode", "10", "out", "1")
            check(result.returncode == 0, "gpio.py changes a pin", result.stdout[-300:] + result.stderr[-300:])
            alpha.pump(2.5)
            out = run(alpha, "gpio read 10")
            check("out" in out and "high" in out, "and alpha shows it", out[-300:])
            alpha.pump(2.5)
            result = host("wrong key", "read", "9")
            check(result.returncode != 0, "gpio.py with another key gets no answer", result.stdout[-300:] + result.stderr[-300:])
        finally:
            for guest in guests.values():
                guest.stop()
    print("gpio: all checks passed")


if __name__ == "__main__":
    main()
