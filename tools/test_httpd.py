#!/usr/bin/env python3
"""The `httpd` tape: a small web server run as a service on a user-mode NIC, fetched from the host through a forwarded port.

The guest has two NICs: NIC 1 carries the `term net` session that types for the shell; NIC 2 is QEMU's user-mode network (guest
10.0.2.15, gateway 10.0.2.2) with `hostfwd` of a host port to guest port 80. A second disk is formatted as a labelled volume so
`root=@LABEL` is tested too. Checks: file bodies and types, `/` serves index.html, a folder without a slash redirects, a listing
with `listing=on` (and none without), 404, 405, HEAD, blocked config files, `..` traversal, a multi-megabyte file with a matching checksum,
a root on the second volume, and that the shell stays usable while the service runs. `Guest` is reused by test_web.py.
Run with `make test-httpd`."""
import hashlib
import http.client
import os
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import (BROADCAST, CODE_CANDIDATES, IMAGE, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)

HOST_PORT = port(28080)


class Guest:
    """A booted guest with a shell on the term net session. `with Guest(forward=...) as g: g.line("uptime", "ticks")`."""

    def __init__(self, forward=None, data_disk=False, timeout=240, guest_port=80, user_extra="", save_data=None, extra_command=()):
        self.save_data = save_data  # where to copy the data disk when the guest is gone
        self.forward, self.data_disk, self.timeout, self.guest_port = forward, data_disk, timeout, guest_port
        self.user_extra = user_extra
        self.extra_command = list(extra_command)  # more QEMU arguments, e.g. a third NIC

    def __enter__(self):
        code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
        self.tmp = tempfile.TemporaryDirectory()
        tmp = Path(self.tmp.name)
        image, vars_copy = tmp / "image.img", tmp / "vars.fd"
        shutil.copy(IMAGE, image)
        shutil.copy(variables, vars_copy)
        user = "user,id=net1" + self.user_extra + (f",hostfwd=tcp:127.0.0.1:{self.forward}-10.0.2.15:{self.guest_port}" if self.forward else "")
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={image}"]
        if self.data_disk:
            data = tmp / "data.img"
            if isinstance(self.data_disk, Path):  # a ready-made disk image
                shutil.copy(self.data_disk, data)
            else:
                data.write_bytes(bytes(64 << 20))
            command += ["-drive", f"format=raw,file={data}"]
        command += ["-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:7a",
                    "-netdev", user, "-device", "e1000,netdev=net1,mac=52:54:00:12:34:7b"] + self.extra_command
        self.wire = Wire()
        self.process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.raw = bytearray()
        self.deadline = time.monotonic() + self.timeout
        while "M3 complete" not in clean(bytes(self.raw)):
            if time.monotonic() > self.deadline or self.process.poll() is not None:
                raise SystemExit(f"FAIL: never booted\n{clean(bytes(self.raw))[-3500:]}")
            ready, _, _ = select.select([self.process.stdout], [], [], 0.2)
            if ready:
                self.raw.extend(os.read(self.process.stdout.fileno(), 4096))
        self.wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
        self.guest = None
        for _ in range(30):
            for source, tag, _ in self.wire.frames(0.2):
                if tag == TAG_DISCOVER_REPLY:
                    self.guest = source
            if self.guest:
                break
        self.wire.send(self.guest, TAG_HELLO)
        self.wire.until("fos> ", 10)
        return self

    def line(self, text, marker="fos> ", timeout=30):
        self.wire.send(self.guest, TAG_DATA, text.encode() + b"\r")
        return self.wire.until(marker, timeout)

    def type(self, data):
        for byte in data:
            self.wire.send(self.guest, TAG_DATA, bytes([byte]))
            time.sleep(0.06)

    def __exit__(self, *exc):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
        if self.save_data and self.data_disk:
            shutil.copy(Path(self.tmp.name) / "data.img", self.save_data)
        self.tmp.cleanup()


def get(method, path, body=None, attempts=5):
    """One request to the forwarded port; (status, headers, body). Retries: the first SYNs can race the ARP of the guest."""
    for attempt in range(attempts):
        try:
            connection = http.client.HTTPConnection("127.0.0.1", HOST_PORT, timeout=15)
            connection.request(method, path, body=body)
            response = connection.getresponse()
            data = response.read()
            connection.close()
            return response.status, dict(response.getheaders()), data
        except (OSError, http.client.HTTPException):
            if attempt == attempts - 1:
                raise
            time.sleep(1)


def raw(data, want_responses=1, timeout=15):
    """Sends `data` on a fresh connection and reads until the server closes it (or, for one answer, until its body is in)."""
    with socket.create_connection(("127.0.0.1", HOST_PORT), timeout=timeout) as sock:
        sock.sendall(data)
        got = b""
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            got += chunk
            if want_responses == 1 and b"\r\n\r\n" in got:
                head, _, rest = got.partition(b"\r\n\r\n")
                length = [int(line.split(b":")[1]) for line in head.split(b"\r\n") if line.lower().startswith(b"content-length")]
                if length and len(rest) >= length[0]:
                    break
        return got


def main():
    tape = Path(__file__).resolve().parent.parent / "build" / "fosfor" / "tapes" / "fosplay"
    with Guest(forward=HOST_PORT, data_disk=True) as g:
        g.line("mkdir www")
        g.line("write www/index.html <h1>Hello fos</h1>")
        g.line("write www/a.txt plain text here")
        g.line("mkdir www/docs")
        g.line("write www/docs/b.html <p>bee</p>")
        g.line("write settings no-msi")
        g.line("format disk2 data v4")
        g.line("format disk2 confirm", "fos> ", 60)
        g.line("cp www/a.txt @data")
        g.line("cp bin/fosplay www/big.bin", "fos> ", 60)

        screen = g.line("run httpd root=www listing=on log ip=10.0.2.15/24 gateway=10.0.2.2 &2", "fos> ", 40)
        check("httpd" in screen.lower(), "httpd starts as a service on NIC 2", screen)

        status, headers, body = get("GET", "/a.txt")
        check(status == 200 and body == b"plain text here", "a file is served with its body", f"{status} {body!r}")
        check(headers.get("Content-Type", "").startswith("text/plain"), "with a text/plain type", str(headers))
        status, headers, body = get("GET", "/")
        check(status == 200 and b"Hello fos" in body and headers.get("Content-Type", "").startswith("text/html"),
              "`/` serves index.html as HTML", f"{status} {body!r}")
        status, headers, _ = get("GET", "/docs")
        check(status == 301 and headers.get("Location") == "/docs/", "a folder without a slash redirects to the slash", f"{status} {headers}")
        status, _, body = get("GET", "/docs/")
        check(status == 200 and b"b.html" in body, "a folder without an index shows a listing (listing=on)", f"{status} {body!r}")
        status, _, body = get("GET", "/docs/b.html")
        check(status == 200 and body == b"<p>bee</p>", "a file in a folder is served", f"{status} {body!r}")
        status, _, _ = get("GET", "/nothing.txt")
        check(status == 404, "a missing file is 404", str(status))
        status, _, _ = get("POST", "/a.txt", body=b"x")
        check(status == 405, "POST is 405", str(status))
        status, headers, body = get("HEAD", "/a.txt")
        check(status == 200 and body == b"" and headers.get("Content-Length") == "15", "HEAD gives the head and no body", f"{status} {headers}")
        for name in ("/settings", "/config/settings", "/synckey", "/hostname"):
            status, _, _ = get("GET", name)
            check(status == 404, f"{name} is never served", str(status))
        status, _, _ = get("GET", "/../settings")
        check(status in (400, 404), "`..` is refused", str(status))
        status, _, _ = get("GET", "/%2e%2e/settings")
        check(status in (400, 404), "an encoded `..` is refused", str(status))
        check("ticks at 100 Hz" in g.line("uptime", "ticks at 100 Hz"), "the shell stays usable while the service runs", "")

        # A multi-megabyte file crosses many chunks and segments; the checksum shows nothing was lost or reordered.
        status, _, body = get("GET", "/big.bin")
        check(status == 200 and hashlib.sha256(body).digest() == hashlib.sha256(tape.read_bytes()).digest(),
              "a large file arrives intact (SHA-256 matches the tape)", f"{status} {len(body)} bytes")

        # Keep-alive, ranges, pipelining.
        connection = http.client.HTTPConnection("127.0.0.1", HOST_PORT, timeout=15)
        connection.request("GET", "/a.txt")
        first = connection.getresponse()
        first_body = first.read()
        connection.request("GET", "/docs/b.html")
        second = connection.getresponse()
        second_body = second.read()
        check(first_body == b"plain text here" and second_body == b"<p>bee</p>" and first.version == 11
              and (first.getheader("Connection") or "").lower() == "keep-alive", "two requests share one connection (keep-alive)", f"{first_body!r} {second_body!r}")
        connection.close()
        answers = raw(b"GET /a.txt HTTP/1.1\r\nHost: x\r\n\r\nGET /docs/b.html HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", 2)
        check(answers.count(b"HTTP/1.1 200") == 2 and answers.index(b"plain text here") < answers.index(b"<p>bee</p>"),
              "pipelined requests are answered in order", repr(answers))
        answer = raw(b"GET /a.txt HTTP/1.0\r\n\r\n")
        check(answer.startswith(b"HTTP/1.1 200") and b"Connection: close" in answer and answer.endswith(b"plain text here"),
              "HTTP/1.0 gets Connection: close and the body", repr(answer))
        for header, want_status, want_body, want_range in (("bytes=6-9", 206, b"text", "bytes 6-9/15"), ("bytes=-4", 206, b"here", "bytes 11-14/15"),
                                                           ("bytes=11-", 206, b"here", "bytes 11-14/15"), ("bytes=0-1000", 206, b"plain text here", "bytes 0-14/15"),
                                                           ("bytes=15-", 416, None, "bytes */15"), ("bytes=0-1,3-4", 200, b"plain text here", None)):
            connection = http.client.HTTPConnection("127.0.0.1", HOST_PORT, timeout=15)
            connection.request("GET", "/a.txt", headers={"Range": header})
            response = connection.getresponse()
            data = response.read()
            connection.close()
            check(response.status == want_status and (want_body is None or data == want_body) and response.getheader("Content-Range") == want_range,
                  f"Range: {header} -> {want_status}", f"{response.status} {data!r} {response.getheader('Content-Range')}")
        status, headers, _ = get("HEAD", "/a.txt")
        check(headers.get("Accept-Ranges") == "bytes", "files advertise Accept-Ranges", str(headers))
        big = tape.read_bytes()
        start = 300_000
        connection = http.client.HTTPConnection("127.0.0.1", HOST_PORT, timeout=30)
        connection.request("GET", "/big.bin", headers={"Range": f"bytes={start}-"})
        response = connection.getresponse()
        data = response.read()
        connection.close()
        check(response.status == 206 and data == big[start:], "a ranged download of a large file resumes mid-chunk", f"{response.status} {len(data)} of {len(big) - start}")

        # A client that sends half a request and stalls must not hold up the others.
        slow = socket.create_connection(("127.0.0.1", HOST_PORT), timeout=15)
        slow.sendall(b"GET /a.tx")
        status, _, body = get("GET", "/a.txt")
        check(status == 200 and body == b"plain text here", "a stalled half request does not block another client", f"{status} {body!r}")
        slow.sendall(b"t HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        slow.settimeout(15)
        reply = b""
        while chunk := slow.recv(4096):
            reply += chunk
        slow.close()
        check(reply.startswith(b"HTTP/1.1 200") and reply.endswith(b"plain text here"), "and finishes once the rest arrives", repr(reply))

        g.line("kill httpd")
        g.line("run httpd root=@data ip=10.0.2.15/24 gateway=10.0.2.2 &2", "fos> ", 40)
        status, _, body = get("GET", "/a.txt")
        check(status == 200 and body == b"plain text here", "root=@LABEL serves a file of another volume", f"{status} {body!r}")
        status, _, _ = get("GET", "/")
        check(status == 404, "and a folder without an index file is 404 while listing is off (the default)", str(status))
    print("httpd: all checks passed")


if __name__ == "__main__":
    main()
