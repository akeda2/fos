#!/usr/bin/env python3
"""The `ssh` tape on the server kernel (`make test-mf-ssh`): `ssh keygen` makes the guest's key, a real OpenSSH `sshd` on the
host (run as the current user on a loopback port, reached through QEMU guestfwd) is told to accept it, and the tape then runs
a command (output and exit status), opens a shell on the console (a typed command runs, `exit` returns to the fos prompt),
and remembers the server's key in known_hosts. A changed host key is refused. Skipped without /usr/sbin/sshd."""
import getpass
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
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_SSH", ROOT / "build" / "fosfor-mf" / "fosfor-ssh.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
SSHD = "/usr/sbin/sshd"
SERVER_PORT = port(28524)
failures = []


def check(ok, what, detail=""):
    print(("PASS: " if ok else "FAIL: ") + what)
    if not ok:
        failures.append(what + "\n" + detail)


def start_sshd(tmp, name, keys):
    host_key = Path(tmp) / name
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(host_key)], check=True)
    config = Path(tmp) / (name + ".conf")
    config.write_text(f"Port {SERVER_PORT}\nListenAddress 127.0.0.1\nHostKey {host_key}\nAuthorizedKeysFile {keys}\n"
                      f"PidFile {tmp}/{name}.pid\nUsePAM no\nStrictModes no\nPasswordAuthentication no\n")
    server = subprocess.Popen([SSHD, "-D", "-f", str(config)], stderr=subprocess.DEVNULL)
    time.sleep(1)
    return server


def main():
    if not Path(SSHD).exists():
        print("skipped: no OpenSSH sshd on this host")
        return
    user = getpass.getuser()
    with tempfile.TemporaryDirectory() as tmp:
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
            out = line(f"tape ssh {user}@10.0.2.100 true", "fos> ", 20)
            check("no settings/ssh_key" in out, "a login without a key says what to do", out)
            out = line("tape ssh keygen")
            found = re.search(r"(ssh-ed25519 [A-Za-z0-9+/=]+)", out)
            check(found is not None, "keygen prints the public key line", out)
            if not found:
                raise SystemExit("\n".join(failures))
            keys.write_text(found.group(1) + "\n")
            server = start_sshd(tmp, "host1", keys)

            out = line(f"tape ssh {user}@10.0.2.100 echo ran-$((6*7))", "fos> ", 60)
            check("ran-42" in out, "a command runs and its output comes back", out)
            out = line(f"tape ssh {user}@10.0.2.100 exit 3", "fos> ", 60)
            check("exit status 3" in out, "the exit status is reported", out)
            hosts = line("cat known_hosts")
            check("10.0.2.100 SHA256:" in hosts, "the host key was remembered in known_hosts", hosts)

            # The sshd of this test cannot verify a password (no PAM): what is checked is the prompt, that the typed
            # password is not echoed, and that a refusal ends the tape cleanly.
            mark = len(log)
            qemu.stdin.write(f"tape ssh password {user}@10.0.2.100 true\n".encode())
            qemu.stdin.flush()
            check(wait_for("password: ", 30, mark), "password login asks for the password", log[mark:].decode("utf8", "replace"))
            out = line("hunter2zz", "fos> ", 60)
            check("hunter2zz" not in out and "did not accept the login" in out, "the password is not echoed; a refusal is reported", out)

            mark = len(log)
            qemu.stdin.write(f"tape ssh {user}@10.0.2.100\n".encode())
            qemu.stdin.flush()
            time.sleep(6)
            out = line("echo shell-$((20+22))", "shell-42", 30)
            check("shell-42" in out, "a typed command runs in the shell session", out)
            out = line("exit", "fos> ", 30)
            check("fos> " in out, "exit returns to the fos prompt", log[mark:].decode("utf8", "replace")[-800:])

            # Another host key behind the same name is refused.
            server.terminate()
            server.wait()
            server = start_sshd(tmp, "host2", keys)
            out = line(f"tape ssh {user}@10.0.2.100 true", "fos> ", 60)
            check("CHANGED" in out and "ran-" not in out, "a changed host key is refused", out)
        finally:
            qemu.kill()
            if server:
                server.terminate()
    if failures:
        raise SystemExit("\n".join(failures))
    print("mf ssh: all checks passed")


if __name__ == "__main__":
    main()
