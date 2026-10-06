# `recorder` and the pieces under it (v2)

`v2/tapes/recorder` is a field recorder for a USB audio interface: meters from the moment the tape starts, a
pre-roll buffer, takes started by hand or by sound and ended by silence, written straight to disk as WAV.
`tape run recorder [NAME] [pre=SECONDS] [rate=HZ] [auto] [thresh=DB] [sound=MS] [silence=MS]`
(`tone=HZ [gate=ON,OFF]` replaces the device with a test tone, for tests without audio hardware).
`NAME` may carry a volume (`disk2/gig` preselects disk2 in the menu, `@label/gig` is used as typed); files are
`NAMEnnn.wav`, the next free number on the chosen disk. The `Record to:` line shows the disk (FOSFS, FAT32 or exFAT,
size, model); `D` steps through the disks that have a volume, starting at the first exFAT disk, else the first FAT32 disk, else a FOSFS disk that is not the boot disk, else the boot disk. The line below it shows free space for FOSFS and exFAT (FAT32 would need a slow scan of its FAT), as the longest
contiguous run, which is what one take can use, and how long a recording that is at the current format. The list comes from the
`disk_info` tape call (kernel `api_disk_info`); on a kernel without it the tape records to the boot volume.

Channels: a take records only the channels you pick (`ch=9-12`, or `C` in the UI: `9-12`, `1,3,5-8` or `all`; not
during a take). The meters always show every input (`Rec` marks the recorded ones, `In` the others), the silence trigger listens
to the recorded channels only, and the WAV has just those channels (in the device's own sample format). Changing the channels
starts the pre-roll over, since it holds frames of the old selection.

Keys: Space/R take on/off, A auto on/off, C choose channels, D choose the disk to record to and F type the file name (neither during a take), N/B meter pages, U Nordic/dBFS, H clear holds, Q/Esc quit.

## Libraries (all host-testable)

| Crate | What |
| --- | --- |
| `v2/tapes/tape-panel` | Buffered screen output (`draw!`, `at`, `Text`) and `MeterBank`: Nordic-style PPM bars, peak hold, clip latch, paging. Used by `audio-net` and `recorder`. |
| `v2/tapes/audio-rec` | `wav` (plain PCM / WAVE_FORMAT_EXTENSIBLE / RF64 headers), `preroll` (ring of the last seconds), `silence` (sound/silence trigger with timing), `peaks` (meters from device-format samples), `recorder` (`Recorder` over a `Sink`: block writes, header and length move together at checkpoints, split when a file is full). |
| `v2/tapes/tape-rt` `abort` | The sticky `should_abort` wrapper (was in `audio-net`). |
| `v2/fat` | FAT32 and exFAT: mount, root-directory lookup and read, and one contiguous file at a time written as it grows. |

`cargo test --manifest-path v2/fat/Cargo.toml` runs the file systems against images made by `mkfs.vfat` /
`mkfs.exfat`, checked with `fsck.vfat` / `fsck.exfat` (and read back by 7-Zip for FAT32; 7-Zip cannot open exFAT).
`make test-v2-recorder` (FOSFS, gated tone through manual and automatic takes) and `make test-v2-recorder-fat`
(FAT32 and exFAT disks in QEMU) are the end-to-end tests.

## Pre-roll and the trigger

Capture starts with the tape. Audio goes to the ring while no take runs; a take begins by writing the last `pre`
seconds from the ring, then the live audio. A take started by the trigger additionally gets `sound` ms of lead, so it
starts at the sound's onset rather than `sound` ms late. After a take ends the ring is cleared (what was in it is
already in the file). The silence that ended an automatic take (`silence` ms) stays at the end of the file.

## Files on FAT32 and exFAT

The kernel's `fs_stream_*` tape calls (and `fs_read`, used to find a free name) take `diskN/NAME` for a disk that
has no FOSFS partition but has a FAT32 or exFAT volume: a GPT basic-data partition, an MBR partition of type
0x0b/0x0c/0x07, or a whole-disk volume (`v2/kernel/src/fatvol.rs`). A recording is one contiguous run of clusters
reserved when the file is created (exFAT: "no FAT chain" and a bitmap claim; FAT32: the chain written once), so
nothing is allocated while audio flows; the directory entry's size advances at checkpoints (every 2 s) and the
unused tail is freed when the take ends.

* FAT32 files are capped at 4 GiB - 1; a take that reaches the cap continues in the next numbered file with no gap.
  WAV headers go to RF64 past 4 GiB, which only exFAT (and FOSFS) can hold.
* Only the root directory, ASCII names up to 60 characters, 512-byte sectors, one FAT copy on exFAT.
* Files are stamped with the clock (`v2/kernel/src/clock.rs`): the CMOS RTC read at boot, taken as local time, then
  advanced by the APIC timer; `date` shows it and `date set YYYY-MM-DD HH:MM` sets it and writes the RTC. A dead
  battery (no valid RTC time) gives the fixed stamp 2026-01-01 until `date set`. The modify time follows each checkpoint.
* A power cut leaves FAT32's reserved run chained to the file (`fsck` trims it) and exFAT's unused part marked
  allocated in the bitmap (a check frees it); the file itself holds everything up to the last checkpoint.
* FAT is reached only by naming its disk: `ls diskN`, `rm diskN NAME`, `copy diskN NAME diskM [NEW]` (either end may be
  FAT; FOSFS to FAT keeps the name, FAT to FOSFS needs a legal FOSFS name or a new one). A bare name, `@LABEL`, scripts,
  tapes and the start/config files only ever look at FOSFS. Whole files go through memory (64 MiB limit). `cat diskN NAME` shows files up to 64 KiB and `write diskN NAME TEXT` creates or replaces a file (not an empty one).
* Not supported: subdirectories (FOSFS is flat, FAT/exFAT are only for moving files to another OS).

## Formatting a stick as exFAT

`fat::format_exfat` (`v2/fat/src/format.rs`) writes a complete exFAT volume: boot region and its backup, FAT, allocation
bitmap, an up-case table (`v2/tools/gen_upcase.py` generates `upcase.bin` from Unicode data) and an empty root directory. The
cluster size follows the volume size as Windows chooses it (4 KiB up to 256 MiB, 32 KiB up to 32 GiB, 128 KiB above). The
kernel (`fatvol::format_exfat`) first writes a GPT with one basic-data partition at 1 MiB, so a PC mounts the stick directly.

* Shell: `format diskN exfat [LABEL]` says what the disk holds now and what will be erased, then `format diskN confirm`.
* Recorder: `X` on the chosen disk asks for a volume label (default `FOS`, up to 11 characters; Esc cancels), then for the disk's number as confirmation (any other key cancels). A disk with no volume is
  listed as `no volume` and a take on it is refused until it is formatted.
* Both go through the kernel and refuse the boot disk; the tape call is `disk_format_exfat`. Formatting takes a second or
  two, during which the recorder's capture overruns.
* Tests: `formatted_exfat_volumes_pass_fsck_and_work` (host, `fsck.exfat`), `make test-v2-format-exfat` (shell) and
  `make test-v2-recorder-format` (the UI).

## Never losing audio

Audio waits in the kernel's capture ring while the tape writes to disk. The recorder asks the kernel for as much memory
as can be spared (all free memory except 256 MiB, at most 4 GiB; `buffer=MB` sets it) through `audio_set_capture_ring`, so a
laptop with 16 GB holds many minutes of audio and a slow stretch or a long flash stall costs nothing. The size granted is
shown at start and the disk line shows the backlog against it: `Backlog 0.2/1006.4 s`.

* **Speed test:** when a disk is chosen (at start and after `D`; `T` repeats it, `notest` skips it) the recorder writes a
  scratch file for up to 3 s or 64 MiB and reports the sustained speed and the slowest block, then deletes the file.
* **Warnings** (line 7, replacing the free-space line): `!! SLOW DISK` when the disk is slower than the audio (with how long the
  buffer lasts), `Marginal disk` below twice the audio rate, `BACKLOG n%` during a take when over a quarter of the buffer is
  waiting, and `AUDIO LOST` if the ring ever overruns.
* The ring takes whole packets only: a full ring used to be able to keep half a packet, shifting every later frame and
  scrambling the channels for the rest of the take.
