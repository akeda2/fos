#!/usr/bin/env python3
"""Real-hardware check of the I350's transmit checksum offload (`nic csum N`, ABI 14).

Needs AF_PACKET (run it with the python3-netraw copy, see AGENTS.md). Asks the fos host MAC/N to broadcast three UDP
frames whose checksum field is 0 and verifies the checksum the card put in each one. Pass --nic for the interface
number on that host (it must be an I350 on the host's segment).

    ~/.local/bin/python3-netraw tools/test_tx_offload.py --iface enp0s31f6 --mac 2 --nic 1
"""
import argparse
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path


def fold(data, start=0):
    total = start
    if len(data) % 2:
        data += b"\0"
    for (word,) in struct.iter_unpack(">H", data):
        total += word
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return total


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--iface", required=True)
    parser.add_argument("--mac", required=True, help="peer number or MAC for devloop.py")
    parser.add_argument("--nic", default="1")
    args = parser.parse_args()
    sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0800))
    sock.bind((args.iface, 0))
    sock.settimeout(0.5)
    found = []

    def listen():
        end = time.time() + 25
        while time.time() < end and len(found) < 3:
            try:
                frame = sock.recv(2048)
            except socket.timeout:
                continue
            if frame[26:30] == bytes([10, 99, 0, 1]) and frame[23] == 17:
                found.append(frame)

    thread = threading.Thread(target=listen)
    thread.start()
    devloop = Path(__file__).with_name("devloop.py")
    subprocess.run([sys.executable, str(devloop), "--iface", args.iface, "--mac", args.mac, "run", f"nic csum {args.nic}"],
                   check=False, capture_output=True, timeout=60)
    thread.join()
    failures = 0
    for frame in found:
        total = struct.unpack(">H", frame[16:18])[0]
        udp = frame[34:14 + total]
        pseudo = fold(frame[26:34] + bytes([0, 17]) + struct.pack(">H", len(udp)))
        ok = fold(udp, pseudo) == 0xFFFF and udp[6:8] != b"\0\0"
        failures += not ok
        print(f"{'PASS' if ok else 'FAIL'}: UDP frame of {len(udp) - 8} payload bytes, checksum {udp[6]:02x}{udp[7]:02x}, "
              f"{len(frame)} bytes on the wire")
    if len(found) != 3:
        print(f"FAIL: saw {len(found)} of 3 frames")
        failures += 1
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
