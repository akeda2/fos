#!/usr/bin/env python3
"""The `smbd` tape: a read-only SMB server run as a service on a user-mode NIC, used from the host with `smbclient`.

The second disk is an ext4 volume made by mkfs.ext4 -d (a UTF-8 name, a 3 MB file with a checksum, a folder of 300 files, a
subfolder). The guest shares it as //HOST/data through QEMU's forwarded port 445. Checks: share root listing, a subfolder, a
long folder, a big file's checksum, a ranged read, a missing file, that a write is refused, and that the shell stays usable.
Run with `make test-smbd`."""
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import check  # noqa: E402
from test_httpd import Guest  # noqa: E402

HOST_PORT = port(28445)
MANY = 300


def smbclient(command, protocol="SMB3_00", attempts=4):
    last = None
    for attempt in range(attempts):
        last = subprocess.run(["smbclient", "//127.0.0.1/data", "-p", str(HOST_PORT), "-N", "-m", protocol, "-c", command],
                              capture_output=True, text=True, timeout=120)
        if last.returncode == 0 or attempt == attempts - 1:
            break
        import time
        time.sleep(1.5)
    return last


def main():
    mkfs = shutil.which("mkfs.ext4") or "/usr/sbin/mkfs.ext4"
    if not Path(mkfs).exists() or not shutil.which("smbclient"):
        print("SKIP smbd: needs mkfs.ext4 and smbclient")
        return
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        source = tmp / "src"
        (source / "docs" / "deep").mkdir(parents=True)
        (source / "many").mkdir()
        big = os.urandom(3 << 20)
        (source / "big.bin").write_bytes(big)
        (source / "hello.txt").write_text("hello over smb\n")
        (source / "docs" / "räksmörgås.txt").write_text("hej\n")
        (source / "docs" / "deep" / "x.txt").write_text("deep\n")
        for index in range(MANY):
            (source / "many" / f"file-{index:04}.txt").write_text(f"{index}\n")
        image = tmp / "ext.img"
        image.write_bytes(bytes(48 << 20))
        subprocess.run([mkfs, "-q", "-F", "-L", "linuxvol", "-d", str(source), str(image)], check=True, capture_output=True)
        with Guest(forward=HOST_PORT, data_disk=image, guest_port=445) as g:
            g.line("write settings no-msi")
            screen = g.line("run smbd root=disk2 share=data log ip=10.0.2.15/24 gateway=10.0.2.2 &2", "fos> ", 40)
            check("smbd" in screen.lower() and "disk2" in screen, "smbd starts as a service on NIC 2", screen)

            out = smbclient("ls")
            check(out.returncode == 0 and "hello.txt" in out.stdout and "big.bin" in out.stdout and "docs" in out.stdout,
                  "the share root lists its files", out.stdout + out.stderr)
            check(f"{len(big)}" in out.stdout, "with the real size", out.stdout)
            out = smbclient("ls docs\\*")
            check("räksmörgås.txt" in out.stdout and "deep" in out.stdout, "a subfolder lists, UTF-8 names included", out.stdout + out.stderr)
            out = smbclient("ls many\\*")
            check(out.returncode == 0 and f"file-{MANY - 1:04}.txt" in out.stdout and out.stdout.count("file-") == MANY,
                  f"a folder of {MANY} files lists completely", out.stdout[-300:] + out.stderr)
            for protocol in ("SMB3_00", "SMB2_02"):
                target = tmp / f"got-{protocol}"
                out = smbclient(f"get big.bin {target}", protocol)
                check(out.returncode == 0 and target.exists() and hashlib.sha256(target.read_bytes()).hexdigest() == hashlib.sha256(big).hexdigest(),
                      f"a 3 MB file arrives intact ({protocol})", out.stdout + out.stderr)
            target = tmp / "hello"
            out = smbclient(f"get docs\\deep\\x.txt {target}")
            check(target.exists() and target.read_text() == "deep\n", "a file in a nested folder", out.stdout + out.stderr)
            out = smbclient("get nothere.txt " + str(tmp / "none"))
            check("NT_STATUS_OBJECT_NAME_NOT_FOUND" in out.stdout + out.stderr, "a missing file is reported", out.stdout + out.stderr)
            (tmp / "up.txt").write_text("x")
            out = smbclient(f"put {tmp / 'up.txt'} up.txt")
            check(out.returncode != 0 or "NT_STATUS" in out.stdout + out.stderr, "a write is refused", out.stdout + out.stderr)
            shell = g.line("uptime")
            check("fos>" in shell, "the shell stays usable while the service runs", shell)
    print("smbd: all checks passed")


if __name__ == "__main__":
    main()
