#!/usr/bin/env python3
"""NETFS on the server kernel (docs/big-kernel.md): `tools/foscopy.py`, unchanged, stores files on a kernel-mf machine and
reads them back over the multicast seam. Checks, byte for byte:

 1. a small file and a 37,000-byte one (37 chunks) round-trip; 300,000 bytes of random data too (the checksum must not overflow);
 2. a read comes from the FOSFS volume, so the PUT reached the disk and not just a buffer;
 3. tape sync: the manifest is signed with `synckey`; another key is refused;
 4. a missing file, a missing disk and the `synckey` secret are refused with the usual messages.
QEMU only: real NIC pacing, and a transfer racing a running tape on silicon, are for hardware."""

import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_debug_mode as base  # noqa: E402
from test_boot import ROOT, check  # noqa: E402

base.KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf"
READY = "entering the service loop"
MAC = "52:54:00:12:34:7b"


def foscopy(machine, args, cwd):
    """foscopy while the guest's console is pumped, so its output cannot fill the pipe and stall it."""
    command = [sys.executable, str(ROOT / "tools/foscopy.py"), "--mcast", f"{base.MCAST_GROUP}:{base.MCAST_PORT}", "--delay", "0.002",
               "--timeout", "15", "--mac", MAC, *args]
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, cwd=cwd)
    while process.poll() is None:
        machine.pump(0.1)
    return process.returncode, process.stdout.read()


def main():
    import random
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        small = b"hello from the host\n"
        big = bytes((index * 7 + index // 251) % 256 for index in range(37_000))
        large = random.Random(7).randbytes(300_000)
        (work / "note.txt").write_bytes(small)
        (work / "big.bin").write_bytes(big)
        (work / "large.bin").write_bytes(large)
        (work / "key").write_text("mf test key\n")
        machine = base.Machine(tmp, {"hostname": "mfhost\n", "synckey": "mf test key\n"}, {"hello": ROOT / "build/fosfor/tapes/hello"})
        machine.start()
        try:
            machine.wait_for(READY)
            code, out = foscopy(machine, ["note.txt", ":disk1/note.txt"], work)
            check(code == 0, "foscopy stores a file on the server kernel", out[-600:])
            code, out = foscopy(machine, [":disk1/note.txt", "note.back"], work)
            check(code == 0 and (work / "note.back").read_bytes() == small, "and reads it back from the volume", out[-600:])
            for name, data in (("big.bin", big), ("large.bin", large)):
                code, out = foscopy(machine, [name, f":disk1/{name}"], work)
                check(code == 0, f"{name} ({len(data)} bytes) is stored", out[-600:])
                code, out = foscopy(machine, [f":disk1/{name}", f"{name}.back"], work)
                check(code == 0 and (work / f"{name}.back").read_bytes() == data, f"and read back byte for byte", out[-600:])

            code, out = foscopy(machine, [":disk1/nosuchfile", "x.bin"], work)
            check(code != 0 and "file not found" in out, "a missing file is reported as such", out[-400:])
            code, out = foscopy(machine, ["note.txt", ":disk9/note.txt"], work)
            check(code != 0 and "disk is not available" in out, "an unavailable disk is refused", out[-400:])
            code, out = foscopy(machine, ["note.txt", ":disk1/synckey"], work)
            check(code != 0, "the synckey secret cannot be written", out[-400:])
            code, out = foscopy(machine, [":disk1/synckey", "k.bin"], work)
            check(code != 0 and not (work / "k.bin").exists(), "or read", out[-400:])

            # Tape sync: the signed manifest of the stamped tapes (tools/tape_manifest.py checks the signature).
            def manifest(key):
                command = [sys.executable, str(ROOT / "tools/tape_manifest.py"), "--mcast", f"{base.MCAST_GROUP}:{base.MCAST_PORT}", "--mac", MAC,
                           "--key", str(key), "hello"]
                process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, cwd=work)
                while process.poll() is None:
                    machine.pump(0.1)
                return process.returncode, process.stdout.read()
            code, out = manifest(work / "key")
            check(code == 0 and "1 of 1 identical to the build" in out, "tape sync: the signed manifest lists the stamped tape and matches the build", out[-600:])
            (work / "wrong").write_text("another key\n")
            machine.pump(2.5)
            code, out = manifest(work / "wrong")
            check(code != 0, "and a peer with another key refuses it", out[-400:])
        finally:
            machine.stop()
    print("test-mf-netfs: all checks passed")


if __name__ == "__main__":
    main()
