#!/usr/bin/env python3
"""Checks what `make all` / `make image-pi4` need beyond the x86 build and says what is missing and how to get it.

Prints nothing and exits 0 when everything is there; the Makefile runs it first (before anything is built) and stops with the
text it prints. Standard library only. The pinned toolchain is read from rust-toolchain.toml."""

import re
import shutil
import socket
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def toolchain():
    text = (ROOT / "rust-toolchain.toml").read_text()
    return re.search(r'channel\s*=\s*"([^"]+)"', text).group(1)


def main():
    missing = []
    channel = toolchain()

    for tool, hint in [
        ("cargo", "install Rust (https://rustup.rs)"),
        ("rustup", "install Rust (https://rustup.rs)"),
        ("python3", "install Python 3"),
        ("make", "install GNU Make"),
        ("clang", "the aarch64 opus library is built with clang (apt install clang)"),
        ("ar", "install binutils (apt install binutils)"),
        ("as", "the BIOS boot code needs GNU as (apt install binutils)"),
        ("ld", "the BIOS boot code needs GNU ld (apt install binutils)"),
    ]:
        if shutil.which(tool) is None:
            missing.append(f"{tool}: not found; {hint}")

    if shutil.which("rustc"):
        probe = subprocess.run(["rustc", f"+{channel}", "--print", "sysroot"], capture_output=True, text=True)
        if probe.returncode != 0:
            missing.append(f"Rust toolchain {channel}: not installed; run `rustup toolchain install {channel} --component rust-src`")
        elif not (Path(probe.stdout.strip()) / "lib/rustlib/src").exists():
            missing.append(f"rust-src for {channel}: not installed; run `rustup component add rust-src --toolchain {channel}`")

    # The Pi's UEFI firmware is downloaded once (pinned by SHA-256) into build/rpi4-firmware.
    if not (ROOT / "build/rpi4-firmware/RPI_EFI.fd").exists():
        try:
            socket.getaddrinfo("github.com", 443)
        except OSError:
            missing.append("Raspberry Pi 4 firmware: build/rpi4-firmware is empty and github.com cannot be reached; run "
                           "`python3 tools/fetch_rpi4_firmware.py` on a machine with internet and copy build/rpi4-firmware here")

    if missing:
        print("make: the build needs the following, which is missing:")
        for line in missing:
            print(f"  - {line}")
        print("(`make image` alone builds only the x86 image, which needs less.)")
        sys.exit(1)


if __name__ == "__main__":
    main()
