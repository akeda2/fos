"""Host-side access to the FOSFS volume of a disk image, for tests that stage files before boot or read them back
after. Handles v3 (flat) and v4 (folders; paths come back as `dir/name`). Files are added to the root (v4: `settings` and `debug` into its settings/ folder)."""

import struct
import zlib
from pathlib import Path

SECTOR = 512


def locate(data) -> tuple[int, int]:
    """(first sector, version) of the first FOSFS volume found through the GPT."""
    entries_lba = struct.unpack_from("<Q", data, SECTOR + 72)[0]
    count, size = struct.unpack_from("<II", data, SECTOR + 80)
    for index in range(count):
        entry = data[entries_lba * SECTOR + index * size:entries_lba * SECTOR + (index + 1) * size]
        first = struct.unpack_from("<Q", entry, 32)[0]
        magic = bytes(data[first * SECTOR:first * SECTOR + 8])
        if entry[:16] != bytes(16) and magic in (b"FOSFS3\0\0", b"FOSFS4\0\0"):
            return first, 3 if magic == b"FOSFS3\0\0" else 4
    raise SystemExit("FAIL: no FOSFS volume in the image")


def _v4_entries(data, first):
    directory_sectors = struct.unpack_from("<I", data, first * SECTOR + 16)[0]
    base = (first + 2) * SECTOR
    for number in range(directory_sectors * SECTOR // 128):
        yield number, base + number * 128


def files(image) -> dict:
    """{path: contents} of every file on the first FOSFS volume."""
    data = Path(image).read_bytes()
    first, version = locate(data)
    found = {}
    if version == 3:
        for sector in range(1, 9):
            directory = data[(first + sector) * SECTOR:(first + sector + 1) * SECTOR]
            for slot in range(8):
                raw = directory[slot * 64:(slot + 1) * 64]
                if raw[0]:
                    start, length = struct.unpack_from("<QQ", raw, 4)
                    found[raw[28:28 + raw[1]].decode()] = data[(first + start) * SECTOR:(first + start) * SECTOR + length]
        return found
    entries = {}
    for number, at in _v4_entries(data, first):
        entry = data[at:at + 128]
        if entry[0] in (1, 2):
            entries[number + 1] = entry

    def path(number):
        entry = entries[number]
        parent = struct.unpack_from("<I", entry, 24)[0]
        name = entry[40:40 + entry[1]].decode()
        return name if parent == 0 else path(parent) + "/" + name

    for number, entry in entries.items():
        if entry[0] != 1:
            continue
        length = struct.unpack_from("<Q", entry, 8)[0]
        body = b""
        for n in range(entry[2]):
            start, count = struct.unpack_from("<QI", entry, 88 + 12 * n)
            body += data[(first + start) * SECTOR:(first + start + count) * SECTOR]
        found[path(number)] = body[:length]
    return found


def add(image, new_files: dict) -> None:
    """Appends `new_files` ({name: str|bytes}) to the root of the first FOSFS volume (first fit after the last extent)."""
    data = bytearray(Path(image).read_bytes())
    first, version = locate(data)
    for name, text in new_files.items():
        body = text if isinstance(text, bytes) else text.encode()
        sectors = -(-len(body) // SECTOR)
        if version == 3:
            end, slot = 9, None
            for sector in range(1, 9):
                for number in range(8):
                    at = (first + sector) * SECTOR + number * 64
                    if data[at]:
                        start, _, used = struct.unpack_from("<QQQ", data, at + 4)
                        end = max(end, start + used)
                    elif slot is None:
                        slot = at
            data[(first + end) * SECTOR:(first + end) * SECTOR + len(body)] = body
            data[slot:slot + 64] = bytes(64)
            data[slot] = 1
            data[slot + 1] = len(name)
            struct.pack_into("<QQQ", data, slot + 4, end, len(body), sectors)
            data[slot + 28:slot + 28 + len(name)] = name.encode()
            continue
        # A v4 volume keeps `settings` and `debug` in its settings/ folder, which wins over a bare root name (and a root file
        # `settings` would collide with the folder): stage them there, replacing the image's own copy.
        parent = 0
        if name in ("settings", "debug"):
            folder = next((number for number, at in _v4_entries(data, first) if data[at] == 2 and bytes(data[at + 40:at + 40 + data[at + 1]]) == b"settings"), None)
            if folder is not None:
                parent = folder + 1
                for _, at in _v4_entries(data, first):
                    if data[at] in (1, 2) and struct.unpack_from("<I", data, at + 24)[0] == parent and bytes(data[at + 40:at + 40 + data[at + 1]]) == name.encode():
                        data[at:at + 128] = bytes(128)
        end, slot = struct.unpack_from("<Q", data, first * SECTOR + 20)[0], None
        for _, at in _v4_entries(data, first):
            if data[at] in (1, 2):
                for n in range(data[at + 2]):
                    start, count = struct.unpack_from("<QI", data, at + 88 + 12 * n)
                    end = max(end, start + count)
            elif slot is None:
                slot = at
        data[(first + end) * SECTOR:(first + end) * SECTOR + len(body)] = body
        entry = bytearray(128)
        entry[0], entry[1], entry[2] = 1, len(name), 1 if sectors else 0
        struct.pack_into("<QQ", entry, 8, len(body), sectors)
        struct.pack_into("<I", entry, 24, parent)
        entry[40:40 + len(name)] = name.encode()
        if sectors:
            struct.pack_into("<QI", entry, 88, end, sectors)
        struct.pack_into("<I", entry, 4, zlib.crc32(bytes(entry)))
        data[slot:slot + 128] = entry
    Path(image).write_bytes(data)
