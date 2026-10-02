#!/usr/bin/env python3
"""Fails if AGENTS.md (CLAUDE.md is a symlink to it) is too large.

Claude Code refuses to load an instruction file above 150,000 bytes, which
would silently drop every rule in it. The budget in AGENTS.md is 100,000
bytes; this check fails at 125,000 so there is room to react, and warns
above the budget. See AGENTS.md, "Keeping this file itself small".
"""
import sys
from pathlib import Path

LIMIT = 150_000   # Claude Code will not load the file above this
FAIL_AT = 125_000
BUDGET = 100_000

size = (Path(__file__).resolve().parent.parent / "AGENTS.md").stat().st_size
if size > FAIL_AT:
    sys.exit(f"AGENTS.md is {size} bytes (> {FAIL_AT}; Claude Code stops loading it at "
             f"{LIMIT}). Move detail to docs/history/ -- see 'Keeping this file itself small'.")
if size > BUDGET:
    print(f"warning: AGENTS.md is {size} bytes, over its {BUDGET}-byte budget.", file=sys.stderr)
print(f"AGENTS.md size OK: {size} bytes (budget {BUDGET}, limit {LIMIT}).")
