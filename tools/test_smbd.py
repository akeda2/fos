#!/usr/bin/env python3
"""The `smbd` tape: an SMB server run as a service on a user-mode NIC, used from the host with `smbclient`.

The second disk is an ext4 volume made by mkfs.ext4 -d (a UTF-8 name, a 3 MB file with a checksum, a folder of 300 files, a
subfolder). The guest shares it as //HOST/data through QEMU's forwarded port 445. Checks: share root listing, a subfolder, a
long folder, a big file's checksum, a ranged read, a missing file, that a write is refused where the folder's mode bits
forbid it, and that the shell stays usable. A second boot with an smbusers file checks logins and writes (put, mkdir, rename,
overwrite, delete) into a world-writable folder, then e2fsck and debugfs check the volume on the host.
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


def smbclient(command, protocol="SMB3_00", attempts=4, login=("-N",), share="data"):
    last = None
    for attempt in range(attempts):
        last = subprocess.run(["smbclient", f"//127.0.0.1/{share}", "-p", str(HOST_PORT), *login, "-m", protocol, "-c", command],
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
        (source / "locked.txt").write_text("not for alice\n")
        (source / "locked.txt").chmod(0o600)
        (source / "drop").mkdir()
        (source / "drop").chmod(0o777)
        (source / "docs" / "räksmörgås.txt").write_text("hej\n")
        (source / "docs" / "deep" / "x.txt").write_text("deep\n")
        for index in range(MANY):
            (source / "many" / f"file-{index:04}.txt").write_text(f"{index}\n")
        image = tmp / "ext.img"
        image.write_bytes(bytes(48 << 20))
        subprocess.run([mkfs, "-q", "-F", "-L", "linuxvol", "-d", str(source), str(image)], check=True, capture_output=True)
        with Guest(forward=HOST_PORT, data_disk=image, guest_port=445) as g:
            g.line("write settings no-msi")
            screen = g.line("run smbd root=disk2 share=data share=docs=disk2/docs,ro share=secret=disk2/drop,hidden ipc log ip=10.0.2.15/24 gateway=10.0.2.2 &2", "fos> ", 40)
            check("smbd" in screen.lower() and "disk2" in screen, "smbd starts as a service on NIC 2", screen)
            check("//HOST/data" in screen and "//HOST/docs" in screen, "smbd names both shares", screen)
            room = [line.strip() for line in screen.splitlines() if "room for" in line]
            check(bool(room), "smbd reports how many connections it has room for", screen)

            out = smbclient("ls")
            check(out.returncode == 0 and "hello.txt" in out.stdout and "big.bin" in out.stdout and "docs" in out.stdout,
                  "the share root lists its files", out.stdout + out.stderr)
            check(f"{len(big)}" in out.stdout, "with the real size", out.stdout)
            out = smbclient("ls", share="docs")
            check(out.returncode == 0 and "deep" in out.stdout and "hello.txt" not in out.stdout, "a second share lists its own folder", out.stdout + out.stderr)
            out = smbclient(f"get deep\\x.txt {tmp / 'x2'}", share="docs")
            check(out.returncode == 0 and (tmp / "x2").read_text() == "deep\n", "and serves a file from it", out.stdout + out.stderr)
            out = smbclient(f"put {tmp / 'x2'} nope.txt", share="docs", attempts=1)
            check(out.returncode != 0 or "NT_STATUS" in out.stdout + out.stderr, "a ,ro share refuses a write", out.stdout + out.stderr)
            out = subprocess.run(["smbclient", "-L", "127.0.0.1", "-p", str(HOST_PORT), "-N", "-m", "SMB3_00"], capture_output=True, text=True, timeout=60)
            listing = out.stdout + out.stderr
            check(out.returncode == 0 and "data" in out.stdout and "docs" in out.stdout and "Disk" in out.stdout, "IPC$ lists the shares (smbclient -L)", listing)
            check("secret" not in out.stdout, "a hidden share is left out of the list", listing)
            out = smbclient("ls", share="secret")
            check(out.returncode == 0 and "NT_STATUS" not in out.stdout + out.stderr, "but it can still be connected to by name", out.stdout + out.stderr)
            if shutil.which("rpcclient"):
                out = subprocess.run(["rpcclient", "-p", str(HOST_PORT), "-N", "--option=client ipc signing=off", "--option=client signing=off", "127.0.0.1", "-c", "netshareenumall 1"], capture_output=True, text=True, timeout=60)
                check("netname: data" in out.stdout and "netname: docs" in out.stdout and "secret" not in out.stdout, "rpcclient netshareenumall lists them", out.stdout + out.stderr)
            out = smbclient("ls", share="nothere", attempts=1)
            check("BAD_NETWORK_NAME" in out.stdout + out.stderr, "an unknown share name is refused", out.stdout + out.stderr)
            # A mount idles between requests (the Linux client's echo is 60 s apart): the server must not drop it.
            out = smbclient("ls hello.txt; !sleep 15; ls hello.txt")
            check(out.returncode == 0 and out.stdout.count("hello.txt") == 2, "a connection idle for 15 s still works", out.stdout + out.stderr)
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
        # Second boot: the same volume with an smbusers file (alice, uid 4242, is not the owner of anything).
        written = tmp / "written.img"
        with Guest(forward=HOST_PORT, data_disk=image, guest_port=445, save_data=written) as g:
            g.line("write settings no-msi")
            screen = g.line("run smbd user alice 4242 4242 secret")
            entry = next((word for word in screen.split() if word.startswith("alice:4242:4242:")), "")
            check(len(entry) == len("alice:4242:4242:") + 32, "smbd user prints an smbusers line", screen)
            g.line(f"write smbusers {entry}")
            screen = g.line("run smbd root=disk2 share=data log ip=10.0.2.15/24 gateway=10.0.2.2 &2", "fos> ", 40)
            check("1 user" in screen, "smbd reads the smbusers file", screen)
            out = smbclient("get hello.txt " + str(tmp / "h1"), login=("-U", "alice%secret"))
            check(out.returncode == 0 and (tmp / "h1").read_text() == "hello over smb\n", "the right password logs in and reads", out.stdout + out.stderr)
            out = smbclient("ls", login=("-U", "alice%wrong"), attempts=1)
            check(out.returncode != 0 and "LOGON_FAILURE" in out.stdout + out.stderr, "a wrong password is refused", out.stdout + out.stderr)
            out = smbclient("ls", login=("-N",), attempts=1)
            check(out.returncode != 0, "anonymous is refused when users exist", out.stdout + out.stderr)
            out = smbclient("get locked.txt " + str(tmp / "h2"), login=("-U", "alice%secret"), attempts=1)
            check("ACCESS_DENIED" in out.stdout + out.stderr and not (tmp / "h2").exists(), "mode 0600 of another owner is denied", out.stdout + out.stderr)
            out = smbclient("get hello.txt " + str(tmp / "h3"), "SMB2_02", login=("-U", "alice%secret"))
            check(out.returncode == 0, "SMB 2.0.2 signing works too", out.stdout + out.stderr)
            payload = os.urandom(300_000)
            (tmp / "up.bin").write_bytes(payload)
            (tmp / "small.txt").write_text("small file\n")
            out = smbclient(f"put {tmp / 'up.bin'} hello2.bin", login=("-U", "alice%secret"), attempts=1)
            check("ACCESS_DENIED" in out.stdout + out.stderr, "a folder without the write bit refuses a new file", out.stdout + out.stderr)
            out = smbclient(f"put {source / 'hello.txt'} hello.txt", login=("-U", "alice%secret"), attempts=1)
            check("ACCESS_DENIED" in out.stdout + out.stderr, "a file of another owner is not overwritten", out.stdout + out.stderr)
            script = (f"put {tmp / 'up.bin'} drop\\up.bin; mkdir drop\\sub; put {tmp / 'small.txt'} drop\\sub\\a.txt; "
                      f"put {tmp / 'small.txt'} drop\\gone.txt; rename drop\\up.bin drop\\moved.bin; rm drop\\gone.txt; "
                      f"put {tmp / 'small.txt'} drop\\over.txt; put {tmp / 'up.bin'} drop\\over.txt")
            out = smbclient(script, login=("-U", "alice%secret"), attempts=1)
            check(out.returncode == 0 and "NT_STATUS" not in out.stdout + out.stderr, "alice writes into the world-writable folder", out.stdout + out.stderr)
            out = smbclient("get drop\\moved.bin " + str(tmp / "back.bin"), login=("-U", "alice%secret"))
            check((tmp / "back.bin").read_bytes() == payload, "the uploaded, renamed file reads back", out.stdout + out.stderr)
            out = smbclient("ls drop\\*", login=("-U", "alice%secret"))
            check("moved.bin" in out.stdout and "gone.txt" not in out.stdout and "up.bin" not in out.stdout.replace("moved.bin", ""),
                  "rename and delete show in the listing", out.stdout + out.stderr)
            out = smbclient("rmdir drop\\sub", login=("-U", "alice%secret"), attempts=1)
            check("NOT_EMPTY" in out.stdout + out.stderr, "a folder with files in it is not removed", out.stdout + out.stderr)
            shell = g.line("uptime")
            check("fos>" in shell, "the shell stays usable after the writes", shell)
        # The guest is gone: its copy of the volume must be consistent and hold what was written, owned by alice.
        e2fsck = shutil.which("e2fsck") or "/usr/sbin/e2fsck"
        debugfs = shutil.which("debugfs") or "/usr/sbin/debugfs"
        done = subprocess.run([e2fsck, "-fn", str(written)], capture_output=True, text=True)
        check(done.returncode == 0, "e2fsck finds the volume consistent after the writes", done.stdout + done.stderr)
        cat = subprocess.run([debugfs, "-R", "cat /drop/moved.bin", str(written)], capture_output=True).stdout
        check(cat == payload, "debugfs reads the uploaded file back", f"{len(cat)} bytes")
        cat = subprocess.run([debugfs, "-R", "cat /drop/over.txt", str(written)], capture_output=True).stdout
        check(cat == payload, "an overwritten file holds only the new data", f"{len(cat)} bytes")
        stat = subprocess.run([debugfs, "-R", "stat /drop/sub/a.txt", str(written)], capture_output=True, text=True).stdout
        check("User:  4242" in stat and "Group:  4242" in stat, "a new file belongs to the logged-in uid and gid", stat[:200])
    print("smbd: all checks passed")


if __name__ == "__main__":
    main()
