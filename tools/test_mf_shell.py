#!/usr/bin/env python3
"""The server kernel's shell gets the shared file commands (`crates/shell`: ls cat write rm cp mv cmp mkdir, docs/shell.md) through
`kernel-mf/src/shellhost.rs`, the same code the embedded kernel's shell runs. Checks: ls shows a file of the boot volume, cat reads it,
write/cat/cp/cmp/mv/rm/mkdir round-trip through the disk, help NAME comes from the shared table, and an unknown command is reported.
QEMU only."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_debug_mode as base  # noqa: E402
from test_boot import ROOT, check  # noqa: E402

base.KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf"
READY = "entering the service loop"


def config_volume_scenario():
    """`fstab` mounts a second FOSFS volume on `settings`: its `hostname` is read from there, not from the boot volume's root."""
    import subprocess
    import uuid
    with tempfile.TemporaryDirectory() as tmp:
        hostname = Path(tmp) / "hostname"
        hostname.write_text("cfgmf\n")
        settings = Path(tmp) / "settings"
        settings.write_text("keymap sv\nforeground amber\nbackground blue\nhwp performance\n")
        config = Path(tmp) / "config.img"
        subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(base.LOADER), "--kernel", str(base.KERNEL),
                        "--output", str(config), "--label", "cfgvol", "--file", f"hostname={hostname}", "--file", f"settings={settings}",
                        "--payload-guid", str(uuid.uuid4()), "--no-bios"], check=True, capture_output=True)
        machine = base.Machine(tmp, {"fstab": "settings @cfgvol\n", "hostname": "bootmf\n"}, {})
        machine.extra = ["-drive", f"format=raw,file={config}"]
        machine.start()
        try:
            machine.wait_for(READY)
            text = machine.text()
            check("read from @cfgvol (mounted at settings)" in text, "a settings mount is read at boot", text[-600:])
            check("announces itself as cfgmf1" in text and "bootmf" not in text, "the mounted volume's hostname wins over the root's", text[-600:])
            check("settings: keymap sv" in text and "settings: foreground amber." in text and "settings: background blue." in text,
                  "keymap and the console colours are applied from the mounted settings", text[-800:])
            check("hwp is not supported by the server kernel yet" in text, "an option mf cannot honour says so", text[-800:])
        finally:
            machine.stop()


def ext4_scenario():
    """`ext4 diskN` reads a Linux volume (crates/ext4, docs/ext4.md): ls, cat (UTF-8), info; a disk without ext4 is named."""
    import shutil
    import subprocess
    mkfs = shutil.which("mkfs.ext4") or "/usr/sbin/mkfs.ext4"
    if not Path(mkfs).exists():
        print("SKIP ext4: no mkfs.ext4")
        return
    with tempfile.TemporaryDirectory() as tmp:
        source = Path(tmp) / "src"
        (source / "sub").mkdir(parents=True)
        (source / "hello.txt").write_text("hello from linux\n")
        (source / "sub" / "Räksmörgås.txt").write_text("hej på dig\n")
        image = Path(tmp) / "linux.img"
        image.write_bytes(b"")
        with image.open("r+b") as handle:
            handle.truncate(32 << 20)
        subprocess.run([mkfs, "-q", "-F", "-L", "linuxvol", "-d", str(source), str(image)], check=True, capture_output=True)
        machine = base.Machine(tmp, {"hostname": "mfext4\n"}, {}, fosfs=4)
        machine.extra = ["-drive", f"format=raw,file={image}"]
        machine.start()
        try:
            machine.wait_for(READY)
            machine.connect()
            out = machine.run("ext4 disk2")
            check("hello.txt" in out and "sub" in out, "ext4 ls lists the root", out[-300:])
            out = machine.run("ext4 disk2 ls /sub")
            check("Räksmörgås.txt" in out, "ext4 ls shows a Unicode name", out[-300:])
            out = machine.run("ext4 disk2 cat /sub/Räksmörgås.txt")
            check("hej på dig" in out, "ext4 cat reads it as UTF-8", out[-300:])
            out = machine.run("ext4 disk2 info")
            check("linuxvol" in out, "ext4 info shows the label", out[-300:])
            out = machine.run("ext4 disk2 cat /nothere")
            check("not found" in out, "a missing file is reported", out[-300:])
            out = machine.run("ext4 disk1")
            check("no ext2/3/4" in out, "the FOSFS boot disk is not ext", out[-300:])
        finally:
            machine.stop()


def ext4_write_scenario():
    """`ext4 diskN mkfs|mkdir|write|mv|rm`: a partition is added after a shrunk FOSFS one, formatted, written; e2fsck agrees."""
    import subprocess
    import uuid
    e2fsck = "/usr/sbin/e2fsck"
    with tempfile.TemporaryDirectory() as tmp:
        second = Path(tmp) / "second.img"
        subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(base.LOADER), "--kernel", str(base.KERNEL),
                        "--output", str(second), "--label", "extwrite", "--fosfs", "4",
                        "--payload-guid", str(uuid.uuid4()), "--no-bios"], check=True, capture_output=True)
        machine = base.Machine(tmp, {"hostname": "mfext4w\n"}, {}, fosfs=4)
        machine.extra = ["-drive", f"format=raw,file={second}"]
        machine.start()
        try:
            machine.wait_for(READY)
            machine.connect()
            machine.run("shrink disk2 8M")
            machine.run("shrink disk2 confirm")
            out = machine.run("ext4 disk2 mkfs scratch")
            check("ext4 disk2 mkfs confirm" in out, "mkfs announces and asks", out[-400:])
            machine.run("echo interrupt")
            out = machine.run("ext4 disk2 mkfs confirm")
            check("nothing is waiting" in out, "another command cancels the mkfs", out[-300:])
            machine.run("ext4 disk2 mkfs scratch")
            out = machine.run("ext4 disk2 mkfs confirm")
            check("ext4: formatted" in out, "confirm formats the new partition", out[-400:])
            out = machine.run("ext4 disk2 info")
            check("scratch" in out, "the new volume has its label", out[-300:])
            machine.run("ext4 disk2 mkdir /docs")
            machine.run("ext4 disk2 write /docs/Räksmörgås.txt hej på dig")
            out = machine.run("ext4 disk2 cat /docs/Räksmörgås.txt")
            check("hej på dig" in out, "write makes a file in a new folder", out[-300:])
            machine.run("ext4 disk2 mv /docs/Räksmörgås.txt /moved.txt")
            out = machine.run("ext4 disk2 ls")
            check("moved.txt" in out and "docs" in out, "mv moves across folders", out[-300:])
            out = machine.run("ext4 disk2 rm /docs")
            check("rm" not in out or "not" not in out, "an empty folder is removed", out[-300:])
            out = machine.run("ext4 disk2 rm /nothere")
            check("not found" in out, "a missing name is reported", out[-300:])
        finally:
            machine.stop()
        if Path(e2fsck).exists():
            # The partition starts at 1 MiB-aligned sector after the 8 MiB FOSFS one; check it from the host.
            import re
            dump = subprocess.run(["sfdisk", "-d", str(second)], capture_output=True, text=True).stdout
            match = re.search(r"start=\s*(\d+), size=\s*(\d+), type=0FC63DAF", dump)
            check(match is not None, "the partition table lists a Linux partition", dump)
            if match:
                start, size = int(match.group(1)), int(match.group(2))
                part = Path(tmp) / "part.img"
                with second.open("rb") as src, part.open("wb") as dst:
                    src.seek(start * 512)
                    dst.write(src.read(size * 512))
                done = subprocess.run([e2fsck, "-fn", str(part)], capture_output=True, text=True)
                check(done.returncode == 0, "e2fsck finds the volume clean", done.stdout[-400:])
                cat = subprocess.run(["/usr/sbin/debugfs", "-R", "cat /moved.txt", str(part)], capture_output=True)
                check(cat.stdout == "hej på dig".encode(), "debugfs reads what the kernel wrote", repr(cat.stdout))


def shrink_scenario():
    """`shrink diskN SIZE` (kernel/src/formatshell.rs): sizes with units, a confirmation, files kept, the partition cut."""
    import subprocess
    import uuid
    with tempfile.TemporaryDirectory() as tmp:
        data = Path(tmp) / "keep.txt"
        data.write_text("still here after the shrink\n")
        second = Path(tmp) / "second.img"
        subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(base.LOADER), "--kernel", str(base.KERNEL),
                        "--output", str(second), "--label", "shrinkme", "--file", f"keep.txt={data}", "--fosfs", "4",
                        "--payload-guid", str(uuid.uuid4()), "--no-bios"], check=True, capture_output=True)
        machine = base.Machine(tmp, {"hostname": "mfshrink\n"}, {}, fosfs=4)
        machine.extra = ["-drive", f"format=raw,file={second}"]
        machine.start()
        try:
            machine.wait_for(READY)
            machine.connect()
            out = machine.run("shrink disk2")
            check("FOSFS partition" in out and "Usage: shrink" in out, "shrink without a size reports the sizes", out[-400:])
            out = machine.run("shrink disk2 banana")
            check("not a size" in out, "a bad size is refused", out[-300:])
            out = machine.run("shrink disk2 64G")
            check("not smaller" in out, "a size that is not smaller is refused", out[-300:])
            out = machine.run("shrink disk2 confirm")
            check("nothing is waiting" in out, "confirm without a plan does nothing", out[-300:])
            out = machine.run("shrink disk2 8MB")
            check("will become 8 MiB" in out and "shrink disk2 confirm" in out, "a plan with units is announced", out[-400:])
            machine.run("echo interrupt")
            out = machine.run("shrink disk2 confirm")
            check("nothing is waiting" in out, "another command cancels the plan", out[-300:])
            machine.run("shrink disk2 8M")
            out = machine.run("shrink disk2 confirm")
            check("shrunk from" in out and "to 8 MiB" in out, "confirm shrinks the partition", out[-400:])
            out = machine.run("shrink disk2")
            check("partition 8 MiB" in out, "the partition table says 8 MiB now", out[-400:])
            out = machine.run("cat disk2/keep.txt")
            check("still here after the shrink" in out, "the files are kept", out[-300:])
        finally:
            machine.stop()


def main():
    with tempfile.TemporaryDirectory() as tmp:
        machine = base.Machine(tmp, {"hostname": "mfshell\n", "start": "# comment\necho from-start-file\n"}, {}, fosfs=4)
        machine.start()
        try:
            machine.wait_for(READY)
            check("start> echo from-start-file" in machine.text() and "start file: running 1 command" in machine.text(), "the start file runs at boot", machine.text()[-400:])
            machine.connect()
            out = machine.run("ls")
            check("hostname" in out, "ls lists the boot volume", out[-400:])
            out = machine.run("cat hostname")
            check("mfshell" in out, "cat reads a file", out[-300:])
            out = machine.run("write note.txt hello from mf")
            out = machine.run("cat note.txt")
            check("hello from mf" in out, "write makes a file and cat reads it back", out[-300:])
            machine.run("write Räksmörgås.txt hej på dig")
            out = machine.run("ls")
            check("Räksmörgås.txt" in out, "ls lists a file whose name has å ä ö", out[-300:])
            out = machine.run("cat Räksmörgås.txt")
            check("hej på dig" in out, "cat reads a file by its Unicode name", out[-300:])
            machine.run("rm Räksmörgås.txt")
            out = machine.run("cp note.txt copy.txt")
            out = machine.run("cmp note.txt copy.txt")
            check("identical" in out or "same" in out, "cp copies and cmp says they match", out[-300:])
            out = machine.run("mv copy.txt moved.txt")
            out = machine.run("ls")
            check("moved.txt" in out and "copy.txt" not in out, "mv renames", out[-400:])
            out = machine.run("rm moved.txt")
            out = machine.run("ls")
            check("moved.txt" not in out and "note.txt" in out, "rm removes", out[-400:])
            out = machine.run("cat nothere.txt")
            check("no such file" in out, "a missing file is reported", out[-300:])
            out = machine.run("help ls")
            check("ls [" in out, "help NAME shows the shared usage line", out[-300:])
            out = machine.run("help")
            check("ls cat write" in out, "help lists the file commands", out[-400:])
            out = machine.run("dmesg")
            check("kernel" in out.lower() and len(out) > 500, "dmesg replays the boot log", out[-300:])
            out = machine.run("sector disk1 1")
            check("EFI PART" in out, "sector dumps the GPT header", out[-400:])
            out = machine.run("sector disk1")
            check("Usage: sector" in out, "sector without an LBA prints usage", out[-300:])
            out = machine.run("mount data @scratch")
            out = machine.run("mount")
            check("data -> @scratch" in out, "mount adds and lists a mount point", out[-300:])
            out = machine.run("umount data")
            check("no longer" in out, "umount removes it", out[-300:])
            out = machine.run("format disk1")
            check("boot disk" in out, "format refuses the boot disk", out[-300:])
            out = machine.run("format disk1 confirm")
            check("boot disk" in out, "format confirm on the boot disk is refused too", out[-300:])
            out = machine.run("format disk9")
            check("no such disk" in out, "format names a missing disk", out[-300:])
            out = machine.run("sha256 hostname")
            check("mfshell" not in out and "hostname" in out and len([w for w in out.split() if len(w) == 64]) >= 1, "sha256 prints a digest", out[-300:])
            out = machine.run("fsck")
            check("fsck" in out.lower() or "files" in out.lower() or "ok" in out.lower(), "fsck checks the boot volume", out[-400:])
            out = machine.run("label")
            check("disk1" in out, "label lists the volumes", out[-300:])
            out = machine.run("grow disk9")
            check("no such disk" in out, "grow names a missing disk", out[-300:])
            out = machine.run("install")
            check("install" in out.lower(), "install answers", out[-400:])
            out = machine.run("bootcode")
            check("boot" in out.lower(), "bootcode reports the boot code", out[-400:])
            out = machine.run("start")
            check("start> echo from-start-file" in out, "the start command reruns the start file", out[-300:])
            out = machine.run("echo åäöÅÄÖ")
            check("åäöÅÄÖ" in out.split("echo")[-1], "the shell takes UTF-8 å ä ö", out[-200:])
            out = machine.run("echo abc\x7fåx\x7f\x7f")
            check(out.count("\x08 \x08") >= 3 and "\n" in out, "Backspace removes a whole å", out[-200:])
            out = machine.run("keymap")
            check("us" in out, "keymap shows the default", out[-200:])
            out = machine.run("keymap sv")
            check("sv" in out, "keymap sv switches", out[-200:])
            out = machine.run("keymap xx")
            check("sv" not in out.split("keymap xx")[-1] or "unknown" in out.lower() or "usage" in out.lower(), "keymap refuses an unknown layout", out[-200:])
            machine.run("keymap us")
            out = machine.run("io r8 0x70")
            check("0x0070 ->" in out, "io reads a port", out[-200:])
            out = machine.run("io r64 0x70")
            check("Usage" in out, "io refuses a bad width", out[-200:])
            out = machine.run("sleep 1")
            check("Slept 1 s." in out, "sleep sleeps", out[-200:])
            out = machine.run("memmap")
            check("usable" in out and "MiB of conventional" in out, "memmap lists the map", out[-300:])
            out = machine.run("bootinfo")
            check("RSDP" in out and "graphics" in out, "bootinfo prints the hand-over", out[-300:])
            out = machine.run("cpuinfo")
            check("vendor" in out and "features" in out, "cpuinfo prints CPUID", out[-300:])
            out = machine.run("bogus")
            check("Unknown command" in out, "an unknown command is reported", out[-300:])
        finally:
            machine.stop()
    config_volume_scenario()
    ext4_scenario()
    ext4_write_scenario()
    shrink_scenario()
    print("test-mf-shell: all checks passed")


if __name__ == "__main__":
    main()
