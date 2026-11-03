#!/usr/bin/env python3
"""The `smb` tape (client shell command) against a host-side server: `crates/smb/examples/serve.rs` shares a folder on a
loopback port, QEMU's user-mode network (guestfwd) makes it reachable as 10.0.2.100:445 from the guest's NIC 2. Checks `smb ls`
(root, a subfolder, UTF-8 names, a long folder), `smb cat`, `smb get` of a 3 MB file (SHA-256 on the guest compared with the
host's), a missing file and a bad URL. Run with `make test-smb-client`."""
import hashlib
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import check  # noqa: E402
from test_httpd import Guest  # noqa: E402

SERVER_PORT = port(28446)
MANY = 200


def main():
    root = Path(__file__).resolve().parent.parent
    built = subprocess.run(["cargo", "build", "--release", "--example", "serve"], cwd=root / "crates" / "smb", capture_output=True, text=True)
    if built.returncode != 0:
        raise SystemExit("FAIL: cannot build the host server\n" + built.stderr[-1500:])
    server_binary = root / "crates" / "smb" / "target" / "release" / "examples" / "serve"
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        share = tmp / "share"
        (share / "docs").mkdir(parents=True)
        (share / "many").mkdir()
        big = os.urandom(3 << 20)
        (share / "big.bin").write_bytes(big)
        (share / "hello.txt").write_text("hello from the host\n")
        (share / "docs" / "räksmörgås.txt").write_text("hej\n")
        for index in range(MANY):
            (share / "many" / f"f{index:04}.txt").write_text("x")
        server = subprocess.Popen([str(server_binary), str(share), str(SERVER_PORT)], stdout=subprocess.DEVNULL)
        time.sleep(0.5)
        try:
            extra = f",guestfwd=tcp:10.0.2.100:445-cmd:nc 127.0.0.1 {SERVER_PORT}"
            with Guest(user_extra=extra) as g:
                g.line("write settings no-msi")
                g.line("nic use 2")
                url = "//10.0.2.100/share"
                out = g.line(f"run smb ls {url}", "fos> ", 60)
                check("hello.txt" in out and "big.bin" in out and "<dir>" in out and str(len(big)) in out and "4 entries" in out, "ls lists the share root", out[-500:])
                out = g.line(f"run smb ls {url}/docs", "fos> ", 40)
                check("räksmörgås.txt" in out and "1 entries" in out, "ls of a subfolder, UTF-8 name", out[-500:])
                out = g.line(f"run smb ls {url}/many", "fos> ", 60)
                check(f"{MANY} entries" in out, f"ls of a {MANY}-file folder", out[-300:])
                out = g.line(f"run smb cat {url}/hello.txt", "fos> ", 40)
                check("hello from the host" in out, "cat prints a file", out[-300:])
                out = g.line(f"run smb get {url}/big.bin got.bin", "fos> ", 120)
                check("3145728 bytes" in out and "got.bin" in out, "get copies a 3 MB file", out[-400:])
                out = g.line("sha256 got.bin", "fos> ", 60)
                digest = hashlib.sha256(big).hexdigest()
                check(digest in out, "the copy has the same SHA-256", out[-300:])
                out = g.line(f"run smb get {url}/hello.txt", "fos> ", 40)
                check("20 bytes" in out, "get without a name keeps the file's name", out[-300:])
                out = g.line("cat hello.txt", "fos> ", 20)
                check("hello from the host" in out, "and the small copy reads back", out[-300:])
                out = g.line(f"run smb cat {url}/nothere.txt", "fos> ", 40)
                check("smb: " in out and "no such file" in out.lower(), "a missing file is reported", out[-300:])
                out = g.line("run smb ls 10.0.2.100/share", "fos> ", 20)
                check("//HOST/SHARE" in out, "a URL without // is refused", out[-300:])
        finally:
            server.terminate()
        # A second server that wants alice/secret: the client logs in with NTLMv2 and signs.
        server = subprocess.Popen([str(server_binary), str(share), str(SERVER_PORT + 1)], stdout=subprocess.DEVNULL,
                                  env={**os.environ, "SMB_LOGIN": "alice:secret"})
        time.sleep(0.5)
        try:
            extra = f",guestfwd=tcp:10.0.2.100:445-cmd:nc 127.0.0.1 {SERVER_PORT + 1}"
            with Guest(user_extra=extra) as g:
                g.line("write settings no-msi")
                g.line("nic use 2")
                url = "//10.0.2.100/share"
                out = g.line(f"run smb ls {url} user=alice password=secret", "fos> ", 60)
                check("hello.txt" in out and "4 entries" in out, "a password login lists the share", out[-400:])
                out = g.line(f"run smb get {url}/big.bin signed.bin user=alice password=secret", "fos> ", 120)
                check("3145728 bytes" in out, "signed reads of 3 MB work", out[-400:])
                out = g.line("sha256 signed.bin", "fos> ", 60)
                check(hashlib.sha256(big).hexdigest() in out, "and arrive intact", out[-300:])
                out = g.line(f"run smb ls {url} user=alice password=wrong", "fos> ", 40)
                check("smb: " in out.lower() and "hello.txt" not in out, "a wrong password is refused", out[-300:])
                out = g.line(f"run smb ls {url}", "fos> ", 40)
                check("hello.txt" not in out, "a guest is refused", out[-300:])
        finally:
            server.terminate()
    print("smb client: all checks passed")


if __name__ == "__main__":
    main()
