# Software RAID on Fosfor OS: a tutorial

FOS-MF (the server kernel) can use disks that Linux's `mdadm` made into a RAID array, and keep them going when a disk
dies. Nothing here is fos-specific: the same disks assemble on any Linux (`mdadm --assemble --scan`), which is the way
out if you ever want to recover files with a live USB. The technical details are in [`md.md`](md.md).

You need the server kernel (`make image-mf`, or `fosadm deploy --kernel mf`). The embedded kernel has no RAID.

## 1. Which level?

| Level | Disks | Survives | Capacity | Use it for |
| --- | --- | --- | --- | --- |
| RAID 1 | 2 or more | all but one lost | one disk | small, simple, safest |
| RAID 5 | 3 or more | one lost | all but one disk | capacity with some safety |
| RAID 6 | 4 or more | two lost | all but two disks | bigger arrays |
| RAID 10 (near) | 4 or more, even | one per mirror pair | half | speed and safety |
| RAID 0 | 2 or more | nothing | all | speed only: one lost disk loses everything |

RAID is not a backup. It saves you from a dead disk, not from `rm`, a bad write or a fire.

## 2. Make an array (on Linux)

fos assembles arrays; it does not create them yet. Make one on any Linux machine with the disks attached (this erases them):

```sh
sudo mdadm --create /dev/md0 --level=5 --raid-devices=3 --metadata=1.2 /dev/sdb /dev/sdc /dev/sdd
sudo mkfs.ext4 /dev/md0
```

Wait for the first sync to finish (`cat /proc/mdstat`) before moving the disks. Two options worth knowing:

- `--bitmap=internal` (RAID 1, 5, 6, 10): after a power cut only the regions that were being written are re-checked,
  instead of the whole array.
- `--consistency-policy=ppl` (RAID 5 only; not together with a bitmap): the "write hole" is closed. A power cut in the
  middle of a write cannot leave a stripe whose parity is wrong, so a later disk failure cannot corrupt data the
  write never touched.

Without either, an array that was not closed cleanly needs a full resync before it is safe again.

## 3. Use it on fos

Put the disks in the machine and add this line to the `settings` file on the boot disk:

```
md auto
```

At boot a background thread waits up to 10 seconds (`md wait SECONDS` changes that) for every member, assembles the
array and registers it as one more disk. Boot never waits for it. Then it is an ordinary disk:

```
disks                      # the array shows up, e.g. as disk5
ext4 disk5                 # mount the ext4 inside (see docs/ext4.md)
ls disk5/
```

Without `md auto`, `md assemble` does it by hand. `md` shows the state: members in use, the events counter and any
running job. The array is opened for writing on the first write and closed clean after 3 idle seconds, on `md stop`,
`shutdown` and `reboot`.

## 4. When a disk fails

The array keeps working, degraded. fos prints a warning at assembly and `md` shows "members in use: 2 of 3". Nothing is
lost yet, but there is no spare copy: replace the disk soon.

1. Put in a new disk of at least the same size. Anything on it will be erased.
2. `md add diskN` (N is the new disk's number from `disks`). It says what it will erase.
3. `md add diskN confirm` as the very next command. The rebuild runs in the background; `md` shows the percentage.
   Reads and writes carry on meanwhile.
4. When `md: rebuild finished` appears the array is whole again.

If the power fails or you `md stop` in the middle, the rebuild's progress is saved on the new disk. The next
`md assemble` (or boot with `md auto`) says "a rebuild was interrupted"; `md resume` goes on from there. (If the array
was written to without the new disk, it starts again from the beginning instead.)

## 5. After a power cut

An array that was open when the power failed is "not clean". It comes up read-only so nothing makes it worse:

- With a **bitmap**: `md resync` re-checks only the marked regions (seconds).
- With a **PPL**: the log is replayed at assembly ("partial parity log: N entries replayed") and the array is clean.
- With neither: `md resync` compares the whole array. A degraded RAID 5 or 6 that is also unclean refuses to assemble:
  its parity cannot be trusted to rebuild the missing disk. Assemble it on Linux, or use a PPL next time.

`md force-rw` allows writes to an unclean array anyway (nothing resyncs it; run a check on Linux afterwards).

## 6. Cheat sheet

| Command | Does |
| --- | --- |
| `md` | state, members, progress of a resync or rebuild |
| `md assemble` / `md stop` | bring the array up now / close it clean |
| `md resync` | repair an unclean array in the background |
| `md add diskN` + `md add diskN confirm` | erase diskN and rebuild onto it |
| `md resume` | go on with an interrupted rebuild |
| `md force-rw` | allow writes to an unclean array |

Settings: `md auto`, `md wait SECONDS` (`docs/config.md`).

## 7. Checking that it works on your hardware

On a Linux machine as root (loop devices only, no real disk is touched):

```sh
(cd crates/md && cargo build --example probe) && sudo tools/check_md_mdadm.sh raid5-ppl
```

It lets mdadm make arrays, lets our code write to and rebuild them, and has mdadm fsck and `check` the results.
`make test-mf-md` does the same inside QEMU with the server kernel. Real-hardware results (timings, pulled USB disks)
are what is still missing; see `DEFERRED.md`.
