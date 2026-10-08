#!/usr/bin/env python3
"""`bootcode`: replace the BIOS boot code (stage one in the MBR, stage two at LBA 64) of a running fos disk, without rewriting it.

The image is built with stage1.bin and stage2.bin as files on its boot volume, where stage2.bin is the real stage two with one
letter of its banner changed ("(BIOS path)" -> "(BIOS PATH)"). Booted through SeaBIOS:
 * `bootcode` reports the installed code and that the files' stage two differs from it;
 * `bootcode update` only announces; any other command in between cancels it; `bootcode update confirm` then writes stage two
   and stage one's code, reading both back;
 * after a `reboot` the second boot prints the *changed* banner: the new stage two was written and runs;
 * bad inputs are refused before anything is written: a missing file, a stage one that is not one sector, a stage two that is not
   stage two, and a disk that is not a fos disk's.
Run with `make test-bios-bootcode`."""
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import IMAGE, ROOT, check, clean  # noqa: E402

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
STAGE1 = ROOT / "build/bios/stage1.bin"
STAGE2 = ROOT / "build/bios/stage2.bin"
OLD, NEW = b"(BIOS path)", b"(BIOS PATH)"


class Machine:
    def __init__(self, image):
        self.process = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "q35", "-m", "256", "-display", "none", "-serial", "stdio",
             "-drive", f"format=raw,file={image}"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.raw = bytearray()

    def text(self):
        return clean(bytes(self.raw))

    def wait_for(self, marker, timeout=60, after=0):
        deadline = time.monotonic() + timeout
        while marker not in self.text()[after:]:
            if time.monotonic() > deadline or self.process.poll() is not None:
                raise SystemExit(f"FAIL: never saw {marker!r}\n{self.text()[after:][-2000:]}")
            ready, _, _ = select.select([self.process.stdout], [], [], 0.2)
            if ready:
                self.raw.extend(os.read(self.process.stdout.fileno(), 4096))

    def run(self, command, marker="fos> ", timeout=40):
        mark = len(self.text())
        self.process.stdin.write(command.encode() + b"\n")
        self.process.stdin.flush()
        self.wait_for(marker, timeout, mark + len(command))
        return self.text()[mark:]

    def close(self):
        self.process.kill()
        self.process.wait()


def build(tmp, name, stage1, stage2):
    """An image whose boot volume holds stage1.bin and stage2.bin with the given contents (None = leave the file out)."""
    files = []
    for file_name, content in (("stage1.bin", stage1), ("stage2.bin", stage2)):
        if content is not None:
            path = tmp / f"{name}-{file_name}"
            path.write_bytes(content)
            files += ["--file", f"{file_name}={path}"]
    image = tmp / f"{name}.img"
    subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
                    "--output", str(image), *files], check=True, capture_output=True)
    return image


def main():
    stage1, stage2 = STAGE1.read_bytes(), STAGE2.read_bytes()
    if OLD not in stage2:
        sys.exit("stage2.bin does not contain the banner this test patches")
    patched = stage2.replace(OLD, NEW)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)

        # --- a real update, and proof that the new stage two runs ---
        machine = Machine(build(tmp, "good", stage1, patched))
        try:
            machine.wait_for("fos> ", 90)
            check("Fosfor stage 2 (BIOS path)" in machine.text(), "the disk boots with the original stage two", machine.text()[:600])
            report = machine.run("bootcode")
            check("stage one present in the MBR" in report and "different from the installed one" in report,
                  "`bootcode` shows the installed boot code and that the file's stage two differs", report)
            announce = machine.run("bootcode update")
            check("bootcode update confirm" in announce and "LBA 64-87" in announce, "`bootcode update` only announces", announce)
            machine.run("uptime")
            cancelled = machine.run("bootcode update confirm")
            check("nothing is waiting" in cancelled, "another command in between cancels it", cancelled)
            machine.run("bootcode update")
            done = machine.run("bootcode update confirm")
            check("Done: both read back identical" in done, "`bootcode update confirm` writes both and verifies them", done)
            report = machine.run("bootcode")
            check("identical to the installed one" in report, "`bootcode` now says the installed stage two is the file's", report)
            before = len(machine.text())
            machine.process.stdin.write(b"reboot\n")
            machine.process.stdin.flush()
            machine.wait_for("Fosfor stage 2", 90, before)
            machine.wait_for("fos> ", 90, before)
            second = machine.text()[before:]
            check("Fosfor stage 2 (BIOS PATH)" in second and "(BIOS path)" not in second,
                  "after a reboot the NEW stage two runs (its banner differs)", second[:600])
            check("A20 line: ok" in second and "Entering long mode" in second, "and it boots the kernel as before", second[:1500])
        finally:
            machine.close()

        # --- refusals: nothing may be written ---
        cases = (
            ("no files", build(tmp, "none", None, None), "there is no stage1.bin"),
            ("no stage two", build(tmp, "no2", stage1, None), "there is no stage2.bin"),
            ("stage one of the wrong size", build(tmp, "short", stage1[:100], stage2), "exactly 512 bytes"),
            ("stage one with code in the partition table", build(tmp, "table", stage1[:450] + b"\x01" + stage1[451:], stage2), "partition-table area"),
            ("a stage two that is not stage two", build(tmp, "junk", stage1, b"\xfa" + bytes(200)), "lacks stage two's banner"),
        )
        for label, image, expected in cases:
            machine = Machine(image)
            try:
                machine.wait_for("fos> ", 90)
                answer = machine.run("bootcode update")
                check(expected in answer and "confirm" not in answer.replace("bootcode: ", ""), f"{label}: refused ({expected})", answer)
                if label == "no files":
                    check("no such disk" in machine.run("bootcode update disk9"), "an unknown disk is refused", machine.text()[-300:])
            finally:
                machine.close()
    print("bios bootcode: all checks passed")


if __name__ == "__main__":
    main()
