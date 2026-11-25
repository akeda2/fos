#!/usr/bin/env python3
"""Fails when a kernel's in-memory image would not fit the room the BIOS loader reserves for it.

Stage two (boot/bios/stage2.S) places the kernel in a region of KERNEL_SPAN bytes (8 MiB) and stops with "the kernel image
is larger than the 8 MiB reserved for it" otherwise. What counts is the span of the ELF's PT_LOAD segments in memory, from
the lowest address to the end of the highest segment's memory size: zero-filled .bss included, although it takes no room in
the file. A 1.7 MB kernel file can therefore be over the limit, and only a BIOS boot used to show it. Run by `make image`,
`make image-mf` and `make test`; prints each kernel's span and headroom, warns below WARN_HEADROOM.

Usage: check_kernel_span.py [ELF...]   (default: the embedded and server kernels that exist in build/cargo-target)
"""

import os
import re
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STAGE2 = ROOT / "boot" / "bios" / "stage2.S"
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "build" / "cargo-target")) / "x86_64-fos-kernel" / "release"
DEFAULTS = [TARGET / "fosfor-kernel", TARGET / "fosfor-kernel-mf"]
WARN_HEADROOM = 512 * 1024
PT_LOAD = 1


def kernel_span_limit():
    """KERNEL_SPAN from stage2.S, so the limit has one source."""
    match = re.search(r"^\s*\.set\s+KERNEL_SPAN\s*,\s*(0x[0-9a-fA-F]+|\d+)", STAGE2.read_text(), re.M)
    if not match:
        raise SystemExit(f"check_kernel_span: no `.set KERNEL_SPAN` in {STAGE2}")
    return int(match.group(1), 0)


def load_span(path):
    """Bytes from the lowest PT_LOAD address to the end of the highest one's memory size (ELF64 little-endian)."""
    data = path.read_bytes()
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        raise SystemExit(f"check_kernel_span: {path} is not a little-endian ELF64 file")
    phoff, = struct.unpack_from("<Q", data, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    low, high = None, 0
    for n in range(phnum):
        kind, _flags, _offset, vaddr, _paddr, _filesz, memsz = struct.unpack_from("<LLQQQQQ", data, phoff + n * phentsize)
        if kind == PT_LOAD and memsz:
            low = vaddr if low is None else min(low, vaddr)
            high = max(high, vaddr + memsz)
    if low is None:
        raise SystemExit(f"check_kernel_span: {path} has no PT_LOAD segment")
    return high - low


def main():
    limit = kernel_span_limit()
    paths = [Path(arg) for arg in sys.argv[1:]] or [path for path in DEFAULTS if path.exists()]
    if not paths:
        raise SystemExit("check_kernel_span: no kernel built yet (make kernel)")
    failed = False
    for path in paths:
        span = load_span(path)
        headroom = limit - span
        line = f"{path.name}: {span / 1048576:.2f} MiB in memory, limit {limit / 1048576:.0f} MiB"
        if headroom < 0:
            print(f"FAIL: {line}: {-headroom // 1024} KiB over. The BIOS loader would refuse it (KERNEL_SPAN, stage2.S).")
            print("      Move large statics to the page allocator (see nicctl::reserve_frames, ext4vol::cache).")
            failed = True
        elif headroom < WARN_HEADROOM:
            print(f"WARN: {line}: only {headroom // 1024} KiB left.")
        else:
            print(f"ok: {line}, {headroom // 1024} KiB left.")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
