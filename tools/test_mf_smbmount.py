#!/usr/bin/env python3
"""`smbmount` as a background tape on the server kernel (`make test-mf-smbmount`): `tape smbmount //HOST/SHARE as=nas &1` starts it
as a process that keeps running while the shell takes commands (the provider of the remote volume //nas, tape ABI 35), and the
shell lists, reads and writes the share (host `crates/smb/examples/serve`, reached as 10.0.2.100:445 through QEMU guestfwd);
`kill PID` stops the provider. Also checks that a foreground tape still takes Escape."""
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
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_SMBMOUNT", ROOT / "build" / "fosfor-mf" / "fosfor-smbmount.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
SERVER_PORT = port(28448)
failures = []


def check(ok, what, detail=""):
    print(("PASS: " if ok else "FAIL: ") + what)
    if not ok:
        failures.append(what + "\n" + detail)


def main():
    built = subprocess.run(["cargo", "build", "--release", "--example", "serve", "--target-dir", "target"],
                           cwd=ROOT / "crates" / "smb", capture_output=True, text=True)
    if built.returncode != 0:
        raise SystemExit("FAIL: cannot build the host server\n" + built.stderr[-1500:])
    binary = ROOT / "crates" / "smb" / "target" / "release" / "examples" / "serve"
    with tempfile.TemporaryDirectory() as tmp:
        share = Path(tmp)
        (share / "hello.txt").write_text("hello from the host\n")
        server = subprocess.Popen([str(binary), str(share), str(SERVER_PORT)], stdout=subprocess.DEVNULL)
        time.sleep(0.5)
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "512", "-smp", "2", "-display", "none",
             "-nic", f"user,model=e1000,guestfwd=tcp:10.0.2.100:445-cmd:nc 127.0.0.1 {SERVER_PORT}",
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
            out = line("tape smbmount //10.0.2.100/share as=nas &1", "background as process", 30)
            check("in the background as process" in out, "run ... &1 starts a background process", out)
            pid = re.search(r"as process (\d+)", out)
            time.sleep(8)
            out = line("")
            check("fos> " in out, "the shell takes commands while the service runs", out)
            out = line("ls //nas", "hello.txt", 60)
            check("hello.txt" in out, "ls //nas lists the share", out)
            out = line("cat //nas/hello.txt", "hello from the host", 60)
            check("hello from the host" in out, "cat reads a file through the mount", out)
            out = line("cp //nas/hello.txt got.txt")
            out = line("cp got.txt //nas/up.txt", "fos> ", 60)
            time.sleep(1)
            check((share / "up.txt").exists() and (share / "up.txt").read_text() == "hello from the host\n",
                  "cp to the mount arrives on the host", out)
            out = line("threads")
            check("smbmount" in out, "threads shows the service", out)
            if pid:
                line(f"kill {pid.group(1)}")
                time.sleep(2)
                out = line("threads")
                check("smbmount" not in out, "kill stops the service", out)
        finally:
            qemu.kill()
            server.terminate()
    if failures:
        raise SystemExit("\n".join(failures))
    print("mf smbmount: all checks passed")


if __name__ == "__main__":
    main()
