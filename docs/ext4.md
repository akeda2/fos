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

`crates/ext4/src/write.rs`: `Writer::format` (4 KiB blocks, journal optional, no checksums), `create`, `mkdir`, `write`,
`truncate`, `remove`, `rename`, `flush`. `Writer` is ~100 KiB: box it. The superblock says "not clean" from the first
change until `flush`. Only volumes without `metadata_csum`/`uninit_bg`/`bigalloc`/inline data are writable; the
`mkfs.ext4` default (checksums) is refused with `ReadOnly` until metadata_csum lands. Files are limited to four
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
escapes and revokes) before mounting. Refused: journal checksums, async/fast commit, external journals, a journal that
is not one contiguous run. `format` adds a journal (1024-16384 blocks, an eighth of the volume at most; Linux's
minimum is 1024, so volumes under 32 MiB get none). Journal-less volumes keep working, written straight through.
`ext4 diskN recover` replays and marks clean. Tests (`make test-ext4`): a power cut at 150 points through a
multi-operation script, each judged by Linux's own replay (`e2fsck -fy`) and by ours (`open`, then `e2fsck -fn`); a
transaction written by `debugfs jo/jw/jc` is replayed by `open`. There is no write barrier in `Device`: a disk with a
volatile cache may reorder log and commit; only real hardware shows how much that matters. mf thread stacks became
256 KiB because the volume code keeps four 4 KiB caches per `Fs` and moves it by value. Not run: the embedded kernel
under QEMU with a journaled volume (its static grows by 256 KiB).
