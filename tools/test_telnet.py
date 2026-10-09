#!/usr/bin/env python3
"""The `telnet` tape: connect, negotiate, show what the server sends, send what is typed, notice a disconnect.

The guest has two NICs: the first carries the `term net` session that types for the tape (multicast segment); the second sits on QEMU's
user-mode network and is chosen with `nic use 2`, which a tape's single-interface network calls follow. The user-mode network is 10.0.2.0/24 with the gateway and the host
at 10.0.2.2; a small Telnet server on the host (127.0.0.1) plays the remote machine: it asks for TERMINAL-TYPE and NAWS and prints a greeting with UTF-8 text; the test types a line through the
term session and the server echoes it back. Checks: the tape connects (static addressing, ARP for the gateway, a TCP handshake);
its Telnet replies agree to terminal type and window size; the greeting and the UTF-8 text reach the screen; a typed line reaches the
server as a line; the echoed answer comes back; and when the server closes the connection the tape returns to a usable shell.
A second run with `dhcp` checks the DHCP client against QEMU's DHCP server (the legacy net-dhcp test covered it on its own). Ported from the legacy tapes/test_telnet_e2e.py, which implemented ARP/IPv4/TCP by hand; slirp does that here. Run with
`make test-telnet`."""
import os
import re
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import (BROADCAST, CODE_CANDIDATES, IMAGE, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)

IAC, DONT, DO, WONT, WILL, SB, SE = 255, 254, 253, 252, 251, 250, 240
TERMINAL_TYPE, NAWS = 24, 31
SERVER_PORT = 23123


class Server(threading.Thread):
    """One connection: negotiates, greets, echoes complete lines, closes after 'bye'."""

    def __init__(self):
        super().__init__(daemon=True)
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", SERVER_PORT))
        self.listener.listen(1)
        self.received = bytearray()
        self.replies = bytearray()
        self.connected = threading.Event()

    def run(self):
        while True:
            connection, _ = self.listener.accept()
            self.state = "data"
            self.connected.set()
            try:
                self.serve(connection)
            except Exception as error:  # a test-server bug must show up in the output, not as a silent disconnect
                print(f'telnet test server error: {error!r}')
            finally:
                connection.close()

    def serve(self, connection):
        connection.sendall(bytes([IAC, DO, TERMINAL_TYPE, IAC, DO, NAWS]))
        connection.sendall("Welcome to the test server: gröövy ✓\r\nlogin: ".encode())
        line = bytearray()
        connection.settimeout(0.2)
        while True:
            try:
                data = connection.recv(1024)
            except socket.timeout:
                continue
            except OSError:
                return
            if not data:
                return
            self.received.extend(data)
            if IAC in data:
                self.replies.extend(data)
            for byte in self.strip_commands(data):
                if byte in (10, 13):
                    if line:
                        text = bytes(line).decode("latin-1")
                        line.clear()
                        connection.sendall(f"\r\nyou typed: {text}\r\n".encode())
                        if text == "bye":
                            connection.close()
                            return
                elif 32 <= byte < 127:
                    line.append(byte)

    state = "data"

    def strip_commands(self, data):
        """The data bytes of `data` with Telnet commands (IAC xx, IAC WILL/WONT/DO/DONT xx, IAC SB ... IAC SE) removed."""
        out = bytearray()
        for byte in data:
            if self.state == "data":
                if byte == IAC:
                    self.state = "iac"
                else:
                    out.append(byte)
            elif self.state == "iac":
                if byte in (WILL, WONT, DO, DONT):
                    self.state = "option"
                elif byte == SB:
                    self.state = "sub"
                elif byte == IAC:
                    out.append(IAC)
                    self.state = "data"
                else:
                    self.state = "data"
            elif self.state == "option":
                self.state = "data"
            elif self.state == "sub":
                if byte == IAC:
                    self.state = "sub-iac"
            elif self.state == "sub-iac":
                self.state = "data" if byte == SE else "sub"
        return bytes(out)


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    server = Server()
    server.start()
    with tempfile.TemporaryDirectory() as tmp:
        image = Path(tmp) / "image.img"
        shutil.copy(IMAGE, image)
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={image}",
                   # NIC 1 carries the term net session that types for the tape; NIC 2 is the user-mode network, chosen with
                   # `nic use 2` (the tape's single-interface network calls follow it, ABI version 4).
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:7a",
                   "-netdev", "user,id=net1", "-device", "e1000,netdev=net1,mac=52:54:00:12:34:7b"]
        wire = Wire()
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 180

        def pump(marker):
            while marker not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: never saw {marker!r}\n{clean(bytes(raw))[-3500:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))

        try:
            pump("M3 complete")
            wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            guest = None
            for _ in range(30):
                for source, tag, _ in wire.frames(0.2):
                    if tag == TAG_DISCOVER_REPLY:
                        guest = source
                if guest:
                    break
            wire.send(guest, TAG_HELLO)
            wire.until("fos> ", 10)

            def line(text, marker, timeout=20):
                wire.send(guest, TAG_DATA, text.encode() + b"\r")
                return wire.until(marker, timeout)

            check("Using NIC 2" in line("nic use 2", "fos> "), "`nic use 2` selects the user-mode NIC for the tape's network calls", "")
            screen = line(f"run telnet static 10.0.2.15/24 10.0.2.2 10.0.2.2 {SERVER_PORT}", "login:", 40)
            check(server.connected.wait(5), "the tape connects to the server (ARP for the gateway, then TCP)", screen)
            check("Welcome to the test server" in screen, "the server's greeting is shown", screen)
            # UTF-8 from the server arrives whole: "ö" is a character now (the term client gets it as UTF-8; the console shows '?' for the check mark, which the font lacks).
            check("gröövy" in screen, "UTF-8 text from the server reaches the screen as characters", screen)

            time.sleep(0.5)
            replies = bytes(server.replies)
            check(bytes([IAC, WILL, TERMINAL_TYPE]) in replies, "it agrees to send its terminal type (IAC WILL TERMINAL-TYPE)", repr(replies))
            check(bytes([IAC, WILL, NAWS]) in replies, "and to report the window size (IAC WILL NAWS)", repr(replies))

            for byte in b"hello\r":
                wire.send(guest, TAG_DATA, bytes([byte]))
                time.sleep(0.06)
            answer = wire.until("you typed: hello", 15)
            check("you typed: hello" in answer, "a line typed in the term session reaches the server and its answer comes back", answer)

            for byte in b"bye\r":
                wire.send(guest, TAG_DATA, bytes([byte]))
                time.sleep(0.06)
            back = wire.until("fos> ", 30)
            check("fos> " in back, "when the server closes the connection the tape returns to the shell", back)
            check("ticks at 100 Hz" in line("uptime", "ticks at 100 Hz"), "and the shell is still usable", "")

            # The DHCP client: the same tape with `dhcp` takes its address from QEMU's DHCP server (10.0.2.15) and connects again.
            server.connected.clear()
            screen = line(f"run telnet dhcp 10.0.2.2 {SERVER_PORT}", "login:", 60)
            check("TELNET CONFIG" in screen and "10.0.2.15" in screen, "`telnet dhcp` gets its address from a real DHCP server (10.0.2.15)", screen)
            check(server.connected.wait(5), "and connects with it", screen)
            for byte in b"bye\r":
                wire.send(guest, TAG_DATA, bytes([byte]))
                time.sleep(0.06)
            check("fos> " in wire.until("fos> ", 30), "and returns to the shell as before", "")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
    print("telnet: all checks passed")


if __name__ == "__main__":
    main()
