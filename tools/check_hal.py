#!/usr/bin/env python3
"""Fail when a kernel file reaches past kernel/src/hal.rs into an arch-private module.

Drivers may use crate::{cpu,paging,timer,msi,pci_config,memory} and println!. The names below belong to the
architecture; ALLOWED lists the files that still use them (docs/big-kernel.md, stage 1a). The list may only shrink.
"""
import re
import sys
from pathlib import Path

SRC = Path(__file__).resolve().parent.parent / "kernel" / "src"
PRIVATE = r"(apic|ioapic|gdt|idt|acpi|serial|cpupower|gic|early|exceptions|pl011)"
PATTERN = re.compile(r"\b" + PRIVATE + r"::[A-Za-z_]+")

# Entry files, the arch layer and the shell/terminal/diagnostic code, which are core-specific, are not checked.
SKIP = {"main.rs", "main_x86_64.rs", "main_aarch64.rs", "hal.rs", "term.rs", "shell.rs", "shellcmds.rs",
        "kupshell.rs", "tape.rs", "tape_aarch64.rs", "diag.rs", "crash.rs", "config.rs", "power.rs",
        "nicshell.rs", "netfs.rs", "netservice.rs", "keyboard.rs"}
# Files that still reach past hal; the list may only shrink.
ALLOWED: set[str] = set()


def main() -> int:
    bad = []
    for path in sorted(SRC.glob("*.rs")):
        if path.name in SKIP or path.name in ALLOWED:
            continue
        for number, line in enumerate(path.read_text().splitlines(), 1):
            if line.lstrip().startswith("//"):
                continue
            match = PATTERN.search(line)
            if match:
                bad.append(f"{path.name}:{number}: {match.group(0)}")
    for item in bad:
        print("hal bypass:", item)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
