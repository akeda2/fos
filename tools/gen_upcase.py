#!/usr/bin/env python3
"""Writes fat/src/upcase.bin: an exFAT up-case table made from Python's Unicode data (simple one-to-one upper-case
mappings of the Basic Multilingual Plane), in the format's compressed form: a run of unchanged characters is the pair
0xFFFF, count; everything else is the upper-case value of the next character. The table ends after the last character that
changes (the rest map to themselves). Run it again only if the table should change; the result is committed."""

import struct
import sys
from pathlib import Path


def main():
    mapping = []
    for code in range(0x10000):
        if 0xD800 <= code < 0xE000:
            mapping.append(code)
            continue
        upper = chr(code).upper()
        mapping.append(ord(upper) if len(upper) == 1 and ord(upper) < 0x10000 and not 0xD800 <= ord(upper) < 0xE000 else code)
    end = max(code for code in range(0x10000) if mapping[code] != code) + 1
    table = []
    code = 0
    while code < end:
        if mapping[code] == code:
            run = 0
            while code + run < end and mapping[code + run] == code + run and run < 0xFFFF:
                run += 1
            if run >= 3:
                table += [0xFFFF, run]
            else:
                table += [code + offset for offset in range(run)]
            code += run
        else:
            table.append(mapping[code])
            code += 1
    data = struct.pack(f"<{len(table)}H", *table)
    out = Path(__file__).resolve().parents[1] / "fat/src/upcase.bin"
    out.write_bytes(data)
    print(f"{out}: {len(data)} bytes, covers characters up to {end - 1:#x}", file=sys.stderr)


if __name__ == "__main__":
    main()
