#!/usr/bin/env python3
"""`kernel use` switches between the two kernels kept on the boot volume (`make test-mf-kernel-switch`): the image boots the
server kernel with `kernel.embedded` and `kernel.mf` on its FOSFS volume. `kernel use embedded` writes the inactive slot and
restarts; the embedded kernel must come up (its `version` is not the server kernel's), `kernel use mf` takes it back, and a
file that is not a kernel is refused with nothing written. QEMU runs without -no-reboot and on a copy of the image."""
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF", ROOT / "build" / "fosfor-mf" / "fosfor-kswitch.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
failures = []


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}: {detail}")


def main():
    with tempfile.TemporaryDirectory() as tmp:
        disk = Path(tmp) / "disk.img"
        shutil.copy(IMAGE, disk)
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
             "-serial", "stdio", "-monitor", "none",
             "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={disk}"],
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

        def command(line, expect, timeout=60):
            nonlocal log
            log = b""
            qemu.stdin.write(line.encode() + b"\n")
            qemu.stdin.flush()
            ok = wait_for(expect, timeout)
            time.sleep(0.3)
            wait_for("fos> ", 10)
            return ok, log.decode("utf8", "replace")

        def reboot_into(line, expect):
            nonlocal log
            log = b""
            qemu.stdin.write(line.encode() + b"\n")
            qemu.stdin.flush()
            ok = wait_for("restarting to try it", 60) and wait_for("fos> ", 180)
            time.sleep(0.5)
            return ok, log.decode("utf8", "replace")

        try:
            if not wait_for("fos> ", 150):
                failures.append("no shell prompt:\n" + log[-800:].decode("utf8", "replace"))
                return finish()
            ok, text = command("kernel", "kernel.mf")
            check("Running: the mf kernel" in text and "kernel.embedded:" in text and "bytes (running)" in text, "kernel lists both files and the running one", text[-400:])
            ok, text = command("kernel use mf", "already running")
            check(ok, "kernel use of the running kernel does nothing", text[-300:])
            ok, text = reboot_into("kernel use embedded", "")
            check(ok, "kernel use embedded restarts and a shell comes back", text[-500:])
            ok, text = command("version", "Fosfor OS")
            check(ok and "server kernel" not in text, "the embedded kernel is running", text[-300:])
            ok, text = command("kernel", "kernel.mf")
            check("Running: the embedded kernel" in text, "kernel says the embedded kernel runs", text[-300:])
            ok, text = reboot_into("kernel use mf", "")
            check(ok, "kernel use mf restarts and a shell comes back", text[-500:])
            ok, text = command("version", "Fosfor OS")
            check(ok and "server kernel (mf)" in text, "the server kernel is back", text[-300:])
            ok, text = command("write kernel.embedded not-a-kernel", "fos")
            ok, text = command("kernel use embedded", "nothing was written")
            check(ok and "not a kernel" in text, "a file that is not a kernel is refused", text[-300:])
            ok, text = command("kernel use nosuch", "Usage")
            check(ok, "an unknown kernel name shows the usage", text[-300:])
        finally:
            qemu.kill()
    return finish()


def finish():
    if failures:
        print("\n".join(failures))
        return 1
    print("ALL OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
