#!/usr/bin/env python3
"""`recorder` writing onto FAT32 and exFAT volumes a PC would have formatted (v2).

Two extra SATA disks, one made by `mkfs.vfat -F 32` and one by `mkfs.exfat`, are attached to the guest; the tape records a
test tone onto each as `diskN/NAMEnnn.wav`. Afterwards `fsck.vfat` and `fsck.exfat` check the volumes (they know the
formats independently of this kernel) and the WAV files are read back out of the images and parsed.
Run with `make test-v2-recorder-fat`."""

import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402
from test_recorder import loud  # noqa: E402
from test_record import wav  # noqa: E402


def exfat_files(path: Path):
    """{name: bytes} of the root directory of an exFAT image (contiguous or chained files, root followed through the FAT)."""
    data = path.read_bytes()
    fat_offset, fat_length, heap_offset, clusters, root = struct.unpack_from("<IIIII", data, 80)
    sector_shift, cluster_shift = data[108], data[109]
    cluster_bytes = 1 << (sector_shift + cluster_shift)

    def cluster_at(number):
        at = (heap_offset << sector_shift) + (number - 2) * cluster_bytes
        return data[at:at + cluster_bytes]

    def fat(number):
        return struct.unpack_from("<I", data, (fat_offset << sector_shift) + number * 4)[0]

    directory, number = b"", root
    while 2 <= number < 0xfffffff8:
        directory += cluster_at(number)
        number = fat(number)
    files, index = {}, 0
    while index < len(directory) and directory[index] != 0:
        entry = directory[index:index + 32]
        if entry[0] == 0x85:
            count = entry[1]
            stream = directory[index + 32:index + 64]
            name = b"".join(directory[index + 64 + 32 * n + 2:index + 64 + 32 * n + 32] for n in range(count - 1)).decode("utf-16le")
            length = stream[3]
            flags, first, size = stream[1], struct.unpack_from("<I", stream, 20)[0], struct.unpack_from("<Q", stream, 24)[0]
            if flags & 2:
                start = (heap_offset << sector_shift) + (first - 2) * cluster_bytes
                files[name[:length]] = data[start:start + size]
            else:
                body, number = b"", first
                while 2 <= number < 0xfffffff8 and len(body) < size:
                    body += cluster_at(number)
                    number = fat(number)
                files[name[:length]] = body[:size]
            index += 32 * (count + 1)
        else:
            index += 32
    return files


def fat32_file(path: Path, name: str):
    output = subprocess.run(["7z", "e", "-so", "-y", str(path), name], capture_output=True)
    return output.stdout if output.returncode == 0 else None


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        fat32, exfat = tmp / "fat32.img", tmp / "exfat.img"
        for path, command in ((fat32, ["mkfs.vfat", "-F", "32", "-s", "1"]), (exfat, ["mkfs.exfat"])):
            path.write_bytes(bytes(96 << 20))
            subprocess.run([*command, str(path)], check=True, capture_output=True)
        extra = ["-device", "ich9-ahci,id=ahci",
                 "-drive", f"if=none,id=f1,format=raw,file={fat32}", "-device", "ide-hd,drive=f1,bus=ahci.0",
                 "-drive", f"if=none,id=f2,format=raw,file={exfat}", "-device", "ide-hd,drive=f2,bus=ahci.1"]
        guest = Guest(tmp, "recfat", "52:54:00:12:34:e3", extra=extra)
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("disks")
            guest.pump(1.0)
            listing = guest.text()[mark:]
            check("disk3" in listing, "three disks are found", listing)
            for disk, base, presses in (("disk2", "fa", 2), ("disk3", "ex", 0)):
                # No volume in the name: the menu starts on the first exFAT disk (disk3) and `D` steps on through the disks.
                mark = guest.send(f"run recorder {base} tone=440 notest pre=1")
                guest.wait_for("fos recorder", 30, mark)
                guest.wait_for("Record to: disk3  exFAT", 10, mark)  # the default: the first exFAT disk
                guest.wait_for("longest run", 10, mark)
                for _ in range(presses):
                    guest.process.stdin.write(b"d")
                    guest.process.stdin.flush()
                    guest.pump(0.6)
                guest.wait_for(f"Record to: {disk}  {'FAT32' if disk == 'disk2' else 'exFAT'}", 10, mark)
                if disk == "disk2":
                    guest.wait_for("Free space: not shown for this kind of volume", 10, mark)
                else:
                    guest.wait_for("longest run", 10, mark)
                    check("room for" in guest.text()[mark:] and "GiB" in guest.text()[mark:] or "MiB" in guest.text()[mark:], "an exFAT disk shows its free space and the time that fits", "")
                guest.pump(2.0)
                guest.process.stdin.write(b" ")
                guest.process.stdin.flush()
                guest.wait_for(f"Recording {disk}/{base}001.wav", 10, mark)
                guest.process.stdin.write(b"d")
                guest.process.stdin.flush()
                guest.wait_for("stop it (Space) before changing disk", 10, mark)
                check(f"Record to: {disk}" in guest.text()[mark:], "the disk cannot be changed during a take", "")
                guest.pump(3.0)
                guest.process.stdin.write(b" ")
                guest.process.stdin.flush()
                guest.wait_for("Saved 1 file", 15, mark)
                guest.process.stdin.write(b"q")
                guest.process.stdin.flush()
                guest.wait_for("file(s) written", 30, mark)
                check("(with errors)" not in guest.text()[mark:], f"the take on {disk} had no errors", guest.text()[mark:][-1500:])
            # The disk speed test: it runs on the chosen disk at start and after `D`, reports the speed, and leaves nothing behind.
            mark = guest.send("run recorder st tone=440")
            guest.wait_for("Testing the speed of disk3", 30, mark)  # shown while the test runs: the UI is not frozen
            guest.wait_for("disk3: ", 60, mark)
            guest.wait_for("KiB/s over", 60, mark)
            for _ in range(2):  # disk3 -> disk1 -> disk2 (FAT32)
                guest.process.stdin.write(b"d")
                guest.process.stdin.flush()
                guest.pump(0.8)
            guest.wait_for("disk2: ", 60, mark)
            check("KiB/s over" in guest.text()[mark:], "the speed test reports the disk's speed", guest.text()[mark:][-800:])
            guest.process.stdin.write(b"t")  # and T tests again
            guest.process.stdin.flush()
            guest.pump(3.0)
            guest.process.stdin.write(b"q")
            guest.process.stdin.flush()
            guest.wait_for("file(s) written", 30, mark)
        finally:
            guest.process.kill()
            guest.process.wait()
        for command, image in ((["fsck.vfat", "-n"], fat32), (["fsck.exfat", "-n"], exfat)):
            result = subprocess.run([*command, str(image)], capture_output=True, text=True)
            check(result.returncode == 0, f"{command[0]} finds {image.name} clean", result.stdout + result.stderr)
        check("spdtest.tmp" not in exfat_files(exfat) and not fat32_file(fat32, "spdtest.tmp"), "the speed test file is gone from both volumes", "")
        for name, blob in (("fa001.wav", fat32_file(fat32, "fa001.wav")), ("ex001.wav", exfat_files(exfat).get("ex001.wav"))):
            check(blob is not None, f"{name} can be read back out of its volume", "")
            channels, rate, bits, samples, declared = wav(blob)
            seconds = len(samples) / rate
            check((channels, rate, bits) == (2, 48000, 16), f"{name} is 2 channels, 48 kHz, 16 bit", "")
            check(3.8 < seconds < 6.2, f"{name} holds {seconds:.1f} s (1 s pre-roll + 3 s by hand)", "")
            check(loud(samples, 0, rate) > 4000 and loud(samples, 2 * rate, 3 * rate) > 4000, f"and the tone is in it", "")
    print("v2 recorder on FAT: all checks passed")


if __name__ == "__main__":
    main()
