#!/usr/bin/env python3
"""Builds a bootable fos v2 disk image: a GPT disk with one FAT32 EFI System
Partition holding the loader at \\EFI\\BOOT\\BOOTX64.EFI, the removable-media path
every UEFI firmware tries when it has no boot entry for the disk.

Pure Python standard library, like every other tool here: no mkfs.fat, no mtools,
no sgdisk. The FAT32 writer is deliberately minimal (one cluster per sector, no
long file names, files laid out contiguously) because it only ever writes a
handful of files once.

Later milestones add the payload partition (A/B kernel slots) and the FOSFS data
partition; the layout is described in docs/v2-design.md, section 5.
"""

import argparse
import struct
import uuid
import zlib
from pathlib import Path

SECTOR = 512
IMAGE_SECTORS = 96 * 2048          # 96 MiB
ESP_START = 2048                   # 1 MiB aligned, like every partitioning tool
ESP_SECTORS = 66 * 2048            # 66 MiB: comfortably above FAT32's 65525-cluster floor
GPT_ENTRIES = 128
GPT_ENTRY_SIZE = 128
GPT_ENTRY_SECTORS = GPT_ENTRIES * GPT_ENTRY_SIZE // SECTOR

ESP_TYPE_GUID = uuid.UUID("C12A7328-F81F-11D2-BA4B-00A0C93EC93B")
# Fixed GUIDs so two builds of the same inputs give the same image.
DISK_GUID = uuid.UUID("f05b2a00-0000-4f05-8000-000000000001")
ESP_GUID = uuid.UUID("f05b2a00-0000-4f05-8000-000000000002")


def crc32(data: bytes) -> int:
    return zlib.crc32(data) & 0xFFFFFFFF


def fat_date_time():
    # 2026-10-03 12:00:00, fixed for reproducible images.
    date = ((2026 - 1980) << 9) | (10 << 5) | 3
    time = (12 << 11)
    return date, time


def short_entry(name83: bytes, attribute: int, cluster: int, size: int) -> bytes:
    assert len(name83) == 11
    date, time = fat_date_time()
    return struct.pack(
        "<11sBBBHHHHHHHI",
        name83, attribute, 0, 0, time, date, date, cluster >> 16, time, date, cluster & 0xFFFF, size,
    )


def build_esp(loader: bytes) -> bytes:
    """A FAT32 volume of ESP_SECTORS sectors holding /EFI/BOOT/BOOTX64.EFI."""
    sectors = ESP_SECTORS
    reserved = 32
    fats = 2
    # One sector per cluster; solve for the FAT size.
    fat_sectors = 1
    while True:
        clusters = sectors - reserved - fats * fat_sectors
        needed = (clusters + 2) * 4 // SECTOR + 1
        if needed <= fat_sectors:
            break
        fat_sectors = needed
    clusters = sectors - reserved - fats * fat_sectors
    assert clusters >= 65525, "too few clusters for FAT32"
    data_start = reserved + fats * fat_sectors

    # --- boot sector, FSInfo, backups ---
    boot = bytearray(SECTOR)
    boot[0:3] = b"\xEB\x58\x90"
    boot[3:11] = b"FOSV2   "
    struct.pack_into("<HBHBHHBHHHII", boot, 11,
                     SECTOR, 1, reserved, fats, 0, 0, 0xF8, 0, 63, 255, ESP_START, sectors)
    struct.pack_into("<IHHIHH12s", boot, 36, fat_sectors, 0, 0, 2, 1, 6, b"\0" * 12)
    struct.pack_into("<BBBI11s8s", boot, 64, 0x80, 0, 0x29, 0xF05B2A00, b"NO NAME    ", b"FAT32   ")
    boot[510:512] = b"\x55\xAA"
    fsinfo = bytearray(SECTOR)
    struct.pack_into("<I", fsinfo, 0, 0x41615252)
    struct.pack_into("<I", fsinfo, 484, 0x61417272)
    # Free cluster count and the next one to try, as a mounting OS would compute them.
    file_clusters_used = max(1, (len(loader) + SECTOR - 1) // SECTOR)
    struct.pack_into("<II", fsinfo, 488, clusters - 3 - file_clusters_used, 5 + file_clusters_used)
    struct.pack_into("<I", fsinfo, 508, 0xAA550000)

    # --- allocation: root = cluster 2, EFI = 3, BOOT = 4, file from 5 ---
    file_clusters = max(1, (len(loader) + SECTOR - 1) // SECTOR)
    fat = bytearray(fat_sectors * SECTOR)
    struct.pack_into("<II", fat, 0, 0x0FFFFFF8, 0x0FFFFFFF)
    end_of_chain = 0x0FFFFFFF
    for cluster in (2, 3, 4):
        struct.pack_into("<I", fat, cluster * 4, end_of_chain)
    for index in range(file_clusters):
        cluster = 5 + index
        struct.pack_into("<I", fat, cluster * 4, cluster + 1 if index + 1 < file_clusters else end_of_chain)

    def directory(entries) -> bytes:
        return b"".join(entries).ljust(SECTOR, b"\0")

    root = directory([short_entry(b"EFI        ", 0x10, 3, 0)])
    efi = directory([
        short_entry(b".          ", 0x10, 3, 0),
        short_entry(b"..         ", 0x10, 0, 0),
        short_entry(b"BOOT       ", 0x10, 4, 0),
    ])
    boot_dir = directory([
        short_entry(b".          ", 0x10, 4, 0),
        short_entry(b"..         ", 0x10, 3, 0),
        short_entry(b"BOOTX64 EFI", 0x20, 5, len(loader)),
    ])

    volume = bytearray(sectors * SECTOR)
    volume[0:SECTOR] = boot
    volume[SECTOR:2 * SECTOR] = fsinfo
    volume[6 * SECTOR:7 * SECTOR] = boot
    volume[7 * SECTOR:8 * SECTOR] = fsinfo
    for copy in range(fats):
        start = (reserved + copy * fat_sectors) * SECTOR
        volume[start:start + len(fat)] = fat

    def put_cluster(cluster: int, data: bytes):
        offset = (data_start + cluster - 2) * SECTOR
        volume[offset:offset + len(data)] = data

    put_cluster(2, root)
    put_cluster(3, efi)
    put_cluster(4, boot_dir)
    put_cluster(5, loader.ljust(file_clusters * SECTOR, b"\0"))
    return bytes(volume)


def gpt_header(current_lba, backup_lba, entries_lba, entries_crc) -> bytes:
    header = bytearray(92)
    header[0:8] = b"EFI PART"
    struct.pack_into("<IIIIQQQQ", header, 8, 0x00010000, 92, 0, 0,
                     current_lba, backup_lba, 34, IMAGE_SECTORS - 34)
    header[56:72] = DISK_GUID.bytes_le
    struct.pack_into("<QIII", header, 72, entries_lba, GPT_ENTRIES, GPT_ENTRY_SIZE, entries_crc)
    struct.pack_into("<I", header, 16, crc32(bytes(header)))
    return bytes(header).ljust(SECTOR, b"\0")


def build_image(loader: bytes) -> bytes:
    image = bytearray(IMAGE_SECTORS * SECTOR)
    # Protective MBR: one partition of type 0xEE covering the disk.
    mbr = bytearray(SECTOR)
    struct.pack_into("<B3sB3sII", mbr, 446, 0, b"\x00\x02\x00", 0xEE, b"\xFF\xFF\xFF", 1, IMAGE_SECTORS - 1)
    mbr[510:512] = b"\x55\xAA"
    image[0:SECTOR] = mbr

    entry = bytearray(GPT_ENTRY_SIZE)
    entry[0:16] = ESP_TYPE_GUID.bytes_le
    entry[16:32] = ESP_GUID.bytes_le
    struct.pack_into("<QQQ", entry, 32, ESP_START, ESP_START + ESP_SECTORS - 1, 0)
    entry[56:56 + 24] = "EFI System".encode("utf-16-le")
    entries = bytes(entry).ljust(GPT_ENTRIES * GPT_ENTRY_SIZE, b"\0")
    entries_crc = crc32(entries)

    image[SECTOR:2 * SECTOR] = gpt_header(1, IMAGE_SECTORS - 1, 2, entries_crc)
    image[2 * SECTOR:(2 + GPT_ENTRY_SECTORS) * SECTOR] = entries
    backup_entries_lba = IMAGE_SECTORS - 1 - GPT_ENTRY_SECTORS
    image[backup_entries_lba * SECTOR:(backup_entries_lba + GPT_ENTRY_SECTORS) * SECTOR] = entries
    image[(IMAGE_SECTORS - 1) * SECTOR:] = gpt_header(IMAGE_SECTORS - 1, 1, backup_entries_lba, entries_crc)

    esp = build_esp(loader)
    image[ESP_START * SECTOR:ESP_START * SECTOR + len(esp)] = esp
    return bytes(image)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--loader", required=True, type=Path, help="the loader's PE image (BOOTX64.EFI)")
    parser.add_argument("--output", required=True, type=Path)
    arguments = parser.parse_args()
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_bytes(build_image(arguments.loader.read_bytes()))
    print(f"wrote {arguments.output}")


if __name__ == "__main__":
    main()
