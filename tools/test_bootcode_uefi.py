#!/usr/bin/env python3
"""`bootcode`: replace the UEFI loader (`\\EFI\\BOOT\\BOOTX64.EFI` on the EFI System Partition) of a running fos disk.

The image is built with `loader.efi` on its boot volume: the real loader with one word of its banner changed ("running" ->
"RUNNING") and 100 KB of padding appended (so the new file needs a longer run of clusters than the old one). Booted through OVMF:
 * `bootcode` shows the installed loader and that the file's loader differs from it;
 * `bootcode update` only announces (any other command cancels it); `bootcode update confirm` writes the new loader beside the old
   one and switches it in with one directory write, reading it back;
 * the ESP, taken out of the disk image afterwards, passes `fsck.vfat` and 7-Zip reads the new loader from it;
 * after a `reset` the second boot prints the *changed* banner: the new loader was written and runs;
 * bad inputs are refused before anything is written: a file that is not a PE image, one for another machine, and one without the
   loader's banner.
Run with `make test-bootcode-uefi`."""
import os
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import CODE_CANDIDATES, IMAGE, ROOT, VARS_CANDIDATES, check, clean, find  # noqa: E402

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
OLD, NEW = b"UEFI application running.", b"UEFI application RUNNING."
ESP_TYPE = bytes.fromhex("28732ac11ff8d211ba4b00a0c93ec93b")


class Machine:
    def __init__(self, tmp, image):
        code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        self.process = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
             "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
             "-drive", f"format=raw,file={image}"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.raw = bytearray()

    def text(self):
        return clean(bytes(self.raw))

    def wait_for(self, marker, timeout=90, after=0):
        deadline = time.monotonic() + timeout
        while marker not in self.text()[after:]:
            if time.monotonic() > deadline or self.process.poll() is not None:
                raise SystemExit(f"FAIL: never saw {marker!r}\n{self.text()[after:][-2000:]}")
            ready, _, _ = select.select([self.process.stdout], [], [], 0.2)
            if ready:
                self.raw.extend(os.read(self.process.stdout.fileno(), 4096))

    def run(self, command, marker="fos> ", timeout=60):
        mark = len(self.text())
        self.process.stdin.write(command.encode() + b"\n")
        self.process.stdin.flush()
        self.wait_for(marker, timeout, mark + len(command))
        return self.text()[mark:]

    def close(self):
        self.process.kill()
        self.process.wait()


def build(tmp, name, loader):
    """An image whose boot volume holds loader.efi with the given contents (None = no such file)."""
    files = []
    if loader is not None:
        path = tmp / f"{name}-loader.efi"
        path.write_bytes(loader)
        files += ["--file", f"loader.efi={path}"]
    image = tmp / f"{name}.img"
    subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
                    "--output", str(image), *files], check=True, capture_output=True)
    return image


def esp_of(image, destination):
    """Copies the EFI System Partition out of a GPT disk image."""
    data = image.read_bytes()
    entries_lba, count, size = struct.unpack_from("<QII", data, 512 + 72)
    for index in range(count):
        entry = data[entries_lba * 512 + index * size:][:size]
        if entry[:16] == ESP_TYPE:
            first, last = struct.unpack_from("<QQ", entry, 32)
            destination.write_bytes(data[first * 512:(last + 1) * 512])
            return destination
    raise SystemExit("FAIL: no EFI System Partition in the image")


def main():
    loader = LOADER.read_bytes()
    if OLD not in loader:
        sys.exit("the loader does not contain the banner text this test patches")
    patched = loader.replace(OLD, NEW) + bytes(100 * 1024)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image = build(tmp, "good", patched)
        machine = Machine(tmp, image)
        try:
            machine.wait_for("fos> ")
            check("Fosfor OS loader (milestone M1): UEFI application running." in machine.text(), "the first boot runs the original loader", machine.text()[:1500])
            shown = machine.run("bootcode")
            check("UEFI loader: \\EFI\\BOOT\\BOOTX64.EFI" in shown and f"{len(loader)} bytes" in shown, "`bootcode` shows the installed loader", shown)
            check("different from the installed loader" in shown, "and that loader.efi on the boot volume differs from it", shown)

            announced = machine.run("bootcode update")
            check("the UEFI loader" in announced and "replaces \\EFI\\BOOT\\BOOTX64.EFI" in announced and "To go ahead, type exactly: bootcode update confirm" in announced,
                  "`bootcode update` only announces what it would do", announced)
            machine.run("uptime")
            stale = machine.run("bootcode update confirm")
            check("nothing is waiting for confirmation" in stale, "any other command in between cancels the announcement", stale)

            machine.run("bootcode update")
            done = machine.run("bootcode update confirm", timeout=120)
            check("Done: the loader reads back identical" in done, "`bootcode update confirm` writes the loader, switches it in and reads it back", done)
            again = machine.run("bootcode")
            check("identical to the installed loader" in again, "afterwards the installed loader is the file's loader", again)
        finally:
            pass
        # Independent verification: the ESP, taken out of the image file (QEMU wrote it through the virtual disk), is a valid FAT32
        # volume and holds the new loader. The guest is still running, so give its writes a moment to land.
        time.sleep(1)
        esp = esp_of(image, tmp / "esp.img")
        fsck = subprocess.run(["fsck.vfat", "-n", str(esp)], capture_output=True, text=True)
        check(fsck.returncode == 0, "the ESP passes fsck.vfat after the update", fsck.stdout + fsck.stderr)
        if shutil.which("7z"):
            listing = subprocess.run(["7z", "l", str(esp)], capture_output=True, text=True).stdout
            check("bootx64.new" not in listing.lower(), "no BOOTX64.NEW is left behind", listing)
            # 7-Zip wants the name as stored (upper case from the image builder): take it from the listing.
            path = next((line.split()[-1] for line in listing.splitlines() if line.lower().endswith("efi/boot/bootx64.efi")), "EFI/BOOT/BOOTX64.EFI")
            read = subprocess.run(["7z", "e", "-so", str(esp), path], capture_output=True).stdout
            check(read == patched, "7-Zip reads the new loader from \\EFI\\BOOT\\BOOTX64.EFI", f"{len(read)} bytes")

        # The new loader runs.
        mark = len(machine.text())
        machine.process.stdin.write(b"reset\n")
        machine.process.stdin.flush()
        machine.wait_for("Fosfor OS loader (milestone M1): UEFI application RUNNING.", 120, mark)
        machine.wait_for("fos> ", 120, mark)
        check(True, "after a reset the second boot prints the changed banner: the new loader runs, and the kernel comes up")
        machine.close()

        # Bad inputs are refused before anything is written.
        other_machine = bytearray(loader)
        struct.pack_into("<H", other_machine, struct.unpack_from("<I", loader, 0x3c)[0] + 4, 0x14c)   # the PE machine field: i386
        for label, bad, expect in (
            ("a file that is not a PE image", b"this is not a loader", "MZ signature"),
            ("an image for another machine", other_machine, "x86-64"),
            ("a loader without the banner", loader.replace(b"Fosfor OS loader", b"Some other loader"), "banner"),
        ):
            image = build(tmp, "bad", bytes(bad))
            machine = Machine(tmp, image)
            try:
                machine.wait_for("fos> ")
                refused = machine.run("bootcode update")
                check(expect in refused and "To go ahead" not in refused, f"{label} is refused", refused)
            finally:
                machine.close()
        image = build(tmp, "none", None)
        machine = Machine(tmp, image)
        try:
            machine.wait_for("fos> ")
            nothing = machine.run("bootcode update")
            check("put stage1.bin and stage2.bin" in nothing and "To go ahead" not in nothing, "an update with no files on the boot volume says what to put there", nothing)
        finally:
            machine.close()
    print("bootcode uefi: all checks passed")


if __name__ == "__main__":
    main()
