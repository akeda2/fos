#!/usr/bin/env python3
"""The server kernel's AoE target (`make test-mf-aoe`, docs/block-sharing.md). The host plays an AoE initiator over the
QEMU multicast-socket network (no root needed) against a guest that serves a second disk.

Boot 1: `aoe-shelf 5`, `aoe-export 2` and `aoe-export 3` in settings. A broadcast Query Config finds shelf 5 slot 2; IDENTIFY gives the
disk's capacity; reads of the first, a middle and the last sector match the host's file; an out-of-range read and a write
to the read-only export are aborted and change nothing. In the shell: `rw` without `confirm` is refused, so is disk 1
writable; `aoe unexport 2` and `aoe export 2 rw confirm` make it writable; a two-sector write lands in the disk image
(after FLUSH CACHE EXT); `aoe` shows the counters.
QEMU only: the e1000 and AHCI models. Interoperability with the Linux `aoe` module is a manual step (docs/block-sharing.md)."""
import os
import random
import select
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import GROUP, PORT  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
GUEST_MAC = bytes.fromhex("525400123 4c1".replace(" ", ""))
HOST_MAC = bytes.fromhex("525400aabbcc")
SECTORS = 16384
failures = []


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}: {detail}")


class Guest:
    def __init__(self, boot_image, data_images):
        args = ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
                "-serial", "stdio", "-monitor", "none", "-no-reboot",
                "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={boot_image},snapshot=on",
                *[arg for image in data_images for arg in ("-drive", f"format=raw,file={image}")],
                "-netdev", f"socket,id=net0,mcast={GROUP}:{PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:c1"]
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


class Initiator:
    """Just enough of an AoE initiator: raw frames in UDP datagrams to the QEMU multicast group."""

    def __init__(self):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("", PORT))
        self.sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton(GROUP) + socket.inet_aton("0.0.0.0"))
        self.sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
        self.tag = random.randrange(1, 1 << 31)

    def exchange(self, shelf, slot, command, body, destination=GUEST_MAC, timeout=3.0, attempts=3):
        self.tag += 1
        header = struct.pack(">BBHBBI", 0x10, 0, shelf, slot, command, self.tag)
        frame = destination + HOST_MAC + b"\x88\xa2" + header + body
        for _ in range(attempts):
            self.sock.sendto(frame, (GROUP, PORT))
            end = time.time() + timeout
            while time.time() < end:
                if not select.select([self.sock], [], [], 0.1)[0]:
                    continue
                reply = self.sock.recv(65536)
                if reply[12:14] == b"\x88\xa2" and reply[6:12] == GUEST_MAC and reply[14] & 0x08 and struct.unpack(">I", reply[20:24])[0] == self.tag:
                    return reply
        return None

    def ata(self, slot, command, lba=0, count=0, data=b"", write=False, shelf=5):
        aflags = 0x40 | (1 if write else 0)
        body = struct.pack("<BBBB", aflags, 0, count, command) + lba.to_bytes(6, "little") + b"\0\0" + data
        return self.exchange(shelf, slot, 0, body)


def ata_status(reply):
    return reply[27], reply[25]


def build_image(tmp, settings_text):
    settings = tmp / "settings"
    settings.write_text(settings_text)
    out = tmp / "boot.img"
    subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", os.environ["LOADER_EFI"], "--kernel", os.environ["KERNEL_MF_ELF"],
                    "--output", str(out), "--bios-stage1", os.environ["BIOS_STAGE1"], "--bios-stage2", os.environ["BIOS_STAGE2"],
                    "--file", f"settings={settings}"], check=True, capture_output=True)
    return out


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        data = os.urandom(SECTORS * 512)
        disk = tmp / "data.img"
        disk.write_bytes(data)
        data3 = os.urandom(SECTORS * 512)
        disk3 = tmp / "data3.img"
        disk3.write_bytes(data3)
        boot = build_image(tmp, "aoe-shelf 5\naoe-export 2\naoe-export 3\n")
        guest = Guest(boot, [disk, disk3])
        host = Initiator()
        try:
            check(guest.wait_for("fos> ", 150), "the guest booted to the prompt", guest.log[-800:].decode("utf8", "replace"))
            check("aoe: disk 2 exported read-only as shelf 5 slot 2" in guest.log.decode("utf8", "replace"), "the settings line exported disk 2 read-only")

            reply = host.exchange(0xFFFF, 0xFF, 1, struct.pack(">HHBBH", 0, 0, 0, 0x10, 0), destination=b"\xff" * 6)
            check(reply is not None and struct.unpack(">HB", reply[16:19]) == (5, 2), "a broadcast Query Config is answered by shelf 5 slot 2", reply)
            if reply:
                check(reply[28] == 2 and struct.unpack(">H", reply[24:26])[0] > 0, "it advertises 2 sectors per frame and a window", reply[24:32].hex())

            reply = host.ata(2, 0xEC)
            check(reply is not None and ata_status(reply) == (0x50, 0), "IDENTIFY is answered", reply)
            if reply and len(reply) >= 36 + 512:
                words = struct.unpack("<256H", reply[36:36 + 512])
                check(words[83] & (1 << 10) and words[100] == SECTORS and words[106] == 0x4000, "it reports LBA48, the capacity and 512-byte sectors", words[100:104])
                model = b"".join(struct.pack(">H", w) for w in words[27:47])
                check(b"QEMU" in model.upper() or model.strip() != b"", "and a model string", model)

            for lba, count in [(0, 2), (SECTORS // 2, 2), (SECTORS - 1, 1), (7, 1)]:
                reply = host.ata(2, 0x24, lba, count)
                ok = reply is not None and ata_status(reply) == (0x50, 0) and reply[36:] [:count * 512] == data[lba * 512:(lba + count) * 512]
                check(ok, f"READ of {count} sector(s) at {lba} matches the host's file", reply and reply[24:36].hex())

            reply = host.ata(2, 0x24, SECTORS, 1)
            check(reply is not None and ata_status(reply) == (0x51, 4), "a read past the end is aborted", reply)
            payload = bytes(range(256)) * 4
            reply = host.ata(2, 0x34, 50, 2, payload, write=True)
            check(reply is not None and ata_status(reply) == (0x51, 4), "a write to the read-only export is aborted", reply)
            check(disk.read_bytes()[50 * 512:52 * 512] == data[50 * 512:52 * 512], "and the disk is unchanged")

            ok, text = guest.command("aoe export 2 rw", "confirm")
            check(ok and "READ-WRITE" not in text.split("Type")[0], "`rw` without `confirm` is refused", text)
            ok, text = guest.command("aoe export 1 rw confirm", "boot disk")
            check(ok, "disk 1 is never exported writable", text)
            ok, text = guest.command("aoe unexport 2", "no longer exported")
            check(ok, "aoe unexport 2", text)
            reply = host.ata(2, 0x24, 0, 1)
            check(reply is None, "an unexported disk no longer answers", reply)
            ok, text = guest.command("aoe export 2 rw confirm", "READ-WRITE")
            check(ok, "aoe export 2 rw confirm", text)

            reply = host.ata(2, 0x34, 50, 2, payload, write=True)
            check(reply is not None and ata_status(reply) == (0x50, 0), "a two-sector write is accepted", reply)
            reply = host.ata(2, 0xEA)
            check(reply is not None and ata_status(reply) == (0x50, 0), "FLUSH CACHE EXT is accepted", reply)
            reply = host.ata(2, 0x24, 50, 2)
            check(reply is not None and reply[36:36 + 1024] == payload, "reading it back through AoE returns what was written", reply and reply[24:36].hex())
            time.sleep(1)
            check(disk.read_bytes()[50 * 512:52 * 512] == payload, "and it reached the disk image")
            check(disk.read_bytes()[:50 * 512] == data[:50 * 512], "nothing before it changed")

            ok, text = guest.command("aoe", "disk 2")
            check(ok and "READ-WRITE" in text and "1 writes" in text, "`aoe` lists the export with its counters", text)

            # The flusher: a write nobody flushes still reaches the drive after a pause in writing.
            late = bytes(reversed(range(256))) * 2
            reply = host.ata(2, 0x34, 100, 1, late, write=True)
            check(reply is not None and ata_status(reply) == (0x50, 0), "a write with no FLUSH CACHE is accepted", reply)
            time.sleep(1.5)
            check(disk.read_bytes()[100 * 512:101 * 512] == late, "and the flusher puts it on the disk after a pause")

            # Two exports at once, each with its own queue and worker.
            reply = host.ata(3, 0x24, 9, 2)
            check(reply is not None and reply[36:36 + 1024] == data3[9 * 512:11 * 512], "disk 3, exported by settings, is served beside disk 2", reply and reply[24:36].hex())

            # AoE reads on disk 3 while the guest shell reads the boot disk: the disk lock keeps the two apart.
            bad = []
            stop = threading.Event()

            def hammer():
                rng = random.Random(1)
                while not stop.is_set():
                    lba = rng.randrange(0, SECTORS - 2)
                    answer = host.ata(3, 0x24, lba, 2)
                    if answer is None or answer[36:36 + 1024] != data3[lba * 512:(lba + 2) * 512]:
                        bad.append(lba)
                        return
                    hammer.count += 1
            hammer.count = 0
            thread = threading.Thread(target=hammer)
            thread.start()
            shell_ok = True
            for _ in range(6):
                ok, text = guest.command("sector disk1 0 4", "disk1 sector 3:", 30)
                shell_ok = shell_ok and ok and "read failed" not in text
            stop.set()
            thread.join()
            check(not bad and hammer.count > 50, f"{hammer.count} AoE reads on disk 3 were all correct while the shell read disk 1", bad)
            check(shell_ok, "and the shell's disk reads kept working")

            # The worker's cache (crates/aoe/src/cache.rs) against the real block layer: a long sequential read (read-ahead
            # fills of 64 KiB) and a run of consecutive writes (gathered into few disk writes).
            good = True
            for lba in range(1000, 1400, 2):
                answer = host.ata(3, 0x24, lba, 2)
                good = good and answer is not None and answer[36:36 + 1024] == data3[lba * 512:(lba + 2) * 512]
            check(good, "200 sequential reads on disk 3 match the host's file (read-ahead)")
            run = os.urandom(32 * 512)
            good = True
            for part in range(16):
                answer = host.ata(2, 0x34, 3000 + part * 2, 2, run[part * 1024:(part + 1) * 1024], write=True)
                good = good and answer is not None and ata_status(answer) == (0x50, 0)
            for part in range(16):
                answer = host.ata(2, 0x24, 3000 + part * 2, 2)
                good = good and answer is not None and answer[36:36 + 1024] == run[part * 1024:(part + 1) * 1024]
            check(good, "16 consecutive writes are accepted and read back through AoE")
            time.sleep(1.5)
            check(disk.read_bytes()[3000 * 512:3032 * 512] == run, "and they reach the disk image (gathered, then flushed)")
            ok, text = guest.command("aoe", "disk commands")
            check(ok and "frames from read-ahead" in text and "FAILED" not in text, "`aoe` shows the cache's counters", text)

            ok, text = guest.command("aoe unexport 2", "no longer exported")
            reply = host.ata(3, 0x24, 0, 1)
            check(ok and reply is not None and reply[36:36 + 512] == data3[:512], "unexporting disk 2 leaves disk 3 answering", text)
        finally:
            guest.kill()
    if failures:
        print(f"{len(failures)} check(s) failed:")
        for failure in failures:
            print("  " + str(failure)[:300])
        return 1
    print("PASS: mf-aoe")
    return 0


if __name__ == "__main__":
    sys.exit(main())
