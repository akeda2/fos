#!/usr/bin/env python3
"""Real-hardware checks of the NIC filters and offloads, run from a host on the same segment (needs AF_PACKET: use the
python3-netraw copy, see AGENTS.md).

  vlan      registers VLAN 5 on the target's interface, sends 200 tagged 5, 200 tagged 6 and 200 untagged frames to it and
            compares its received counter: only the registered tag and untagged frames may arrive.
  nettest   runs the nettest UDP generator on the target (gen.dst answered by this host's ARP) and verifies the UDP
            checksums the card filled in (the generator leaves them 0 when the card does it).

    ~/.local/bin/python3-netraw tools/test_nic_offload_hw.py vlan    --iface enp0s31f6 --mac 1 --nic 2
    ~/.local/bin/python3-netraw tools/test_nic_offload_hw.py nettest --iface enp0s31f6 --mac 1 --nic 2
"""
import argparse
import re
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

DEVLOOP = Path(__file__).with_name("devloop.py")


def run(args, *commands):
    out = subprocess.run([sys.executable, str(DEVLOOP), "--iface", args.iface, "--mac", args.mac, "run", *commands],
                         capture_output=True, text=True, timeout=120).stdout
    return out


def nic_block(args):
    """(mac, received) of interface --nic from the `nic` command."""
    out = run(args, "nic")
    found = re.search(rf"^{args.nic}: \S+ ((?:[0-9a-f]{{2}}:){{5}}[0-9a-f]{{2}}).*\n\s+frames: (\d+) received", out, re.M)
    if not found:
        sys.exit(f"FAIL: cannot read interface {args.nic}:\n{out}")
    return bytes.fromhex(found.group(1).replace(":", "")), int(found.group(2))


def fold(data, start=0):
    total = start
    if len(data) % 2:
        data += b"\0"
    for (word,) in struct.iter_unpack(">H", data):
        total += word
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return total


def vlan(args):
    mac, _ = nic_block(args)
    if "no VLAN filter" in run(args, f"nic vlan {args.nic} add 5"):
        print("PASS: this card has no VLAN filter (82579/I219) and says so")
        return True
    sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW)
    sock.bind((args.iface, 0))
    src = bytes.fromhex("020000aabbcc")
    results = {}
    try:
        for name, tag in (("tag 5", b"\x81\x00\x00\x05"), ("tag 6", b"\x81\x00\x00\x06"), ("untagged", b"")):
            _, before = nic_block(args)
            frame = mac + src + tag + b"\x88\xb5" + bytes(60)
            for _ in range(200):
                sock.send(frame)
                time.sleep(0.002)
            time.sleep(0.5)
            _, after = nic_block(args)
            results[name] = after - before
    finally:
        run(args, f"nic vlan {args.nic} clear")
    # A few frames of background traffic (announcements) are expected on top.
    ok = results["tag 5"] >= 195 and results["untagged"] >= 195 and results["tag 6"] < 40
    print(f"{'PASS' if ok else 'FAIL'}: VLAN filter received {results} of 200 each")
    return ok


def nettest(args):
    mac, _ = nic_block(args)
    sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(3))
    sock.bind((args.iface, 0))
    sock.settimeout(0.5)
    mine = sock.getsockname()[4]
    udp = []
    stop = threading.Event()

    def listen():
        while not stop.is_set():
            try:
                frame = sock.recv(2048)
            except socket.timeout:
                continue
            if frame[12:14] == b"\x08\x06" and frame[38:42] == bytes([10, 99, 0, 6]) and frame[20:22] == b"\x00\x01":
                reply = frame[6:12] + mine + b"\x08\x06" + b"\x00\x01\x08\x00\x06\x04\x00\x02" + mine + bytes([10, 99, 0, 6]) \
                    + frame[22:28] + frame[28:32] + bytes(18)
                sock.send(reply)
            elif frame[12:14] == b"\x08\x00" and frame[23] == 17 and frame[30:34] == bytes([10, 99, 0, 6]):
                udp.append(frame)

    thread = threading.Thread(target=listen)
    thread.start()
    # The generator waits at the nettest prompt for `gen start`; its own timer (gen.time) ends the run, `exit` leaves.
    run(args, f"run nettest gen={args.nic} gen.ip=10.99.0.5 gen.dst=10.99.0.6 gen.size=1000 gen.rate=20000 gen.time=4",
        "gen start")
    time.sleep(1)
    stop.set()
    thread.join()
    bad = zero = 0
    for frame in udp:
        total = struct.unpack(">H", frame[16:18])[0]
        part = frame[34:14 + total]
        if part[6:8] == b"\0\0":
            zero += 1
        elif fold(part, fold(frame[26:34] + bytes([0, 17]) + struct.pack(">H", len(part)))) != 0xFFFF:
            bad += 1
    ok = len(udp) >= 50 and bad == 0 and zero == 0
    print(f"{'PASS' if ok else 'FAIL'}: nettest sent {len(udp)} UDP frames; {bad} with a wrong checksum, {zero} with none")
    return ok


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("test", choices=["vlan", "nettest"])
    parser.add_argument("--iface", required=True)
    parser.add_argument("--mac", required=True)
    parser.add_argument("--nic", required=True)
    args = parser.parse_args()
    sys.exit(0 if {"vlan": vlan, "nettest": nettest}[args.test](args) else 1)


if __name__ == "__main__":
    main()
