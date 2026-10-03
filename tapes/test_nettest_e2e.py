#!/usr/bin/env python3
"""End-to-end test for the `nettest` link impairment tape.

One QEMU guest runs `nettest` between two e1000 NICs, each on its own
TCP-attached "segment". This script is the host on both segments: it injects raw
test frames on one segment and listens on the other, with the guest's console
(its serial output) used to see the tape's own messages. It checks, over the
real forwarding path, that

  * a clean link forwards frames in both directions,
  * `loss` drops about the configured share (and only in the chosen direction),
  * `delay` holds frames back,
  * `rate` spaces frames out and
  * duplication, reordering and corruption do what they say,
  * `reset`/`quit` work and return control to the shell.

The queue-size line is also checked with the default 128 MiB guest (static
fallback queue) and 256 MiB (big-pool queue).

Outside `make test` (wall-clock timing); run it with
`make test-tape-nettest`.
"""

import os
import selectors
import socket
import struct
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
SEGMENT_A_PORT = 4280
SEGMENT_B_PORT = 4281
NIC_A = bytes.fromhex("525400ce0001")
NIC_B = bytes.fromhex("525400ce0002")
HOST_A = bytes.fromhex("525400ce00a1")  # pretend host on segment A
HOST_B = bytes.fromhex("525400ce00b1")  # pretend host on segment B
CLIENT = bytes.fromhex("02deadbeef81")
CHAT_ETHERTYPE = bytes((0x88, 0xB6))
TEST_ETHERTYPE = bytes((0x88, 0xB7))
TERM_HELLO, TERM_DATA = 0x20, 0x21


class Segment:
    """One Ethernet segment, played by this script: a TCP server that a QEMU
    `-netdev socket,connect=` NIC attaches to (4-byte big-endian length, then
    the frame). A point-to-point wire, unlike QEMU's multicast netdev, never
    echoes a guest's own transmissions back to it -- an echo would bounce every
    forwarded frame between the tape's two NICs forever."""

    def __init__(self, port):
        self.port = port
        self.server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.server.bind(("127.0.0.1", port))
        self.server.listen(1)
        self.sock = None
        self.buffer = bytearray()

    def accept(self, timeout=20):
        self.server.settimeout(timeout)
        self.sock, _ = self.server.accept()
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.sock.setblocking(False)

    def send(self, frame):
        self.sock.setblocking(True)
        self.sock.sendall(struct.pack(">I", len(frame)) + frame)
        self.sock.setblocking(False)

    def frames(self, wait=0.0):
        """Yields (time, frame) for every complete frame that arrives."""
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
                yield time.monotonic(), bytes(self.buffer[4:4 + length])
                del self.buffer[:4 + length]
                progressed = True
            if not progressed:
                if time.monotonic() >= end:
                    return
                time.sleep(0.003)

    def drain(self, wait=0.0):
        """Returns [(time, test sequence number)] for test frames received."""
        return [
            (when, struct.unpack(">I", data[14:18])[0])
            for when, data in self.frames(wait)
            if data[12:14] == TEST_ETHERTYPE
        ]

    def close(self):
        for sock in (self.sock, self.server):
            if sock:
                sock.close()


def test_frame(destination, source, sequence, size=60):
    frame = bytearray(destination + source + TEST_ETHERTYPE + struct.pack(">I", sequence))
    frame.extend(b"\0" * max(0, size - len(frame)))
    return bytes(frame)


def term_frame(tag, payload=b""):
    frame = bytearray(NIC_A + CLIENT + CHAT_ETHERTYPE + bytes((tag,)) + payload)
    frame.extend(b"\0" * max(0, 60 - len(frame)))
    return bytes(frame)


class Guest:
    def __init__(self, memory, model="e1000"):
        command = ["qemu-system-i386", "-m", str(memory), "-drive", f"format=raw,file={IMAGE}", "-snapshot",
                   "-serial", "stdio", "-monitor", "none", "-no-reboot", "-no-shutdown", "-display", "none"]
        for number, (port, mac) in enumerate(((SEGMENT_A_PORT, NIC_A), (SEGMENT_B_PORT, NIC_B))):
            command += ["-netdev", f"socket,id=n{number},connect=127.0.0.1:{port}",
                        "-device", f"{model},netdev=n{number},mac={':'.join(f'{b:02x}' for b in mac)}"]
        self.process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.output = bytearray()

    def pump(self, seconds=0.0):
        end = time.monotonic() + seconds
        while True:
            remaining = max(0.0, end - time.monotonic())
            for key, _ in self.selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if chunk:
                    self.output.extend(chunk)
            if time.monotonic() >= end:
                return

    def wait_for(self, marker, start=0, timeout=20):
        end = time.monotonic() + timeout
        while marker not in self.output[start:]:
            if time.monotonic() > end:
                raise SystemExit(f"timed out waiting for {marker!r}; output:\n{self.text(start)}")
            self.pump(0.05)
        return self.text(start)

    def text(self, start=0):
        return self.output[start:].decode("utf-8", "replace")

    def stop(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def check(condition, message, guest=None):
    if not condition:
        raise SystemExit(f"FAIL: {message}" + (f"\n{guest.text()}" if guest else ""))
    print(f"PASS: {message}")


def start_tape(guest, segment_a, arguments):
    guest.wait_for(b"Waiting for a remote terminal client", 0, 40)
    segment_a.send(term_frame(TERM_HELLO))
    guest.wait_for(b"Remote terminal connected", 0, 10)
    guest.wait_for(b"fos> ", 0, 10)
    mark = len(guest.output)
    segment_a.send(term_frame(TERM_DATA, b"tape load disk nettest.bin\n"))
    guest.wait_for(b"Loaded ", mark, 15)
    guest.wait_for(b"fos> ", mark, 10)
    mark = len(guest.output)
    segment_a.send(term_frame(TERM_DATA, b"tape run nettest " + arguments.encode() + b"\n"))
    return guest.wait_for(b"nettest> ", mark, 15)


def command(guest, segment_a, line, marker=b"nettest> "):
    mark = len(guest.output)
    segment_a.send(term_frame(TERM_DATA, line.encode() + b"\n"))
    return guest.wait_for(marker, mark, 10)


def burst(sender, count, destination, source, size=60, gap=0.002, first=0):
    for index in range(count):
        sender.send(test_frame(destination, source, first + index, size))
        time.sleep(gap)


def main():
    if not (ROOT / "build" / "tapes" / "nettest.bin").exists():
        raise SystemExit("build/tapes/nettest.bin missing; run `make tape-nettest` and `make image`.")
    def launch(memory, model="e1000"):
        a, b = Segment(SEGMENT_A_PORT), Segment(SEGMENT_B_PORT)
        guest = Guest(memory, model)
        a.accept()
        b.accept()
        return guest, a, b

    # A guest too small for `alloc` to find even frame-pool room (the kernel keeps
    # a 4 MiB frame reserve): the static fallback queue is used. (A 128 MiB guest
    # has no big pool but `alloc` now falls back to the frame pool, so it gets
    # the full-size queue: checked below.)
    guest, seg_a, seg_b = launch(12)
    try:
        # The shipped example file (ports, a preset, a per-direction setting, a seed).
        banner = start_tape(guest, seg_a, "@nettest.cfg")
        check("256 frames of queue per way" in banner, "a 12 MiB guest falls back to the static queue", guest)
        check("NIC 1 <-> NIC 2" in banner and "loss 10.0% x6" in banner and "b>a  loss 2.0%" in banner,
              "the shipped nettest.cfg configures ports, a preset and a per-direction loss", guest)
        command(guest, seg_a, "quit", b"fos> ")
    finally:
        guest.stop()
        seg_a.close()
        seg_b.close()

    guest, seg_a, seg_b = launch(128)
    try:
        banner = start_tape(guest, seg_a, "1 2 seed=5")
        check("frames of queue per way" in banner and "256 frames" not in banner,
              "a 128 MiB guest (no big pool) takes the queue from the frame pool", guest)
        command(guest, seg_a, "quit", b"fos> ")
    finally:
        guest.stop()
        seg_a.close()
        seg_b.close()

    guest, seg_a, seg_b = launch(256)
    try:
        banner = start_tape(guest, seg_a, "1 2 seed=5")
        check("frames of queue per way" in banner and "256 frames" not in banner,
              "256 MiB guest uses the big-pool queue", guest)

        # Clean link, both directions.
        # QEMU's e1000 model delivers nothing for about a second after the guest
        # turns on promiscuous mode (the e1000e model does not; the bridge tape
        # shows the same), so give it time before timing anything.
        seg_a.drain(1.5), seg_b.drain(1.5)
        burst(seg_a, 5, HOST_B, HOST_A)
        got = seg_b.drain(0.5)
        check(sorted(s for _, s in got) == [0, 1, 2, 3, 4], f"clean link carries A to B {got}", guest)
        burst(seg_b, 5, HOST_A, HOST_B)
        got = seg_a.drain(0.5)
        check(len(got) == 5, "clean link carries B to A", guest)

        # Loss in one direction only.
        shown = command(guest, seg_a, "loss ab 50")
        check("a>b" in shown and "loss 50.0%" in shown, "loss command reports the new settings", guest)
        seg_b.drain()
        burst(seg_a, 200, HOST_B, HOST_A)
        lost_run = len(seg_b.drain(0.7))
        check(60 <= lost_run <= 140, f"50% loss A to B delivered {lost_run} of 200 frames", guest)
        burst(seg_b, 40, HOST_A, HOST_B)
        check(len(seg_a.drain(0.5)) == 40, "loss ab leaves B to A untouched", guest)

        # Same seed, same drops: the test is repeatable.
        runs = []
        for _ in range(2):
            command(guest, seg_a, "seed 42")
            seg_b.drain()
            burst(seg_a, 60, HOST_B, HOST_A, first=1000)
            runs.append([s for _, s in seg_b.drain(0.5)])
        check(runs[0] == runs[1] and 0 < len(runs[0]) < 60, "same seed gives the same loss pattern", guest)

        # Burst loss: runs of consecutive drops.
        command(guest, seg_a, "loss ab 20 burst 5")
        command(guest, seg_a, "seed 9")
        seg_b.drain()
        burst(seg_a, 200, HOST_B, HOST_A)
        survivors = [s for _, s in seg_b.drain(0.7)]
        gaps = [b - a - 1 for a, b in zip(survivors, survivors[1:]) if b - a > 1]
        check(gaps and max(gaps) >= 5, f"burst loss drops runs of frames (longest gap {max(gaps or [0])})", guest)

        # Delay.
        command(guest, seg_a, "reset")
        command(guest, seg_a, "delay ab 150")
        seg_b.drain()
        sent = time.monotonic()
        seg_a.send(test_frame(HOST_B, HOST_A, 7))
        got = seg_b.drain(1.0)
        check(len(got) == 1, "delayed frame still arrives", guest)
        latency = got[0][0] - sent
        check(0.12 <= latency <= 0.45, f"delay 150 ms held the frame {latency * 1000:.0f} ms", guest)

        # Rate limit: 20 frames of 1000 bytes at 400 kbit/s take about 0.4 s.
        command(guest, seg_a, "reset")
        command(guest, seg_a, "rate ab 400")
        seg_b.drain()
        sent = time.monotonic()
        burst(seg_a, 20, HOST_B, HOST_A, size=1000, gap=0.001)
        got = seg_b.drain(1.5)
        check(len(got) == 20, "rate limit queues rather than drops (20 of 20 arrive)", guest)
        spread = got[-1][0] - sent
        check(spread >= 0.3, f"rate 400 kbit/s spread 20 frames over {spread * 1000:.0f} ms", guest)

        # Queue limit: a tiny queue behind a slow link tail-drops.
        command(guest, seg_a, "queue ab 3")
        seg_b.drain()
        burst(seg_a, 20, HOST_B, HOST_A, size=1000, gap=0.001)
        got = seg_b.drain(1.5)
        check(0 < len(got) < 20, f"queue 3 tail-dropped ({len(got)} of 20 arrived)", guest)

        # Duplication, reordering and corruption.
        command(guest, seg_a, "reset")
        command(guest, seg_a, "dup ab 100")
        seg_b.drain()
        burst(seg_a, 10, HOST_B, HOST_A)
        got = [seq for _, seq in seg_b.drain(0.5)]
        check(sorted(got) == sorted(list(range(10)) * 2), "dup 100% delivers every frame twice", guest)

        command(guest, seg_a, "reset")
        command(guest, seg_a, "reorder ab 100")
        seg_b.drain()
        burst(seg_a, 10, HOST_B, HOST_A, gap=0.01)
        got = [seq for _, seq in seg_b.drain(0.5)]
        check(got == [1, 0, 3, 2, 5, 4, 7, 6, 9, 8], f"reorder 100% swaps neighbouring frames {got}", guest)
        seg_b.drain()
        seg_a.send(test_frame(HOST_B, HOST_A, 77))
        got = [seq for _, seq in seg_b.drain(0.5)]
        check(got == [77], "a frame held for reordering is still released if nothing follows", guest)

        command(guest, seg_a, "reset")
        command(guest, seg_a, "corrupt ab 100")
        seg_b.drain()
        sent = [test_frame(HOST_B, HOST_A, i, 200) for i in range(15)]
        for frame in sent:
            seg_a.send(frame)
            time.sleep(0.003)
        received = [data for _, data in seg_b.frames(0.5) if data[12:14] == TEST_ETHERTYPE]
        flips = [sum(bin(x ^ y).count("1") for x, y in zip(a, b)) for a, b in zip(sent, received)]
        check(len(received) == 15 and all(f == 1 for f in flips)
              and all(a[:14] == b[:14] for a, b in zip(sent, received)),
              f"corrupt 100% flips exactly one payload bit per frame (flips {flips})", guest)
        counters = command(guest, seg_a, "stats")
        check("corrupted 15" in counters, "stats counts the corrupted frames", guest)

        # Presets and the optional statistics panel.
        listing = command(guest, seg_a, "presets")
        check("badwifi" in listing and "satellite" in listing, "presets lists the named presets", guest)
        shown = command(guest, seg_a, "preset ab badwifi")
        check("loss 10.0% x6" in shown and "delay 40+-60ms" in shown and "rate 3000kbit" in shown,
              "preset badwifi sets bursty loss, jitter and a slow rate", guest)
        check("b>a  clean" in shown, "a preset on one direction leaves the other clean", guest)
        bad = command(guest, seg_a, "preset ab nonsense")
        check("bad value" in bad, "an unknown preset is refused", guest)
        command(guest, seg_a, "reset")
        command(guest, seg_a, "reset stats")
        mark = len(guest.output)
        command(guest, seg_a, "view on")
        panel = guest.text(mark)
        check("nettest  NIC 1 <-> NIC 2" in panel and "\x1b[13;25r" in panel,
              "view on draws the panel and confines scrolling below it", guest)
        seg_b.drain()
        burst(seg_a, 10, HOST_B, HOST_A)
        seg_b.drain(0.2)
        guest.pump(1.2)
        check("in 10 (" in guest.text(mark), "the panel shows live counters", guest)
        mark = len(guest.output)
        command(guest, seg_a, "view off")
        check("\x1b[r" in guest.text(mark), "view off restores full-screen scrolling", guest)

        # Counters, reset, quit.
        stats = command(guest, seg_a, "stats")
        check("lost" in stats and "qdrop" in stats, "stats prints loss and queue-drop counters", guest)
        command(guest, seg_a, "reset")
        seg_b.drain()
        burst(seg_a, 5, HOST_B, HOST_A)
        check(len(seg_b.drain(0.5)) == 5, "reset restores a clean link", guest)
        quit_text = command(guest, seg_a, "quit", b"fos> ")
        check("nettest stopped." in quit_text and "Tape returned" in quit_text, "quit returns to the shell", guest)
    finally:
        guest.stop()
        seg_a.close()
        seg_b.close()

    # A burst of 1000 back-to-back frames into each emulated NIC family. This
    # used to overrun the 8-descriptor rings and reset the adapter (printing a
    # "NIC watchdog" line each time). What each model can hold differs:
    #  * e1000e applies back-pressure, so with a 128-descriptor ring everything
    #    arrives (QEMU loses only the very first frame after promiscuous mode
    #    is enabled, with or without this change);
    #  * e1000 drops what does not fit the ring instantly, so at least a ring's
    #    worth must arrive, with the overruns counted and no adapter reset;
    #  * rtl8139 and ne2k_pci have small fixed on-chip buffers and still reset
    #    on overflow; they must only come back and keep forwarding.
    for model, at_least, strict in (("e1000e", 995, True), ("e1000", 128, True),
                                    ("rtl8139", 0, False), ("ne2k_pci", 0, False)):
        guest, seg_a, seg_b = launch(256, model)
        try:
            start_tape(guest, seg_a, "1 2")
            seg_a.drain(3), seg_b.drain(0.1)
            for sequence in range(1000):
                seg_a.send(test_frame(HOST_B, HOST_A, sequence, 1000))
            delivered = len(seg_b.drain(3))
            check(delivered >= at_least, f"{model}: 1000 back-to-back frames, {delivered} delivered "
                  f"(at least {at_least} expected)", guest)
            if strict:
                guest.pump(0.5)
                check("NIC watchdog" not in guest.text(), f"{model}: no adapter reset under a full-speed burst", guest)
            # Whatever happened, the link must carry traffic again afterwards.
            # Whatever happened, the link must carry traffic again afterwards (the
            # small-buffer cards reset themselves, which can take a few seconds).
            resumed = False
            for attempt in range(5):
                time.sleep(1.5)
                seg_b.drain()
                burst(seg_a, 5, HOST_B, HOST_A, first=5000 + attempt * 10)
                if len(seg_b.drain(1.0)) >= 4:
                    resumed = True
                    break
            check(resumed, f"{model}: still forwarding after the burst", guest)
            if model == "e1000":
                command(guest, seg_a, "quit", b"fos> ")
                health = command(guest, seg_a, "nic list", b"fos> ")
                check("receive overrun event(s)" in health and " 0 recovery/recoveries" in health,
                      "e1000: overruns are counted and did not reset the adapter", guest)
        finally:
            guest.stop()
            seg_a.close()
            seg_b.close()
    print("PASS: nettest loss, burst loss, delay, rate and queue limit work over a live two-NIC link.")


if __name__ == "__main__":
    main()
