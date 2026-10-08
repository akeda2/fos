#!/usr/bin/env python3
"""Diagnostics for what is on the boot disk: `sector diskN LBA`, the metadata write-back check in stage two, and the reason
`debug slots` gives when the slot-metadata sector is not valid. Boots through SeaBIOS on a private copy of the image.

 * stage two reads the metadata sector back after writing it and says so ("written back and read back identical");
 * `sector disk1 0` shows the MBR (stage one's code, the protective partition entry, the 0x55 0xaa signature), `sector disk1 1`
   the GPT header, `sector disk1 N` (N = the payload partition's first sector) the metadata ("FOSM", version 1);
 * the host then overwrites that sector with zeros behind the running kernel's back, and `debug slots` must say that the sector
   is not valid and show how it starts (this is what a real board that cannot read its metadata needs to show).
Run with `make test-bios-sector`."""
import shutil
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_image  # noqa: E402
from test_bios_video import Machine  # noqa: E402
from test_boot import IMAGE, check  # noqa: E402


def main():
    payload = build_image.PAYLOAD_START
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image = tmp / "copy.img"
        shutil.copy(IMAGE, image)
        machine = Machine(tmp, image)
        try:
            text = machine.wait_for("fos> ")
            check("Slot metadata written back and read back identical." in text, "stage two verifies its metadata write-back", text[:3000])

            def run(command, marker):
                mark = machine.send(command)
                machine.wait_for(marker, 20, mark)
                machine.wait_for("fos> ", 20, mark)
                return machine.text()[mark:]

            mbr = run("sector disk1 0", "1f0")
            check("disk1 sector 0:" in mbr and "55 aa" in mbr.split("1f0")[-1][:60], "`sector disk1 0` shows the MBR with its 0x55 0xaa signature", mbr)
            check("ee" in mbr.split("1b0")[-1][:40] or "ee" in mbr, "and the protective partition entry (type 0xee)", mbr)
            gpt = run("sector disk1 1", "EFI PART")
            check("EFI PART" in gpt, "`sector disk1 1` shows the GPT header", gpt)
            meta = run(f"sector disk1 {payload}", "FOSM")
            check("FOSM" in meta and "46 4f 53 4d 01" in meta, "`sector disk1 N` shows the slot metadata (FOSM, version 1)", meta)
            multi = run("sector disk1 0 2", "disk1 sector 1:")
            check("disk1 sector 0:" in multi and "disk1 sector 1:" in multi, "a COUNT dumps several sectors", multi)
            bad = run("sector disk1 999999999", "failed")
            check("read failed" in bad, "a sector beyond the disk is a clear error", bad)

            modes = run("videomodes", "mode(s) from the video BIOS")
            machine.wait_for("fos> ", 20)
            modes = machine.text()[machine.text().rindex("fos> videomodes") if "fos> videomodes" in machine.text() else 0:]
            check("EDID call: AX=004f" in modes and "native size from EDID: 1280x800" in modes, "`videomodes` shows what the monitor told stage two (QEMU's 1280x800)", modes)
            check("mode(s) from the video BIOS" in modes and " * 0x" in modes.replace("  ", " ") or "* 0x" in modes, "and the video BIOS's mode list with the usable ones starred", modes)
            check("1280x800, 32 bits per pixel" in modes, "including the mode that was chosen", modes)

            good = run("debug slots", "watchdog:")
            check("slot metadata: not readable" not in good and "crash count:" in good, "`debug slots` reads the metadata normally", good)
            with open(image, "r+b") as handle:      # corrupt it behind the kernel's back
                handle.seek(payload * 512)
                handle.write(bytes(512))
            broken = run("debug slots", "wanted:")
            check("not readable (the sector is not valid metadata" in broken and "it starts: [00, 00" in broken,
                  "with a zeroed metadata sector `debug slots` says why, and how the sector starts", broken)
        finally:
            machine.close()
    print("bios sector: all checks passed")


if __name__ == "__main__":
    main()
