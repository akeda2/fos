#!/usr/bin/env python3
"""`smbd` as a background tape on the server kernel (`make test-mf-smbd`): a real client (smbclient on the host, through a
forwarded port) connects, which makes smbd ask the kernel for random bytes (the SMB challenge). The kernel once wrote those
straight into user pages, which faults under SMAP: rack hard-reset on the first connection."""
import os
import select
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_SMBD", ROOT / "build" / "fosfor-mf" / "fosfor-smbd.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
HOST_PORT = port(28445)


def main():
    if not shutil.which("smbclient"):
        print("SKIP: smbclient is not installed")
        return 0
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
         "-nic", f"user,model=e1000,hostfwd=tcp::{HOST_PORT}-:445", "-serial", "stdio", "-monitor", "none", "-no-reboot",
         "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={IMAGE},snapshot=on"],
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

    failures = []
    try:
        if not wait_for("fos> ", 150):
            failures.append("no shell prompt")
        else:
            qemu.stdin.write(b"run smbd @smbd.cfg &1\n")
            qemu.stdin.flush()
            if not wait_for("sharing disk1", 30):
                failures.append("smbd did not start")
            else:
                listing = subprocess.run(["smbclient", "//127.0.0.1/data", "-p", str(HOST_PORT), "-N", "-c", "ls"],
                                         capture_output=True, text=True, timeout=60)
                print(("PASS: " if "readme.txt" in listing.stdout else "FAIL: ") + "smbclient lists the share")
                if "readme.txt" not in listing.stdout:
                    failures.append("smbclient did not list the share:\n" + listing.stdout + listing.stderr)
                if b"CPU exception" in log:
                    failures.append("the kernel faulted:\n" + log[-1200:].decode("utf8", "replace"))
    finally:
        qemu.kill()
    for failure in failures:
        print("FAIL: " + failure)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
