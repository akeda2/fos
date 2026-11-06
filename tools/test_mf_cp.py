#!/usr/bin/env python3
"""`cp`, `mv` and `cmp` on the server kernel have no size limit (`make test-mf-cp`): a 40 MiB random file on an ext4 disk (over
the 32 MiB the embedded kernel loads whole) is copied, compared and moved within the volume, and the SHA-256 the guest
prints for the copy must equal the host's. Also `mount PATH @LABEL` of an ext volume and `cd`/`pwd`."""
import hashlib
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF", ROOT / "build" / "fosfor-mf" / "fosfor.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
failures = []


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}: {detail}")


def main():
    mkfs = shutil.which("mkfs.ext4") or "/usr/sbin/mkfs.ext4"
    if not Path(mkfs).exists():
        print("SKIP: no mkfs.ext4")
        return 0
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp / "src").mkdir()
        blob = os.urandom(40 << 20 | 4321)
        (tmp / "src" / "big.bin").write_bytes(blob)
        digest = hashlib.sha256(blob).hexdigest()
        disk = tmp / "ext.img"
        disk.write_bytes(bytes(256 << 20))
        subprocess.run([mkfs, "-q", "-F", "-L", "linuxvol", "-d", str(tmp / "src"), str(disk)], check=True, capture_output=True)
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
             "-serial", "stdio", "-monitor", "none", "-no-reboot",
             "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={IMAGE},snapshot=on",
             "-drive", f"format=raw,file={disk},snapshot=on"],
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

        def command(line, expect, timeout=120):
            nonlocal log
            log = b""
            qemu.stdin.write(line.encode() + b"\n")
            qemu.stdin.flush()
            ok = wait_for(expect, timeout)
            time.sleep(0.3)
            wait_for("> ", 10)
            return ok, log.decode("utf8", "replace")

        try:
            if not wait_for("fos> ", 150):
                failures.append("no shell prompt:\n" + log[-800:].decode("utf8", "replace"))
                return finish()
            ok, text = command("cp @linuxvol/big.bin @linuxvol/copy.bin", "Copied")
            check(ok and "the limit" not in text, "cp of a file over 32 MiB", text[-300:])
            ok, text = command("cmp @linuxvol/big.bin @linuxvol/copy.bin", "cmp:")
            check("identical" in text, "cmp says the copy is identical", text[-300:])
            ok, text = command("sha256 @linuxvol/copy.bin", digest, 120)
            check(ok, "the copy's SHA-256 matches the host's", text[-300:])
            ok, text = command("mv @linuxvol/copy.bin @linuxvol/moved.bin", "Moved")
            check(ok, "mv inside the volume", text[-300:])
            ok, text = command("ls @linuxvol", "moved.bin")
            check(ok and "copy.bin" not in text, "the moved file is there and the old name gone", text[-300:])
            # A folder of the root mounted from the ext volume, and a working folder on it.
            ok, text = command("mount data @linuxvol", "mount", 10)
            ok, text = command("ls data", "moved.bin")
            check(ok, "ls of a mount point backed by an ext volume", text[-300:])
            ok, text = command("mkdir data/sub", "fos", 20)
            ok, text = command("cd data/sub", "fos data/sub>", 20)
            check(ok, "cd into a folder under the mount point shows it in the prompt", text[-300:])
            ok, text = command("write note.txt hello", "fos", 20)
            ok, text = command("cat /sub/note.txt", "hello", 20)
            check(ok, "a relative write lands in the working folder; / is the volume's root", text[-300:])
            ok, text = command("write zebra.txt quagga42", "fos", 20)
            ok, text = command("ebb zebra.txt", "quagga42", 20)
            qemu.stdin.write(b"\x11")
            qemu.stdin.flush()
            wait_for("fos data/sub>", 10)
            check(ok, "a tape that takes a file name opens it under the working folder (resolve_path, ABI 39)", text[-300:])
            ok, text = command("cd ../..", "fos", 20)
            ok, text = command("pwd", "boot volume", 20)
            check(ok, "cd ../.. returns to the boot volume", text[-300:])
            ok, text = command("cd data", "fos data>", 20)
            ok, text = command("cd sub", "fos data/sub>", 20)
            check(ok, "cd into a subfolder of the working folder (a name relative to it)", text[-300:])
            ok, text = command("cd ../..", "fos", 20)
            ok, text = command("cd nosuch", "no such folder", 20)
            check(ok, "cd to a missing folder fails", text[-300:])
        finally:
            qemu.kill()
    return finish()


def finish():
    for failure in failures:
        print("FAIL:", failure)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
