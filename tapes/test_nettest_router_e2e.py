#!/usr/bin/env python3
"""End-to-end test: nettest's routable UDP traffic through the `router` tape.

Two QEMU guests joined by two virtual cables (QEMU socket netdevs):

    lab guest:    generator (NIC 1) -----> router guest NIC 1 [router] NIC 2 -----> sink (NIC 2)
                  10.0.1.2 gw 10.0.1.1       10.0.1.1          10.0.2.1             10.0.2.2

The generator ARPs for the router, the router ARPs for the sink (which answers),
forwards every datagram with its TTL decremented, and the sink checks them off.
This exercises the generator, the sink and the router's ARP-miss handling
together. Run with `make test-tape-nettest-router`."""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_nettest_selftest_e2e import Lab, check, run_scenario  # noqa: E402

PORT_A, PORT_B = 4291, 4292


def nics(prefix, mode):
    args = []
    for number, port in ((1, PORT_A), (2, PORT_B)):
        endpoint = f"listen=127.0.0.1:{port}" if mode == "listen" else f"connect=127.0.0.1:{port}"
        args += ["-netdev", f"socket,id=w{number},{endpoint}",
                 "-device", f"e1000,netdev=w{number},mac=52:54:00:{prefix}:00:0{number}"]
    return args


def main():
    router = Lab(nics("ae", "listen"))
    lab = None
    try:
        router.shell("tape load disk router.bin", b"Loaded ")
        mark = len(router.output)
        router.process.stdin.write(b"tape run router no-nat 1 10.0.1.1/24 2 10.0.2.1/24\n")
        router.process.stdin.flush()
        router.wait_for(b"Forwarding IPv4 frames", mark, 30)
        lab = Lab(nics("ad", "connect"))
        # The router has no static route knowledge beyond its two directly connected
        # subnets, which is all this needs.
        result = run_scenario(
            lab,
            "gen=1 sink=2 gen.ip=10.0.1.2 gen.dst=10.0.2.2 gen.gw=10.0.1.1 sink.ip=10.0.2.2 "
            "gen.rate=2000 gen.size=500 gen.time=4 gen.delay=3 gen.run=on",
            4,
            sink_nic=2,
        )
        check(result["sent"] > 800, f"the generator sent {result['sent']} datagrams", result)
        check(result["lost"] <= 2 and result["rx"] >= result["sent"] - 3,
              f"through the router: {result['rx']} of {result['sent']} arrived, {result['lost']} lost "
              "(the first one waits for the router's ARP)", result)
        check(1800 <= result["avg_kbit"] <= 2200, f"the sink measured {result['avg_kbit']} kbit/s for 2000 offered", result)
        print("PASS: nettest UDP/IP traffic crosses the router tape, ARP and all.")
    finally:
        if lab:
            lab.stop()
        router.stop()


if __name__ == "__main__":
    main()
