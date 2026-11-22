#!/usr/bin/env python3
"""AoE target throughput on the server kernel, measured from the host (docs/block-sharing.md). Not a test: it prints numbers.

The host is a pipelined initiator (a window of outstanding READs or WRITEs of 2 sectors, the most a 1500-byte frame carries)
over the QEMU multicast-socket network, against a guest that serves one disk. Scenarios, each a few seconds:
  idle        nothing else runs in the guest
  printing    the guest shell prints all the time (`help` in a loop), and touches no disk
  disk        the guest shell reads the boot disk all the time (`sector disk1 0 4` in a loop)
Reads, then writes (an `rw` export). QEMU on the host's CPU, Python as initiator: read the numbers as relative, not absolute.

    python3 tools/bench_mf_aoe.py [--seconds 4] [--window 16] [--kvm]
Needs the same environment as `make test-mf-aoe` (LOADER_EFI, KERNEL_MF_ELF, BIOS_STAGE1, BIOS_STAGE2)."""
import argparse
import os
import select
import socket
import struct
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_mf_aoe as base  # noqa: E402
from test_mf_aoe import GROUP, GUEST_MAC, HOST_MAC, PORT  # noqa: E402

SECTORS = 65536   # 32 MiB


def run_window(seconds, window, write, shelf=5, slot=2):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("", PORT))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton(GROUP) + socket.inet_aton("0.0.0.0"))
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
    payload = bytes(range(256)) * 4
    next_lba = 0
    tag = 1 << 20
    outstanding = {}
    done = timeouts = errors = 0

    def request():
        nonlocal next_lba, tag
        tag += 1
        lba = next_lba
        next_lba = (next_lba + 2) % (SECTORS - 2)
        aflags = 0x40 | (1 if write else 0)
        body = struct.pack("<BBBB", aflags, 0, 2, 0x34 if write else 0x24) + lba.to_bytes(6, "little") + b"\0\0" + (payload if write else b"")
        frame = GUEST_MAC + HOST_MAC + b"\x88\xa2" + struct.pack(">BBHBBI", 0x10, 0, shelf, slot, 0, tag) + body
        outstanding[tag] = (time.time(), frame)
        sock.sendto(frame, (GROUP, PORT))

    start = time.time()
    for _ in range(window):
        request()
    while time.time() - start < seconds:
        if select.select([sock], [], [], 0.05)[0]:
            while select.select([sock], [], [], 0)[0]:
                reply = sock.recv(65536)
                if len(reply) < 36 or reply[12:14] != b"\x88\xa2" or reply[6:12] != GUEST_MAC or not reply[14] & 0x08:
                    continue
                if outstanding.pop(struct.unpack(">I", reply[20:24])[0], None) is None:
                    continue
                if reply[27] != 0x50:
                    errors += 1
                done += 1
                request()
        now = time.time()
        for key, (sent, frame) in list(outstanding.items()):
            if now - sent > 0.5:   # a lost frame: send it again, as an initiator would
                timeouts += 1
                outstanding[key] = (now, frame)
                sock.sendto(frame, (GROUP, PORT))
    elapsed = time.time() - start
    sock.close()
    return done, elapsed, timeouts, errors


def feed(guest, line, stop):
    """Types `line` into the guest over and over and drains its output, until `stop`."""
    while not stop.is_set():
        try:
            guest.qemu.stdin.write(line.encode() + b"\n")
            guest.qemu.stdin.flush()
        except OSError:
            return
        end = time.time() + 0.15
        while time.time() < end:
            if select.select([guest.qemu.stdout], [], [], 0.05)[0]:
                if not os.read(guest.qemu.stdout.fileno(), 65536):
                    return


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--seconds", type=float, default=4)
    parser.add_argument("--window", type=int, default=16)
    parser.add_argument("--kvm", action="store_true", help="run QEMU with -enable-kvm")
    parser.add_argument("--stats", action="store_true", help="print the target's counters (`aoe`) after each window run")
    parser.add_argument("--smp", type=int, default=2, help="CPUs of the guest (the test default is 2)")
    parser.add_argument("--windows", default="", help="comma list of windows to run the idle read/write scenario with (instead of the default matrix)")
    args = parser.parse_args()
    orig = base.subprocess.Popen

    def popen(command, *a, **k):
        command = list(command)
        if "-smp" not in command:
            return orig(command, *a, **k)
        command[command.index("-smp") + 1] = str(args.smp)
        if args.kvm:
            command = command[:1] + ["-enable-kvm"] + command[1:]
        return orig(command, *a, **k)
    base.subprocess.Popen = popen
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        disk = tmp / "data.img"
        disk.write_bytes(os.urandom(SECTORS * 512))
        boot = base.build_image(tmp, "aoe-shelf 5\naoe-export 2\n")
        guest = base.Guest(boot, [disk])
        try:
            if not guest.wait_for("fos> ", 150):
                raise SystemExit("the guest never reached the prompt")
            if args.windows:
                print(f"idle, 2 sectors (1 KiB) per frame, MTU 1500, {args.seconds:g} s per run")
                print(f"{'window':>7} {'direction':9} {'frames/s':>9} {'MB/s':>7} {'resent':>7} {'errors':>7}")
                for direction in ("read", "write"):
                    if direction == "write":
                        guest.command("aoe unexport 2", "no longer exported")
                        guest.command("aoe export 2 rw confirm", "READ-WRITE")
                    for window in (int(word) for word in args.windows.split(",")):
                        guest.command("aoe reset", "cleared")
                        done, elapsed, timeouts, errors = run_window(args.seconds, window, direction == "write")
                        print(f"{window:7d} {direction:9} {done / elapsed:9.0f} {done * 1024 / elapsed / 1e6:7.2f} {timeouts:7d} {errors:7d}")
                        if args.stats:
                            _, text = guest.command("aoe", "net-rx:")
                            for line in text.splitlines():
                                if any(key in line for key in ("worker:", "queued to", "per frame", "of which", "disk commands", "FAILED", "drained by", "net-rx:")):
                                    print("        " + line.strip())
                return
            print(f"window {args.window}, {args.seconds:g} s per run, 2 sectors (1 KiB) per frame, MTU 1500")
            print(f"{'':10} {'direction':9} {'frames/s':>9} {'MB/s':>7} {'resent':>7} {'errors':>7}")
            for direction in ("read", "write"):
                if direction == "write":
                    guest.command("aoe unexport 2", "no longer exported")
                    guest.command("aoe export 2 rw confirm", "READ-WRITE")
                for name, line in (("idle", None), ("printing", "help"), ("disk", "sector disk1 0 4")):
                    stop = threading.Event()
                    thread = None
                    if line:
                        thread = threading.Thread(target=feed, args=(guest, line, stop))
                        thread.start()
                        time.sleep(0.5)
                    done, elapsed, timeouts, errors = run_window(args.seconds, args.window, direction == "write")
                    stop.set()
                    if thread:
                        thread.join()
                        guest.pump = None
                        time.sleep(1.0)
                        while select.select([guest.qemu.stdout], [], [], 0.2)[0]:
                            os.read(guest.qemu.stdout.fileno(), 65536)
                    print(f"{name:10} {direction:9} {done / elapsed:9.0f} {done * 1024 / elapsed / 1e6:7.2f} {timeouts:7d} {errors:7d}")
        finally:
            guest.kill()


if __name__ == "__main__":
    main()
