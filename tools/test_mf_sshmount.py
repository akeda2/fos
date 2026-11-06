#!/usr/bin/env python3
"""`sshmount` as a background tape on the server kernel (`make test-mf-sshmount`): `sshmount keygen` makes the guest's key, the
host's `crates/ssh/examples/serve` (the same protocol code as sshd, on a host folder) is told to accept it, and `tape sshmount
tester@10.0.2.100 as=host &1` mounts that folder as //host (reached through QEMU guestfwd). The shell then lists, reads, writes,
renames and removes through the mount, and the host folder shows the result; the server's key is remembered in known_hosts
(trust on first use). A changed host key is refused: the second server runs with another key."""
import os
import re
import select
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_SSHMOUNT", ROOT / "build" / "fosfor-mf" / "fosfor-sshmount.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
SERVER_PORT = port(28522)
failures = []


def check(ok, what, detail=""):
    print(("PASS: " if ok else "FAIL: ") + what)
    if not ok:
        failures.append(what + "\n" + detail)


def main():
    built = subprocess.run(["cargo", "build", "--release", "--example", "serve", "--target-dir", "target"],
                           cwd=ROOT / "crates" / "ssh", capture_output=True, text=True)
    if built.returncode != 0:
        raise SystemExit("FAIL: cannot build the host server\n" + built.stderr[-1500:])
    binary = ROOT / "crates" / "ssh" / "target" / "release" / "examples" / "serve"
    with tempfile.TemporaryDirectory() as tmp:
        share = Path(tmp) / "share"
        share.mkdir()
        (share / "hello.txt").write_text("hello from the host\n")
        (share / "folder").mkdir()
        (share / "folder" / "inner.txt").write_text("inside\n")
        keys = Path(tmp) / "authorized_keys"
        server = None
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
             "-nic", f"user,model=e1000,guestfwd=tcp:10.0.2.100:22-cmd:nc 127.0.0.1 {SERVER_PORT}",
             "-serial", "stdio", "-monitor", "none", "-no-reboot",
             "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={IMAGE},snapshot=on"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        log = b""

        def wait_for(text, timeout, start=0):
            nonlocal log
            end = time.time() + timeout
            while time.time() < end:
                if text.encode() in log[start:]:
                    return True
                if select.select([qemu.stdout], [], [], 0.2)[0]:
                    log += os.read(qemu.stdout.fileno(), 4096)
            return text.encode() in log[start:]

        def line(command, expect="fos> ", timeout=40):
            mark = len(log)
            qemu.stdin.write((command + "\n").encode())
            qemu.stdin.flush()
            wait_for(expect, timeout, mark)
            time.sleep(0.3)
            wait_for("\x00", 0.2, mark)
            return log[mark:].decode("utf8", "replace")

        try:
            check(wait_for("fos> ", 150), "the shell prompt", log[-600:].decode("utf8", "replace"))
            out = line("tape sshmount USER@HOST", "fos> ", 20)
            check("no settings/ssh_key" in out, "a mount without a key says what to do", out)
            out = line("tape sshmount keygen")
            found = re.search(r"(ssh-ed25519 [A-Za-z0-9+/=]+)", out)
            check(found is not None, "keygen prints the public key line", out)
            if not found:
                raise SystemExit("\n".join(failures))
            keys.write_text(f"user=tester {found.group(1)}\n")
            out = line("tape sshmount keygen")
            check(found.group(1) in out, "the key is kept: keygen prints the same line again", out)

            server = subprocess.Popen([str(binary), str(SERVER_PORT), str(share), str(keys)], stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL)
            time.sleep(0.5)
            out = line("tape sshmount tester@10.0.2.100 as=host &1", "background as process", 30)
            check("in the background as process" in out, "run ... &1 starts a background process", out)
            pid = re.search(r"as process (\d+)", out)
            out = line("ls //host", "hello.txt", 60)
            check("hello.txt" in out and "folder" in out, "ls //host lists the folder", out)
            out = line("cat //host/hello.txt", "hello from the host", 60)
            check("hello from the host" in out, "cat reads a file through the mount", out)
            out = line("cat //host/folder/inner.txt", "inside", 60)
            check("inside" in out, "a file in a folder reads", out)
            out = line("cp //host/hello.txt got.txt", "fos> ", 60)
            out = line("cp got.txt //host/up.txt", "fos> ", 60)
            time.sleep(1)
            check((share / "up.txt").exists() and (share / "up.txt").read_text() == "hello from the host\n",
                  "cp to the mount arrives on the host", out)
            out = line("mkdir //host/made", "fos> ", 60)
            out = line("mv //host/up.txt //host/made/moved.txt", "fos> ", 60)
            time.sleep(1)
            check((share / "made" / "moved.txt").exists() and not (share / "up.txt").exists(), "mkdir and mv work", out)
            out = line("rm //host/made/moved.txt", "fos> ", 60)
            out = line("rm //host/made", "fos> ", 60)
            time.sleep(1)
            check(not (share / "made").exists(), "rm removes a file and then its folder", out)
            out = line("cat //host/nothing", "fos> ", 60)
            check("hello" not in out and "fos> " in out, "a missing file is an error, not a hang", out)
            if pid:
                line(f"kill {pid.group(1)}")
                time.sleep(2)
                out = line("threads")
                check("sshmount" not in out, "kill stops the service", out)

            hosts = line("cat known_hosts")
            check("10.0.2.100 SHA256:" in hosts, "the host key was remembered in known_hosts", hosts)

            # Another host key behind the same name is refused.
            server.terminate()
            server.wait()
            server = subprocess.Popen([str(binary), str(SERVER_PORT), str(share), str(keys)], stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL, env={**os.environ, "SERVE_SEED": "43"})
            time.sleep(0.5)
            out = line("tape sshmount tester@10.0.2.100 as=other &1", "fos> ", 40)
            time.sleep(8)
            out += line("")
            check("CHANGED" in out, "a changed host key is refused", out)
        finally:
            qemu.kill()
            if server:
                server.terminate()
    if failures:
        raise SystemExit("\n".join(failures))
    print("mf sshmount: all checks passed")


if __name__ == "__main__":
    main()
