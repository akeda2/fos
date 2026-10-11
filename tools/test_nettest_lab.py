#!/usr/bin/env python3
"""nettest's generator and sink measure the impairment link through a four-NIC loop.

One guest with four e1000 NICs; two virtual cables (QEMU hubs) join NIC 1 to NIC 2 and NIC 3 to NIC 4. `nettest` runs the whole lab at
once: generator (NIC 1) -> cable -> NIC 2 [impairment link] NIC 3 -> cable -> sink (NIC 4), so the sink's counters are an independent
measurement of what the link did: the throughput it allowed, the frames it dropped and the delay it added (generator and sink share
the tape's clock). The guest is driven over the serial port, which a tape reads as its keyboard (a lone Escape ends it).

Scenarios: a clean link (with a `gen.save` report file); a rate sweep; both directions at once (`bidir=on`); a rate limit below the offered load; random loss plus a fixed delay; the ready-made `selftest` keyword; and
re-pointing the generator, sink and link from the prompt (`gen nic`, `sink nic`, `link`). Ported from the legacy
test_nettest_selftest_e2e and test_nettest_retarget_e2e. Run with `make test-nettest-lab`."""
import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, check, clean, find  # noqa: E402

MACS = [f"52:54:00:ac:00:0{n}" for n in range(1, 5)]


class Lab:
    def __init__(self, tmp, nics=None):
        """`nics`: extra QEMU arguments for the NICs; by default four e1000 joined in pairs by virtual cables (QEMU hubs)."""
        code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
        image = Path(tmp) / "image.img"
        shutil.copy(IMAGE, image)
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={image}"]
        if nics is None:
            nics = []
            for number, hub in enumerate((1, 1, 2, 2), start=1):
                nics += ["-netdev", f"hubport,id=p{number},hubid={hub}", "-device", f"e1000,netdev=p{number},mac={MACS[number - 1]}"]
        command += nics
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.raw = bytearray()
        self.wait_for("fos> ", 0, 120)

    def text(self, start=0):
        return clean(bytes(self.raw))[start:]

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([self.process.stdout], [], [], 0.05)
            if ready:
                self.raw.extend(os.read(self.process.stdout.fileno(), 4096))

    def wait_for(self, marker, start, timeout):
        end = time.monotonic() + timeout
        while marker not in self.text(start):
            if time.monotonic() > end or self.process.poll() is not None:
                raise SystemExit(f"FAIL: never saw {marker!r}\n{self.text(start)[-3000:]}")
            self.pump(0.1)

    def type(self, line):
        self.process.stdin.write(line.encode() + b"\n")
        self.process.stdin.flush()

    def escape(self):
        self.process.stdin.write(b"\x1b")
        self.process.stdin.flush()

    def stop(self):
        self.process.kill()
        self.process.wait()


def shell(lab, line):
    """Runs one shell command at the `fos> ` prompt and returns its output."""
    start = len(lab.text())
    lab.type(line)
    lab.wait_for("fos> ", start, 20)
    return lab.text(start)


def run_scenario(lab, arguments, seconds, sink_nic=4):
    """Runs the tape until the generator has finished, stops it with Escape and returns the final counters."""
    start = len(lab.text())
    lab.type(f"run nettest {arguments}")
    lab.wait_for("nettest> ", start, 30)
    lab.wait_for("generator finished", start, seconds + 40)
    lab.wait_for("summary:", start, 20)
    if "gen.save=" in arguments:
        lab.wait_for("bytes to ", start, 20)
    lab.pump(0.5)
    lab.escape()
    lab.wait_for("fos> ", start + lab.text(start).index("nettest> "), 30)
    text = lab.text(start)
    stats = text[text.index("nettest stopped."):]
    sink = re.search(rf"sink NIC {sink_nic}\s+rx (\d+)\s+avg (\d+) kbit/s \(now \d+\)\s+lost (\d+) \(([\d.]+)%\)"
                     r"\s+dup (\d+)\s+reord (\d+)\s+latency us: min (\d+)\s+avg (\d+)\s+max (\d+)\s+jitter (\d+)", stats)
    gen = re.search(r"gen\s+sent (\d+), failed (\d+)", stats)
    if not sink or not gen:
        raise SystemExit(f"FAIL: could not parse the final counters:\n{stats}")
    names = ("rx", "avg_kbit", "lost", "loss_pct", "dup", "reord", "lat_min", "lat_avg", "lat_max", "jitter")
    result = dict(zip(names, (float(v) if "." in v else int(v) for v in sink.groups())))
    result["sent"], result["failed"] = int(gen.group(1)), int(gen.group(2))
    reverse = re.search(r"rev\s+sent (\d+), failed (\d+)\s+rx (\d+) avg (\d+) kbit/s lost (\d+) dup (\d+) reord (\d+)", stats)
    if reverse:
        result["rev"] = dict(zip(("sent", "failed", "rx", "avg_kbit", "lost", "dup", "reord"), (int(v) for v in reverse.groups())))
    result["output"] = text
    result["sweep"] = [tuple(float(v) for v in step) for step in re.findall(
        r"sweep (\d+) kbit: sent (\d+) rx (\d+) loss ([\d.]+)% got (\d+) kbit lat (\d+)/(\d+) us", text)]
    return result


def socket_nics(prefix, mode, ports):
    """Two NICs on QEMU socket netdevs: one guest listens, the other connects (a cable between two guests)."""
    args = []
    for number, port in enumerate(ports, start=1):
        endpoint = f"listen=127.0.0.1:{port}" if mode == "listen" else f"connect=127.0.0.1:{port}"
        args += ["-netdev", f"socket,id=w{number},{endpoint}", "-device", f"e1000,netdev=w{number},mac=52:54:00:{prefix}:00:0{number}"]
    return args


def main():
    base = "2 3 gen=1 sink=4 gen.size=1000 gen.time=4 gen.delay=3 gen.run=on "
    with tempfile.TemporaryDirectory() as tmp:
        lab = Lab(tmp)
        try:
            result = run_scenario(lab, base + "gen.rate=8000", 4)
            check(abs(result["rx"] - result["sent"]) <= 2 and result["lost"] == 0, f"clean link: {result['rx']} of {result['sent']} frames arrived", result["output"])
            check(6500 <= result["avg_kbit"] <= 9500, f"clean link: the sink measured {result['avg_kbit']} kbit/s for 8000 sent", result["output"])
            check("summary: sent" in result["output"], "the end-of-test summary is printed by itself", result["output"])

            result = run_scenario(lab, base + "gen.rate=8000 rate=2000 queue=30", 4)
            check(1700 <= result["avg_kbit"] <= 2300, f"a 2000 kbit/s limiter: the sink measured {result['avg_kbit']}", result["output"])
            check(result["lost"] > result["sent"] // 2, f"and the excess was dropped ({result['lost']} lost of {result['sent']})", result["output"])

            result = run_scenario(lab, base + "gen.rate=4000 loss=10 delay=20 seed=3", 4)
            share = 100.0 * result["lost"] / max(1, result["rx"] + result["lost"])
            check(4 <= share <= 16, f"loss 10%: the sink saw {share:.1f}% lost", result["output"])
            check(19_000 <= result["lat_min"] <= 26_000, f"delay 20 ms: minimum latency {result['lat_min']} us", result["output"])

            # A report file written when the test ends.
            result = run_scenario(lab, base + "gen.rate=4000 gen.time=3 gen.save=report1", 3)
            report = shell(lab, "cat report1")
            check("nettest report" in report and "summary: sent" in report and "latency histogram (us):" in report,
                  "gen.save=report1 left a report file with settings, summary and histogram", report)

            # A rate sweep against a 2000 kbit/s limiter: loss appears once the offered rate passes the limit and the
            # delivered rate flattens at the limit.
            result = run_scenario(lab, "2 3 gen=1 sink=4 gen.size=1000 gen.delay=3 gen.sweep=1000:4000:1000:2 rate=2000 queue=30", 8)
            steps = result["sweep"]
            check([int(step[0]) for step in steps] == [1000, 2000, 3000, 4000],
                  f"the sweep reports one line per step ({[int(step[0]) for step in steps]})", result["output"])
            check(steps[0][3] < 1.0 and 900 <= steps[0][4] <= 1100, f"1000 kbit/s offered passes untouched: {steps[0]}", result["output"])
            check(steps[2][3] > 20 and steps[3][3] > steps[2][3] and 1800 <= steps[3][4] <= 2200,
                  f"above the limit loss grows and throughput stays at 2000: {steps[2:]}", result["output"])

            # Both directions at once: a 2000 kbit/s limiter each way, 3000 offered each way.
            result = run_scenario(lab, base + "gen.rate=3000 rate=2000 queue=30 bidir=on", 4)
            reverse = result.get("rev")
            check(reverse is not None and 1800 <= result["avg_kbit"] <= 2200 and 1800 <= reverse["avg_kbit"] <= 2200,
                  f"bidirectional: forward {result['avg_kbit']} and reverse {reverse and reverse['avg_kbit']} kbit/s behind 2000 limiters",
                  result["output"])
            check(reverse["sent"] > 600 and reverse["lost"] > 0 and reverse["dup"] == 0,
                  f"the reverse stream sent {reverse['sent']} frames and lost the excess ({reverse['lost']})", result["output"])

            result = run_scenario(lab, "selftest 1 2 3 4 gen.rate=4000 gen.time=3 rate=1000 queue=30", 3)
            check(850 <= result["avg_kbit"] <= 1150 and result["lost"] > 0, f"the `selftest` keyword: {result['avg_kbit']} kbit/s behind a 1000 kbit/s limiter", result["output"])

            # Re-pointing roles from the prompt: refusals first, then the stream reversed.
            start = len(lab.text())
            lab.type("run nettest 2 3 gen=1 sink=4 gen.size=500 gen.rate=2000 gen.count=200")
            lab.wait_for("nettest> ", start, 30)

            def command(line, wait=0.5):
                mark = len(lab.text())
                lab.type(line)
                lab.wait_for("nettest> ", mark, 20)
                lab.pump(wait)
                return lab.text(mark)

            check("another role" in command("gen nic 2"), "a NIC in use by the link is refused", "")
            check("use: nic N" in command("gen nic 9"), "a NIC that does not exist is refused", "")
            check("another role" in command("gen nic 4"), "a NIC held by the sink is refused", "")
            check("switched off" in command("sink nic off"), "`sink nic off` switches the sink off", "")
            check("4 is now the NIC" in command("gen nic 4"), "the generator moves to NIC 4", "")
            check("1 is now the NIC" in command("sink nic 1"), "the sink moves to NIC 1", "")
            text = command("gen start", 3) + command("sink show")
            rx = re.search(r"rx (\d+)", text)
            check(rx is not None and int(rx.group(1)) >= 150, f"the reversed stream reached NIC 1 ({rx and rx.group(1)} frames)", text)
            check("another role" in command("link 1 4"), "`link` refuses NICs that have a role", "")
            check("link off" in command("link off"), "`link off`", "")
            check("moved" in command("link 2 3"), "and `link 2 3` puts it back", "")
            mark = len(lab.text())
            lab.escape()
            lab.wait_for("fos> ", mark, 30)
        finally:
            lab.stop()

        # nettest's routable UDP traffic through the `router` tape, on two guests joined by two cables:
        #   lab guest: generator (NIC 1) -> router guest NIC 1 [router] NIC 2 -> sink (NIC 2), 10.0.1.2 -> 10.0.2.2
        # The generator ARPs for the router, the router ARPs for the sink, forwards every datagram, and the sink counts them.
        router_tmp, lab_tmp = Path(tmp) / "router", Path(tmp) / "lab"
        router_tmp.mkdir()
        lab_tmp.mkdir()
        router = Lab(router_tmp, socket_nics("ae", "listen", (port(4392), port(4393))))
        lab = None
        try:
            start = len(router.text())
            router.type("run router no-nat 1 10.0.1.1/24 2 10.0.2.1/24")
            router.wait_for("Forwarding IPv4 frames", start, 40)
            lab = Lab(lab_tmp, socket_nics("ad", "connect", (port(4392), port(4393))))
            result = run_scenario(lab, "routertest 1 2 gen.rate=2000 gen.size=500 gen.time=4", 4, sink_nic=2)
            check(result["sent"] > 800, f"through the router: the generator sent {result['sent']} datagrams", result["output"])
            check(result["lost"] <= 2 and result["rx"] >= result["sent"] - 3,
                  f"{result['rx']} of {result['sent']} arrived, {result['lost']} lost (the first waits for the router's ARP)", result["output"])
            check(1800 <= result["avg_kbit"] <= 2200, f"the sink measured {result['avg_kbit']} kbit/s for 2000 offered", result["output"])
            reverse = result.get("rev")
            check(reverse is not None and reverse["sent"] > 800 and reverse["rx"] >= reverse["sent"] - 3 and reverse["lost"] <= 2,
                  f"the reverse stream crosses the router too ({reverse and reverse['rx']} of {reverse and reverse['sent']})", result["output"])
        finally:
            if lab:
                lab.stop()
            router.stop()
    print("nettest lab: all checks passed")


if __name__ == "__main__":
    main()
