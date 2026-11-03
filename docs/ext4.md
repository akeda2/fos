# ext4 data volumes

Plan: a disk carries a small FOSFS (OS, tapes, config) and an ext4 data volume for the files the SMB server will serve;
Linux can read the disk when pulled. FOSFS stays; ext4 is a second filesystem in the shared `crates/ext4` (`no_std`,
no allocation, usable by the embedded and the mf kernel).

Order of work: 1. read-only driver (done), 2. FOSFS `shrink` (done), 3. writes (extents, folder index, checksums; checked
with `e2fsck`), 4. journal, 5. one installer (ESP, payload, small FOSFS, ext4 for the rest), 6. SMB server.
mf-specific fast paths (block cache, hash-tree lookup, read-ahead) go on top of the plain driver.

## Read-only driver (`crates/ext4`)

- ext2/3/4: extents or indirect blocks, 1/2/4 KiB blocks, 32/64-byte group descriptors, flex_bg, 64bit, large folders
  (read by linear scan). Refused: meta_bg, compression, encryption, casefold, blocks over 4 KiB. Inline-data files
  are refused per file. A volume with an unreplayed journal is refused (`NeedsRecovery`) until `ext4 diskN recover` (or any write command) has replayed it.
- Names are bytes (UTF-8); lookup is byte-exact.
- Host tests (`make test`, `crates/ext4/tests/images.rs`) read `mkfs.ext4`/`mkfs.ext2` images and compare every file.

## Shell

`ext4 diskN [ls|cat|info] [PATH]` on both kernels (`crates/kernel-core/src/ext4vol.rs`): finds a GPT "Linux filesystem"
partition, an MBR type 0x83 partition or a whole-disk volume. `make test-mf-shell` covers it under QEMU.

## `shrink diskN [SIZE]`

Makes a big FOSFS partition smaller so the rest of the disk can hold an ext4 volume, without a reinstall. SIZE takes
units, binary: `512M`, `2G`, `1.5GiB`, `100K`; a bare number is MB. It rounds up to whole MiB, says what it will do and
waits for `shrink diskN confirm` as the very next command (any other command cancels it). Files that reach past the new
end are copied into a free run below it, one extent at a time; the directory slot (one sector write) is repointed only
after the copy, so a power cut leaves every file whole, either where it was or where it went. The volume size in the
superblock changes next and the GPT entry (primary and backup) last. If no single free run holds an extent it stops
with nothing lost. It works on the boot disk too (the volume is remounted). Code: `Volume::shrink` in
`crates/kernel-core/src/fosfs.rs`, `gpt::shrink_partition`, `kernel/src/formatshell.rs` (shared by both kernels);
tests in `crates/kernel-core/tests/fosfs.rs` and `make test-mf-shell`.

## Writing (step 3)

`crates/ext4/src/write.rs`: `Writer::format` (4 KiB blocks, journal and metadata_csum optional), `create`, `mkdir`, `write`,
`truncate`, `remove`, `rename`, `flush`. `Writer` is ~100 KiB: box it. The superblock says "not clean" from the first
change until `flush`. Only volumes without `uninit_bg`-only/`bigalloc`/inline data are writable; `mkfs.ext4`
defaults (metadata_csum, orphan_file, metadata_csum_seed) are written correctly, see "metadata_csum" below. Files are limited to four
extent leaf blocks (1360 extents at 4 KiB); beyond that `TooFragmented`. Folder hash indexes are dropped (flag
cleared) on change; lookups scan. Tests: `crates/ext4/tests/writes.rs` (e2fsck -fn and debugfs judge the results).
Shell (`kernel-core/src/ext4vol.rs`): `ext4 diskN mkfs [LABEL]` (adds a Linux partition over all free space after
the last partition, formats it; asks, then `ext4 diskN mkfs confirm` as the very next command), `mkdir`, `rm`, `mv`,
`write PATH TEXT`. `make test-mf-shell` runs it in QEMU and lets e2fsck/debugfs judge the disk. Rack's boot disk was
formatted this way (243 GB, `rackdata`) after `shrink disk1 1G`. Formatting 243 GB takes minutes and blocks the shell:
rack reset itself (60 s watchdog) once it was done; the volume was complete. Not yet: installer integration,
copying files from FOSFS.

## Journal (step 4)

`crates/ext4/src/journal.rs`. `Journaled<D>` wraps the device: the writer's metadata writes are staged (64 blocks,
`Stage`, 256 KiB, a static in the kernel) and `commit` makes them one jbd2 transaction: descriptor, blocks and commit
block go to the log, the journal superblock's `start` is set (the commit point), the blocks are checkpointed to their
home places, `start` goes back to 0. At most one transaction is in the log, so no revoke records are ever written;
`needs_recovery` is set before the first commit and cleared by `flush`. File data (`write_data`) bypasses the journal
and goes to disk first ("ordered" mode). `open` replays a log Linux or we left behind (scan, revoke, replay passes, with
escapes and revokes) before mounting. Refused: the older journal checksum modes (v1, v2), async/fast commit, external
journals, a journal in more than 8 pieces (mke2fs cuts the journal of a small volume around group metadata). `format` adds a journal (1024-16384 blocks, an eighth of the volume at most; Linux's
minimum is 1024, so volumes under 32 MiB get none). Journal-less volumes keep working, written straight through.
`ext4 diskN recover` replays and marks clean. Tests (`make test-ext4`): a power cut at 150 points through a
multi-operation script, each judged by Linux's own replay (`e2fsck -fy`) and by ours (`open`, then `e2fsck -fn`); a
transaction written by `debugfs jo/jw/jc` is replayed by `open`. There is no write barrier in `Device`: a disk with a
volatile cache may reorder log and commit; only real hardware shows how much that matters. mf thread stacks became
256 KiB because the volume code keeps four 4 KiB caches per `Fs` and moves it by value. Not run: the embedded kernel
under QEMU with a journaled volume (its static grows by 256 KiB).

## metadata_csum (step 5)

`crates/ext4/src/crc.rs` (CRC32C) and `write.rs`: a volume with `metadata_csum` (Linux's `mkfs.ext4` default, with
`metadata_csum_seed` and an empty `orphan_file`) is written with every checksum kept right: superblock (offset 1020),
group descriptors (16 bits, per group), block and inode bitmaps (in the descriptor), inodes (lo and hi halves, over the
whole inode with the generation in the seed), extent leaf blocks (4-byte tail after the 340 entries) and folder blocks
(a 12-byte fake entry, inode 0, type 0xDE, ends each block, so a folder block holds entries in `size - 12` bytes).
`BLOCK_UNINIT` / `INODE_UNINIT` groups (what `lazy_itable_init` leaves) get their bitmaps built on first use and the
flags cleared; `itable_unused` is kept. A hash-tree folder (`INDEX_FL`) is made linear on its first change (the `..` entry takes back the index space, index nodes become empty blocks, leaves stay as they are). A journal Linux has mounted carries
`CSUM_V3`; `open` verifies and replays such a log (descriptor, revoke, commit and per-block checksums) and then clears
the feature, so we only write plain logs and Linux sets it again at its next mount. `Writer::format` makes volumes with
checksums (`FormatOptions.checksums`, on in the kernel shell; 32-byte descriptors, so no `64bit`). A volume with only `uninit_bg` (descriptor checksums) stays read-only. Tests: Linux-made
volumes (default, lazy tables, 1 KiB blocks without orphan_file, no checksums with many groups, 768 MiB spilling into
uninitialised groups) are filled with files, folders that grow, fragmented files, renames and removals, then judged by
`e2fsck -fn` (which verifies every checksum); a v3 journal written by `debugfs jo -c` is replayed.

## Performance (read cache)

`crates/ext4/src/cache.rs`: a direct-mapped read cache of 4 KiB units (`Cache`, and `Cached<D>` as a `Device` wrapper for
tests). Reads of at most one aligned unit fill and use a slot; bigger reads (file data runs) bypass it. Writes go through and
update the slots they overlap, so it is never stale. `kernel-core/src/ext4vol.rs` keeps one static cache (512 slots, 2 MiB of
BSS) behind `Partition`, emptied at every `locate` and `mkfs`, and between candidate partitions (slots are keyed by sector
within the partition). Measured with `tests/perf.rs` (counts device operations; `SLOTS=n`) on a 512 MiB checksummed volume:
`mkdir` + 2000 creates 57481 -> 153 reads, 20 lookups in a 2000-name folder 236 -> 0, 100 small appends 214 -> 3; writes are
unchanged (the journal commits each change). Not done: hash-tree lookup in the reader (indexed folders are still scanned
linearly, and our writer linearises them on change), multi-block write coalescing.

## TRIM / discard

`ext4 diskN trim` tells an SSD which blocks of the volume are free (like `fstrim`). It is on request, not at every delete:
a freed block may not be discarded before the journal transaction that freed it has committed, and the journal commits per
change. `Writer::trim` flushes first and then walks the group bitmaps, so only blocks free in the committed state are
sent; it merges runs across groups and calls back with batches of at most 64 ranges. Nothing is written to the volume.
`mkfs`/`remkfs`/`install` also discard the whole partition before formatting (best effort; `format_raw` writes every
metadata block it needs, so undefined contents afterwards do not matter).

Plumbing: `block::discard(disk, &[(sector, sectors)])` (both kernels, `Platform::block_discard` in `fos-hal`). AHCI sends
DATA SET MANAGEMENT (0x06, feature 1, 48-bit; needs IDENTIFY word 169 bit 0), 64 entries per sector, up to 16 sectors per
command. NVMe sends Dataset Management (0x09, AD bit; needs ONCS bit 2), up to 256 ranges of at most 65535 sectors per
command. USB mass storage (SCSI UNMAP), virtio and SD report `Unsupported`. Contents of trimmed ranges are undefined
afterwards on some SATA SSDs, which is fine for free space.

Verified in QEMU with `discard=unmap` (the host image shrinks): `make test-mf-shell` on AHCI and NVMe, `make test-install`.
Only real hardware shows whether a given SSD honours it.
