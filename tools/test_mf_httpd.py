#!/usr/bin/env python3
"""`httpd` on the server kernel (`make test-mf-httpd`): a web root on an ext4 disk, several large downloads at once through
the worker threads (each must arrive whole, checked by SHA-256), Range and If-Range on an ext file, ETag/Last-Modified and
304, keep-alive and a listing. The guest takes its address by DHCP from QEMU's user-mode network."""
import hashlib
import http.client
import os
import select
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_HTTPD", ROOT / "build" / "fosfor-mf" / "fosfor-httpd.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
HOST_PORT = port(28081)
failures = []


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}: {detail}")


def request(method, path, headers=None, connection=None):
    own = connection is None
    connection = connection or http.client.HTTPConnection("127.0.0.1", HOST_PORT, timeout=60)
    connection.request(method, path, headers=headers or {})
    response = connection.getresponse()
    data = response.read()
    if own:
        connection.close()
    return response, data


def main():
    mkfs = shutil.which("mkfs.ext4") or "/usr/sbin/mkfs.ext4"
    if not Path(mkfs).exists():
        print("SKIP: no mkfs.ext4")
        return 0
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        source = tmp / "src" / "www"
        (source / "albums").mkdir(parents=True)
        big = [os.urandom((5 + index) << 20 | 123) for index in range(4)]
        for index, blob in enumerate(big):
            (source / "albums" / f"side{index}.flac").write_bytes(blob)
        (source / "hello.txt").write_text("hello from ext4\n")
        disk = tmp / "ext.img"
        disk.write_bytes(bytes(96 << 20))
        subprocess.run([mkfs, "-q", "-F", "-L", "linuxvol", "-d", str(tmp / "src"), str(disk)], check=True, capture_output=True)
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
             "-nic", f"user,model=e1000,hostfwd=tcp::{HOST_PORT}-:80", "-serial", "stdio", "-monitor", "none", "-no-reboot",
             "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={IMAGE},snapshot=on",
             "-drive", f"format=raw,file={disk},snapshot=on"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        log = b""

        def wait_for(text, timeout):
            nonlocal log
            end = time.time() + timeout
            while time.time() < end and text.encode() not in log:
                if select.select([qemu.stdout], [], [], 0.2)[0]:
                    data = os.read(qemu.stdout.fileno(), 4096)
                    if not data:
                        break
                    log += data
            return text.encode() in log

        try:
            if not wait_for("fos> ", 150):
                failures.append("no shell prompt:\n" + log[-800:].decode("utf8", "replace"))
                return finish()
            qemu.stdin.write(b"run httpd @httpd.cfg &1\n")
            qemu.stdin.flush()
            if not wait_for("worker threads", 40):
                failures.append("httpd did not start with worker threads:\n" + log[-800:].decode("utf8", "replace"))
                return finish()
            time.sleep(1)
            response, data = request("GET", "/hello.txt")
            check(response.status == 200 and data == b"hello from ext4\n", "a small file from the ext4 root", f"{response.status} {data!r}")
            etag, modified = response.getheader("ETag"), response.getheader("Last-Modified")
            check(bool(etag) and bool(modified) and response.getheader("Accept-Ranges") == "bytes", "ext files carry ETag, Last-Modified and Accept-Ranges", str(response.getheaders()))
            response, data = request("GET", "/hello.txt", {"If-None-Match": etag})
            check(response.status == 304 and data == b"", "If-None-Match with the ETag is 304", str(response.status))
            response, data = request("GET", "/hello.txt", {"If-Modified-Since": modified})
            check(response.status == 304 and data == b"", "If-Modified-Since with the date is 304", str(response.status))
            response, data = request("GET", "/hello.txt", {"If-None-Match": '"nope"'})
            check(response.status == 200, "a different ETag gets the file", str(response.status))
            response, data = request("GET", "/hello.txt", {"Range": "bytes=6-9", "If-Range": etag})
            check(response.status == 206 and data == b"from", "Range with a matching If-Range is 206", f"{response.status} {data!r}")
            response, data = request("GET", "/hello.txt", {"Range": "bytes=6-9", "If-Range": '"stale"'})
            check(response.status == 200 and data == b"hello from ext4\n", "Range with a stale If-Range is the whole file", f"{response.status} {data!r}")
            response, data = request("GET", "/albums/")
            check(response.status == 200 and b"side3.flac" in data, "an ext4 folder lists", f"{response.status} {data[:200]!r}")

            results = {}

            def download(index):
                response, data = request("GET", f"/albums/side{index}.flac")
                results[index] = (response.status, hashlib.sha256(data).hexdigest(), len(data))

            started = time.time()
            threads = [threading.Thread(target=download, args=(index,)) for index in range(4)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(300)
            elapsed = time.time() - started
            ok = all(results.get(index, (0, ""))[:2] == (200, hashlib.sha256(blob).hexdigest()) for index, blob in enumerate(big))
            check(ok, f"four large downloads at once all arrive intact ({elapsed:.1f} s)", repr(results))

            blob = big[1]
            start = (1 << 20) + 777
            response, data = request("GET", "/albums/side1.flac", {"Range": f"bytes={start}-{start + 400_000}"})
            check(response.status == 206 and data == blob[start:start + 400_001], "a range in the middle of a large file", f"{response.status} {len(data)}")
            response, data = request("GET", "/albums/side1.flac", {"Range": "bytes=-1000"})
            check(response.status == 206 and data == blob[-1000:], "the last 1000 bytes", f"{response.status} {len(data)}")
            connection = http.client.HTTPConnection("127.0.0.1", HOST_PORT, timeout=60)
            response, first = request("GET", "/hello.txt", connection=connection)
            response, second = request("GET", "/albums/side0.flac", {"Range": "bytes=0-99"}, connection=connection)
            check(first == b"hello from ext4\n" and second == big[0][:100], "keep-alive: two requests on one connection", f"{first!r} {len(second)}")
            connection.close()

            # Clients that hang up between requests must free their slots at once (a SYN otherwise waits 1 s to retry).
            def flood():
                for _ in range(25):
                    t = time.time()
                    request("GET", "/hello.txt")
                    lats.append(time.time() - t)
            lats = []
            started = time.time()
            workers = [threading.Thread(target=flood) for _ in range(8)]
            [worker.start() for worker in workers]
            [worker.join() for worker in workers]
            elapsed = time.time() - started
            lats.sort()
            check(elapsed < 6, "200 requests on new connections, 8 at a time, do not stall on busy slots", f"{elapsed:.1f} s")
            if b"CPU exception" in log:
                failures.append("the kernel faulted:\n" + log[-1200:].decode("utf8", "replace"))
        finally:
            qemu.kill()
    return finish()


def finish():
    for failure in failures:
        print("FAIL: " + failure)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
