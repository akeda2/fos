#!/usr/bin/env python3
"""NETFS on the server kernel (docs/big-kernel.md): `tools/foscopy.py`, unchanged, stores files on a kernel-mf machine and
reads them back over the multicast seam. Checks, byte for byte:

 1. a small file and a 37,000-byte one (37 chunks) round-trip; 300,000 bytes of random data too (the checksum must not overflow);
 2. a read comes from the FOSFS volume, so the PUT reached the disk and not just a buffer;
 3. tape sync: the manifest is signed with `synckey`; another key is refused;
 4. a missing file, a missing disk and the `synckey` secret are refused with the usual messages.
 5. a path into a folder (`settings/note.txt`) is stored and read back; `synckey` in a folder is refused too.
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


def folder_checks(machine, work, copy=None):
    """A path into a v4 folder (`settings/`, made by the image builder) round-trips; a trailing slash keeps the source's
    name; a missing folder is "file not found"; the key file is refused in a folder too. Shared with test_netfs.py."""
    copy = copy or (lambda args: foscopy(machine, args, work))
    code, out = copy(["note.txt", ":disk1/settings/note.txt"])
    check(code == 0, "foscopy stores a file in a folder (settings/note.txt)", out[-600:])
    code, out = copy([":disk1/settings/note.txt", "folder.back"])
    check(code == 0 and (work / "folder.back").read_bytes() == (work / "note.txt").read_bytes(),
          "and reads it back from the folder", out[-600:])
    code, out = copy(["note.txt", ":disk1/settings/"])
    check(code == 0 and "settings/note.txt" in out, "a trailing slash keeps the source's name", out[-600:])
    code, out = copy(["note.txt", ":disk1/nosuchfolder/note.txt"])
    check(code != 0 and "file not found" in out, "a missing folder is reported as not found", out[-400:])
    code, out = copy(["note.txt", ":disk1/settings/synckey"])
    check(code != 0, "the synckey secret cannot be written in a folder", out[-400:])
    code, out = copy([":disk1/settings/synckey", "k2.bin"])
    check(code != 0 and not (work / "k2.bin").exists(), "or read from one", out[-400:])


def mount_checks(machine, work):
    """A path under a mount point reaches the mounted volume, as the shell's `cp` does, not the boot volume's own folder
    of that name (which the mount hides). On rack `settings` is an exFAT volume mounted so: a NETFS write into the boot
    volume's settings/ folder was stored and acknowledged, and the machine never read it."""
    machine.connect()
    disks = machine.run("disks")
    number = next((line.split()[0].rstrip(":") for line in disks.splitlines() if line.startswith("disk") and "64 MiB" in line), None)
    check(number is not None, "the blank second disk is found", disks)
    machine.run(f"format {number} exfat CFG")
    out = machine.run(f"format {number} confirm")
    listing = machine.run(f"label {number}")
    check("@CFG (exFAT)" in listing, "and formatted exFAT, labelled CFG", out[-400:] + listing)
    out = machine.run("mount cfg @cfg")
    listing = machine.run("mount")
    check("is now @cfg" in out and "not present" not in listing and "cfg -> @cfg" in listing, "and mounted at cfg/", out + listing)
    machine.run("mkdir cfg")  # the boot volume's own cfg/ folder, hidden by the mount: NETFS must not write there
    code, out = foscopy(machine, ["note.txt", ":disk1/cfg/note.txt"], work)
    check(code == 0, "foscopy stores a file under a mount point", out[-600:])
    out = machine.run("cat cfg/note.txt")
    check("hello from the host" in out, "and the shell sees it through the mount", out[-400:])
    out = machine.run("cat disk1/cfg/note.txt")  # names disk1 itself: the mount table is not consulted
    check("hello from the host" not in out, "it is on the mounted volume, not in the boot volume's hidden folder", out[-400:])
    code, out = foscopy(machine, [":disk1/cfg/note.txt", "mounted.back"], work)
    check(code == 0 and (work / "mounted.back").read_bytes() == (work / "note.txt").read_bytes(),
          "and reads back through the mount", out[-600:])
    code, out = foscopy(machine, [":disk1/cfg/nosuchfile", "x.bin"], work)
    check(code != 0 and "file not found" in out, "a missing file under a mount point is not found", out[-400:])


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
        # FOSFS v4, as `make image` builds, so a path can reach a folder: tapes in bin/, a settings/ folder. A key in
        # settings/ too, so refusing `settings/synckey` is the guard at work and not a missing file.
        (work / "readme").write_text("settings folder\n")
        machine = base.Machine(tmp, {"hostname": "mfhost\n", "synckey": "mf test key\n"},
                               {"bin/hello": ROOT / "build/fosfor/tapes/hello", "settings/readme": work / "readme",
                                "settings/synckey": work / "key"}, fosfs=4)
        # A blank second disk, formatted exFAT and mounted at cfg/ by `mount_checks` (rack keeps its settings so).
        blank = work / "cfg.img"
        with open(blank, "wb") as image:
            image.truncate(64 << 20)
        machine.extra = ["-drive", f"format=raw,file={blank},if=ide,index=2"]
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
            folder_checks(machine, work)
            mount_checks(machine, work)

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
