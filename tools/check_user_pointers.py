#!/usr/bin/env python3
"""Fails if the server kernel's tape-API code touches memory through a raw pointer without saying why it is safe.

A tape pointer is only a number: SMAP faults on a write to user memory outside `process::user_access`, and an unmapped or
kernel-half address must be refused, not followed (the `random` crash, docs/big-kernel.md). User memory goes through
`process::copy_to_user`, `user_bytes` and friends. A line with `from_raw_parts`, `ptr::read/write/copy`, `write_bytes` or a
cast to a raw pointer in kernel-mf/src/tape*.rs must end in `// kernel-memory` (the pointer is the kernel's own) or sit
inside `user_access` after `vm::user_range_ok` (mark it `// checked-user`)."""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RISKY = re.compile(r"from_raw_parts|ptr::(read|write|copy)|write_bytes|as \*(mut|const)\b|\.cast::<")
bad = []
for path in sorted((ROOT / "kernel-mf" / "src").glob("tape*.rs")):
    for number, line in enumerate(path.read_text().splitlines(), 1):
        code = line.split("//")[0]
        if RISKY.search(code) and "// kernel-memory" not in line and "// checked-user" not in line:
            bad.append(f"{path.relative_to(ROOT)}:{number}: {line.strip()}")
if bad:
    print("raw pointer use in tape API code without `// kernel-memory` or `// checked-user`:")
    print("\n".join(bad))
    sys.exit(1)
print("PASS: no unmarked raw pointer use in kernel-mf/src/tape*.rs")
