#!/usr/bin/env python3
"""Times a big `put` (and `get`) against smbd in QEMU: `python3 tools/bench_smb_write.py [MB]`.

QEMU's user-mode network is slow and its disk is a file, so the absolute numbers mean little; compare before and after a
change. FOSFOR_IMAGE picks the kernel (the embedded image by default; build/fosfor-mf/fosfor.img is the server kernel).
Prints smbd's own counters (`log`): the time spent in file writes against the time spent otherwise."""
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_httpd import Guest, clean  # noqa: E402

HOST_PORT = port(28446)


def main():
    megabytes = int(sys.argv[1]) if len(sys.argv) > 1 else 64
    mkfs = shutil.which("mkfs.ext4") or "/usr/sbin/mkfs.ext4"
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        source = tmp / "src"
        (source / "drop").mkdir(parents=True)
        (source / "drop").chmod(0o777)
        image = tmp / "ext.img"
        image.write_bytes(bytes((megabytes * 2 + 64) << 20))
        subprocess.run([mkfs, "-q", "-F", "-L", "linuxvol", "-d", str(source), str(image)], check=True, capture_output=True)
        payload = tmp / "payload.bin"
        payload.write_bytes(os.urandom(megabytes << 20))
        back = tmp / "back.bin"
        with Guest(forward=HOST_PORT, data_disk=image, guest_port=445) as g:
            g.line("write settings no-msi")
            g.line("run smbd root=disk2 share=data stats ip=10.0.2.15/24 gateway=10.0.2.2 &2", "fos> ", 40)

            def client(command):
                start = time.monotonic()
                run = subprocess.run(["smbclient", "//127.0.0.1/data", "-p", str(HOST_PORT), "-N", "-c", command],
                                     capture_output=True, text=True, timeout=900)
                return time.monotonic() - start, run

            seconds, run = client(f"cd drop; put {payload} big.bin")
            print(f"put {megabytes} MB: {seconds:.1f} s, {megabytes * 8 / seconds:.0f} Mbit/s (rc {run.returncode})")
            seconds, run = client(f"cd drop; get big.bin {back}")
            print(f"get {megabytes} MB: {seconds:.1f} s, {megabytes * 8 / seconds:.0f} Mbit/s (rc {run.returncode})")
            same = back.exists() and hashlib.sha256(back.read_bytes()).digest() == hashlib.sha256(payload.read_bytes()).digest()
            print("round trip " + ("intact" if same else "DIFFERS"))
            time.sleep(3)
            seen = g.line("uptime", "ticks", 10) + clean(bytes(g.raw))
            print("\n".join(sorted({line.strip() for line in seen.splitlines() if line.strip().startswith("smbd:") and "KB in" in line})))
        return 0 if same else 1


if __name__ == "__main__":
    sys.exit(main())
