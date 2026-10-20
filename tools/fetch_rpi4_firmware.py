#!/usr/bin/env python3
"""Downloads the Raspberry Pi 4 UEFI firmware (pftf/RPi4, edk2) that `make image-pi4` puts on the SD card's first FAT volume.

    python3 tools/fetch_rpi4_firmware.py [DIR]      (default build/rpi4-firmware)

The release is pinned by version and SHA-256, so a changed or tampered download is refused. It is a third-party binary
(edk2 plus the Raspberry Pi firmware blobs, licences in the zip); nothing of ours is sent anywhere. Standard library only."""

import hashlib
import io
import sys
import urllib.request
import zipfile
from pathlib import Path

VERSION = "v1.53"
URL = f"https://github.com/pftf/RPi4/releases/download/{VERSION}/RPi4_UEFI_Firmware_{VERSION}.zip"
SHA256 = "ca9973e2a7a546b3df871cfb7382e656829114b6dfa424f40dc67cc90a217d88"


# Settings we need on top of the pftf config.txt. max_framebuffers=2: without it the Pi 4 firmware drives one framebuffer
# and ignores the display select, so a second HDMI screen cannot get its own picture (kernel `mailbox.rs`).
EXTRA_SETTINGS = ["max_framebuffers=2"]


def add_settings(target):
    config = target / "config.txt"
    lines = config.read_text().splitlines()
    missing = [line for line in EXTRA_SETTINGS if line not in lines]
    if missing:
        config.write_text("\n".join(lines + missing) + "\n")
        print(f"config.txt: added {', '.join(missing)}")


def main():
    target = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent / "build" / "rpi4-firmware")
    if (target / "RPI_EFI.fd").exists():
        print(f"{target} already holds the firmware ({VERSION}).")
        add_settings(target)
        return
    print(f"downloading {URL}")
    with urllib.request.urlopen(URL, timeout=60) as response:
        data = response.read()
    if hashlib.sha256(data).hexdigest() != SHA256:
        sys.exit(f"the download's SHA-256 is not the pinned {SHA256}; refusing it")
    target.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        for name in archive.namelist():
            # No absolute paths or `..` out of the target directory.
            if name.startswith("/") or ".." in Path(name).parts:
                sys.exit(f"unsafe path in the archive: {name}")
        archive.extractall(target)
    print(f"firmware {VERSION} unpacked into {target}")
    add_settings(target)


if __name__ == "__main__":
    main()
