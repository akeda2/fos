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
  are refused per file. A volume with an unreplayed journal is refused (`NeedsRecovery`) until Linux has mounted it.
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
