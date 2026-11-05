#!/usr/bin/env python3
"""FOS as an SMB gateway (`make test-smbd-reexport`): `smbmount` mounts a share of a host server (NIC 3, through guestfwd) as //nas
and `smbd root=//nas` (NIC 2) shares it again, so smbclient on the host reads and writes the host's own files through the guest.
The smbd service waits on the remote-volume mailbox from inside a service."""
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import check  # noqa: E402
from test_httpd import Guest  # noqa: E402

SERVER_PORT = port(28449)
HOST_PORT = port(28450)


def smbclient(command):
    return subprocess.run(["smbclient", f"//127.0.0.1/data", "-p", str(HOST_PORT), "-N", "-c", command], capture_output=True, text=True, timeout=120)


def main():
    if not shutil.which("smbclient"):
        print("SKIP smbd-reexport: needs smbclient")
        return
    root = Path(__file__).resolve().parent.parent
    built = subprocess.run(["cargo", "build", "--release", "--example", "serve", "--target-dir", "target"], cwd=root / "crates" / "smb", capture_output=True, text=True)
    if built.returncode != 0:
        raise SystemExit("FAIL: cannot build the host server\n" + built.stderr[-1500:])
    binary = root / "crates" / "smb" / "target" / "release" / "examples" / "serve"
    with tempfile.TemporaryDirectory() as tmp:
        share = Path(tmp) / "share"
        (share / "docs").mkdir(parents=True)
        big = os.urandom(2 << 20)
        (share / "big.bin").write_bytes(big)
        (share / "hello.txt").write_text("hello from the host\n")
        (share / "docs" / "deep.txt").write_text("deep\n")
        server = subprocess.Popen([str(binary), str(share), str(SERVER_PORT)], stdout=subprocess.DEVNULL)
        time.sleep(0.5)
        try:
            third = ["-netdev", f"user,id=net2,guestfwd=tcp:10.0.2.100:445-cmd:nc 127.0.0.1 {SERVER_PORT}", "-device", "e1000,netdev=net2,mac=52:54:00:12:34:7c"]
            with Guest(forward=HOST_PORT, guest_port=445, extra_command=third) as g:
                g.line("write settings no-msi")
                out = g.line("run smbmount //10.0.2.100/share as=nas &3", "fos> ", 60)
                check("//nas" in out, "smbmount mounts the host share on NIC 3", out[-400:])
                time.sleep(1)
                out = g.line("run smbd root=//nas share=data ip=10.0.2.15/24 gateway=10.0.2.2 &2", "fos> ", 40)
                check("//nas" in out or "data" in out, "smbd starts on the mount", out[-400:])
                out = smbclient("ls")
                check(out.returncode == 0 and "hello.txt" in out.stdout and "docs" in out.stdout, "smbclient lists the re-exported share", out.stdout + out.stderr)
                out = smbclient("ls docs\\*")
                check("deep.txt" in out.stdout, "a subfolder lists", out.stdout + out.stderr)
                target = Path(tmp) / "got.bin"
                out = smbclient(f"get big.bin {target}")
                check(target.exists() and hashlib.sha256(target.read_bytes()).hexdigest() == hashlib.sha256(big).hexdigest(), "a 2 MB file arrives intact through both hops", out.stdout + out.stderr)
                (Path(tmp) / "up.txt").write_text("written through the gateway\n")
                out = smbclient(f"put {Path(tmp) / 'up.txt'} up.txt")
                check((share / "up.txt").exists() and (share / "up.txt").read_text() == "written through the gateway\n", "a put lands on the host", out.stdout + out.stderr)
                out = smbclient("mkdir newdir; rename up.txt moved.txt; rm moved.txt; rmdir newdir")
                check(not (share / "up.txt").exists() and not (share / "moved.txt").exists() and not (share / "newdir").exists(), "mkdir, rename, rm, rmdir", out.stdout + out.stderr)
                check("fos>" in g.line("uptime"), "the shell stays usable")
        finally:
            server.terminate()
    print("smbd re-export: all checks passed")


if __name__ == "__main__":
    main()
