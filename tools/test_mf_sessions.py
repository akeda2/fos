#!/usr/bin/env python3
"""Several `term net` sessions on the server kernel at once (kernel-mf/src/session.rs):

 1. Two clients (different MACs) each get a prompt; a command's output goes to its own client only.
 2. The working folder and script variables are per session.
 3. `top` running in one session does not stop the other from using its shell, and its screen output stays in its session.
 4. A client that says BYE frees its slot and leaves the others alone; a fifth client is refused while four are connected.
 5. `term-net N` in `settings` limits the interfaces: a machine that only allows NIC 2 gives a HELLO on NIC 1 no session.
QEMU only: real NICs and a person at two terminals are for hardware."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_debug_mode as base  # noqa: E402
import test_boot  # noqa: E402
from test_boot import BROADCAST, ETHERTYPE, ROOT, TAG_BYE, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST, TAG_HELLO, Wire, check  # noqa: E402

base.KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf"
READY = "entering the service loop"


class Client:
    """One terminal: its own MAC and its own socket on the guest's network."""

    def __init__(self, number, guest):
        self.mac = bytes.fromhex(f"02deadbeef{number:02x}")
        self.guest = guest
        self.wire = Wire()
        self.seen = bytearray()

    def send(self, tag, payload=b""):
        frame = self.guest + self.mac + test_boot.ETHERTYPE + bytes([tag]) + payload
        self.wire.sock.sendto(frame.ljust(60, b"\0"), (test_boot.MCAST_GROUP, test_boot.MCAST_PORT))

    def read(self, seconds):
        """Collects DATA addressed to this client for `seconds`."""
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.wire.sock.settimeout(max(0.01, end - time.monotonic()))
            try:
                packet = self.wire.sock.recv(2048)
            except OSError:
                break
            if len(packet) >= 15 and packet[:6] == self.mac and packet[6:12] == self.guest and packet[12:14] == ETHERTYPE and packet[14] == TAG_DATA:
                self.seen.extend(packet[15:])
        return self.seen.decode("utf-8", "replace")

    def until(self, marker, timeout=15):
        end = time.monotonic() + timeout
        while marker.encode() not in self.seen[self.mark:]:
            if time.monotonic() > end:
                return None
            self.read(0.2)
        return self.seen[self.mark:].decode("utf-8", "replace")

    mark = 0

    def hello(self):
        self.mark = len(self.seen)
        self.send(TAG_HELLO)
        return self.until("fos> ", 10)

    def run(self, line, timeout=20):
        self.mark = len(self.seen)
        self.send(TAG_DATA, line.encode() + b"\n")
        return self.until("> ", timeout)


def discover(machine):
    for _ in range(60):
        machine.wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
        for source, tag, _ in machine.wire.frames(0.25):
            if tag == TAG_DISCOVER_REPLY:
                return source
        machine.pump(0.05)
    raise SystemExit("FAIL: no discovery reply\n" + machine.text()[-2000:])


def main():
    with tempfile.TemporaryDirectory() as tmp:
        machine = base.Machine(tmp, {"settings": "term-net 1\n"})
        machine.start()
        try:
            machine.wait_for(READY)
            check("term-net -- the network terminal is open on those interfaces only" in machine.text(), "settings: term-net 1 is read", machine.text()[-1500:])
            guest = discover(machine)
            a, b = Client(0xa1, guest), Client(0xb2, guest)
            check(a.hello(), "client A gets a prompt", a.seen.decode(errors="replace"))
            check(b.hello(), "client B gets a prompt while A is connected", b.seen.decode(errors="replace"))

            out = a.run("echo only-for-a")
            check(out and "only-for-a" in out, "A's command runs", out or "")
            b.read(1)
            check(b"only-for-a" not in b.seen, "and its output does not reach B", b.seen.decode(errors="replace"))
            out = b.run("echo only-for-b")
            check(out and "only-for-b" in out, "B's command runs", out or "")
            a.read(1)
            check(b"only-for-b" not in a.seen, "and its output does not reach A", a.seen.decode(errors="replace"))

            a.run("x=from-a")
            out = b.run("echo [$x]")
            check(out and "[]" in out and "from-a" not in out, "script variables are per session", out or "")
            out = a.run("echo [$x]")
            check(out and "[from-a]" in out, "and A still has its own", out or "")

            out = a.run("cd esp")
            out = a.run("pwd")
            check(out and "esp" in out, "A changes folder", out or "")
            out = b.run("pwd")
            check(out and "esp" not in out, "B's working folder is unaffected", out or "")
            check("fos esp> " in a.run("echo again"), "and A's prompt shows its folder", "")

            a.mark = len(a.seen)
            a.send(TAG_DATA, b"top 1\n")
            check(a.until("collecting", 10), "A starts top", a.seen.decode(errors="replace"))
            out = b.run("echo b-works-during-a-top")
            check(out and "b-works-during-a-top" in out and "\x1b[2J" not in out, "B's shell works while A runs top, and gets none of its screen", out or "")
            a.mark = len(a.seen)
            check(a.until("CPU", 10), "A's top draws its frames", a.seen[a.mark:].decode(errors="replace"))
            a.mark = len(a.seen)
            a.send(TAG_DATA, b"q")
            check(a.until("> ", 10), "q ends A's top and returns the prompt", a.seen.decode(errors="replace"))

            b.send(TAG_BYE)
            time.sleep(0.5)
            check(a.run("echo a-after-b-left") and b"a-after-b-left" in a.seen, "A keeps working after B says BYE", "")
            clients = [a]
            for number in (0xc3, 0xd4):
                client = Client(number, guest)
                check(client.hello(), f"client {number:#x} gets a prompt", client.seen.decode(errors="replace"))
                clients.append(client)
            check(b.hello(), "B reconnects into the freed slot (four in all)", b.seen.decode(errors="replace"))
            fifth = Client(0xe5, guest)
            check(not fifth.hello(), "a fifth client gets no session", fifth.seen.decode(errors="replace"))
            fifth.read(1)
            check(b"Remote terminal connected" not in fifth.seen, "and is told nothing", fifth.seen.decode(errors="replace"))
        finally:
            machine.stop()

    with tempfile.TemporaryDirectory() as tmp:
        machine = base.Machine(tmp, {"settings": "term-net 2\n"})
        machine.start()
        try:
            machine.wait_for(READY)
            guest = discover(machine)
            client = Client(0xa1, guest)
            check(not client.hello(), "term-net 2: a HELLO on NIC 1 gets no session", client.seen.decode(errors="replace"))
            check("Remote terminal connected" not in machine.text(), "and the console says nothing", machine.text()[-800:])
        finally:
            machine.stop()
    print("test-mf-sessions: all checks passed")


if __name__ == "__main__":
    main()
