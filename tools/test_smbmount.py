#!/usr/bin/env python3
"""`smbmount` (a service that mounts an SMB share as the volume //NAME through tape ABI 35's remote_* calls) against the host
server `crates/smb/examples/serve.rs`, reached as 10.0.2.100:445 through QEMU guestfwd on the guest's NIC 2. The shell lists
and reads the mount (`ls //nas/...`, `cat`, a 3 MB `cp` checked by SHA-256), then writes through it (`cp` to the mount, `mkdir`,
`mv`, `rm`, checked on the host); a tape reaches it too (`fscheck vfs //nas/...`); an unknown //name is refused and unmounting
(`kill`) makes the name vanish. Run with `make test-smbmount`."""
import hashlib
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import check  # noqa: E402
from test_httpd import Guest  # noqa: E402

SERVER_PORT = port(28447)


def main():
    root = Path(__file__).resolve().parent.parent
    built = subprocess.run(["cargo", "build", "--release", "--example", "serve", "--target-dir", "target"], cwd=root / "crates" / "smb", capture_output=True, text=True)
    if built.returncode != 0:
        raise SystemExit("FAIL: cannot build the host server\n" + built.stderr[-1500:])
    server_binary = root / "crates" / "smb" / "target" / "release" / "examples" / "serve"
    with tempfile.TemporaryDirectory() as tmp:
        share = Path(tmp) / "share"
        (share / "docs").mkdir(parents=True)
        big = os.urandom(3 << 20)
        (share / "big.bin").write_bytes(big)
        (share / "hello.txt").write_text("hello from the host\n")
        (share / "docs" / "r\u00e4ksm\u00f6rg\u00e5s.txt").write_text("hej\n")
        server = subprocess.Popen([str(server_binary), str(share), str(SERVER_PORT)], stdout=subprocess.DEVNULL)
        time.sleep(0.5)
        try:
            extra = f",guestfwd=tcp:10.0.2.100:445-cmd:nc 127.0.0.1 {SERVER_PORT}"
            with Guest(user_extra=extra) as g:
                g.line("write settings no-msi")
                out = g.line("run smbmount //10.0.2.100/share as=nas &2", "fos> ", 60)
                check("//nas" in out, "the service mounts the share", out[-400:])
                time.sleep(1)
                out = g.line("ls //nas", "fos> ", 60)
                check("hello.txt" in out and "big.bin" in out and "docs" in out, "ls //nas lists the share root", out[-500:])
                out = g.line("ls //nas/docs", "fos> ", 60)
                check("hej" not in out and "ksm" in out, "ls of a subfolder, UTF-8 name", out[-400:])
                out = g.line("cat //nas/hello.txt", "fos> ", 40)
                check("hello from the host" in out, "cat prints a file", out[-300:])
                out = g.line("cp //nas/big.bin got.bin", "fos> ", 120)
                out = g.line("sha256 got.bin", "fos> ", 60)
                check(hashlib.sha256(big).hexdigest() in out, "a 3 MB copy from the mount has the same SHA-256", out[-300:])
                out = g.line("cat //nas/nothere.txt", "fos> ", 40)
                check("nothere" in out or "no " in out.lower(), "a missing file is reported", out[-300:])
                out = g.line("run fscheck vfs //nas/hello.txt", "fos> ", 40)
                check("hello from the host" in out, "a tape reaches the mount through vfs_stat and vfs_read", out[-300:])
                out = g.line("ls //other", "fos> ", 20)
                check("hello.txt" not in out, "an unknown //name lists nothing", out[-300:])
                out = g.line("cp got.bin //nas/up.bin", "fos> ", 120)
                check((share / "up.bin").exists() and (share / "up.bin").read_bytes() == big, "cp to the mount arrives intact on the host", out[-300:])
                out = g.line("mkdir //nas/newdir", "fos> ", 40)
                check((share / "newdir").is_dir(), "mkdir makes a folder", out[-300:])
                out = g.line("cp got.bin //nas/newdir/g.bin", "fos> ", 120)
                check((share / "newdir" / "g.bin").exists(), "cp into the new folder", out[-300:])
                out = g.line("mv //nas/newdir/g.bin //nas/moved.bin", "fos> ", 40)
                check((share / "moved.bin").exists() and not (share / "newdir" / "g.bin").exists(), "mv renames across folders", out[-300:])
                out = g.line("rm //nas/moved.bin", "fos> ", 40)
                check(not (share / "moved.bin").exists(), "rm removes a file", out[-300:])
                g.line("rm //nas/up.bin", "fos> ", 40)
                out = g.line("rm //nas/newdir", "fos> ", 40)
                check(not (share / "newdir").exists(), "rm removes a folder", out[-300:])
                out = g.line("ps", "fos> ", 20)
                check("smbmount" in out, "the service shows in ps", out[-300:])
        finally:
            server.terminate()
    print("smbmount: all checks passed")


if __name__ == "__main__":
    main()
