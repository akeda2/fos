#!/usr/bin/env python3
"""Validate the GPT geometry emitted by tools/build_image.py."""

import sys
from pathlib import Path
import struct
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parent))
from write_layout import DATA_START_LBA  # noqa: E402

SECTOR_SIZE = 512
FOSFS_TYPE = bytes.fromhex("62736f0f0000534f8000000000000004")


def check_header(image: bytes, lba: int, expected_backup: int, entries_lba: int) -> None:
    header = image[lba * SECTOR_SIZE:(lba + 1) * SECTOR_SIZE]
    assert header[:8] == b"EFI PART"
    assert struct.unpack_from("<Q", header, 24)[0] == lba
    assert struct.unpack_from("<Q", header, 32)[0] == expected_backup
    assert struct.unpack_from("<Q", header, 72)[0] == entries_lba
    stored = struct.unpack_from("<I", header, 16)[0]
    copy = bytearray(header[:92])
    struct.pack_into("<I", copy, 16, 0)
    assert zlib.crc32(copy) & 0xFFFFFFFF == stored


def main() -> None:
    image = Path("build/fos.img").read_bytes()
    sectors = len(image) // SECTOR_SIZE
    backup_header = sectors - 1
    backup_entries = sectors - 33
    check_header(image, 1, backup_header, 2)
    check_header(image, backup_header, 1, backup_entries)
    entries = image[2 * SECTOR_SIZE:3 * SECTOR_SIZE]
    assert entries[:16] != b"\0" * 16
    data_entry = entries[128:256]
    assert data_entry[:16] == FOSFS_TYPE
    first, last = struct.unpack_from("<QQ", data_entry, 32)
    assert (first, last) == (DATA_START_LBA, sectors - 34)
    print("PASS: primary and backup GPT metadata are valid")


if __name__ == "__main__":
    main()
