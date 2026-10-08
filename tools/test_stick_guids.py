#!/usr/bin/env python3
"""Fresh random GUIDs per stick (`build_image.randomize_guids`, used by `write_stick.py`).

1. The rewritten GPT is valid: both header CRCs and both entry-array CRCs check out, the
   payload partition's GUID is the one returned and differs from the original and from a
   second randomisation, and nothing outside the GPT sectors changed.
2. A randomised image still boots (UEFI would refuse a GPT with a wrong CRC).
3. Two sticks written from the *same* image, each randomised, are told apart: both are
   attached, the one that was booted is not the first the drivers find, and the kernel must
   still pick the right one as the boot disk (without distinct GUIDs it picked the first)."""

import os
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_image  # noqa: E402
from test_boot import CODE_CANDIDATES, IMAGE, ROOT, VARS_CANDIDATES, check, clean, find  # noqa: E402

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
SECTOR = 512


def gpt_problems(image: bytes):
    """Checks both GPT copies; returns (problems, payload GUID bytes of the primary)."""
    problems = []
    guids = []
    for name, lba in (("primary", 1), ("backup", len(image) // SECTOR - 1)):
        header = bytearray(image[lba * SECTOR:lba * SECTOR + 92])
        stored = struct.unpack_from("<I", header, 16)[0]
        struct.pack_into("<I", header, 16, 0)
        if zlib.crc32(bytes(header)) & 0xFFFFFFFF != stored:
            problems.append(f"{name} header CRC")
        entries_lba, count, size, entries_crc = struct.unpack_from("<QIII", header, 72)
        entries = image[entries_lba * SECTOR:entries_lba * SECTOR + count * size]
        if zlib.crc32(entries) & 0xFFFFFFFF != entries_crc:
            problems.append(f"{name} entry array CRC")
        guids.append(bytes(entries[size + 16:size + 32]))  # entry 1: the payload partition
    if guids[0] != guids[1]:
        problems.append("the two copies disagree on the payload GUID")
    return problems, guids[0]


def boot(images, boot_index):
    """Boots a guest with the images on one AHCI controller (boot_index gets the boot priority);
    returns the serial text up to the FOSFS mount line."""
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-device", "ich9-ahci,id=ahci"]
        for number, path in enumerate(images):
            command += ["-drive", f"if=none,id=d{number},format=raw,file={path}",
                        "-device", f"ide-hd,drive=d{number},bus=ahci.{number}" + (",bootindex=0" if number == boot_index else "")]
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 90
        try:
            while "FOSFS: mounted" not in clean(bytes(raw)) and "FOSFS:" not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    break
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))
            time.sleep(0.3)
            ready, _, _ = select.select([process.stdout], [], [], 0.2)
            if ready:
                raw.extend(os.read(process.stdout.fileno(), 4096))
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
        return clean(bytes(raw))


def default_image_checks():
    """write_stick.py's default image is the one `make image` builds, and it is found from any working directory."""
    printed = subprocess.run(["make", "-s", "print-image"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    check(Path(printed) == build_image.DEFAULT_IMAGE, f"the default image path equals the Makefile's IMAGE ({printed})", str(build_image.DEFAULT_IMAGE))
    # From another directory, with a device that is not a whole disk: the complaint must be about the device, not about a
    # missing image (the old default, "build/fosfor/fosfor.img", only resolved from the repository root).
    result = subprocess.run([sys.executable, str(ROOT / "tools" / "write_stick.py"), "/dev/null"], cwd="/", capture_output=True, text=True)
    check("not a whole-disk block device" in result.stderr and "does not exist" not in result.stderr,
          "write_stick.py finds its default image when run from another directory", result.stderr)


def main():
    default_image_checks()
    original = IMAGE.read_bytes()
    first = bytearray(original)
    guid = build_image.randomize_guids(first)
    problems, payload = gpt_problems(bytes(first))
    check(not problems, "a randomised image has valid GPT CRCs in both copies", repr(problems))
    check(payload == guid.bytes_le, "and carries the payload GUID that was returned", "")
    check(payload != gpt_problems(original)[1], "which differs from the original image's", "")
    second = bytearray(original)
    build_image.randomize_guids(second)
    check(gpt_problems(bytes(second))[1] != payload, "and from a second randomisation's", "")
    gpt_sectors = set(range(1, 34)) | set(range(len(original) // SECTOR - 33, len(original) // SECTOR))
    changed = {at // SECTOR for at in range(0, len(original), SECTOR) if original[at:at + SECTOR] != first[at:at + SECTOR]}
    check(changed <= gpt_sectors, "only GPT sectors changed", repr(sorted(changed - gpt_sectors)[:5]))

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        single = tmp / "single.img"
        single.write_bytes(first)
        text = boot([single], 0)
        check("Boot disk: disk1" in text and "FOSFS: mounted on disk1" in text, "a randomised image boots and finds itself", text[-1500:])

        # Two sticks from one build, told apart only by their random GUIDs and their files.
        images = []
        for name, extra in (("a", [("only-a", b"a\n")]), ("b", [("only-b", b"b\n"), ("also-b", b"b\n")])):
            image = bytearray(build_image.build_image(LOADER.read_bytes(), KERNEL.read_bytes(),
                                                      [("readme.txt", b"hi\n")] + extra))
            build_image.randomize_guids(image)
            path = tmp / f"{name}.img"
            path.write_bytes(image)
            images.append(path)
        text = boot(images, 1)  # b is booted but found second by the drivers
        check("renumbering it to disk1" in text, "the booted stick, found second, is renumbered to disk1", text[-1500:])
        check("FOSFS: mounted on disk1 (32 MiB, 3 file(s))" in text, "and it is the right stick (b: three files), not the first one found", text[-1500:])
    print("stick guids: all checks passed")


if __name__ == "__main__":
    main()
