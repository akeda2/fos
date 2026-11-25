#!/usr/bin/env python3
"""`md create` and an md array served over AoE (`make test-mf-md-aoe`, docs/md.md, docs/block-sharing.md). Two server
kernels on one QEMU multicast segment:

- t (the target, 52:54:00:12:34:f1): three blank 32 MiB data disks full of old random bytes, and in settings `md auto`
  and `aoe-export md slot 1 rw for <i's MAC>`, which must wait at boot because there is no array yet.
- i (the initiator, 52:54:00:12:34:f2): `big.bin` (192 KiB of random bytes) on its boot disk.

On t: `md create raid5 disk2 disk3 disk4` announces what it erases and wants `confirm` as the very next command (another
command in between cancels it); confirmed, it writes the array, assembles it, starts the resync, and the waiting export
goes out as slot 1. Then a member disk cannot be exported on its own, and `md stop` is refused while the array is
exported. On i: the array is attached rw, a FOSFS volume made on it and big.bin copied there with the host's SHA-256.
t is booted again: `md auto` assembles the array in the background and the export from settings follows; i reads the
file back. At the end the host checks the member images itself, with code independent of crates/md: the superblocks
(checksum, clean), every stripe row's parity, and big.bin in the array's contents, also with a member left out."""

import hashlib
import os
import struct
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_mf_aoe_initiator import Guest, build_image  # noqa: E402
from test_mf_md import layout5  # noqa: E402

MEMBER = 32 << 20            # bytes per data disk
DATA_OFFSET = 2048           # sectors (md::create::DATA_OFFSET)
TAIL = 2048
CHUNK = 1024                 # 512 KiB, the default
I_MAC = "52:54:00:12:34:f2"
failures = []


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}: {detail}")


def read_super(path):
    """Checksum, events and resync_offset of the v1.2 superblock at sector 8 (header and role table, md_p.h)."""
    with open(path, "rb") as f:
        f.seek(8 * 512)
        b = f.read(4096)
    assert struct.unpack_from("<L", b, 0)[0] == 0xA92B4EFC
    n = 256 + struct.unpack_from("<L", b, 220)[0] * 2
    z = bytearray(b[:n])
    struct.pack_into("<L", z, 216, 0)
    total = sum(struct.unpack_from("<%dL" % (n // 4), z))
    total = (total & 0xFFFFFFFF) + (total >> 32)
    return {"csum_ok": total & 0xFFFFFFFF == struct.unpack_from("<L", b, 216)[0],
            "events": struct.unpack_from("<Q", b, 200)[0], "resync": struct.unpack_from("<Q", b, 208)[0]}


def array_image(paths, absent=None):
    """The RAID 5 (left-symmetric) as the host reads it from the members: (image, every row's parity right)."""
    rows = (MEMBER // 512 - DATA_OFFSET - TAIL) // CHUNK
    files = [open(p, "rb") for p in paths]
    out, parity_ok = bytearray(), True
    for row in range(rows):
        chunks = []
        for f in files:
            f.seek((DATA_OFFSET + row * CHUNK) * 512)
            chunks.append(f.read(CHUNK * 512))
        total = 0
        for n, chunk in enumerate(chunks):
            if n != absent:
                total ^= int.from_bytes(chunk, "little")
        if absent is None:
            parity_ok = parity_ok and total == 0
        else:
            chunks[absent] = total.to_bytes(CHUNK * 512, "little")
        for i in range(len(paths) - 1):
            out += chunks[layout5(2, len(paths), row, i)[0]]
    for f in files:
        f.close()
    return bytes(out), parity_ok


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        disks = []
        for n in range(3):
            disk = tmp / f"m{n}.img"
            disk.write_bytes(os.urandom(MEMBER))
            disks.append(disk)
        big = os.urandom(192 * 1024)
        (tmp / "big.bin").write_bytes(big)
        digest = hashlib.sha256(big).hexdigest()
        t_image = build_image(tmp / "t", f"aoe-shelf 6\nmd auto\nmd wait 1\naoe-export md slot 1 rw for {I_MAC}\n")
        i_image = build_image(tmp / "i", "", [("big.bin", tmp / "big.bin")])
        t = Guest("t", t_image, disks, "52:54:00:12:34:f1")
        i = None
        try:
            if not t.wait_for("fos> ", 150):
                raise SystemExit(f"t never reached the prompt: {t.log[-2000:]!r}")
            check(b"aoe-export md: waits for the md array" in t.log, "`aoe-export md` waits while there is no array",
                  t.log[-1500:].decode("utf8", "replace"))

            create = "md create raid5 disk2 disk3 disk4 name tank"
            ok, text = t.command(create, "confirm", 30)
            check(ok and "would be made on" in text and "EVERYTHING on these disks is erased" in text and "disk4: 32 MiB" in text,
                  "`md create` announces the disks it would erase", text[-800:])
            t.command("disks", "disk4", 10)
            ok, text = t.command(create + " confirm", "fos> ", 30)
            check("nothing announced" in text, "another command in between cancels it", text[-400:])
            t.command(create, "confirm", 30)
            ok, text = t.command(create + " confirm", "resync started", 60)
            check(ok and "array assembled as disk5" in text, "confirmed, it writes and assembles the array as disk5", text[-1200:])
            check("the md array (disk 5) exported READ-WRITE as shelf 6 slot 1" in text,
                  "and the export that waited in settings goes out", text[-1200:])
            check(t.wait_for("md: resync finished.", 180), "the resync finishes", t.log[-800:].decode("utf8", "replace"))
            ok, text = t.command("md", "md:", 10)
            check("fos:tank" in text and "3 of 3" in text and "dirty" not in text, "`md` shows it clean, 3 of 3", text[-500:])
            ok, text = t.command("aoe export 3 slot 7", "aoe:", 20)
            check("member of an md array" in text, "a member disk is not exported on its own", text[-400:])
            ok, text = t.command("md stop", "md:", 20)
            check("exported over AoE" in text, "`md stop` is refused while the array is exported", text[-400:])

            i = Guest("i", i_image, [], I_MAC)
            if not i.wait_for("fos> ", 150):
                raise SystemExit(f"i never reached the prompt: {i.log[-2000:]!r}")
            ok, text = i.command("aoe attach 6.1 rw", "is disk 2", 30)
            check(ok and "READ-WRITE" in text, "i attaches the array rw", text[-600:])
            ok, text = i.command("format disk2 remote force", "confirm", 20)
            ok, text = i.command("format disk2 confirm", "fos", 120)
            check("rror" not in text and "fail" not in text, "a FOSFS volume is made on it over AoE", text[-400:])
            ok, text = i.command("cp big.bin @remote/big.bin", "fos", 120)
            ok, text = i.command("sha256 @remote/big.bin", digest, 120)
            check(ok, "big.bin copied onto it reads back with the host's SHA-256", text[-600:])
            ok, text = i.command("aoe detach 6.1", "detached", 20)

            # Boot t again: the array comes up in the background and the export from settings follows it.
            t.command("aoe unexport 1", "aoe:", 20)
            t.command("md stop", "stopped", 30)
            t.kill()
            t = Guest("t", t_image, disks, "52:54:00:12:34:f1")
            check(t.wait_for("exported READ-WRITE as shelf 6 slot 1", 150), "after a reboot, `md auto` assembles it and the export follows",
                  t.log[-1500:].decode("utf8", "replace"))
            t.wait_for("fos> ", 60)
            ok, text = i.command("aoe attach 6.1", "is disk 2", 30)
            ok, text = i.command("sha256 @remote/big.bin", digest, 120)
            check(ok, "i reads big.bin back after the target's reboot", text[-600:])
            i.command("aoe detach 6.1", "detached", 20)
            t.command("aoe unexport 1", "aoe:", 20)
            ok, text = t.command("md stop", "stopped", 30)
            check(ok, "unexported, the array stops clean", text[-400:])
        finally:
            t.kill()
            if i:
                i.kill()

        # The host's own reading of the members.
        supers = [read_super(path) for path in disks]
        check(all(s["csum_ok"] for s in supers) and all(s["resync"] == 0xFFFFFFFFFFFFFFFF for s in supers),
              "every superblock checksums and says clean", str(supers))
        with open(disks[0], "rb") as f:
            head = f.read(8 * 512)
            level, layout = struct.unpack_from("<lL", (f.seek(8 * 512 + 72), f.read(8))[1])
        check(head == bytes(len(head)) and (level, layout) == (5, 2), "the old partition area is zeroed; RAID 5, left-symmetric")
        image, parity_ok = array_image(disks)
        check(parity_ok, "every stripe row's parity is right")
        check(big in image, "big.bin is in the array's contents")
        degraded, _ = array_image(disks, absent=1)
        check(big in degraded and degraded == image, "and the same with member 2 rebuilt from parity")
    if failures:
        print("\n\n".join(failures))
        sys.exit(1)
    print("test-mf-md-aoe: all checks passed.")


if __name__ == "__main__":
    main()
