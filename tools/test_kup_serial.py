#!/usr/bin/env python3
"""Serial KUP test: `kup serial` takes a kernel over the console UART (tools/send_kernel_serial.py's protocol) into the
inactive A/B slot and resets; the loader tries it once and the kernel confirms it. `test_kup_serial.py [aarch64|x86]`:
aarch64 needs qemu-system-aarch64 and AAVMF like test_aarch64_boot.py, x86 qemu-system-x86_64 and OVMF.
Run `make test-aarch64-kup` or `make test-kup-serial`."""
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import send_kernel_serial as sender  # noqa: E402
from test_aarch64_boot import CODE, IMAGE, ROOT, VARS, check  # noqa: E402

ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
if ARCH == "aarch64":
    KERNEL = Path(os.environ.get("FOSFOR_AARCH64_KERNEL", ROOT / "build/cargo-target/aarch64-unknown-none-softfloat/release/fosfor-kernel"))
    SLOTS = "slots"
    MACHINE = ["qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72", "-m", "1G", "-device", "ramfb"]
    DISK = ["-device", "virtio-blk-pci,drive=boot"]
else:
    CODE = os.environ.get("OVMF_CODE", "/usr/share/OVMF/OVMF_CODE_4M.fd")
    VARS = os.environ.get("OVMF_VARS", "/usr/share/OVMF/OVMF_VARS_4M.fd")
    IMAGE = Path(os.environ.get("FOSFOR_IMAGE", ROOT / "build/fosfor/fosfor.img"))
    KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
    SLOTS = "debug slots"
    MACHINE = ["qemu-system-x86_64", "-machine", "q35", "-m", "512"]
    DISK = ["-device", "ide-hd,drive=boot"]


def main():
    if not shutil.which(MACHINE[0]) or not Path(CODE).exists():
        raise SystemExit("qemu-system-aarch64 or the AAVMF firmware is missing")
    with tempfile.TemporaryDirectory() as scratch:
        image, variables = Path(scratch) / "disk.img", Path(scratch) / "vars.fd"
        shutil.copyfile(IMAGE, image)
        shutil.copyfile(VARS, variables)
        # No -no-reboot and no snapshot: the reset must restart the guest and the slot write must persist.
        qemu = subprocess.Popen(
            [*MACHINE, "-display", "none", "-serial", "stdio", "-monitor", "none",
             "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"if=pflash,format=raw,file={variables}",
             "-drive", f"file={image},format=raw,if=none,id=boot", *DISK],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        received = bytearray()
        lock = threading.Lock()

        def pump():
            while True:
                data = os.read(qemu.stdout.fileno(), 4096)
                if not data:
                    return
                with lock:
                    received.extend(data)

        threading.Thread(target=pump, daemon=True).start()
        consumed = 0

        def read(timeout: float) -> bytes:
            nonlocal consumed
            end = time.time() + timeout
            while time.time() < end:
                with lock:
                    if len(received) > consumed:
                        data = bytes(received[consumed:])
                        consumed = len(received)
                        return data
                time.sleep(0.01)
            return b""

        def write(data: bytes) -> None:
            qemu.stdin.write(data)
            qemu.stdin.flush()

        def text() -> str:
            with lock:
                return bytes(received).decode("utf-8", "replace").replace("\r", "")

        def wait_for(marker: str, seconds: float = 60, after: int = 0) -> bool:
            end = time.time() + seconds
            while time.time() < end:
                if marker in text()[after:]:
                    return True
                time.sleep(0.2)
            return False

        try:
            check(wait_for("fos> "), "the first boot reaches the shell", text())
            check("booting slot 0" in text(), "the first boot runs slot 0", text())
            write(SLOTS.encode() + b"\r")
            check(wait_for("active slot: 0, pending: None"), "slot 0 is active, nothing pending", text())

            try:
                sender.push(read, write, b"\0" * (5 * 1024 * 1024), log=lambda message: None)
                check(False, "an image larger than the staging area is refused")
            except SystemExit as refusal:
                check("too large" in str(refusal), "an image larger than a slot can take is refused before anything is written", str(refusal) + text())
            check(wait_for("nothing was written"), "the shell reports the refusal and returns", text())

            kernel = KERNEL.read_bytes()
            mark = len(text())
            write(b"\r")
            try:
                slot = sender.push(read, write, kernel, log=lambda message: print("  " + message), baud=921600 if ARCH == "aarch64" else 0, set_speed=lambda baud: None)
            except SystemExit as failure:
                time.sleep(6)
                check(False, str(failure), text()[mark:])
            check(slot == 1, "the kernel is written to the inactive slot (1)", text())
            check(wait_for("booting slot 1", 90, mark), "after the reset the loader tries slot 1", text()[mark:])
            check(wait_for("Kernel update confirmed: slot 1", 60, mark), "the new kernel reaches its milestone and is confirmed", text()[mark:])
            mark = len(text())
            check(wait_for("fos> ", 30, mark - 5), "the shell is back on the new kernel", text()[mark:])
            write(SLOTS.encode() + b"\r")
            check(wait_for("active slot: 1, pending: None", 30, mark), "slot 1 is now the active slot", text()[mark:])
        finally:
            qemu.kill()
            qemu.wait()
    print("aarch64 serial KUP: all checks passed")


if __name__ == "__main__":
    main()
