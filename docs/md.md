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
   unknown fields survive. Arrays with a bad-block log are refused for writing; so are ones with a bitmap this code
   does not trust (see 5c).
3. **RAID 0 and RAID 10** (done in the library, same `Array`): chunk `k` of the array is copy `c` at position
   `p = k * near + c`, which is member role `p % raid_disks`, row `p / raid_disks`, physical sector `row * chunk + offset`
   (Linux `raid10.c`'s near layout; RAID 0 is `near = 1`). RAID 10 reads the layout from the superblock (`near` copies in
   the low byte); "far" and "offset" layouts are refused (`Unsupported`), and so is a RAID 0 or 10 with more than
   `MAX_MEMBERS` members. RAID 0 needs equal-sized members (one zone; mdadm's `RAID0_LAYOUT` feature bit is accepted then) and every one of them, writes no superblocks on
   `start`/`stop` (it has no resync state) and fails a write instead of dropping a member. RAID 10 drops a failed member
   as long as every chunk keeps a copy (copies sit on `near` consecutive roles starting at a multiple of
   `gcd(near, raid_disks)`); a member whose loss would leave a chunk with no copy stays and the write fails. Assembly
   needs the same coverage.
4. **Resync and re-add** (done in the library, RAID 1 and 10): `begin_resync` + `step` compare every piece of a dirty array
   across its copies and write the first readable copy over the others (the whole array unless there is a bitmap, see 5c; what Linux does
   after an unclean stop); at the end the array counts as clean and `stop` records it. `begin_rebuild(member)` +
   `step` fill a blank or stale disk in as the missing member: its superblock (cloned from a member's, new `dev_number`
   and device UUID, `FEATURE_RECOVERY_OFFSET`, `recovery_offset` 0) is written first, so a crash leaves it flagged and
   left out at the next assembly; the data is then copied from the surviving copies (per chunk for RAID 10) while every
   write also goes to it; at the end the flag is cleared, the role tables name it and every superblock gets events +1,
   the new member's last. The dev number is the member's old one when the array lists it as gone, else the one the role
   table gives the missing role. A failed write to the target abandons the rebuild, never an active member. Progress is
   not saved: a rebuild interrupted by `stop` or a crash starts again from sector 0. RAID 0 has neither.
5. **RAID 5** (done in the library, same `Array`; RAID 6 is item 5b): the four md layouts (left/right, symmetric/asymmetric;
   mdadm's default is left-symmetric), chunk size from the superblock, array size `(disks - 1) * (size / chunk * chunk)`.
   Any member of a stripe row is the XOR of the others, so one rule serves reads of a missing or failing member,
   a degraded write, the resync (parity recomputed from the data) and the rebuild. A write is read-modify-write
   (old data, old parity) when both members are there, else the parity is the XOR of the other data. One member may
   be missing; a second failure fails the write. During a rebuild every write also reaches the target, parity
   included. The write hole is the same as in Linux without a journal: a crash during a write leaves the array dirty
   and the resync recomputes parity. A dirty array that is also degraded is not assembled (Linux refuses it too:
   the parity cannot be trusted to rebuild the missing member). No journal, no PPL, no reshape.
   Job cursors count sectors of one member here (`job()` reports array sectors).
5b. **RAID 6** (`crates/md/src/array/raid6.rs`, GF(2^8) in `gf.rs`, polynomial 0x11d, generator 2): the same four layouts,
   array size `(disks - 2) * (size / chunk * chunk)`, at least 4 members. P sits where RAID 5 puts parity, Q on the next
   member. Q is the sum of g^j times the data, j counting the data members round the ring from the one after Q (for the
   asymmetric layouts that is not the chunk order). Any two members of a row can be missing: one data member comes from P
   (or from Q when P is gone too), two from the pair of equations, P and Q are recomputed. Writes are read-modify-write
   of data, P and Q when all three are there, else the row is read whole, rebuilt and P and Q recomputed; the resync
   rewrites a P or Q that differs; a rebuild fills one target while another member may still be missing. Pieces are 8
   sectors (the row buffers live on the stack). A dirty array missing two members is not assembled.
5c. **Write-intent bitmap** (`crates/md/src/bitmap.rs`, RAID 1, 5 and 6; `mdadm --bitmap=internal`): the 256-byte header
   and the bits sit at `super_sector + bitmap_offset` on every member (layout checked against bytes mdadm 4.5 wrote; `check_md_mdadm.sh raid1-bm raid5-bm raid6-bm` passes: mdadm sees our dirty bits, assembles and fscks after our writes and resync, and its own resync clears them).
   One bit covers `chunk` sectors of the resync cursor space (array sectors for RAID 1, member sectors for RAID 5/6),
   `sync_size` must equal that span. With a trusted bitmap `start` leaves the superblocks in step (no `resync_offset` 0,
   no event bump); every `write` first sets the bits of the chunks it touches and flushes them on all members;
   `commit` keeps the bitmap's events level with the array's (Linux distrusts an older bitmap); a clean `stop` (and a
   finished resync) clears the bits, and the kernel's idle stop does so after a few quiet seconds. After a crash the
   superblocks say "in step" but set bits make the array dirty, and `begin_resync` then compares only those chunks.
   A stale or failed bitmap, a foreign uuid, a different `sync_size`, one that overlaps data or the superblock, or one
   over 16 KiB keeps the array read-only; RAID 10 with a bitmap too. The union of all members' bits is used. A rebuilt
   member gets the bitmap written first. The bitmap narrows the resync, it does not close the write hole (a stripe
   torn by the crash is repaired by the resync, but a degraded array still refuses to assemble while dirty).
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

`tools/check_md_mdadm.sh` (run as root, loop devices only) is the cross-check against the real driver: mdadm creates RAID 1, 0, 10, 5 and
6 (every layout), `probe` dumps, writes and rebuilds them, and mdadm must assemble, `check` (0 mismatches) and fsck the results.

`cargo run --example probe -- DISK1 DISK2 [--dump FILE]` (in `crates/md`) assembles real members read-only, prints what
`mdadm --examine` should agree with and compares the mirrors; `--dump` writes the array so `fsck.ext4 -n` can check it. `--write-test` (members opened read-write) starts the array,
writes and reads back a pattern in its last 8 sectors, restores them and stops it clean: afterwards `mdadm --examine`
must show a clean array with events +2 and `mdadm --assemble --scan` must still work.

**Verification status.** RAID 6 (all four layouts, 4 and 5 members) passed `tools/check_md_mdadm.sh` too (2026-11-09), with
the same checks as RAID 5; one blank member was rebuilt, the double-loss paths are covered by the host and QEMU tests only. `tools/check_md_mdadm.sh` (mdadm 4.5, loop devices, 2026-11-09) passed for RAID 1, 0, 10 (near 2) and
RAID 5 (all four layouts, 3 and 4 members): our dump fscks clean, mdadm assembles and fscks the array after our write, and a blank
member rebuilt by `probe --rebuild` makes `mdadm` check report 0 mismatches. It found two real-mdadm differences, both fixed:
RAID 0 carries feature bit `RAID0_LAYOUT` and `size` 0 (the length is the members' `data_size`). Not verified: RAID 0 members of
unequal size (several zones, refused), real disks (timings, a pulled USB disk), and
the kernel glue against an mdadm-made array on real hardware.
