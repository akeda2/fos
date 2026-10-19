#!/usr/bin/env python3
"""Builds a bootable Fosfor OS disk image: a GPT disk with a FAT32 EFI System
Partition holding the loader at \\EFI\\BOOT\\BOOTX64.EFI (the removable-media path
every UEFI firmware tries when it has no boot entry for the disk) and a payload
partition holding the A/B kernel slots.

Pure Python standard library, like every other tool here: no mkfs.fat, no mtools,
no sgdisk. The FAT32 writer is deliberately minimal (one cluster per sector, no
long file names, files laid out contiguously) because it only ever writes a
handful of files once.

Payload partition layout (docs/design.md section 5, abi/src/lib.rs): sector 0
is the slot-metadata block in v1's byte layout, slot 0 starts at sector 1 and slot 1
SLOT_SECTORS later. M1 writes the kernel ELF into slot 0. The FOSFS data partition
arrives with M4.
"""

import argparse
import struct
import uuid
import zlib
from pathlib import Path

# Where `make image` writes the finished disk image (the Makefile's IMAGE; a test keeps the two equal). Absolute, built from
# this file's location, so a tool that defaults to it works from any directory.
REPO = Path(__file__).resolve().parent.parent
DEFAULT_IMAGE = REPO / "build" / "fosfor" / "fosfor.img"

SECTOR = 512
IMAGE_SECTORS = 96 * 2048          # 96 MiB
ESP_START = 2048                   # 1 MiB aligned, like every partitioning tool
ESP_SECTORS = 36 * 2048            # 36 MiB: above FAT32's 65525-cluster floor at 512 B/cluster
SLOT_SECTORS = 16384               # 8 MiB per kernel slot (abi: SLOT_SECTORS)
PAYLOAD_START = ESP_START + ESP_SECTORS
PAYLOAD_SECTORS = 17 * 2048        # metadata sector + two slots, rounded up to 1 MiB
FOSFS_TYPE_GUID = uuid.UUID("0f6f7362-0000-4f53-8000-000000000004")
FOSFS_GUID = uuid.UUID("f05b2a00-0000-4f05-8000-000000000004")
FOSFS_START = PAYLOAD_START + PAYLOAD_SECTORS
FOSFS_SECTORS = 32 * 2048          # 32 MiB data partition (FOSFS v3, see kernel/src/fosfs.rs)
GPT_ENTRIES = 128
GPT_ENTRY_SIZE = 128
GPT_ENTRY_SECTORS = GPT_ENTRIES * GPT_ENTRY_SIZE // SECTOR

ESP_TYPE_GUID = uuid.UUID("C12A7328-F81F-11D2-BA4B-00A0C93EC93B")
# The fos payload partition type (abi: PAYLOAD_TYPE_GUID).
PAYLOAD_TYPE_GUID = uuid.UUID("f05b2a01-0000-4f05-8000-0000000000a1")
PAYLOAD_GUID = uuid.UUID("f05b2a00-0000-4f05-8000-000000000003")
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


def build_esp(loader: bytes, loader_name: bytes = b"BOOTX64 EFI") -> bytes:
    """A FAT32 volume of ESP_SECTORS sectors holding /EFI/BOOT/BOOTX64.EFI (BOOTAA64.EFI for aarch64)."""
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
        short_entry(loader_name, 0x20, 5, len(loader)),
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


def gpt_header(current_lba, backup_lba, entries_lba, entries_crc, disk_guid=DISK_GUID) -> bytes:
    header = bytearray(92)
    header[0:8] = b"EFI PART"
    struct.pack_into("<IIIIQQQQ", header, 8, 0x00010000, 92, 0, 0,
                     current_lba, backup_lba, 34, IMAGE_SECTORS - 34)
    header[56:72] = disk_guid.bytes_le
    struct.pack_into("<QIII", header, 72, entries_lba, GPT_ENTRIES, GPT_ENTRY_SIZE, entries_crc)
    struct.pack_into("<I", header, 16, crc32(bytes(header)))
    return bytes(header).ljust(SECTOR, b"\0")


def gpt_entry(type_guid, unique_guid, first, sectors, name) -> bytes:
    entry = bytearray(GPT_ENTRY_SIZE)
    entry[0:16] = type_guid.bytes_le
    entry[16:32] = unique_guid.bytes_le
    struct.pack_into("<QQQ", entry, 32, first, first + sectors - 1, 0)
    entry[56:56 + 2 * len(name)] = name.encode("utf-16-le")
    return bytes(entry)


def build_payload(kernel: bytes) -> bytes:
    """The payload partition: the metadata sector, then kernel slot 0 (and an
    empty slot 1)."""
    sectors = (len(kernel) + SECTOR - 1) // SECTOR
    assert sectors <= SLOT_SECTORS, "the kernel does not fit a slot"
    partition = bytearray(PAYLOAD_SECTORS * SECTOR)
    metadata = bytearray(SECTOR)
    metadata[0:4] = b"FOSM"
    metadata[4] = 1            # version
    metadata[5] = 0            # active slot
    metadata[6] = 0xFF         # no pending slot
    struct.pack_into("<HH", metadata, 10, sectors, 0)
    partition[0:SECTOR] = metadata
    partition[SECTOR:SECTOR + len(kernel)] = kernel
    return bytes(partition)


def build_fosfs(files, label: str = "") -> bytes:
    """A preformatted FOSFS v3 volume (the same layout as v1's tools/build_image.py):
    a superblock, eight directory sectors of 64-byte entries, contiguous extents."""
    directory_sectors, entry_size, data_start = 8, 64, 9
    volume = bytearray(FOSFS_SECTORS * SECTOR)
    superblock = bytearray(SECTOR)
    superblock[0:8] = b"FOSFS3\0\0"
    struct.pack_into("<HHIIQQ", superblock, 8, 3, entry_size, 1, directory_sectors, data_start, FOSFS_SECTORS - data_start)
    if label:
        raw_label = label.encode("ascii")
        if not (len(raw_label) <= 16 and all(byte in b"abcdefghijklmnopqrstuvwxyz0123456789._-" for byte in raw_label)):
            raise SystemExit(f"invalid FOSFS label {label!r}")
        superblock[64:64 + len(raw_label)] = raw_label  # the volume label (see kernel/src/fosfs.rs)
    volume[0:SECTOR] = superblock
    next_sector = data_start
    for index, (name, contents) in enumerate(files):
        raw = name.encode("ascii")
        if not (0 < len(raw) <= 16 and all(byte in b"abcdefghijklmnopqrstuvwxyz0123456789._-" for byte in raw)):
            raise SystemExit(f"invalid FOSFS name {name!r}")
        if index >= directory_sectors * SECTOR // entry_size:
            raise SystemExit("too many FOSFS files")
        count = (len(contents) + SECTOR - 1) // SECTOR
        if next_sector + count > FOSFS_SECTORS:
            raise SystemExit("FOSFS volume full")
        entry = bytearray(entry_size)
        entry[0], entry[1] = 1, len(raw)
        struct.pack_into("<QQQ", entry, 4, next_sector, len(contents), count)
        entry[28:28 + len(raw)] = raw
        at = SECTOR + index * entry_size
        volume[at:at + entry_size] = entry
        volume[next_sector * SECTOR:next_sector * SECTOR + len(contents)] = contents
        next_sector += count
    return bytes(volume)


def write_gpt(image: bytearray, disk_guid: uuid.UUID, payload_guid: uuid.UUID, fosfs_guid: uuid.UUID) -> None:
    """Writes both copies of the GPT (headers, entry arrays, CRCs) into `image`."""
    entries = (gpt_entry(ESP_TYPE_GUID, ESP_GUID, ESP_START, ESP_SECTORS, "EFI System")
               + gpt_entry(PAYLOAD_TYPE_GUID, payload_guid, PAYLOAD_START, PAYLOAD_SECTORS, "fos payload")
               + gpt_entry(FOSFS_TYPE_GUID, fosfs_guid, FOSFS_START, FOSFS_SECTORS, "FOSFS data")
               ).ljust(GPT_ENTRIES * GPT_ENTRY_SIZE, b"\0")
    entries_crc = crc32(entries)
    image[SECTOR:2 * SECTOR] = gpt_header(1, IMAGE_SECTORS - 1, 2, entries_crc, disk_guid)
    image[2 * SECTOR:(2 + GPT_ENTRY_SECTORS) * SECTOR] = entries
    backup_entries_lba = IMAGE_SECTORS - 1 - GPT_ENTRY_SECTORS
    image[backup_entries_lba * SECTOR:(backup_entries_lba + GPT_ENTRY_SECTORS) * SECTOR] = entries
    image[(IMAGE_SECTORS - 1) * SECTOR:] = gpt_header(IMAGE_SECTORS - 1, 1, backup_entries_lba, entries_crc, disk_guid)


def randomize_guids(image: bytearray) -> uuid.UUID:
    """Gives a finished image fresh random GUIDs (the disk, the payload partition and the FOSFS
    partition), rewriting both GPT copies and their CRCs; returns the payload partition's.

    The kernel finds the disk it booted from by the payload partition's GUID, so two sticks
    written from the same image (identical GUIDs) could not be told apart."""
    payload_guid = uuid.uuid4()
    write_gpt(image, uuid.uuid4(), payload_guid, uuid.uuid4())
    return payload_guid


# The BIOS boot chain (boot/bios): stage one is the MBR's code (bytes 0-445; the partition table and signature stay),
# stage two sits in the GPT "BIOS boot gap" between the entry array (LBA 2-33) and the first partition (LBA 2048),
# where nothing else lives. Keep both constants equal to boot/bios/stage1.S.
BIOS_STAGE2_LBA = 64
BIOS_STAGE2_SECTORS = 24


def build_image(loader: bytes, kernel: bytes, files=(), label: str = "", payload_guid: uuid.UUID = PAYLOAD_GUID,
                bios_stage1=None, bios_stage2=None, arch: str = "x86_64") -> bytes:
    if arch == "aarch64":
        bios_stage1 = bios_stage2 = b""  # no BIOS boot code for a Pi
    if bios_stage1 is None and bios_stage2 is None:
        # Not told: use the BIOS boot code `make` built, if it is there, so every image the tests build is bootable by BIOS too.
        root = Path(__file__).resolve().parent.parent
        first, second = root / "build/bios/stage1.bin", root / "build/bios/stage2.bin"
        bios_stage1, bios_stage2 = (first.read_bytes(), second.read_bytes()) if first.exists() and second.exists() else (b"", b"")
    image = bytearray(IMAGE_SECTORS * SECTOR)
    # Protective MBR: one partition of type 0xEE covering the disk.
    mbr = bytearray(SECTOR)
    if bios_stage1:
        if len(bios_stage1) != SECTOR or bios_stage1[510:512] != b"\x55\xAA" or any(bios_stage1[446:510]):
            raise SystemExit("stage one must be a 512-byte sector with its code below byte 446 and the 0x55AA signature")
        if len(bios_stage2) > BIOS_STAGE2_SECTORS * SECTOR:
            raise SystemExit(f"stage two is {len(bios_stage2)} bytes; stage one loads only {BIOS_STAGE2_SECTORS * SECTOR}")
        mbr[0:446] = bios_stage1[0:446]
    struct.pack_into("<B3sB3sII", mbr, 446, 0, b"\x00\x02\x00", 0xEE, b"\xFF\xFF\xFF", 1, IMAGE_SECTORS - 1)
    mbr[510:512] = b"\x55\xAA"
    image[0:SECTOR] = mbr
    if bios_stage2:
        image[BIOS_STAGE2_LBA * SECTOR:BIOS_STAGE2_LBA * SECTOR + len(bios_stage2)] = bios_stage2

    write_gpt(image, DISK_GUID, payload_guid, FOSFS_GUID)

    esp = build_esp(loader, b"BOOTAA64EFI" if arch == "aarch64" else b"BOOTX64 EFI")
    image[ESP_START * SECTOR:ESP_START * SECTOR + len(esp)] = esp
    payload = build_payload(kernel)
    image[PAYLOAD_START * SECTOR:PAYLOAD_START * SECTOR + len(payload)] = payload
    fosfs = build_fosfs(files, label)
    image[FOSFS_START * SECTOR:FOSFS_START * SECTOR + len(fosfs)] = fosfs
    return bytes(image)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--loader", required=True, type=Path, help="the loader's PE image (BOOTX64.EFI)")
    parser.add_argument("--kernel", required=True, type=Path, help="the kernel ELF64, written to slot 0")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--file", action="append", default=[], metavar="NAME=PATH", help="preload a FOSFS file (repeatable)")
    parser.add_argument("--label", default="", help="the FOSFS volume label (lowercase name, up to 16 characters)")
    parser.add_argument("--payload-guid", type=uuid.UUID, default=PAYLOAD_GUID,
                        help="unique GUID of the payload partition; the kernel finds its boot disk by it, so two disks "
                             "built from the same image need different ones")
    parser.add_argument("--bios-stage1", type=Path, help="boot/bios stage one (512 bytes): makes the disk bootable by BIOS too "
                        "(default: build/bios/stage1.bin if `make` has built it)")
    parser.add_argument("--bios-stage2", type=Path, help="boot/bios stage two, written into the GPT boot gap at LBA 64 "
                        "(default: build/bios/stage2.bin)")
    parser.add_argument("--arch", choices=("x86_64", "aarch64"), default="x86_64", help="the CPU the loader and kernel are built for (names the loader BOOTAA64.EFI for aarch64; aarch64 images have no BIOS boot code)")
    parser.add_argument("--no-bios", action="store_true", help="build a UEFI-only image")
    arguments = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    if arguments.arch == "aarch64":
        arguments.no_bios = True
    if not arguments.no_bios and not arguments.bios_stage1 and not arguments.bios_stage2:
        stage1, stage2 = root / "build/bios/stage1.bin", root / "build/bios/stage2.bin"
        if stage1.exists() and stage2.exists():
            arguments.bios_stage1, arguments.bios_stage2 = stage1, stage2
    if bool(arguments.bios_stage1) != bool(arguments.bios_stage2):
        parser.error("--bios-stage1 and --bios-stage2 go together")
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    files = [("readme.txt", b"Welcome to FOSFS on Fosfor OS.\nTry: ls, cat readme.txt, write NAME TEXT.\n")]
    for spec in arguments.file:
        name, _, path = spec.partition("=")
        files = [item for item in files if item[0] != name] + [(name, Path(path).read_bytes())]
    arguments.output.write_bytes(build_image(arguments.loader.read_bytes(), arguments.kernel.read_bytes(), files, arguments.label, arguments.payload_guid,
                                                arguments.bios_stage1.read_bytes() if arguments.bios_stage1 else b"",
                                                arguments.bios_stage2.read_bytes() if arguments.bios_stage2 else b"", arguments.arch))
    print(f"wrote {arguments.output}")


if __name__ == "__main__":
    main()
