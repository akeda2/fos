#!/usr/bin/env python3
"""Check that migrated 32-bit Rust tapes remain valid ELF32 ET_EXEC files."""

from pathlib import Path
import struct

TAPE_NAMES = (
    "hello-rs",
    "vga-colors",
    "bigsieve",
    "calc",
    "beep",
    "morse",
    "net-chat",
    "ip-chat",
    "telnet",
    "bridge",
    "router",
)


def main() -> None:
    for name in TAPE_NAMES:
        image = Path(f"build/tapes/{name}.bin").read_bytes()
        assert image[:4] == b"\x7fELF", name
        assert image[4:7] == b"\x01\x01\x01", name
        assert struct.unpack_from("<H", image, 16)[0] == 2, name
        assert struct.unpack_from("<H", image, 18)[0] == 3, name
        entry = struct.unpack_from("<I", image, 24)[0]
        phoff = struct.unpack_from("<I", image, 28)[0]
        phentsize = struct.unpack_from("<H", image, 42)[0]
        phnum = struct.unpack_from("<H", image, 44)[0]
        assert phentsize >= 32 and 0 < phnum <= 32, name
        assert phoff + phentsize * phnum <= len(image), name
        assert any(
            struct.unpack_from("<I", image, phoff + index * phentsize)[0] == 1
            for index in range(phnum)
        ), name
        assert entry >= 0x100000, name
    print("PASS: migrated Rust tapes are valid ELF32 ET_EXEC images")


if __name__ == "__main__":
    main()
