#!/usr/bin/env python3
"""Pack the assembled NIC option ROM into a BIOS-loadable image.

`boot/netrom.S` cannot know its own final size at assemble time (the header
size byte, the PCI Data Structure's image-length field, and the whole-image
checksum byte all depend on the padded output size), so -- mirroring
`tools/build_image.py`'s role for the disk image -- this script does that
patching after assembly, the same way `tools/build_image.py` assembles the
final disk image only after both loader stages and the kernel are built.

See README.md's "Booting from a NIC ROM" section for how BIOS validates and
loads a PCI expansion ROM: 0x55/0xaa signature, a size-in-512-byte-blocks
byte, and a whole-image checksum requirement this script satisfies by
patching the image's very last byte.
"""

from argparse import ArgumentParser
from pathlib import Path

SECTOR_SIZE = 512
SIGNATURE_OFFSET = 0
SIZE_BYTE_OFFSET = 2
PCI_STRUCT_POINTER_OFFSET = 0x18
PCIR_IMAGE_LENGTH_OFFSET = 0x10
# A real option ROM chip is a fixed, discrete size (2 KiB, 4 KiB, ... up to
# 64 KiB); this cap keeps the image within the largest size BIOS's 512-byte
# block-count byte can even describe (255 * 512 bytes).
MAX_ROM_BYTES = 255 * SECTOR_SIZE


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--rom", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    rom = bytearray(args.rom.read_bytes())
    if rom[SIGNATURE_OFFSET:SIGNATURE_OFFSET + 2] != b"\x55\xaa":
        raise SystemExit("assembled ROM is missing its 0x55 0xaa signature")

    padded_size = ((len(rom) + SECTOR_SIZE - 1) // SECTOR_SIZE) * SECTOR_SIZE
    if padded_size == 0 or padded_size > MAX_ROM_BYTES:
        raise SystemExit(f"ROM image must be between 1 and {MAX_ROM_BYTES} bytes once padded")
    rom.extend(b"\0" * (padded_size - len(rom)))
    blocks = padded_size // SECTOR_SIZE

    rom[SIZE_BYTE_OFFSET] = blocks

    struct_offset = int.from_bytes(rom[PCI_STRUCT_POINTER_OFFSET:PCI_STRUCT_POINTER_OFFSET + 2], "little")
    if rom[struct_offset:struct_offset + 4] != b"PCIR":
        raise SystemExit("PCI Data Structure pointer does not point at a 'PCIR' signature")
    length_offset = struct_offset + PCIR_IMAGE_LENGTH_OFFSET
    rom[length_offset:length_offset + 2] = blocks.to_bytes(2, "little")

    # BIOS requires the sum of every byte in the image to be 0 modulo 256;
    # the last byte (otherwise unused end-of-image padding) is the
    # conventional place to absorb that adjustment.
    checksum_index = len(rom) - 1
    rom[checksum_index] = 0
    rom[checksum_index] = (-sum(rom)) & 0xff

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(rom)


if __name__ == "__main__":
    main()
