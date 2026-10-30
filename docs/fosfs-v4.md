# FOSFS v4 and the mount tree (step 1 built)

Status: **step 1 is built (2026-10-30)**: `kernel/src/fosfs.rs` mounts v3 and v4; `format diskN [NAME] v4` makes a v4
volume; `tools/build_image.py --fosfs 4` builds one; `make test-fosfs4`. v3 stays the default. The rest is a plan.

What step 1 does exactly: magic `FOSFS4`; 128-byte entries whose first 28 bytes match v3 (used, name length, start,
length, sector count; name at 28, up to 48 characters of letters either case, digits, `. _ - +`, compared
case-sensitively; the remaining bytes are zero, reserved); a directory of one entry per 4 MiB of volume, 256 to 4096
entries, fixed at format time; **still one contiguous extent per file** (inline multi-extent is not built: first-fit already
fills a stick, only fragmentation would need it); labels stay v3-style (16 lowercase). No `/` in names yet (folders arrive
with the mount table). The directory is scanned 16 sectors per request; extent searches and `fsck` use a 96 KiB static
scratch table (`fsck` names a duplicate by sector, since it checks names by hash). Not yet built: conversion v3 to v4,
prefix `ls`, the stream handles and `fs_read_at`. Everything below is the plan; check the
code (`kernel/src/fosfs.rs`, `crates/fat`, `crates/shell`) before relying on a detail.

## Why

FOSFS v3 is flat: one superblock sector, eight directory sectors of 64 entries, names of at most 16 characters, files as one
contiguous run each. The root is crowded (tapes, configs, boot scripts, recordings) and the 64-entry and 16-character limits
bite sooner than the clutter does. Folders alone would not fix the limits.

## The model

1. **FOSFS v4 has real folders** (a parent field in each entry; `mkdir`, `ls PATH`; see "v4 layout revision" below). New
   magic, a larger directory region, names of up to 48 characters per component. There is no current directory: paths are
   always absolute.
2. **A small mount table gives the tree.** A path is resolved by the longest mounted prefix; the rest of the path goes to the
   filesystem mounted there. No match means the root volume with the whole path as the file name.
   - Single-drive machines need nothing: `config/` is then simply a prefix on the root volume. Mounting a volume at `config`
     replaces it. Both cases use the same lookup, so "falls back to the root" is not a separate code path.
   - Real subfolders exist only on **exFAT** mounts (`/rec/session1/lp1.flac` is a real path on the stick). FAT32 stays
     root-only and is to be avoided.
3. **Mount by label, resolve lazily.** `@LABEL` survives disk renumbering; a stick that appears after boot (or after
   `usb rescan`) starts working at the next lookup. `diskN/NAME` and `@LABEL/NAME` stay as explicit volume paths.
4. **The table lives on the root** (`fstab`, plain text, one `PATH @LABEL` per line) and is read first.
5. **Bare names keep meaning the root** (`phono.cfg`), so old tapes and the tape ABI do not change; new code (`tape-rt`
   config, the shell) uses full paths such as `config/phono.cfg`. `fs_*` already take names as strings.

## The folders

| Folder | Holds | Typically on |
| --- | --- | --- |
| `bin/` | tapes | root (can be rewritten at will) |
| `doc/` | help and documentation files | root |
| `config/` | per-tape configs (`phono.cfg`, `TAPE.cfg`, `@NAME` files) | a small USB stick, or the root |
| `settings/` | FOS configs: `settings`, `start`, `hostname`, `debug` | the config volume, or the root |
| `rec/` | recordings | an exFAT disk (readable on other systems) |

The point of the split: the root (kernel, tapes) can be rewritten or reflashed without touching the machine's configuration,
and a fast NVMe boot disk need not carry the slow stick's data.

## Boot order (the one hard constraint)

Some settings are needed **before** the drivers that bring up the config volume. `no-msi` exists because MSI breaks some
machines, and a USB stick sits behind that same xHCI controller; if its only copy lived on the stick the machine could not
boot. So:

1. Read `fstab` and the root's `settings/` first, before driver init (as today the config is read from the boot volume).
2. Bring up disks and USB; wait for the config volume with a **bounded** wait (about 10 s).
3. Mount it at `settings` and `config` as `fstab` says; its files override the root's copies.
4. Settings read after the drivers are up (`keymap`, `hostname`, `start`) may come from the stick alone.
5. If the volume never appears, the root's directories stand and the shell says so. Nothing blocks.

**Writes when the volume is missing** go to the root (the same prefix). The stick's copy wins when it returns; the shell
says which one a save used.

## Compatibility and migration

- The new kernel mounts v3 and v4. Old kernels need not read v4 (the legacy tree is frozen at `legacy-v1`).
- A host/shell command converts a v3 volume to v4 in place (rewrite directory, same data extents) and moves the known files
  into their folders; a reformat is the fallback.
- Touch points to keep in sync: `kernel/src/fosfs.rs` (name rules), the tape file-system calls, `tools/build_image.py`,
  `tools/foscopy.py`, NETFS and `tools/devloop.py`, `crates/tape-sync` (`tapesync` pulls `bin/` tapes), `crates/tape-rt`
  config search, the shell's `ls`/`copy`/`cat`, boot-time config in the kernel. `AGENTS.md` repeats the name rule: change
  it with the code.

## Order of work (each step testable in QEMU)

1. v4 on disk (done, except the conversion command and prefix `ls`): bigger directory, longer names, prefix `ls`, v3 mount, conversion command, host tools.
2. Mount table (**built**, `kernel/src/mounts.rs`, `make test-mounts`): `fstab` (`PATH @LABEL` lines, `#` comments),
   lazy label lookup at every use, longest mount point wins, `mount` / `umount` / `mount fstab` shell commands. A path
   under a mount goes to the volume without the mount path; reads fall back to the boot volume's own folder when the
   volume lacks the file or is absent, writes go there when it is absent; a write under a mount makes the folders on the
   way. Only FOSFS volumes mount (exFAT needs step 3). `fstab` is read after `config::load()`; the **bounded boot wait
   and the settings overlay move to step 4**, when `settings/` actually exists. The fallback to a boot-volume folder
   needs a v4 boot volume (v3 has no folders), so it is untested until the default flips.
3. exFAT subfolders in `crates/fat` (**built**, `make test-format-exfat` and `cargo test` in `crates/fat`): `Fs::enter`,
   `mkdir`, `remove_dir`; a FAT-chained directory grows (its own entry's size follows), a contiguous (NoFatChain)
   directory made by another system is read and filled but not grown (`NoSpace`). `tape_fs` resolves
   `diskN/rec/session1/x` on exFAT; `ls`, `mkdir`, `rm`, `write`, `cat` and the tape stream calls work in folders.
   `fsck.exfat -n` accepts the result. FAT32 stays root-only (names stay ASCII, 60 characters). Not done: mounting an
   exFAT volume at a path (needs its label read; the mount table takes FOSFS labels only), and a test of a
   contiguous directory.
4. Move tapes, configs and settings into their folders; `tape-rt` and the tapes use the new paths; deploy tooling follows.
5. Crash safety, only if real hardware shows a need (see "Journaling" below).
6. Wildcards in `ls`, `cp`, `rm` and a new `mv` (see "Wildcards in shell tools" below).

## Wildcards in shell tools (2026-10-30: to build, step 6)

`ls`, `cp`/`copy`, `rm` and a new `mv` take bash-style patterns. Today every argument is one literal reference.

- **Syntax.** `*` (any run of characters), `?` (one character), `[abc]` / `[a-z]` / `[!a-z]` (a set). Case-sensitive on FOSFS v4,
  case-insensitive on FAT/exFAT (as those volumes already compare names). `/` is never matched by `*` or `?`: a pattern
  works within one folder, and each folder component of a path may carry its own pattern (`rec/*/lp1.flac`). No `**`, no
  brace expansion, no quoting rules beyond a backslash to take the next character literally.
- **Where it expands.** In the shell, not in the kernel file calls or the tape ABI, so tapes keep taking exact names. The
  shell host asks `Host::list` for the folder, filters with a shared matcher (a `no_std` function in `crates/shell`, with
  host unit tests), and runs the command once per match, in listing order. The volume selector (`diskN`, `@LABEL`, `esp`)
  and mount points are literal; wildcards start after them. Dot files are not special.
- **No match.** An argument with wildcard characters that matches nothing is an error ("no match for PATTERN"), never passed
  on literally. An argument without wildcard characters behaves exactly as now (a file called `a*b` is reachable with
  `a\*b`).
- **`ls PATTERN`** lists the matches (folders by their own name, not their contents, unlike `ls FOLDER`).
- **`cp PATTERN DST`** needs DST to be a folder or a volume (`diskN`, `@LABEL/FOLDER`): several sources cannot share one
  file name. It makes DST's folders when they are missing on FOSFS v4 and exFAT, reports each file and carries on after a
  failure, with a count at the end. Copying a folder is not part of this (`-r` is a later step).
- **`mv SRC DST`** (new): same volume and FOSFS v4 means renaming the directory entry (one sector write, crash-safe, no data
  copied; moving between folders changes the parent field); across volumes it is copy, then remove the source only after
  the copy read back right. The pattern and destination rules are those of `cp`. exFAT rename is a rewrite of the entry set
  (new checksum), FAT32 stays copy and remove.
- **`rm PATTERN`** removes every match; more than one match asks first (`rm ... confirm` as the very next command, as the
  disk-destroying commands do) because a typo such as `rm *` has no undo. Folders match only when named by a pattern that
  matches no files, and are removed only when empty.
- **Mount points.** A pattern under a mount point expands on the mounted volume (or the boot volume's folder when it is
  absent), the same lookup as a literal path.
- **Tests.** Matcher unit tests in `crates/shell` (classes, `?`, escapes, `/` not matched, empty pattern); a QEMU test over a
  v4 volume and an exFAT volume for `ls`, `cp`, `mv` and `rm` with and without matches, fsck afterwards.

## Journaling (2026-10-30: decided, not built)

No full journal: it doubles the writes on sticks and small disks, and the directory is already the only record of what is
used (no allocation map to disagree with it). What the code does today:

- **`write` is copy-on-write.** The new data goes to free extents, and one directory-sector write (the commit) points the
  entry at them. A power cut leaves the old file or the new one. `mkdir`, `rm` and a rename-like replace are one entry
  write each; `grow` and `label` write only the superblock (backup in sector 1 first, then sector 0), and `mount` falls
  back to the backup. A torn entry write is caught by its CRC and the entry is skipped (`fsck` reports it).
- **Known gaps.**
  1. Replacing a file when the disk has no room for the new copy beside the old one reuses the old extents, which is not
     crash-safe. The alternative, failing with "disk full", is safer but refuses a rewrite that would fit.
  2. `reserve` / `write_extent` (recorder streams) overwrite in place by design; the length is written last, so a reset
     keeps everything up to the previous length.
  3. A torn entry sector loses that entry (the CRC detects it; nothing repairs it).
  4. Operations over several files (`copy` of a folder, `tapesync`) are not atomic as a set.
- **If real hardware shows trouble:** (a) make gap 1 refuse unless a flag allows it; (b) keep a second copy of each directory
  sector (or a one-sector intent record replayed at mount) so a torn write can be repaired instead of only detected;
  (c) let `fsck` repair, not just report. Only (b) is real work; (a) and (c) are small.

## Decisions (2026-10-30)

- `doc/` exists on disk but may be empty.
- Every tape lives in `bin/`; `run NAME` finds `bin/NAME`, from the shell **and** from tapes starting tapes (the tape call
  resolves the same way).
- Names: FOSFS prefers lowercase, but v4 and exFAT mounts accept upper and lower case, any sensible characters and long
  names, as a modern filesystem would. Case is kept and FOSFS lookups are **case-sensitive** (decided).
- `rec/` falls back to the root when no exFAT disk is mounted, and v4 must be able to fill a 16 GB stick: files need a short
  **extent list** instead of one contiguous run (free space fragments as takes are deleted), still first-fit, no defrag.
- More than one stream: the one-stream limit is only two kernel statics (`tape_fs.rs`). New append-only ABI calls with a stream
  handle (about 4 at once), explicit `max_bytes` per stream (0 = "largest extent" would starve the next stream), trim on close.
  exFAT: check that `fat::Stream` and the allocation bitmap are safe with two writers. This enables live decode in phono.

## Proposals awaiting a yes (2026-10-30)

- Entry: 128 bytes, 48-character name, length, 4-6 inline `(start, count)` extents; a chained extent block only if inline
  proves too few. Data first, length last, as today.
- Streams: `fs_stream_open2/write2/close2` with a handle (about 4 at once); `max_bytes` becomes a limit and the file grows in
  chunks of about 64 MiB as it is written; a full disk is a write error. Tape end closes all handles.
- `fs_read_at(name, len, offset, buf, cap)` in the same ABI bump (version 29), so a decode need not hold the whole file.
- exFAT side (`crates/fat`) to be reviewed separately for two concurrent writers.

## v4 layout revision (2026-10-30, agreed: "all points sound good")

v4 is not deployed, so the on-disk layout is changed freely before step 1 is final. Folders (parent index in the flat
entry table) are built; this revision adds:

- **Backup superblock** in sector 1 (the directory starts at sector 2). Written before the primary; `mount` falls back to
  it when sector 0 is bad. `grow` and `set_label` write both.
- **Per-entry CRC32** (IEEE, as `zlib.crc32`) over the entry with its CRC field zeroed. A slot that fails it counts as
  damaged: skipped by `for_each`, named by `fsck`.
- **Timestamps**: `mtime`, Unix seconds of the local clock reading (the clock holds local time, as FAT does), 0 when the clock is unset; `ls` shows it as on exFAT.
- **Up to 3 extents per file.** A write takes one free run when one fits, else the three largest free runs. Streams
  (`reserve`) stay one extent. Rewriting a file that cannot be allocated alongside its old copy frees the old copy first
  (not crash-safe, as before).

Entry (128 bytes): 0 kind (1 file, 2 folder), 1 name length, 2 extent count, 4 CRC32, 8 length, 16 total sectors,
24 parent (slot+1), 32 mtime, 40 name (48), 88 three extents of (start u64, count u32), 124 reserved.
Built and tested (`make test-fosfs4`: a fragmented 4 MiB disk, a broken CRC, a wiped sector 0).

Not done, deliberately: an allocation bitmap or mount-time free-space cache (rescanning is the cost, measure on a real
stick before adding one) and 4 KiB allocation units.
