#!/usr/bin/env python3
"""Create a fixed-layout GPT disk image without touching host block devices.

Produces `build/fos.img`: a 16 MiB raw disk with a protective MBR, primary
and backup GPT, a raw boot payload partition (stage two loader + kernel), and
a separate FOSFS data partition. The data partition is preformatted with
`README.TXT` and every compiled tape passed through `--file NAME PATH`. See
the top-level README's "Disk image layout" section for the full diagram.
"""

from argparse import ArgumentParser
from pathlib import Path
import struct
import zlib

from write_layout import DATA_SECTORS, DATA_START_LBA, IMAGE_SECTORS

SECTOR_SIZE = 512
PAYLOAD_START_LBA = 2048
STAGE2_SECTORS = 32
MIN_IMAGE_SECTORS = IMAGE_SECTORS
KERNEL_LOAD_ADDRESS = 0x10000
KERNEL_RESERVED_END = 0x50000
# FOSFS reserves one superblock sector plus this many fixed directory
# sectors (64 entries total, 64 bytes each) before file data begins.
DIRECTORY_SECTORS = 8
ENTRY_SIZE = 64
DATA_START = 1 + DIRECTORY_SECTORS
GPT_ENTRY_COUNT = 128
GPT_ENTRY_SIZE = 128
GPT_PRIMARY_HEADER_LBA = 1
GPT_PRIMARY_ENTRIES_LBA = 2
GPT_BACKUP_HEADER_LBA = IMAGE_SECTORS - 1
GPT_BACKUP_ENTRIES_LBA = IMAGE_SECTORS - 33


def valid_name(name: bytes) -> bool:
    return (
        0 < len(name) <= 16
        and all(byte in b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-" for byte in name)
    )


def preformat_fosfs(image: bytearray, files: list[tuple[str, bytes]]) -> None:
    """Writes a ready-to-use FOSFS filesystem into the data partition.

    Lays down the superblock (magic, version, geometry), a flat directory,
    and contiguous file extents, so a fresh image boots straight into a
    usable filesystem without requiring a `format` first. See
    `kernel/src/fosfs.rs` for the on-disk layout this mirrors.
    """
    offset = DATA_START_LBA * SECTOR_SIZE
    superblock = bytearray(SECTOR_SIZE)
    superblock[:8] = b"FOSFS3\0\0"
    superblock[8:10] = (3).to_bytes(2, "little")
    superblock[10:12] = ENTRY_SIZE.to_bytes(2, "little")
    superblock[12:16] = (1).to_bytes(4, "little")
    superblock[16:20] = DIRECTORY_SECTORS.to_bytes(4, "little")
    superblock[20:28] = DATA_START.to_bytes(8, "little")
    superblock[28:36] = (DATA_SECTORS - DATA_START).to_bytes(8, "little")
    image[offset:offset + SECTOR_SIZE] = superblock
    next_sector = DATA_START
    for index, (name, contents) in enumerate(files):
        name_bytes = name.encode("ascii")
        if not valid_name(name_bytes):
            raise ValueError(f"invalid FOSFS name: {name!r}")
        if index >= DIRECTORY_SECTORS * (SECTOR_SIZE // ENTRY_SIZE):
            raise ValueError("FOSFS directory cannot contain all requested files")
        sector_count = (len(contents) + SECTOR_SIZE - 1) // SECTOR_SIZE
        if next_sector + sector_count > DATA_SECTORS:
            raise ValueError(f"FOSFS data partition is too small for {name!r}")
        entry = bytearray(ENTRY_SIZE)
        entry[0] = 1
        entry[1] = len(name_bytes)
        entry[4:12] = next_sector.to_bytes(8, "little")
        entry[12:20] = len(contents).to_bytes(8, "little")
        entry[20:28] = sector_count.to_bytes(8, "little")
        entry[28:28 + len(name_bytes)] = name_bytes
        directory_offset = offset + SECTOR_SIZE + index * ENTRY_SIZE
        image[directory_offset:directory_offset + ENTRY_SIZE] = entry
        file_offset = offset + next_sector * SECTOR_SIZE
        image[file_offset:file_offset + len(contents)] = contents
        next_sector += sector_count


def pad(data: bytes, sectors: int) -> bytes:
    """Right-pads `data` with zero bytes to fill exactly `sectors` sectors."""
    if len(data) > sectors * SECTOR_SIZE:
        raise ValueError(f"artifact exceeds its {sectors}-sector allocation")
    return data.ljust(sectors * SECTOR_SIZE, b"\0")


def guid_bytes(text: str) -> bytes:
    """Encode a deterministic GPT GUID in the on-disk mixed-endian form."""
    words = text.split("-")
    if len(words) != 5:
        raise ValueError(f"invalid GUID {text!r}")
    first = int(words[0], 16).to_bytes(4, "little")
    second = int(words[1], 16).to_bytes(2, "little")
    third = int(words[2], 16).to_bytes(2, "little")
    return first + second + third + bytes.fromhex(words[3] + words[4])


def write_gpt(image: bytearray, payload_sectors: int) -> None:
    """Write a protective MBR and primary/backup GPT metadata."""
    image[446:462] = bytes([0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF]) + (
        1
    ).to_bytes(4, "little") + min(IMAGE_SECTORS - 1, 0xFFFFFFFF).to_bytes(4, "little")
    image[462:510] = b"\0" * 48

    entries = bytearray(GPT_ENTRY_COUNT * GPT_ENTRY_SIZE)
    fosfs_type = guid_bytes("0f6f7362-0000-4f53-8000-000000000004")
    payload_type = guid_bytes("0f6f7362-0000-4f53-8000-000000000001")
    payload_guid = guid_bytes("0f6f7362-0000-4f53-8000-000000000002")
    data_guid = guid_bytes("0f6f7362-0000-4f53-8000-000000000003")
    entries[0:16] = payload_type
    entries[16:32] = payload_guid
    struct.pack_into("<QQ", entries, 32, PAYLOAD_START_LBA, PAYLOAD_START_LBA + payload_sectors - 1)
    entries[56:56 + len("FOS payload".encode("utf-16le"))] = "FOS payload".encode("utf-16le")
    entries[128:144] = fosfs_type
    entries[144:160] = data_guid
    struct.pack_into(
        "<QQ",
        entries,
        160,
        DATA_START_LBA,
        DATA_START_LBA + DATA_SECTORS - 1,
    )
    entries[184:184 + len("FOSFS data".encode("utf-16le"))] = "FOSFS data".encode("utf-16le")

    def header(current: int, backup: int, entries_lba: int) -> bytes:
        raw = bytearray(SECTOR_SIZE)
        raw[:8] = b"EFI PART"
        struct.pack_into("<I", raw, 8, 0x00010000)
        struct.pack_into("<I", raw, 12, 92)
        struct.pack_into("<Q", raw, 24, current)
        struct.pack_into("<Q", raw, 32, backup)
        struct.pack_into("<QQ", raw, 40, 34, IMAGE_SECTORS - 34)
        raw[56:72] = guid_bytes("0f6f7362-0000-4f53-8000-000000000010")
        struct.pack_into("<Q", raw, 72, entries_lba)
        struct.pack_into("<I", raw, 80, GPT_ENTRY_COUNT)
        struct.pack_into("<I", raw, 84, GPT_ENTRY_SIZE)
        struct.pack_into("<I", raw, 88, zlib.crc32(entries) & 0xFFFFFFFF)
        crc = zlib.crc32(raw[:92]) & 0xFFFFFFFF
        struct.pack_into("<I", raw, 16, crc)
        return bytes(raw)

    primary_entries = GPT_PRIMARY_ENTRIES_LBA * SECTOR_SIZE
    backup_entries = GPT_BACKUP_ENTRIES_LBA * SECTOR_SIZE
    image[primary_entries:primary_entries + len(entries)] = entries
    image[backup_entries:backup_entries + len(entries)] = entries
    image[GPT_PRIMARY_HEADER_LBA * SECTOR_SIZE:(GPT_PRIMARY_HEADER_LBA + 1) * SECTOR_SIZE] = header(
        GPT_PRIMARY_HEADER_LBA, GPT_BACKUP_HEADER_LBA, GPT_PRIMARY_ENTRIES_LBA
    )
    image[GPT_BACKUP_HEADER_LBA * SECTOR_SIZE:(GPT_BACKUP_HEADER_LBA + 1) * SECTOR_SIZE] = header(
        GPT_BACKUP_HEADER_LBA, GPT_PRIMARY_HEADER_LBA, GPT_BACKUP_ENTRIES_LBA
    )


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--stage1", type=Path, required=True)
    parser.add_argument("--stage2", type=Path, required=True)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--file",
        nargs=2,
        action="append",
        default=[],
        metavar=("NAME", "PATH"),
        help="preload one flat FOSFS file from PATH",
    )
    args = parser.parse_args()

    stage1 = args.stage1.read_bytes()
    if len(stage1) != SECTOR_SIZE or stage1[510:] != b"\x55\xaa":
        raise SystemExit("stage 1 must be exactly one boot-signature-bearing sector")
    stage2 = pad(args.stage2.read_bytes(), STAGE2_SECTORS)
    kernel = args.kernel.read_bytes()
    kernel_sectors = (len(kernel) + SECTOR_SIZE - 1) // SECTOR_SIZE
    max_kernel_sectors = (KERNEL_RESERVED_END - KERNEL_LOAD_ADDRESS) // SECTOR_SIZE
    if not 0 < kernel_sectors <= max_kernel_sectors:
        raise SystemExit(f"kernel must occupy between 1 and {max_kernel_sectors} sectors")

    image = bytearray(MIN_IMAGE_SECTORS * SECTOR_SIZE)
    image[:SECTOR_SIZE] = stage1
    payload_sectors = STAGE2_SECTORS + kernel_sectors
    write_gpt(image, payload_sectors)
    image[PAYLOAD_START_LBA * SECTOR_SIZE:(PAYLOAD_START_LBA + STAGE2_SECTORS) * SECTOR_SIZE] = stage2
    kernel_start = (PAYLOAD_START_LBA + STAGE2_SECTORS) * SECTOR_SIZE
    image[kernel_start:kernel_start + len(kernel)] = kernel
    readme = b"Welcome to FOSFS.\nUse ls, cat README.TXT, and write NAME TEXT.\n"
    files = [("README.TXT", readme)]
    files.extend((name, Path(path).read_bytes()) for name, path in args.file)
    if len({name for name, _ in files}) != len(files):
        raise SystemExit("FOSFS file names must be unique")
    try:
        preformat_fosfs(image, files)
    except (OSError, ValueError) as error:
        raise SystemExit(str(error)) from error
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(image)


if __name__ == "__main__":
    main()
