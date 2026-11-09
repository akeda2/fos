# Software RAID: Linux md (mdadm) arrays

Goal: FOS-MF mounts ext4 on an md array, and a Linux live USB can assemble the same disks for recovery
(`mdadm --assemble --scan`). The on-disk format is Linux's md v1.x superblock, so nothing here is fos-specific.

`crates/md` (`no_std`, no allocation, host tests) is the format and the array logic; the caller supplies the disks
through the `Members` trait. Stages:

1. **Read-only RAID 1** (done in the library): finds the v1.0/1.1/1.2 superblock, checks the checksum, keeps the members of
   one array, uses the highest event count, leaves out stale, faulty and recovering members, reads from the first usable
   member and falls back when a read fails. `dirty` and `degraded` are reported, never repaired.
2. **RAID 1 writes** (done in the library): `Array::start` flushes a superblock with `resync_offset` 0 and the event count
   +1 on every member before any data write; `write` goes to every member in use, and a member that fails is dropped and
   recorded as faulty (role `0xfffe`, events +1) in the survivors' superblocks before the write returns, so it cannot
   return as current; `stop` flushes the data and then records `resync_offset` = in step with events +1. A crash between
   `start` and `stop` leaves the array dirty, and Linux then resyncs all of it. Superblocks are patched in place, so
   unknown fields survive. Arrays with a write-intent bitmap or a bad-block log are refused for writing
   (`mdadm --bitmap=none` when creating; mdadm adds a bitmap by default only on large arrays).
3. **RAID 0 and RAID 10** (done in the library, same `Array`): chunk `k` of the array is copy `c` at position
   `p = k * near + c`, which is member role `p % raid_disks`, row `p / raid_disks`, physical sector `row * chunk + offset`
   (Linux `raid10.c`'s near layout; RAID 0 is `near = 1`). RAID 10 reads the layout from the superblock (`near` copies in
   the low byte); "far" and "offset" layouts are refused (`Unsupported`), and so is a RAID 0 or 10 with more than
   `MAX_MEMBERS` members. RAID 0 needs equal-sized members (one zone) and every one of them, writes no superblocks on
   `start`/`stop` (it has no resync state) and fails a write instead of dropping a member. RAID 10 drops a failed member
   as long as every chunk keeps a copy (copies sit on `near` consecutive roles starting at a multiple of
   `gcd(near, raid_disks)`); a member whose loss would leave a chunk with no copy stays and the write fails. Assembly
   needs the same coverage.
4. **Resync and re-add** (done in the library, RAID 1 and 10): `begin_resync` + `step` compare every piece of a dirty array
   across its copies and write the first readable copy over the others (no bitmap, so the whole array; what Linux does
   after an unclean stop); at the end the array counts as clean and `stop` records it. `begin_rebuild(member)` +
   `step` fill a blank or stale disk in as the missing member: its superblock (cloned from a member's, new `dev_number`
   and device UUID, `FEATURE_RECOVERY_OFFSET`, `recovery_offset` 0) is written first, so a crash leaves it flagged and
   left out at the next assembly; the data is then copied from the surviving copies (per chunk for RAID 10) while every
   write also goes to it; at the end the flag is cleared, the role tables name it and every superblock gets events +1,
   the new member's last. The dev number is the member's old one when the array lists it as gone, else the one the role
   table gives the missing role. A failed write to the target abandons the rebuild, never an active member. Progress is
   not saved: a rebuild interrupted by `stop` or a crash starts again from sector 0. RAID 0 has neither.
5. RAID 5/6 (parity; the write hole needs the journal or a bitmap).
6. **Kernel glue** (done in kernel-mf, `kernel-mf/src/mdvol.rs`): the assembled array is registered as one more disk
   (`block::Backend::Md`), so `disks`, `ext4 diskN`, `fstab`, `mount` and `@LABEL` work on it unchanged. Members are whole
   disks or GPT "Linux RAID" partitions. `md auto` in `settings` starts the `md-assemble` thread at boot; boot never waits
   for it. It waits up to `md wait N` seconds (default 10) for every member, then assembles with what it has (degraded:
   loud warning), retrying with a 1, 2, 4 ... 30 s pause (and at once when a disk appears) until it succeeds. The array is
   opened for writing on the first write and closed clean after 3 idle seconds, on `md stop`, `shutdown` and `reboot`;
   only a crash during a write leaves it dirty. A dirty array is read-only until `md force-rw`. A failing member costs a
   failed transfer, never a stall (reads fall back, writes drop the member). Commands: `md`, `md assemble`, `md stop`,
   `md force-rw`, `md resync` (opens a dirty array and resyncs it in the background thread `md-job`; progress in `md`, which
   also reports a rebuild) and `md add diskN` then `md add diskN confirm` (erases that whole disk and rebuilds onto it; refuses
   the boot disk, a disk that is already a member and a dirty array: resync first). An idle close waits for a running job.
   One array per machine for now. `make test-mf-md` boots QEMU with a hand-built mirror, reads and writes
   ext4 on it, checks the superblocks and mirrors on the host and boots again with one member (degraded); the same for a
   four-member RAID 10 (striped by an independent routine in the test; booted again with one member of each pair) and a
   two-member RAID 0 (which must not assemble with a member missing).

`cargo run --example probe -- DISK1 DISK2 [--dump FILE]` (in `crates/md`) assembles real members read-only, prints what
`mdadm --examine` should agree with and compares the mirrors; `--dump` writes the array so `fsck.ext4 -n` can check it. `--write-test` (members opened read-write) starts the array,
writes and reads back a pattern in its last 8 sectors, restores them and stops it clean: afterwards `mdadm --examine`
must show a clean array with events +2 and `mdadm --assemble --scan` must still work.

**Verification status.** The superblock layout and checksum come from the kernel's `md_p.h` and mdadm's description and are
tested against an independent Python builder, not yet against a superblock written by real `mdadm`. First real check:
`mdadm --create /dev/md0 --level=1 --raid-devices=2 --metadata=1.2 /dev/sdX /dev/sdY`, then `probe` on the members and
compare with `mdadm --examine`. The RAID 0 and 10 placement is from `raid10.c` and tested against the test's own striping,
so check it against real members as well (`mdadm --create ... --level=10 --layout=n2 --raid-devices=4`, `--level=0`);
RAID 0 members of unequal size (several zones) are not read, a rebuild has never been run on disks of a real array, and the `sb->size` a real RAID 0 stores is unverified.
