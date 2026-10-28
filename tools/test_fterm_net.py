#!/usr/bin/env python3
"""`fterm` over the network: Telnet and `term net`.

The shell is driven over COM1 (stdio). The guest has two NICs: NIC 1 is the multicast segment where the test plays another fos
machine (`Wire`, MAC 02:de:ad:be:ef:03); NIC 2 is QEMU's user-mode network, where a host Telnet server (the one from
test_telnet.py) answers at 10.0.2.2.

Telnet: static and DHCP addressing, the greeting and UTF-8 text on screen, a typed line reaching the server and its answer coming
back, the terminal-type/NAWS replies, Ctrl-A s, and the tape returning to the shell when the server hangs up. `term net`: HELLO on
start, the peer's DATA on screen, typed keys as DATA, the peer's BYE ending the session, Ctrl-A x sending BYE, and a peer named by
its announced name, a host name resolved through a DNS server on the host, and the panel's machine list (P). Run with `make test-fterm-net`."""
import os
import selectors
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_telnet  # noqa: E402
import testslot  # noqa: E402
from test_boot import (BROADCAST, CODE_CANDIDATES, HOST_MAC, IMAGE, MCAST_GROUP, MCAST_PORT, TAG_BYE, TAG_DATA,  # noqa: E402
                       TAG_DISCOVER_REPLY, TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find)

IAC, WILL, TERMINAL_TYPE, NAWS = 255, 251, 24, 31
SERVER_PORT = testslot.port(23124)
DNS_PORT = testslot.port(15353)
HOST_MAC_TEXT = ":".join(f"{byte:02x}" for byte in HOST_MAC)


class DnsServer(threading.Thread):
    """A tiny DNS server on the host (the guest reaches it as 10.0.2.2): lab.test is 10.0.2.2, everything else NXDOMAIN."""

    def __init__(self):
        super().__init__(daemon=True)
        self.socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.socket.bind(("127.0.0.1", DNS_PORT))
        self.queries = []

    def run(self):
        while True:
            data, peer = self.socket.recvfrom(512)
            end = data.index(b"\0", 12)
            name = data[13:end]
            labels, at = [], 12
            while data[at]:
                labels.append(data[at + 1:at + 1 + data[at]].decode())
                at += 1 + data[at]
            name = ".".join(labels).lower()
            self.queries.append(name)
            question = data[12:end + 5]
            if name == "lab.test":
                answer = b"\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\x0a\x00\x02\x02"
                reply = data[:2] + b"\x81\x80\x00\x01\x00\x01\x00\x00\x00\x00" + question + answer
            else:
                reply = data[:2] + b"\x81\x83\x00\x01\x00\x00\x00\x00\x00\x00" + question
            self.socket.sendto(reply, peer)


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    test_telnet.SERVER_PORT = SERVER_PORT
    server = test_telnet.Server()
    server.start()
    dns_server = DnsServer()
    dns_server.start()
    wire = Wire()
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image = tmp / "image.img"
        shutil.copy(IMAGE, image)
        vars_copy = tmp / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-no-reboot", "-serial", "stdio",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={image}",
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:7a",
                   "-netdev", "user,id=net1", "-device", "e1000,netdev=net1,mac=52:54:00:12:34:7b"]
        qemu = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        selector = selectors.DefaultSelector()
        selector.register(qemu.stdout, selectors.EVENT_READ)
        output = bytearray()
        frames = []  # (source, tag, payload) the guest sent to the wire

        def pump(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                for key, _ in selector.select(0.02):
                    chunk = os.read(key.fd, 4096)
                    if chunk:
                        output.extend(chunk)
                frames.extend(wire.frames(0.01))

        def text():
            return clean(bytes(output))

        def wait_for(marker, timeout=60, start=0):
            end = time.monotonic() + timeout
            while marker not in text()[start:]:
                if time.monotonic() > end or qemu.poll() is not None:
                    raise SystemExit(f"FAIL: timed out waiting for {marker!r}:\n{text()[-2500:]}")
                pump(0.1)

        def type_(data):
            qemu.stdin.write(data)
            qemu.stdin.flush()
            pump(0.3)

        def typed_line(data):
            for byte in data:
                type_(bytes([byte]))

        def wait_frame(tag, marker=b"", timeout=15):
            end = time.monotonic() + timeout
            while True:
                for frame in frames:
                    if frame[1] == tag and marker in frame[2]:
                        return frame
                if time.monotonic() > end:
                    return None
                pump(0.1)

        try:
            wait_for("fos> ", 90)
            mark = len(text())
            type_(b"nic use 2\r")
            wait_for("fos> ", 10, mark)

            mark = len(text())
            type_(f"run fterm telnet 10.0.2.2 {SERVER_PORT} static 10.0.2.15/24 10.0.2.2\r".encode())
            wait_for("login:", 40, mark)
            screen = text()[mark:]
            check(server.connected.wait(5), "telnet: connects with a static address (ARP for the gateway, TCP handshake)", screen)
            check("Welcome to the test server" in screen and "gröövy" in screen, "telnet: the greeting and UTF-8 text are shown", screen)
            pump(0.5)
            replies = bytes(server.replies)
            check(bytes([IAC, WILL, TERMINAL_TYPE]) in replies and bytes([IAC, WILL, NAWS]) in replies,
                  "telnet: it agrees to terminal type and window size", repr(replies))

            mark = len(text())
            typed_line(b"hello\r")
            wait_for("you typed: hello", 15, mark)
            check(True, "telnet: a typed line reaches the server and its answer is shown", text()[mark:])

            mark = len(text())
            type_(b"\x01s")
            wait_for("echo off", 10, mark)
            check("telnet 10.0.2.2" in text()[mark:] and "connected" in text()[mark:], "telnet: Ctrl-A s shows the connection", text()[mark:])

            mark = len(text())
            typed_line(b"bye\r")
            wait_for("fterm: closed.", 30, mark)
            wait_for("fos> ", 10, mark)
            check(True, "telnet: when the server hangs up the tape returns to the shell", text()[mark:])

            # A host name: asked of the DNS server given with `dns`, then the Telnet connection to its answer.
            server.connected.clear()
            mark = len(text())
            type_(f"run fterm telnet Lab.Test {SERVER_PORT} static 10.0.2.15/24 10.0.2.2 dns 10.0.2.2:{DNS_PORT}\r".encode())
            wait_for("login:", 40, mark)
            screen = text()[mark:]
            check("lab.test" in dns_server.queries, "dns: the name is asked of the DNS server", str(dns_server.queries))
            check("Lab.Test is 10.0.2.2" in screen and server.connected.wait(5), "dns: the answer is used for the Telnet connection", screen)
            type_(b"\x01x")
            wait_for("fos> ", 20, mark)

            mark = len(text())
            type_(f"run fterm telnet nothere.test {SERVER_PORT} static 10.0.2.15/24 10.0.2.2 dns 10.0.2.2:{DNS_PORT}\r".encode())
            wait_for("fos> ", 30, mark)
            check("no such name" in text()[mark:], "dns: an unknown name is reported", text()[mark:])

            mark = len(text())
            type_(f"run fterm telnet lab.test {SERVER_PORT} static 10.0.2.15/24 10.0.2.2 dns 10.0.2.2:{DNS_PORT + 1}\r".encode())
            wait_for("fos> ", 30, mark)
            check("did not answer" in text()[mark:], "dns: a silent server ends with a message, not a hang", text()[mark:])

            server.connected.clear()
            mark = len(text())
            type_(f"run fterm telnet 10.0.2.2 {SERVER_PORT}\r".encode())
            wait_for("login:", 60, mark)
            check("leased 10.0.2.15" in text()[mark:] and server.connected.wait(5), "telnet: DHCP leases 10.0.2.15 and connects", text()[mark:])
            mark = len(text())
            type_(b"\x01x")
            wait_for("fos> ", 20, mark)
            check("fterm: closed." in text()[mark:], "telnet: Ctrl-A x leaves", text()[mark:])

            mark = len(text())
            type_(f"run fterm telnet 10.0.2.2 {SERVER_PORT + 1}\r".encode())
            wait_for("fos> ", 60, mark)
            check("refused" in text()[mark:] or "timed out" in text()[mark:], "telnet: a closed port ends with a message, not a hang", text()[mark:])

            mark = len(text())
            type_(b"nic use 1\r")
            wait_for("fos> ", 10, mark)

            frames.clear()
            mark = len(text())
            type_(f"run fterm net {HOST_MAC_TEXT}\r".encode())
            hello = wait_frame(TAG_HELLO)
            check(hello is not None, "term net: HELLO goes to the peer", str(frames))
            guest = hello[0]
            wire.send(guest, TAG_DATA, b"remote shell ready\r\nremote> ")
            wait_for("remote> ", 10, mark)
            check("remote shell ready" in text()[mark:], "term net: the peer's DATA is shown", text()[mark:])

            frames.clear()
            type_(b"ls\r")
            check(wait_frame(TAG_DATA, b"ls\r") is not None, "term net: typed keys go out as DATA (Enter as CR)", str(frames))
            mark = len(text())
            wire.send(guest, TAG_DATA, b"\r\nfile1 file2\r\n")
            wait_for("file1 file2", 10, mark)
            check(True, "term net: the answer is shown", "")

            # The kernel's server sends frames of up to 1024 bytes; a client that cut them at 512 lost the end of a long `ls`.
            mark = len(text())
            wire.send(guest, TAG_DATA, b"".join(b"line%02d-filler-filler-filler\r\n" % n for n in range(24)) + b"remote> ")
            wait_for("line23", 10, mark)
            check("line23" in text()[mark:] and "remote> " in text()[mark:], "term net: a 700-byte frame arrives whole, prompt included", text()[mark:][-200:])

            mark = len(text())
            wire.send(guest, TAG_BYE)
            wait_for("fos> ", 20, mark)
            check("ended the session" in text()[mark:], "term net: the peer's BYE ends the session", text()[mark:])

            frames.clear()
            type_(f"run fterm net {HOST_MAC_TEXT}\r".encode())
            check(wait_frame(TAG_HELLO) is not None, "term net: a second session opens", "")
            frames.clear()
            type_(b"\x01x")
            check(wait_frame(TAG_BYE) is not None, "term net: Ctrl-A x sends BYE", str(frames))
            wait_for("fos> ", 20, mark)

            # An fterm inside another fterm: prefix=b leaves Ctrl-A (and Ctrl-A x) to the machine behind it.
            frames.clear()
            mark = len(text())
            type_(f"run fterm net {HOST_MAC_TEXT} prefix=b\r".encode())
            check(wait_frame(TAG_HELLO) is not None, "prefix=b: the session opens", "")
            check("Ctrl-B x leaves" in text()[mark:], "prefix=b: the banner names Ctrl-B", text()[mark:])
            frames.clear()
            type_(b"\x01x")
            check(wait_frame(TAG_DATA, b"\x01x") is not None, "prefix=b: Ctrl-A x goes to the other end as data", str(frames))
            frames.clear()
            type_(b"\x02x")
            check(wait_frame(TAG_BYE) is not None, "prefix=b: Ctrl-B x sends BYE", str(frames))
            wait_for("fos> ", 20, mark)

            # A peer by its announced name, as `nic peers` lists it.
            wire.send(BROADCAST, TAG_DISCOVER_REPLY, b"labbox")
            pump(1)
            frames.clear()
            mark = len(text())
            type_(b"run fterm net LABBOX\r")
            check(wait_frame(TAG_HELLO) is not None, "term net: a peer is found by its announced name", text()[mark:])
            type_(b"\x01x")
            wait_for("fos> ", 20, mark)

            # The panel's P key lists announced machines; Enter puts the chosen name in the form.
            mark = len(text())
            type_(b"run fterm\r")
            wait_for("Connect to", 20, mark)
            type_(b"p")
            wait_for("Pick a machine", 10, mark)
            check("labbox" in text()[mark:], "panel: P lists the announced machines", text()[mark:])
            mark = len(text())
            type_(b"\r")
            wait_for("Machine (name or MAC)", 10, mark)
            check("labbox" in text()[mark:], "panel: Enter puts the chosen machine in the form", text()[mark:])
            type_(b"q")
            wait_for("fos> ", 20, mark)

            mark = len(text())
            type_(b"run fterm net nosuchhost\r")
            wait_for("fos> ", 20, mark)
            check("no machine called nosuchhost" in text()[mark:], "term net: an unknown name is refused", text()[mark:])
            mark = len(text())
            type_(b"uptime\r")
            wait_for("ticks at 100 Hz", 20, mark)
            check(True, "and the shell is still usable", "")
        finally:
            qemu.terminate()
            try:
                qemu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                qemu.kill()
    print("fterm-net: all checks passed")


if __name__ == "__main__":
    main()
