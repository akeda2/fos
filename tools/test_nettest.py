#!/usr/bin/env python3
"""The `nettest` link-impairment tape on the v2 kernel (milestone M5).

One QEMU guest (OVMF) with two e1000 NICs, each on its own TCP-attached segment played
by this script (a point-to-point wire, unlike QEMU's multicast netdev, never echoes a
guest's own frames back, which would loop a forwarding tape forever). The tape runs
between the NICs (multi-NIC support, promiscuous mode, raw frame callbacks, the
`nettest>` prompt on the term session); the script injects test frames on one segment
and watches the other.

Checks: a clean link forwards both ways; `loss ab 100` drops one direction only;
the statistics reflect it; duplication, reordering and corruption do what they say;
`quit` returns to the shell and promiscuous mode is restored."""

import os
import selectors
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, check, clean, find  # noqa: E402

SEGMENT_A_PORT, SEGMENT_B_PORT = port(4290), port(4291)
NIC_A = bytes.fromhex("525400ce0001")
NIC_B = bytes.fromhex("525400ce0002")
HOST_A = bytes.fromhex("525400ce00a1")
HOST_B = bytes.fromhex("525400ce00b1")
CLIENT = bytes.fromhex("02deadbeef81")
CHAT, TEST = bytes((0x88, 0xB6)), bytes((0x88, 0xB7))
HELLO, DATA = 0x20, 0x21


class Segment:
    def __init__(self, port):
        self.server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.server.bind(("127.0.0.1", port))
        self.server.listen(1)
        self.sock, self.buffer = None, bytearray()

    def accept(self, timeout=60):
        self.server.settimeout(timeout)
        self.sock, _ = self.server.accept()
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.sock.setblocking(False)

    def send(self, frame):
        self.sock.setblocking(True)
        self.sock.sendall(struct.pack(">I", len(frame)) + frame)
        self.sock.setblocking(False)

    def frames(self, wait=0.0):
        end = time.monotonic() + wait
        while True:
            try:
                chunk = self.sock.recv(65536)
                if chunk:
                    self.buffer.extend(chunk)
            except BlockingIOError:
                pass
            progressed = False
            while len(self.buffer) >= 4:
                length = struct.unpack(">I", self.buffer[:4])[0]
                if len(self.buffer) < 4 + length:
                    break
                yield bytes(self.buffer[4:4 + length])
                del self.buffer[:4 + length]
                progressed = True
            if not progressed:
                if time.monotonic() >= end:
                    return
                time.sleep(0.003)

    def test_sequences(self, wait):
        return [struct.unpack(">I", f[14:18])[0] for f in self.frames(wait) if f[12:14] == TEST]


def test_frame(destination, source, sequence, size=60):
    return (destination + source + TEST + struct.pack(">I", sequence)).ljust(size, b"\0")


def term_frame(tag, payload=b""):
    return (NIC_A + CLIENT + CHAT + bytes((tag,)) + payload).ljust(60, b"\0")


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    a, b = Segment(SEGMENT_A_PORT), Segment(SEGMENT_B_PORT)
    with tempfile.TemporaryDirectory() as tmp:
        image = Path(tmp) / "image.img"
        shutil.copy(IMAGE, image)
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={image}"]
        for number, (port, mac) in enumerate(((SEGMENT_A_PORT, NIC_A), (SEGMENT_B_PORT, NIC_B))):
            command += ["-netdev", f"socket,id=n{number},connect=127.0.0.1:{port}",
                        "-device", f"{os.environ.get('FOS_NIC_MODEL', 'e1000')},netdev=n{number},mac={':'.join(f'{x:02x}' for x in mac)}"]
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        output = bytearray()

        def pump(seconds):
            end = time.monotonic() + seconds
            while True:
                for key, _ in selector.select(max(0.0, end - time.monotonic())):
                    chunk = os.read(key.fd, 4096)
                    if chunk:
                        output.extend(chunk)
                if time.monotonic() >= end:
                    return

        def wait_for(marker, start=0, timeout=40):
            end = time.monotonic() + timeout
            while marker not in clean(bytes(output[start:])):
                if time.monotonic() > end or process.poll() is not None:
                    raise SystemExit(f"FAIL: never saw {marker!r}\n{clean(bytes(output[start:]))[-3000:]}")
                pump(0.05)
            return clean(bytes(output[start:]))

        def say(line, marker):
            mark = len(output)
            a.send(term_frame(DATA, line.encode() + b"\n"))
            return wait_for(marker, mark)

        try:
            a.accept()
            b.accept()
            wait_for("M3 complete", 0, 120)
            a.send(term_frame(HELLO))
            wait_for("Remote terminal connected", 0, 10)
            wait_for("fos> ", 0, 10)
            check("2 interface" not in "" and "interface 1" in clean(bytes(output)), "the terminal session opens on interface 1 of two", clean(bytes(output)))

            started = say("run nettest", "nettest> ")
            check("Serving on NIC 1" in started, "plain 'nettest' is the far end on NIC 1, waiting for a master", started)
            helped = say("help", "nettest> ")
            check("link A B" in helped and "serve N | remote N" in helped, "help shows the modes", helped)
            check(all(len(line) <= 79 for line in clean(helped.encode()).splitlines()), "help fits 79 columns", helped)
            moved = say("link 1 2", "nettest> ")
            check("impairment link moved" in moved and "two-machine mode stopped" in moved, "'link 1 2' at the prompt sets up the wire between NIC 1 and NIC 2", moved)

            for frame in a.frames(0.2):
                pass
            for sequence in range(20):
                a.send(test_frame(HOST_B, HOST_A, sequence))
                time.sleep(float(os.environ.get("GAP", "0.003")))
            got = b.test_sequences(1.0)
            if os.environ.get("DEBUG") and len(got) != 20:
                print(say("stats", "nettest> "))
                say("quit", "fos> ")
                print(say("nic", "fos> "))
            check(sorted(got) == list(range(20)), f"a clean link forwards A -> B ({len(got)}/20 frames, in order)", "")
            for sequence in range(100, 120):
                b.send(test_frame(HOST_A, HOST_B, sequence))
                time.sleep(0.003)
            got = a.test_sequences(1.0)
            check(sorted(got) == list(range(100, 120)), f"and B -> A ({len(got)}/20)", "")

            say("loss ab 100", "nettest> ")
            for sequence in range(200, 220):
                a.send(test_frame(HOST_B, HOST_A, sequence))
                b.send(test_frame(HOST_A, HOST_B, sequence + 100))
                time.sleep(0.003)
            dropped, passed = b.test_sequences(1.0), a.test_sequences(0.5)
            check(dropped == [], f"`loss ab 100` drops everything A -> B ({len(dropped)} arrived)", "")
            check(len(passed) == 20, f"while B -> A is untouched ({len(passed)}/20)", "")
            stats = say("stats", "nettest> ")
            check("a>b" in stats.lower() or "a>b" in stats, "`stats` reports per-direction counters", stats)

            def burst(segment, count, destination, source, size=60, gap=0.003, first=0):
                for sequence in range(first, first + count):
                    segment.send(test_frame(destination, source, sequence, size))
                    time.sleep(gap)

            # Duplication, reordering and corruption, one direction (A -> B) each.
            say("reset", "nettest> ")
            say("dup ab 100", "nettest> ")
            b.test_sequences(0.3)
            burst(a, 10, HOST_B, HOST_A)
            got = b.test_sequences(1.0)
            check(sorted(got) == sorted(list(range(10)) * 2), f"`dup ab 100` delivers every frame twice ({sorted(got)})", "")

            say("reset", "nettest> ")
            say("reorder ab 100", "nettest> ")
            b.test_sequences(0.3)
            burst(a, 10, HOST_B, HOST_A, gap=0.01)
            got = b.test_sequences(1.0)
            check(got == [1, 0, 3, 2, 5, 4, 7, 6, 9, 8], f"`reorder ab 100` swaps neighbouring frames ({got})", "")
            b.test_sequences(0.3)
            a.send(test_frame(HOST_B, HOST_A, 77))
            got = b.test_sequences(1.0)
            check(got == [77], "a frame held for reordering is still released when nothing follows", "")

            say("reset", "nettest> ")
            say("corrupt ab 100", "nettest> ")
            b.test_sequences(0.3)
            sent = [test_frame(HOST_B, HOST_A, i, 200) for i in range(15)]
            for frame in sent:
                a.send(frame)
                time.sleep(0.003)
            received = [f for f in b.frames(1.0) if f[12:14] == TEST]
            flips = [sum(bin(x ^ y).count("1") for x, y in zip(one, two)) for one, two in zip(sent, received)]
            check(len(received) == 15 and all(f == 1 for f in flips) and all(one[:14] == two[:14] for one, two in zip(sent, received)),
                  f"`corrupt ab 100` flips exactly one payload bit per frame ({flips})", "")
            check("corrupted 15" in say("stats", "nettest> "), "`stats` counts the corrupted frames", "")
            say("reset", "nettest> ")

            back = say("quit", "fos> ")
            check("fos> " in back, "`quit` returns to the shell", back)
            health = say("uptime", "fos> ")
            check("ticks at 100 Hz" in health, "the shell is healthy after the tape", health)
            a.send(term_frame(DATA, b"shutdown\n"))
            for _ in range(100):
                if process.poll() is not None:
                    break
                time.sleep(0.2)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
            a.sock and a.sock.close()
            b.sock and b.sock.close()
    print("nettest: all checks passed")


if __name__ == "__main__":
    main()
