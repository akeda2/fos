# Software RAID: Linux md (mdadm) arrays

Goal: FOS-MF mounts ext4 on an md array, and a Linux live USB can assemble the same disks for recovery
(`mdadm --assemble --scan`). The on-disk format is Linux's md v1.x superblock, so nothing here is fos-specific.

`crates/md` (`no_std`, no allocation, host tests) is the format and the array logic; the caller supplies the disks
through the `Members` trait. Stages:

1. **Read-only RAID 1** (done in the library): finds the v1.0/1.1/1.2 superblock, checks the checksum, keeps the members of
   one array, uses the highest event count, leaves out stale, faulty and recovering members, reads from the first usable
   member and falls back when a read fails. `dirty` and `degraded` are reported, never repaired.
2. **RAID 1 writes**: write all members, then bump `events` and `utime` in every superblock (data first, then metadata,
   with flushes between); the array is marked in use while mounted and clean on unmount. A write-intent bitmap is
   optional (without one Linux resyncs the whole array after a crash).
3. RAID 0 and RAID 10 (striping arithmetic), 4. RAID 5/6 (parity; the write hole needs the journal or a bitmap).
5. Kernel glue: an `md` volume in `kernel-core` giving `ext4vol` a `Device` over the array; `mdinfo`/`assemble` shell
   commands. Not started.

`cargo run --example probe -- DISK1 DISK2 [--dump FILE]` (in `crates/md`) assembles real members read-only, prints what
`mdadm --examine` should agree with and compares the mirrors; `--dump` writes the array so `fsck.ext4 -n` can check it.

**Verification status.** The superblock layout and checksum come from the kernel's `md_p.h` and mdadm's description and are
tested against an independent Python builder, not yet against a superblock written by real `mdadm`. First real check:
`mdadm --create /dev/md0 --level=1 --raid-devices=2 --metadata=1.2 /dev/sdX /dev/sdY`, then `probe` on the members and
compare with `mdadm --examine`.
