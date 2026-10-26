#!/usr/bin/env python3
"""Collects the built kernels and stamped tapes into a release folder for `fosadm --from`.

  make_release.py [OUT]     (default build/release; `make release` builds everything first)

Layout: OUT/x86_64/{kernel,tapes/*} and OUT/aarch64/{kernel,tapes/*}, plus OUT/manifest.json with the
commit, the build time and every file's size. A CPU whose build is missing is left out.
"""
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CPUS = {
    "x86_64": (ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel", ROOT / "build/fosfor/tapes"),
    "aarch64": (ROOT / "build/cargo-target/aarch64-unknown-none-softfloat/release/fosfor-kernel",
                ROOT / "build/fosfor-aarch64/tapes"),
}


def git(*args):
    return subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True).stdout.strip()


def main() -> int:
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "build/release"
    shutil.rmtree(out, ignore_errors=True)
    manifest = {"commit": git("rev-parse", "--short", "HEAD"), "dirty": bool(git("status", "--porcelain", "--untracked-files=no")),
                "built": time.strftime("%Y-%m-%d %H:%M:%S"), "cpus": {}}
    for cpu, (kernel, tapes) in CPUS.items():
        if not kernel.exists() or not tapes.is_dir():
            print(f"{cpu}: no build, left out")
            continue
        (out / cpu / "tapes").mkdir(parents=True)
        shutil.copy2(kernel, out / cpu / "kernel")
        files = {}
        for tape in sorted(p for p in tapes.iterdir() if p.is_file() and not p.name.startswith(".")):
            shutil.copy2(tape, out / cpu / "tapes" / tape.name)
            files[tape.name] = tape.stat().st_size
        manifest["cpus"][cpu] = {"kernel_bytes": kernel.stat().st_size, "tapes": files}
        print(f"{cpu}: kernel and {len(files)} tapes")
    (out / "manifest.json").write_text(json.dumps(manifest, indent=1) + "\n")
    print(f"release {manifest['commit']}{' (dirty)' if manifest['dirty'] else ''} in {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
