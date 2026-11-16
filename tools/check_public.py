#!/usr/bin/env python3
"""Fails if a public file or an unpushed commit message names something in the private tree (docs/private.md).

  check_public.py

The words to look for are the private tree's own business: `private/denylist.txt`, one regular expression per line
(case-insensitive; `#` starts a comment). Without a private tree there is nothing to compare against and the check
passes. Checked: every file git tracks in this (public) repository, and the messages of the commits not yet on the
upstream branch, so a leak is caught before `git push`.
"""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DENYLIST = ROOT / "private" / "denylist.txt"


def git(*args):
    return subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True).stdout


def main():
    if not DENYLIST.exists():
        print("check_public: no private tree, nothing to check.")
        return 0
    patterns = []
    for line in DENYLIST.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            patterns.append(re.compile(line, re.IGNORECASE))
    hits = []
    for name in git("ls-files", "-z").split("\0"):
        path = ROOT / name
        if not name or not path.is_file() or path.is_symlink():
            continue
        try:
            text = path.read_text(errors="ignore")
        except OSError:
            continue
        for number, line in enumerate(text.splitlines(), 1):
            if any(p.search(line) for p in patterns):
                hits.append(f"{name}:{number}: {line.strip()[:100]}")
    upstream = git("rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}").strip()
    if upstream:
        for entry in git("log", "--format=%h %B%x00", f"{upstream}..HEAD").split("\0"):
            if any(p.search(entry) for p in patterns):
                hits.append(f"commit {entry.strip().split()[0]}: message names a private item")
    if hits:
        print("check_public: public files or unpushed commits name private items (private/denylist.txt):")
        for hit in hits:
            print("  " + hit)
        return 1
    print(f"check_public: {len(patterns)} private names, none in the public tree or unpushed commits.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
