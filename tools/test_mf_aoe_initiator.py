#!/usr/bin/env python3
"""The server kernel's AoE initiator (`aoe discover`, `aoe attach`, `block::Backend::Aoe`) against its own target, two guests
on one QEMU multicast segment (docs/block-sharing.md, "The initiator").

- t (the target, 52:54:00:12:34:e1): boots with `aoe-shelf 5` / `aoe-export 2` and a scratch data disk of random bytes.
- i (the initiator, 52:54:00:12:34:e2): only its boot disk, which carries `big.bin` (192 KiB of random bytes), a `start`
  file that runs `disks`, and `aoe-attach 5.2` / `aoe-attach 5.9` in settings. It boots once t is at its prompt.

Boot-time attach: e5.2 is disk 2 before `start` runs (its `disks` lists it); e5.9 does not exist, so the boot goes on after
the wait and `aoe` says it is still looked for; at the end t exports a disk as slot 9 and i attaches it by itself.

Checks: discovery lists e5.2 with its limits; attach makes it disk 2 with the right size; raw sectors read through AoE
match the data image; a write to the read-only attach fails without touching the wire; then, with the export rw and the
attach rw: a FOSFS v4 volume made over the network, a file copied onto it with the host's SHA-256, read back after a
detach and attach (same disk number); the target sees the same file on its own disk after unexporting; a request to a
target that has gone fails after the resends instead of hanging; and an ext4 disk (the target's disk 3, when the host has
mkfs.ext4) attached, changed on the target, detached and attached again lists the new file (the kept ext4 mount is
dropped on attach; it once kept listing the old folder because the disk number stays the same).
"""

import hashlib
import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import GROUP, PORT  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
SECTORS = 16384
failures = []


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}: {detail}")


class Guest:
    def __init__(self, name, boot_image, data_images, mac):
        self.name = name
        args = ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
                "-serial", "stdio", "-monitor", "none", "-no-reboot",
                "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={boot_image},snapshot=on",
                *[arg for image in data_images for arg in ("-drive", f"format=raw,file={image}")],
                "-netdev", f"socket,id=net0,mcast={GROUP}:{PORT}", "-device", f"e1000,netdev=net0,mac={mac}"]
        if os.environ.get("FOS_KVM"):
            args.insert(1, "-enable-kvm")
        self.qemu = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.log = b""

    def wait_for(self, text, timeout):
        end = time.time() + timeout
        while time.time() < end and text.encode() not in self.log:
            if select.select([self.qemu.stdout], [], [], 0.2)[0]:
                data = os.read(self.qemu.stdout.fileno(), 4096)
                if not data:
                    break
                self.log += data
        return text.encode() in self.log

    def command(self, line, expect, timeout=30):
        self.log = b""
        self.qemu.stdin.write(line.encode() + b"\n")
        self.qemu.stdin.flush()
        ok = self.wait_for(expect, timeout)
        time.sleep(0.3)
        self.wait_for("fos> ", 10)
        return ok, self.log.decode("utf8", "replace")

    def kill(self):
        self.qemu.kill()
        self.qemu.wait()


def build_image(directory, settings_text, files=()):
    directory.mkdir()
    settings = directory / "settings"
    settings.write_text(settings_text)
    out = directory / "boot.img"
    command = [sys.executable, str(ROOT / "tools/build_image.py"), "--loader", os.environ["LOADER_EFI"], "--kernel",
               os.environ["KERNEL_MF_ELF"], "--output", str(out), "--bios-stage1", os.environ["BIOS_STAGE1"],
               "--bios-stage2", os.environ["BIOS_STAGE2"], "--file", f"settings={settings}"]
    for name, path in files:
        command += ["--file", f"{name}={path}"]
    subprocess.run(command, check=True, capture_output=True)
    return out


def hex_row(data, lba, row):
    """The first row of `sector`'s dump of sector `lba`: offset, 16 bytes in hex."""
    chunk = data[lba * 512 + row * 16:lba * 512 + row * 16 + 16]
    return f"{row * 16:03x}  " + "".join(f"{byte:02x} " for byte in chunk)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        data = os.urandom(SECTORS * 512)
        disk = tmp / "data.img"
        disk.write_bytes(data)
        # An ext4 disk for the remount check (whole disk, no partition table), if the host can make one.
        mkfs = shutil.which("mkfs.ext4") or "/usr/sbin/mkfs.ext4"
        ext = tmp / "ext.img"
        if Path(mkfs).exists():
            (tmp / "extroot").mkdir()
            (tmp / "extroot/a.txt").write_text("first\n")
            ext.write_bytes(bytes(8 << 20))
            subprocess.run([mkfs, "-q", "-F", "-L", "remext", "-d", str(tmp / "extroot"), str(ext)], check=True,
                           capture_output=True)
        else:
            ext = None
        big = os.urandom(192 * 1024)
        (tmp / "big.bin").write_bytes(big)
        digest = hashlib.sha256(big).hexdigest()
        t = Guest("t", build_image(tmp / "t", "aoe-shelf 5\naoe-export 2\n"), [disk] + ([ext] if ext else []),
                  "52:54:00:12:34:e1")
        (tmp / "start").write_text("disks\n")
        i = None
        try:
            if not t.wait_for("fos> ", 150):
                raise SystemExit(f"t never reached the prompt: {t.log[-2000:]!r}")
            i = Guest("i", build_image(tmp / "i", "aoe-attach 5.2\naoe-attach 5.9\n", [("big.bin", tmp / "big.bin"), ("start", tmp / "start")]),
                      [], "52:54:00:12:34:e2")
            if not i.wait_for("fos> ", 150):
                raise SystemExit(f"i never reached the prompt: {i.log[-2000:]!r}")
            boot = i.log.decode("utf8", "replace")
            attached, started = boot.find("aoe: e5.2 is disk 2, 8 MiB, read-only."), boot.find("start> disks")
            check(attached >= 0, "`aoe-attach 5.2` in settings attaches e5.2 as disk 2 at boot", boot[-1500:])
            check(0 <= attached < started and "disk2: 8 MiB" in boot[started:], "before `start` runs, whose `disks` lists it", boot[-1500:])
            check("aoe: e5.9 has not answered; it is looked for every 10 s." in boot, "a missing target does not stop the boot", boot[-1500:])
            ok, text = i.command("aoe", "waiting for e5.9", 20)
            check(ok, "`aoe` shows the target still looked for", text[-600:])
            ok, text = i.command("aoe detach 5.2", "detached", 20)

            ok, text = i.command("aoe discover", "e5.2", 20)
            check(ok and "52:54:00:12:34:e1" in text and "2 sector(s) a frame, bufcount 64" in text,
                  "discover lists e5.2 with the target's MAC and limits", text[-600:])
            ok, text = i.command("aoe attach 5.2", "is disk 2", 30)
            check(ok and "8 MiB, read-only" in text, "attach makes it disk 2, 8 MiB, read-only", text[-600:])
            ok, text = i.command("disks", "disk2: 8 MiB", 20)
            check(ok, "disks lists it", text[-800:])

            lbas = (0, 1, 777, SECTORS - 1)
            for lba in lbas:
                ok, text = i.command(f"sector disk2 {lba}", f"disk2 sector {lba}:", 30)
                check(ok and hex_row(data, lba, 0) in text and hex_row(data, lba, 31) in text,
                      f"sector {lba} read through AoE matches the data image", text[-600:])
            ok, text = i.command("readspeed disk2 4 128", "MB/s", 60)
            check(ok and "disk2: 4 MiB in 128 KiB requests" in text, "readspeed reads 4 MiB of it raw", text[-400:])
            ok, text = i.command(f"sector disk2 {SECTORS}", "fos", 30)
            check("Usage" in text or "read failed" in text, "a sector past the end is refused", text[-300:])
            # Random bytes are not a GPT, hence `force`.
            ok, text = i.command("format disk2 remote force", "confirm", 20)
            ok, text = i.command("format disk2 confirm", "fos", 60)
            check("rror" in text or "fail" in text or "could not" in text.lower(),
                  "format on a read-only attach fails", text[-400:])
            ok, text = i.command("sector disk2 0", "disk2 sector 0:", 30)
            check(ok and hex_row(data, 0, 0) in text, "and sector 0 is unchanged", text[-400:])
            ok, text = i.command("aoe", "attached e5.2", 20)
            check(ok and " 0 resent, 0 failed" in text, "the counters show no resends on a clean link", text[-600:])
            # readspeed's 128 KiB requests continue each other, so all but the first two come from read-ahead.
            ahead = re.search(r"read ahead (\d+) KiB, of it used (\d+) KiB", text)
            check(ahead is not None and int(ahead.group(2)) >= 3 * 1024,
                  f"read-ahead serves sequential reads ({ahead.group(0) if ahead else 'no line'})", text[-600:])

            # Writable on both ends.
            ok, text = t.command("aoe unexport 2", "no longer exported", 20)
            ok, text = t.command("aoe export 2 rw confirm", "READ-WRITE", 20)
            ok, text = i.command("aoe detach 5.2", "detached", 20)
            check(ok, "detach", text[-300:])
            ok, text = i.command("sector disk2 0", "fos", 20)
            check("read failed" in text, "a detached disk fails at once", text[-300:])
            ok, text = i.command("aoe attach 5.2 rw", "is disk 2", 30)
            check(ok and "READ-WRITE" in text, "attach rw gives it the same disk number", text[-600:])
            ok, text = i.command("format disk2 remote force", "confirm", 20)
            ok, text = i.command("format disk2 confirm", "fos", 120)
            check("rror" not in text and "fail" not in text, "format makes a FOSFS volume over AoE", text[-400:])
            ok, text = i.command("cp big.bin @remote/big.bin", "fos", 120)
            ok, text = i.command("sha256 @remote/big.bin", digest, 120)
            check(ok, "a 192 KiB file copied onto a FOSFS volume made over AoE has the host's SHA-256", text[-600:])
            ok, text = i.command("aoe detach 5.2", "detached", 20)
            ok, text = i.command("aoe attach 5.2 rw", "is disk 2", 30)
            ok, text = i.command("sha256 @remote/big.bin", digest, 120)
            check(ok, "and again after detach and attach", text[-600:])
            ok, text = i.command("aoe", "attached e5.2", 20)
            print("        " + "\n        ".join(line.strip() for line in text.splitlines()
                                             if "requests" in line or "window" in line or "read ahead" in line))

            # The target's own view of its disk, once nobody else writes it.
            ok, text = i.command("aoe detach 5.2", "detached", 20)
            ok, text = t.command("aoe unexport 2", "no longer exported", 30)
            ok, text = t.command("sha256 @remote/big.bin", digest, 120)
            check(ok, "the target finds the same file on its own disk", text[-600:])

            # The target is gone: the request fails after the resends (about 5 s), and the shell answers again.
            ok, text = t.command("aoe export 2", "read-only", 20)
            ok, text = i.command("aoe attach 5.2", "is disk 2", 30)
            ok, text = t.command("aoe unexport 2", "no longer exported", 20)
            start = time.time()
            ok, text = i.command("sector disk2 5", "read failed", 30)
            elapsed = time.time() - start
            check(ok and 2 < elapsed < 20, f"a target that stops answering fails the read after the resends ({elapsed:.1f} s)",
                  text[-400:])
            ok, text = i.command("aoe", "attached e5.2", 20)
            check(ok and "1 failed" in text, "and counts it as failed", text[-600:])

            # An ext4 volume changed on the target while attached here: after detach and attach the new file shows.
            if ext is None:
                print("SKIP: no mkfs.ext4, so no ext4 remount check")
            else:
                ok, text = t.command("aoe export 3", "read-only", 20)
                ok, text = i.command("aoe attach 5.3", "is disk 3", 30)
                check(ok, "the ext4 disk attaches as disk 3", text[-400:])
                ok, text = i.command("ls ext3/", "a.txt", 30)
                check(ok and "b.txt" not in text, "its folder lists a.txt", text[-400:])
                ok, text = t.command("cp ext3/a.txt ext3/b.txt", "fos", 30)
                ok, text = t.command("ls ext3/", "b.txt", 30)
                check(ok, "the target writes b.txt on its own disk", text[-400:])
                ok, text = i.command("aoe detach 5.3", "detached", 20)
                ok, text = i.command("aoe attach 5.3", "is disk 3", 30)
                ok, text = i.command("ls ext3/", "b.txt", 30)
                check(ok, "after detach and attach the initiator lists b.txt (no stale ext4 mount)", text[-400:])

            # The `aoe-attach 5.9` from settings: once t serves slot 9, the aoe-attach thread finds it (every 10 s).
            ok, text = t.command("aoe export 2 slot 9", "slot 9", 20)
            check(ok and "read-only" in text, "t exports disk 2 as slot 9", text[-400:])
            i.log = b""
            ok = i.wait_for("aoe: e5.9 is disk", 30)
            check(ok, "the initiator attaches e5.9 by itself once it answers", i.log[-600:].decode("utf8", "replace"))
            ok, text = i.command("aoe", "attached e5.9", 20)
            check(ok and "waiting for" not in text, "and no longer waits for it", text[-600:])
        finally:
            for guest in (t, i):
                if guest is not None:
                    guest.kill()
    for failure in failures:
        print("FAIL:", failure)
    if failures:
        return 1
    print("PASS: mf-aoe-initiator")
    return 0


if __name__ == "__main__":
    sys.exit(main())
