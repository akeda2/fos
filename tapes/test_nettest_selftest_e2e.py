#!/usr/bin/env python3
"""End-to-end self-test of nettest's traffic generator and sink.

One QEMU guest with four e1000 NICs. Two virtual cables (QEMU hubs) join NIC 1
to NIC 2 and NIC 3 to NIC 4. `nettest` runs the whole lab at once:

    generator (NIC 1) --cable--> NIC 2 [impairment link] NIC 3 --cable--> sink (NIC 4)

so the sink's counters are an independent measurement of what the impairment
link did: the throughput it allowed, the frames it dropped and the delay it
added. Generator and sink share the tape's clock, so the latency is exact.
Keys go in through the QEMU monitor (Escape ends the tape); the console is read
from the serial port.

Run with `make test-tape-nettest-selftest`."""

import os
import re
import selectors
import socket
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOS_TEST_IMAGE", ROOT / "build" / "fos.img"))
MACS = [f"52:54:00:ac:00:0{n}" for n in range(1, 5)]


class Lab:
    def __init__(self, nic_args=None, memory="256", binary="qemu-system-i386"):
        """`nic_args`: extra QEMU arguments for the NICs; by default four e1000
        joined in pairs by virtual cables (QEMU hubs)."""
        tmp = tempfile.mkdtemp(prefix="fos-selftest-")
        self.monitor = os.path.join(tmp, "mon")
        command = [binary, "-m", str(memory), "-drive", f"format=raw,file={IMAGE}", "-snapshot",
                   "-serial", "stdio", "-monitor", f"unix:{self.monitor},server,nowait", "-display", "none"]
        if nic_args is None:
            nic_args = []
            for number, hub in enumerate((1, 1, 2, 2), start=1):
                nic_args += ["-netdev", f"hubport,id=p{number},hubid={hub}",
                             "-device", f"e1000,netdev=p{number},mac={MACS[number - 1]}"]
        command += nic_args
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.STDOUT)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.output = bytearray()
        self.wait_for(b"Waiting for a remote terminal client", 0, 60)
        self.sendkey("f12")
        self.wait_for(b"fos> ")

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            for key, _ in self.selector.select(0.05):
                chunk = os.read(key.fd, 4096)
                if chunk:
                    self.output.extend(chunk)

    def wait_for(self, marker, start=0, timeout=40):
        end = time.monotonic() + timeout
        while marker not in self.output[start:]:
            if time.monotonic() > end:
                raise SystemExit(f"timed out waiting for {marker!r}:\n{self.text()[-3000:]}")
            self.pump(0.1)

    def text(self, start=0):
        return self.output[start:].decode("utf-8", "replace")

    def sendkey(self, name):
        for _ in range(100):
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                    sock.settimeout(1)
                    sock.connect(self.monitor)
                    sock.sendall(f"sendkey {name}\n".encode())
                return
            except OSError:
                time.sleep(0.05)

    def shell(self, line, marker=b"fos> ", timeout=40):
        mark = len(self.output)
        self.process.stdin.write(line.encode() + b"\n")
        self.process.stdin.flush()
        self.wait_for(marker, mark, timeout)
        return self.text(mark)

    def stop(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def run_scenario(lab, arguments, seconds, sink_nic=4):
    """Runs the tape with the given arguments until the generator has finished,
    stops it, and returns the parsed final counters."""
    lab.shell("tape load disk nettest.bin", b"Loaded ")
    mark = len(lab.output)
    lab.process.stdin.write(f"tape run nettest {arguments}\n".encode())
    lab.process.stdin.flush()
    lab.wait_for(b"nettest> ", mark, 30)
    lab.wait_for(b"generator finished", mark, seconds + 30)
    lab.wait_for(b"summary:", mark, 20)  # printed a second after the generator stops
    if "gen.save=" in arguments:
        lab.wait_for(b"bytes to ", mark, 20)
    lab.pump(0.5)
    lab.sendkey("f12")
    lab.wait_for(b"fos> ", mark, 30)
    text = lab.text(mark)
    stats = text[text.index("nettest stopped."):]
    sink = re.search(rf"sink NIC {sink_nic}\s+rx (\d+)\s+avg (\d+) kbit/s \(now \d+\)\s+lost (\d+) \(([\d.]+)%\)"
                     r"\s+dup (\d+)\s+reord (\d+)\s+latency us: min (\d+)\s+avg (\d+)\s+max (\d+)\s+jitter (\d+)",
                     stats)
    gen = re.search(r"gen\s+sent (\d+), failed (\d+)", stats)
    if not sink or not gen:
        raise SystemExit(f"could not parse the final counters:\n{stats}")
    names = ("rx", "avg_kbit", "lost", "loss_pct", "dup", "reord", "lat_min", "lat_avg", "lat_max", "jitter")
    result = dict(zip(names, (float(v) if "." in v else int(v) for v in sink.groups())))
    result["sent"], result["failed"] = int(gen.group(1)), int(gen.group(2))
    reverse = re.search(r"rev\s+sent (\d+), failed (\d+)\s+rx (\d+) avg (\d+) kbit/s lost (\d+) dup (\d+) reord (\d+)", stats)
    if reverse:
        result["rev"] = dict(zip(("sent", "failed", "rx", "avg_kbit", "lost", "dup", "reord"),
                                 (int(v) for v in reverse.groups())))
    result["output"] = text
    result["sweep"] = [
        tuple(float(v) for v in step)
        for step in re.findall(r"sweep (\d+) kbit: sent (\d+) rx (\d+) loss ([\d.]+)% got (\d+) kbit lat (\d+)/(\d+) us", text)
    ]
    return result


def check(condition, message, result):
    if not condition:
        raise SystemExit(f"FAIL: {message}\n{result}")
    print(f"PASS: {message}")


def main():
    base = "2 3 gen=1 sink=4 gen.size=1000 gen.time=4 gen.delay=3 gen.run=on "
    lab = Lab()
    try:
        # A clean link: everything arrives, at about the rate it was sent.
        result = run_scenario(lab, base + "gen.rate=8000 gen.save=report1", 4)
        check(result["lost"] == 0 and result["rx"] == result["sent"] - 0 or abs(result["rx"] - result["sent"]) <= 2,
              f"clean link: {result['rx']} of {result['sent']} frames arrived", result)
        check(7200 <= result["avg_kbit"] <= 8800, f"clean link: sink measured {result['avg_kbit']} kbit/s for 8000 sent", result)
        check(result["lat_avg"] < 20_000, f"clean link: latency {result['lat_avg']} us", result)
        report = lab.shell("cat report1")
        check("nettest report" in report and "summary: sent" in report and "latency histogram (us):" in report,
              "gen.save=report1 left a report file with settings, summary and histogram", report)
        check("summary: sent" in result["output"] and "latency us: median" in result["output"],
              "the end-of-test summary is printed by itself", result)
    finally:
        lab.stop()

    lab = Lab()
    try:
        # A rate limit below the offered load: the sink sees the limit, the rest is dropped.
        result = run_scenario(lab, base + "gen.rate=8000 rate=2000 queue=30", 4)
        check(1800 <= result["avg_kbit"] <= 2200, f"limiter at 2000 kbit/s: sink measured {result['avg_kbit']}", result)
        check(result["lost"] > result["sent"] // 2, f"the excess was dropped ({result['lost']} lost of {result['sent']})", result)
    finally:
        lab.stop()

    lab = Lab()
    try:
        # Random loss and a fixed delay, measured end to end.
        result = run_scenario(lab, base + "gen.rate=4000 loss=10 delay=20 seed=3", 4)
        share = 100.0 * result["lost"] / max(1, result["rx"] + result["lost"])
        check(5 <= share <= 15, f"loss 10%: sink saw {share:.1f}% lost", result)
        check(19_000 <= result["lat_min"] <= 24_000, f"delay 20 ms: minimum latency {result['lat_min']} us", result)
    finally:
        lab.stop()
    lab = Lab()
    try:
        # Routable mode: the same measurement with UDP/IP frames. The generator ARPs
        # for the sink's address (the sink answers), then sends datagrams.
        result = run_scenario(
            lab, base + "gen.rate=4000 gen.ip=10.0.1.2 gen.dst=10.0.2.2 sink.ip=10.0.2.2 rate=1000 queue=40", 4)
        check(950 <= result["avg_kbit"] <= 1050, f"UDP/IP frames: sink measured {result['avg_kbit']} kbit/s behind a 1000 kbit/s limiter", result)
        check(result["rx"] > 400 and result["dup"] == 0 and result["reord"] == 0,
              f"UDP/IP frames arrive intact and in order ({result['rx']} datagrams)", result)
    finally:
        lab.stop()
    lab = Lab()
    try:
        # A rate sweep against a 2000 kbit/s limiter: loss appears once the offered
        # rate passes the limit, and the delivered rate flattens at the limit.
        result = run_scenario(lab, "2 3 gen=1 sink=4 gen.size=1000 gen.delay=3 gen.sweep=1000:4000:1000:2 "
                              "rate=2000 queue=30", 8)
        steps = result["sweep"]
        check([int(step[0]) for step in steps] == [1000, 2000, 3000, 4000],
              f"the sweep reports one line per step ({[int(step[0]) for step in steps]})", result)
        check(steps[0][3] < 1.0 and 900 <= steps[0][4] <= 1100, f"1000 kbit/s offered passes untouched: {steps[0]}", result)
        check(steps[2][3] > 20 and steps[3][3] > steps[2][3] and 1800 <= steps[3][4] <= 2200,
              f"above the limit loss grows and throughput stays at 2000: {steps[2:]}", result)
    finally:
        lab.stop()
    lab = Lab()
    try:
        # Both directions at once with the same four NICs: the sink's NIC sends a
        # stream back. A 2000 kbit/s limiter on each way, 3000 offered each way.
        result = run_scenario(lab, base + "gen.rate=3000 rate=2000 queue=30 bidir=on", 4)
        reverse = result["rev"]
        check(1800 <= result["avg_kbit"] <= 2200 and 1800 <= reverse["avg_kbit"] <= 2200,
              f"bidirectional: forward {result['avg_kbit']} and reverse {reverse['avg_kbit']} kbit/s behind 2000 limiters",
              result)
        check(reverse["sent"] > 600 and reverse["lost"] > 0 and reverse["dup"] == 0,
              f"the reverse stream sent {reverse['sent']} frames and lost the excess ({reverse['lost']})", result)
    finally:
        lab.stop()
    lab = Lab()
    try:
        # The ready-made `selftest GEN LINK_IN LINK_OUT SINK`, with its 100 Mbit/s
        # default turned down for QEMU and a limiter added after it.
        result = run_scenario(lab, "selftest 1 2 3 4 gen.rate=4000 gen.time=3 rate=1000 queue=30", 3)
        check(900 <= result["avg_kbit"] <= 1100 and result["lost"] > 0,
              f"selftest keyword: {result['avg_kbit']} kbit/s behind a 1000 kbit/s limiter", result)
    finally:
        lab.stop()
    print("PASS: nettest's generator and sink measure the impairment link through a four-NIC loop.")


if __name__ == "__main__":
    main()
