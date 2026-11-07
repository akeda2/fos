#!/usr/bin/env python3
"""`sshd` as a background tape on the server kernel (`make test-mf-sshd`): the real OpenSSH `sftp` on the host, through a
forwarded port, logs in with a key, lists and fetches a file and writes one back; a key that is not in authorized_keys is
refused, and `help sshd` answers without running the tape. A `shell` account gets the kernel's shell over a shell channel
(its own session: `cd`, a command's output, `exit` ends it); an account without `shell` is refused one."""
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_SSHD", ROOT / "build" / "fosfor-mf" / "fosfor-sshd.img"))
KEY = Path(os.environ.get("FOSFOR_SSH_KEY", ROOT / "build" / "fosfor-mf" / "sshkey"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
HOST_PORT = port(28022)
OPTIONS = ["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "IdentitiesOnly=yes",
           "-o", "BatchMode=yes", "-P", str(HOST_PORT)]


def sftp(key, script):
    return subprocess.run(["sftp", *OPTIONS, "-i", str(key), "anna@127.0.0.1"], input=script, capture_output=True,
                          text=True, timeout=120)


def main():
    if not all(shutil.which(tool) for tool in ("sftp", "ssh-keygen", "mkfs.ext4")):
        print("SKIP: the OpenSSH client or mkfs.ext4 is not installed")
        return 0
    # Writes need an ext volume: the files are served from disk2, a fresh ext4 made by Linux's mkfs.
    scratch = tempfile.TemporaryDirectory()
    ext = Path(scratch.name) / "ext.img"
    ext.write_bytes(bytes(64 << 20))
    subprocess.run(["mkfs.ext4", "-q", "-F", "-L", "sshdtest", str(ext)], check=True, capture_output=True)
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
         "-nic", f"user,model=e1000,hostfwd=tcp::{HOST_PORT}-:22", "-serial", "stdio", "-monitor", "none", "-no-reboot",
         "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={IMAGE},snapshot=on", "-drive", f"format=raw,file={ext}"],
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

    def send(line):
        qemu.stdin.write(line.encode() + b"\n")
        qemu.stdin.flush()

    failures = []

    def check(ok, what, detail=""):
        print(("PASS: " if ok else "FAIL: ") + what)
        if not ok:
            failures.append(what + (": " + detail if detail else ""))

    try:
        if not wait_for("fos> ", 150):
            failures.append("no shell prompt")
            return 1
        send("help sshd")
        check(wait_for("public-key logins only", 10) or wait_for("Public-key logins only", 1), "help sshd answers")
        send("run sshd @sshd.cfg &1")
        if not wait_for("serving disk2", 40):
            failures.append("sshd did not start:\n" + log[-800:].decode("utf8", "replace"))
            return 1
        check(wait_for("SHA256:", 1), "the host key fingerprint is shown")
        with tempfile.TemporaryDirectory() as tmp:
            payload = Path(tmp) / "payload"
            payload.write_bytes(os.urandom(300_000))
            result = sftp(KEY, f"ls\nmkdir sub\nrename sub moved\nls moved\nput {payload} payload.up\nget payload.up {tmp}/payload.back\nbye\n")
            check("lost+found" in result.stdout and "moved" in result.stdout, "sftp lists the volume, makes and renames a folder", result.stdout + result.stderr)
            wrote = (Path(tmp) / "payload.back").exists() and (Path(tmp) / "payload.back").read_bytes() == payload.read_bytes()
            check(wrote, "a 300 kB file written and read back is intact", result.stdout + result.stderr)
            def shell(user, script):
                return subprocess.run(["ssh", "-T", *[o if o != "-P" else "-p" for o in OPTIONS], "-i", str(KEY), f"{user}@127.0.0.1"],
                                      input=script, capture_output=True, text=True, timeout=60)
            result = shell("admin", "echo hi-from-ssh\ncd esp\npwd\nexit\n")
            check("hi-from-ssh" in result.stdout and "esp" in result.stdout, "ssh gives a shell: commands run and answer", result.stdout + result.stderr)
            raw = subprocess.run(["ssh", "-T", *[o if o != "-P" else "-p" for o in OPTIONS], "-i", str(KEY), "admin@127.0.0.1"],
                                 input=b"echo one\necho two\nexit\n", capture_output=True, timeout=60).stdout
            check(b"\r\n" in raw and b"\n" not in raw.replace(b"\r\n", b""), "shell output ends its lines with CR LF", repr(raw))
            check(result.returncode == 0, "and `exit` ends the session cleanly", f"{result.returncode} {result.stderr}")
            # Ctrl-D on an empty line ends the session although the client keeps its input open.
            eof = subprocess.Popen(["ssh", "-T", *[o if o != "-P" else "-p" for o in OPTIONS], "-i", str(KEY), "admin@127.0.0.1"],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            eof.stdin.write(b"echo before-eof\n"); eof.stdin.flush(); time.sleep(2)
            eof.stdin.write(b"\x04"); eof.stdin.flush()
            try:
                eof.wait(timeout=20)
                ended = True
            except subprocess.TimeoutExpired:
                eof.kill()
                ended = False
            check(ended, "Ctrl-D on an empty line ends an ssh shell session", "the session stayed open")
            second = shell("admin", "pwd\nexit\n")
            check("(the boot volume)" in second.stdout, "a second login starts at the root with its own folder", second.stdout + second.stderr)
            denied = shell("anna", "echo should-not-run\nexit\n")
            check("should-not-run" not in denied.stdout and denied.returncode != 0, "an account without `shell` gets no shell channel", denied.stdout + denied.stderr)
            bad = Path(tmp) / "bad"
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(bad)], check=True)
            refused = sftp(bad, "ls\nbye\n")
            check(refused.returncode != 0 and "lost+found" not in refused.stdout, "a key that is not authorized is refused")
        if b"CPU exception" in log:
            failures.append("the kernel faulted:\n" + log[-1200:].decode("utf8", "replace"))
    finally:
        qemu.kill()
        scratch.cleanup()
    for failure in failures:
        print("FAIL: " + failure)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
