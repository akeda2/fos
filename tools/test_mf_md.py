#!/usr/bin/env python3
"""Linux software RAID 1 on the server kernel (`make test-mf-md`, docs/md.md). The host builds an md v1.2 mirror the way
mdadm lays it out (superblock at sector 8, data from sector 2048, no bitmap) holding an ext4 volume made by mkfs.ext4, and
boots the server kernel with `md auto` in `settings`.

Boot 1 (both members): the array must assemble in the background, show as a disk, read the host's file, take a write, and
`md stop` must leave both superblocks clean with one more event pair and identical data that e2fsck accepts.
Boot 2 (one member only, `md wait 2`): it must assemble degraded, say so, and still read the file.
The superblock writer here is independent of crates/md (written from md_p.h), so the two check each other."""
import os
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import hashlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF", ROOT / "build" / "fosfor-mf" / "fosfor.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
failures = []
SECTORS = 131072            # 64 MiB members
DATA_OFFSET = 2048
DATA_SECTORS = SECTORS - DATA_OFFSET
UUID = bytes(range(1, 17))
CLEAN = 0xFFFFFFFFFFFFFFFF


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}: {detail}")


def superblock(dev, events, resync=CLEAN):
    max_dev = 128
    b = bytearray(256 + max_dev * 2)
    struct.pack_into("<LLL", b, 0, 0xA92B4EFC, 1, 0)
    b[16:32] = UUID
    b[32:32 + 5] = b"fos:0"
    struct.pack_into("<QlLQLLL", b, 64, 1700000000, 1, 0, DATA_SECTORS, 0, 2, 0)
    struct.pack_into("<QQQQL", b, 128, DATA_OFFSET, DATA_SECTORS, 8, 0, dev)
    b[168:184] = bytes([0x40 + dev]) * 16
    struct.pack_into("<QQQ", b, 192, 1700000100, events, resync)
    struct.pack_into("<L", b, 220, max_dev)
    for i in range(2):
        struct.pack_into("<H", b, 256 + 2 * i, i)
    for i in range(2, max_dev):
        struct.pack_into("<H", b, 256 + 2 * i, 0xFFFF)
    total = sum(struct.unpack_from("<%dL" % (len(b) // 4), b)) & 0xFFFFFFFFFFFFFFFF
    total = (total & 0xFFFFFFFF) + (total >> 32)
    total = (total & 0xFFFFFFFF) + (total >> 32)
    struct.pack_into("<L", b, 216, total & 0xFFFFFFFF)
    return bytes(b)


def read_super(path):
    with open(path, "rb") as f:
        f.seek(8 * 512)
        b = f.read(512)
    assert struct.unpack_from("<L", b, 0)[0] == 0xA92B4EFC
    s = struct.unpack_from("<L", b, 216)[0]
    z = bytearray(b)
    struct.pack_into("<L", z, 216, 0)
    n = 256 + struct.unpack_from("<L", b, 220)[0] * 2
    t = sum(struct.unpack_from("<%dL" % (n // 4), z)) & 0xFFFFFFFFFFFFFFFF
    t = (t & 0xFFFFFFFF) + (t >> 32)
    t = (t & 0xFFFFFFFF) + (t >> 32)
    return {"events": struct.unpack_from("<Q", b, 200)[0], "resync": struct.unpack_from("<Q", b, 208)[0],
            "csum_ok": (t & 0xFFFFFFFF) == s, "roles": struct.unpack_from("<2H", b, 256)}


class Guest:
    def __init__(self, drives, settings_image):
        args = ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
                "-serial", "stdio", "-monitor", "none", "-no-reboot",
                "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={settings_image},snapshot=on"]
        for d in drives:
            args += ["-drive", f"format=raw,file={d}"]
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

    def command(self, line, expect, timeout=60):
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


def build_image(tmp, settings_text):
    settings = tmp / "settings"
    settings.write_text(settings_text)
    out = tmp / "boot.img"
    subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", os.environ["LOADER_EFI"], "--kernel", os.environ["KERNEL_MF_ELF"],
                    "--output", str(out), "--bios-stage1", os.environ["BIOS_STAGE1"], "--bios-stage2", os.environ["BIOS_STAGE2"],
                    "--file", f"settings={settings}"], check=True, capture_output=True)
    return out


def main():
    mkfs = shutil.which("mkfs.ext4") or "/usr/sbin/mkfs.ext4"
    e2fsck = shutil.which("e2fsck") or "/usr/sbin/e2fsck"
    if not Path(mkfs).exists():
        print("SKIP: no mkfs.ext4")
        return 0
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp / "src").mkdir()
        blob = os.urandom(3 << 20 | 777)
        (tmp / "src" / "hello.bin").write_bytes(blob)
        digest = hashlib.sha256(blob).hexdigest()
        fs = tmp / "fs.img"
        fs.write_bytes(bytes(DATA_SECTORS * 512))
        subprocess.run([mkfs, "-q", "-F", "-L", "mdvol", "-d", str(tmp / "src"), str(fs)], check=True, capture_output=True)
        members = []
        for dev in range(2):
            m = tmp / f"m{dev}.img"
            with open(m, "wb") as f:
                f.truncate(SECTORS * 512)
                f.seek(8 * 512)
                f.write(superblock(dev, 10))
                f.seek(DATA_OFFSET * 512)
                f.write(fs.read_bytes())
            members.append(m)

        boot = build_image(tmp, "md auto\nmd wait 20\n")
        g = Guest(members, boot)
        try:
            check(g.wait_for("md: array assembled", 150), "array assembled in the background at boot", g.log[-600:].decode("utf8", "replace"))
            check(g.wait_for("fos> ", 60), "the shell prompt came up")
            ok, text = g.command("md", "members in use: 2 of 2")
            check(ok, "md shows both members", text[-300:])
            ok, text = g.command("sha256 @mdvol/hello.bin", digest, 120)
            check(ok, "a file read through the array has the host's SHA-256", text[-300:])
            ok, text = g.command("write @mdvol/new.txt mirrored42", "fos", 30)
            ok, text = g.command("cat @mdvol/new.txt", "mirrored42", 30)
            check(ok, "a write through the array reads back", text[-300:])
            time.sleep(5)  # the idle close
            ok, text = g.command("md stop", "members are clean", 30)
            check(ok, "md stop closes the array", text[-300:])
        finally:
            g.kill()
        a, b = read_super(members[0]), read_super(members[1])
        check(a["csum_ok"] and b["csum_ok"], "both superblocks keep a valid checksum", f"{a} {b}")
        check(a["events"] == b["events"] and a["events"] > 10, "both members have the same, higher event count", f"{a} {b}")
        check(a["resync"] == CLEAN and b["resync"] == CLEAN, "both are marked clean again", f"{a} {b}")
        check(a["roles"] == (0, 1), "roles are untouched", f"{a}")
        datas = []
        for m in members:
            with open(m, "rb") as f:
                f.seek(DATA_OFFSET * 512)
                datas.append(f.read(DATA_SECTORS * 512))
        check(datas[0] == datas[1], "the two mirrors hold identical data")
        if Path(e2fsck).exists():
            (tmp / "out.img").write_bytes(datas[0])
            r = subprocess.run([e2fsck, "-fn", str(tmp / "out.img")], capture_output=True, text=True)
            check(r.returncode == 0, "e2fsck accepts the volume after the guest wrote to it", r.stdout[-400:])

        # Boot 2: only the second member, which is current: degraded, but readable.
        g = Guest([members[1]], build_image(tmp, "md auto\nmd wait 2\n"))
        try:
            check(g.wait_for("running degraded", 150), "one member alone assembles degraded and says so", g.log[-600:].decode("utf8", "replace"))
            check(g.wait_for("fos> ", 60), "the shell prompt came up")
            ok, text = g.command("sha256 @mdvol/hello.bin", digest, 120)
            check(ok, "the degraded array reads the file", text[-300:])
        finally:
            g.kill()
    for failure in failures:
        print("FAIL:", failure)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
