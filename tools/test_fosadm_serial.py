#!/usr/bin/env python3
"""fosadm over a console serial line (`fosadm.py --serial PORT`), against x86 QEMU with its COM1 on a pty: status, `put`
(the shell's `recv`), a tape deploy through `--from`, and a forced kernel deploy through `kup serial` that must come
back running. Run `make test-fosadm-serial`."""
import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CODE = os.environ.get("OVMF_CODE", "/usr/share/OVMF/OVMF_CODE_4M.fd")
VARS = os.environ.get("OVMF_VARS", "/usr/share/OVMF/OVMF_VARS_4M.fd")
IMAGE = Path(os.environ.get("FOSFOR_IMAGE", ROOT / "build/fosfor/fosfor.img"))
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
TAPES = ROOT / "build/fosfor/tapes"
FOSADM = ROOT / "tools/fosadm.py"
failures = 0


def check(condition: bool, name: str, detail: str = "") -> None:
    global failures
    print(("PASS: " if condition else "FAIL: ") + name)
    if not condition:
        failures += 1
        print("--- output ---\n" + detail[-1500:])


def fosadm(port: str, *words: str, timeout: float = 400) -> str:
    result = subprocess.run([sys.executable, str(FOSADM), "--serial", port, *words], capture_output=True, text=True, timeout=timeout)
    return result.stdout + result.stderr


def wait_prompt(port: str, seconds: float = 90) -> bool:
    fd = os.open(port, os.O_RDWR | os.O_NOCTTY)
    try:
        import termios
        attributes = termios.tcgetattr(fd)
        attributes[0] = attributes[1] = attributes[3] = 0
        termios.tcsetattr(fd, termios.TCSANOW, attributes)
        seen = b""
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([fd], [], [], 0.5)
            if ready:
                seen += os.read(fd, 4096)
                if seen.rstrip(b" ").endswith(b"fos>"):
                    return True
        return False
    finally:
        os.close(fd)


def main() -> None:
    if not shutil.which("qemu-system-x86_64") or not Path(CODE).exists():
        raise SystemExit("qemu-system-x86_64 or the OVMF firmware is missing")
    with tempfile.TemporaryDirectory() as scratch:
        scratch = Path(scratch)
        image, variables = scratch / "disk.img", scratch / "vars.fd"
        shutil.copyfile(IMAGE, image)
        shutil.copyfile(VARS, variables)
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "pty", "-monitor", "none",
             "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"if=pflash,format=raw,file={variables}",
             "-drive", f"file={image},format=raw,if=none,id=boot", "-device", "ide-hd,drive=boot"],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        try:
            line = qemu.stdout.readline()
            port = re.search(r"(/dev/pts/\d+)", line)
            check(port is not None, "QEMU gave COM1 a pty", line)
            port = port.group(1)
            check(wait_prompt(port), "the shell reaches its prompt")

            out = fosadm(port, "status")
            check("x86_64 kernel" in out and "tapes:" in out, "status answers over serial", out)
            check("differs" not in out.split("tapes:")[0], "status: the kernel is the build's", out)

            # put: a small file through `recv`, checked with `run ls`
            blob = scratch / "blob.txt"
            blob.write_bytes(b"over the serial line\n" * 100)
            out = fosadm(port, "put", "serial", str(blob), "blob.txt")
            check(out.strip() == "", "put sends a file", out)
            out = fosadm(port, "run", "serial", "ls")
            check(re.search(r"^blob\.txt\s+2100 bytes", out, re.M) is not None, "put: the file is on the machine, 2100 bytes", out)

            # deploy: a tape whose size differs, and the kernel again (forced)
            source = scratch / "release"
            (source / "tapes").mkdir(parents=True)
            shutil.copyfile(KERNEL, source / "kernel")
            wanted = (TAPES / "hello").read_bytes() + b"\0" * 7
            (source / "tapes/hello").write_bytes(wanted)
            out = fosadm(port, "deploy", "--from", str(source), "--tapes-only", "-y")
            check("tape hello: ok" in out and "verified by size" in out, "deploy: a changed tape goes over `recv`", out)
            out = fosadm(port, "deploy", "--from", str(source), "--force-kernel", "-y")
            check("kernel: running" in out and "FELL BACK" not in out, "deploy: the kernel goes over `kup serial` and runs", out)
            out = fosadm(port, "run", "serial", "ls")
            check(f"hello" in out and str(len(wanted)) in out, "the new tape survived the reboot", out)
        finally:
            qemu.kill()
            qemu.wait()
    if failures:
        raise SystemExit(f"{failures} check(s) failed")
    print("PASS: fosadm over serial")


if __name__ == "__main__":
    main()
