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
   (`mdadm --bitmap=none` when creating; mdadm adds a bitmap by default only on large arrays). Nothing here resyncs a
   dirty or degraded array or re-adds a returned member; use Linux for that.
3. RAID 0 and RAID 10 (striping arithmetic), 4. RAID 5/6 (parity; the write hole needs the journal or a bitmap).
5. **Kernel glue** (done in kernel-mf, `kernel-mf/src/mdvol.rs`): the assembled array is registered as one more disk
   (`block::Backend::Md`), so `disks`, `ext4 diskN`, `fstab`, `mount` and `@LABEL` work on it unchanged. Members are whole
   disks or GPT "Linux RAID" partitions. `md auto` in `settings` starts the `md-assemble` thread at boot; boot never waits
   for it. It waits up to `md wait N` seconds (default 10) for every member, then assembles with what it has (degraded:
   loud warning), retrying with a 1, 2, 4 ... 30 s pause (and at once when a disk appears) until it succeeds. The array is
   opened for writing on the first write and closed clean after 3 idle seconds, on `md stop`, `shutdown` and `reboot`;
   only a crash during a write leaves it dirty. A dirty array is read-only until `md force-rw`. A failing member costs a
   failed transfer, never a stall (reads fall back, writes drop the member). Commands: `md`, `md assemble`, `md stop`,
   `md force-rw`. One array per machine for now. `make test-mf-md` boots QEMU with a hand-built mirror, reads and writes
   ext4 on it, checks the superblocks and mirrors on the host and boots again with one member (degraded).

`cargo run --example probe -- DISK1 DISK2 [--dump FILE]` (in `crates/md`) assembles real members read-only, prints what
`mdadm --examine` should agree with and compares the mirrors; `--dump` writes the array so `fsck.ext4 -n` can check it. `--write-test` (members opened read-write) starts the array,
writes and reads back a pattern in its last 8 sectors, restores them and stops it clean: afterwards `mdadm --examine`
must show a clean array with events +2 and `mdadm --assemble --scan` must still work.

**Verification status.** The superblock layout and checksum come from the kernel's `md_p.h` and mdadm's description and are
tested against an independent Python builder, not yet against a superblock written by real `mdadm`. First real check:
`mdadm --create /dev/md0 --level=1 --raid-devices=2 --metadata=1.2 /dev/sdX /dev/sdY`, then `probe` on the members and
compare with `mdadm --examine`.
