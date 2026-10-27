#!/usr/bin/env python3
"""Adds the `.fos_build` stamp (see crates/tape-sync) to a linked tape: `stamp_tape.py IN OUT`.

The stamp is 16 bytes: the build time (u64, seconds since 1970), the tape ABI version the tape is built for (u32), the CPU
(u8: 0 x86-64, 1 aarch64, from the ELF machine) and three reserved zero bytes. The build time is the commit time of HEAD when the working tree is clean, so two machines
that build the same commit stamp the same value; with local changes it is the current time, so a dirty build counts
as newer than the commit it started from. If OUT already exists as the same program (same bytes without the stamp, same ABI and CPU) it is left alone, so a rebuild that
changed nothing keeps its build time and a deploy or `tapesync` does not send the tape again. `tapesync` compares these stamps to decide which machine has the newer tape.

The input is never modified (cargo owns it); OUT is a copy with the section added (GNU objcopy, as the BIOS boot chain
already needs binutils). The section is not loaded: the kernel's ELF loader only maps program headers.
"""
import glob
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def abi_version():
    text = open(os.path.join(ROOT, "abi", "src", "tape.rs")).read()
    return int(re.search(r"pub const TAPE_API_VERSION: u32 = (\d+);", text).group(1))


def tape_arch(path):
    """The stamp's CPU byte from the ELF e_machine: 0 x86-64, 1 aarch64 (crates/tape-sync ARCH_*)."""
    with open(path, "rb") as handle:
        header = handle.read(20)
    machine = int.from_bytes(header[18:20], "little")
    return {62: 0, 183: 1}[machine]


def build_time():
    forced = os.environ.get("FOS_BUILD_TIME")
    if forced:
        return int(forced)
    try:
        dirty = subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip()
        if not dirty:
            return int(subprocess.run(["git", "log", "-1", "--format=%ct"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip())
    except (OSError, subprocess.CalledProcessError, ValueError):
        pass
    return int(time.time())


def objcopy_for(arch):
    """GNU objcopy only reads x86-64 ELF; an aarch64 tape needs the LLVM one that ships with the Rust toolchain."""
    if arch == 0:
        return "objcopy"
    candidates = [shutil.which(name) for name in ("llvm-objcopy", "aarch64-linux-gnu-objcopy", "rust-objcopy")]
    candidates += sorted(glob.glob(os.path.expanduser("~/.rustup/toolchains/*/lib/rustlib/*/bin/rust-objcopy")), reverse=True)
    for found in filter(None, candidates):
        # Some installed toolchains lack their libLLVM; use the first copy that starts.
        if subprocess.run([found, "--version"], capture_output=True).returncode == 0:
            return found
    sys.exit("stamp_tape.py: no working objcopy for aarch64 (install llvm-objcopy or `rustup component add llvm-tools`)")


def without_stamp(arch, path):
    """The bytes of a tape with its `.fos_build` section removed."""
    with tempfile.NamedTemporaryFile(delete=False) as handle:
        bare = handle.name
    try:
        subprocess.run([objcopy_for(arch), "--remove-section", ".fos_build", path, bare], check=True)
        with open(bare, "rb") as handle:
            return handle.read()
    finally:
        os.unlink(bare)


def unchanged(arch, old, new, stamp):
    """True when `old` (a stamped tape from an earlier build) is the same program as `new` with the same ABI and CPU: it
    then keeps its build time, so a rebuild that changed nothing does not make a deploy or `tapesync` send the tape again."""
    if not os.path.exists(old):
        return False
    try:
        with open(old, "rb") as handle:
            data = handle.read()
        from_old = elf_stamp(data)
        return from_old is not None and from_old[1:] == struct.unpack("<QIB3x", stamp)[1:] and without_stamp(arch, old) == without_stamp(arch, new)
    except (OSError, subprocess.CalledProcessError):
        return False


def elf_stamp(data):
    """(build time, ABI, CPU) from an ELF's `.fos_build` section, or None."""
    shoff, shentsize, shnum, shstrndx = struct.unpack_from("<Q", data, 0x28)[0], *struct.unpack_from("<HHH", data, 0x3A)
    sections = [struct.unpack_from("<IIQQQQIIQQ", data, shoff + index * shentsize) for index in range(shnum)]
    names = sections[shstrndx]
    for section in sections:
        start = names[4] + section[0]
        if data[start:data.index(b"\0", start)] == b".fos_build":
            return struct.unpack_from("<QIB", data, section[4])
    return None


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: stamp_tape.py IN OUT")
    source, destination = sys.argv[1:]
    arch = tape_arch(source)
    stamp = struct.pack("<QIB3x", build_time(), abi_version(), arch)
    with tempfile.NamedTemporaryFile(delete=False) as handle:
        handle.write(stamp)
    try:
        subprocess.run(
            [objcopy_for(arch), "--remove-section", ".fos_build", "--add-section", f".fos_build={handle.name}", "--set-section-flags", ".fos_build=noload,readonly", source, destination + ".tmp"],
            check=True,
        )
        if unchanged(arch, destination, destination + ".tmp", stamp):
            os.unlink(destination + ".tmp")
        else:
            os.replace(destination + ".tmp", destination)
    finally:
        os.unlink(handle.name)


if __name__ == "__main__":
    main()
