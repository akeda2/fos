# Sharing block devices over Ethernet

Status (2026-11-24): the AoE target and initiator run on the server kernel (sections 8 on), with files as exports,
`mkimage`, one writer per export, and jumbo frames end to end in QEMU ("Jumbo frames on the AoE path"). This began as a
design note; the early sections keep that form.

The goal is to hand a disk from one machine to another over the lab's Ethernet: between fos hosts, and between fos and
Linux. File-level sharing already exists (`smbd`, NETFS, `//NAME/path` remote volumes, ABI 35). This note is about the
block level: a whole disk or partition, for imaging, recovery, a recorder that writes to another machine's storage, or
a fos volume a Linux box can read.

## 1. Protocols considered

| Protocol | Layer | Verdict |
| --- | --- | --- |
| **AoE** (ethertype 0x88a2) | L2 | Chosen. About 10 pages of spec; Linux has the `aoe` initiator in the tree and `vblade` as target; discovery by broadcast like our peer announcements. |
| **NBD** | TCP | Second, as the routable fallback and a sustained-load test of `tape-ip` TCP. |
| NVMe-oF/TCP | TCP | Modern and small, more work than NBD for no gain at our speeds. |
| iSCSI | TCP | Login negotiation, many PDU types. Not worth it. |
| FCoE | L2 | Needs lossless Ethernet (DCB/PFC) and a fabric login. |
| NVMe-oF/RDMA (RoCE v1) | L2 | Needs RDMA NICs; ours are e1000, I350, RTL. |
| HyperSCSI, ENBD, GNBD | | Dead. DRBD replicates, it does not export. |

## 2. AoE in one page

Frame: Ethernet header, then `ver/flags`, `error`, `major` (16 bit shelf), `minor` (8 bit slot), `command`, `tag`
(32 bit), then the command's arguments. Commands: 0 = ATA, 1 = Query Config. For ATA: the 8-byte register block
(`aflags`, `err/feature`, `count`, `cmd/status`, `lba0-5`) followed by data. A target needs:

- Query Config (answer the broadcast, so `aoe-discover` finds us);
- IDENTIFY DEVICE (0xEC): capacity and a fake CHS/model string;
- READ/WRITE SECTORS EXT (0x24, 0x34), LBA48;
- FLUSH CACHE EXT (0xEA).

Limits, all inherent in the protocol:

- **One command per frame pair.** The payload is what fits in the frame: at MTU 1500, 2 sectors (1 KiB); at 9000, 17.
  Throughput depends on the initiator keeping a window of commands in flight (Linux does).
- No congestion control, retransmit is by tag after a timeout.
- Only 512-byte logical sectors, no authentication beyond MAC masks, one broadcast domain.

At the recorder's worst case (30 channels, float32, 192 kHz, 23 MB/s) gigabit has room; the risk is latency jitter in
the write path, not bandwidth.

### Sector size: 512 on the wire

AoE counts 512-byte sectors and nothing else; Linux's initiator assumes it. A 4096-byte logical sector is not
expressible in standard AoE, and it would not fit a 1500-byte frame anyway (2 x 512 fits, 1 x 4096 does not). So the
target exports 512 and says so explicitly (IDENTIFY word 106). What 4 KiB changes is **alignment**: ext4 and FOSFS v4
write in 4 KiB blocks, and at MTU 1500 the initiator splits each into two-sector frames (1 KiB), which a 512e or 4Kn disk
behind the target turns into read-modify-write. Remedies, in order: jumbo frames (17 sectors per frame, so a 4 KiB write
is one frame), a target that gathers consecutive frames of one request before writing (an ATA write is acknowledged from
the disk's cache anyway, `FLUSH CACHE EXT` is the durability point), and aligning the exported partition to 1 MiB. FOSBLK
(section 3) can carry a real sector size and a preferred I/O size without changing the unit.

## 3. Two dialects

Linux initiators and targets speak standard AoE. Between fos hosts we can do better, but not inside AoE's namespace:
Linux `aoe` and `vblade` answer unknown commands with an error, and 0x88a2 is not ours.

Decision: **standard AoE on 0x88a2 for Linux, a separate protocol `FOSBLK` on its own ethertype between fos hosts.**
The same disk can be served by both at once. What FOSBLK adds:

1. **Bulk extents.** One request names an extent (LBA, count); the target streams the data frames back without a
   request per frame. This removes most of AoE's per-frame ceiling at MTU 1500, so jumbo is optional.
2. **Selective retransmit** (a bitmap of missing sequence numbers, as NETFS bulk transfer does) and a simple window.
3. **True geometry**: sector size, capacity and a stable identity (label/GUID) instead of the IDENTIFY fiction.
4. **Write barriers** and a real flush, for the ext4 journal.
5. **Claim/lock** (see 4).

Order of work: AoE first (it is the baseline and gets us a Linux test peer for free), measure, and only design FOSBLK
for what the measurement shows AoE cannot do. If a window over plain AoE covers the recorder, FOSBLK may be only
claim/lock and true geometry.

## 4. The sharing problem

Two hosts must never mount the same writable filesystem. Safe shapes:

- **Exclusive hand-over.** One claimant at a time; the owner releases (or the claim times out) before another host
  mounts. FOSBLK carries this; standard AoE has no claim, so a Linux initiator is trusted not to write a volume fos
  has mounted. The target should export read-only unless told otherwise.
- **One writer, many readers.** Readers see stale metadata. FOSFS v4's backup superblock and per-entry CRC32 let a
  reader detect a torn update; it still needs a generation counter or an explicit "refresh".

Never export the boot disk (`disk1`) writable. Exporting is an explicit command, like `format ... confirm`.

## 5. Where it lives

- Target as a **tape** on the mf kernel, using the unhandled-frame hook (`tasks::service_owns(index)` in
  `term.rs` already gives a service a NIC's frames), so the kernel stays small and the protocol is easy to change.
  Disk access through the existing `vfs_*`/disk calls; a block read/write call may need to be added to the tape ABI
  (append-only, version bump).
- Initiator: a block device behind the same abstraction the AHCI/NVMe/USB disks use, so `diskN` names it and FOSFS and
  ext4 mount on it unchanged. It runs on its own thread so a slow remote never stalls the recorder's audio path.
- Frame building in a shared crate (`tape-net`), not copied into the tape.

## 6. Transport: jumbo frames

| Driver | Before this change | Limit of the silicon (Linux's tables) |
| --- | --- | --- |
| e1000 family | `MAX_FRAME` 1518, 2 KiB RX buffers, `RCTL.LPE` on (VLAN), oversize frames dropped | 82540 up to 16 KB, 82574L about 9 KB, I350 about 9.2 KB, I219 about 9 KB |
| RTL8168 | `MAX_FRAME` 1514, 2 KiB buffers, `RMS` from the buffer size | B/C/D about 6-7 KB, G/H about 9 KB |
| RTL8139, 3C905 | 1500 by design | no |

Jumbo is **opt-in per NIC** (`nic mtu N SIZE`), never default: every switch port on the path must carry it, or frames
vanish silently, and most of the stack (the poll buffer, the frame queue, the tape ABI's `nic_max_frame_len`) is sized
for 1518. A NIC left at 1500 behaves exactly as before.

## 7. What exists: e1000 jumbo (prototype)

`crates/kernel-core/src/e1000.rs`:

- `E1000::set_mtu(n)` raises the largest frame accepted and sent, up to `JUMBO_MAX_MTU` (9000, a conservative figure
  under every variant's limit); `mtu()` reads it back. The default stays 1500. The first call above 1500 allocates a
  12 KiB join buffer.
- **Receive** keeps the 2 KiB buffers. A frame larger than one buffer arrives as several descriptors (`RCTL.LPE` was
  already on); `receive` joins them in the join buffer. The card may finish a frame's first descriptor before its last,
  so the pieces are kept between calls (`rx_in_frame`, `assembly_len`). A frame that does not fit the caller's buffer
  is dropped whole and counted (`jumbo_dropped`), never delivered truncated, so callers with the old 1600-byte buffers
  (`term::poll`, `drain_nics`) are safe. At MTU 1500 a multi-descriptor frame is still dropped, as before. A one-buffer
  frame within the old 1518 bytes is still cut to a small caller's buffer, as before. `jumbo_joined` counts joined frames.
- **Transmit** buffers are `TX_BUFFER_SIZE` (12 KiB, 1.5 MiB per NIC, whether or not jumbo is used) so one descriptor
  holds a jumbo frame; `send` refuses anything over `mtu + 18`.
- Shell: `nic mtu N` shows the MTU and the joined/dropped counters, `nic mtu N SIZE` sets it, `nic mtu N send LEN`
  broadcasts a frame of LEN bytes and `nic mtu N listen` takes frames for 5 s into a 9 KB buffer and prints each one
  over 1518 bytes with its length and byte sum. The last two exist for the test: nothing else in the stack can carry a
  frame that size yet (the poll buffer, the frame queue, the tape ABI's `nic_max_frame_len`).
- `make test-jumbo` (`tools/test_jumbo.py`, three QEMU guests): a 4000-byte frame from the host and a 6000-byte frame
  from another guest arrive whole with their bytes intact; a 9001 MTU and a 3000-byte send at MTU 1500 are refused; a
  guest left at 1500 counts the jumbo frames as dropped and carries on. `make test-nic` still passes.

Checked in QEMU only (its e1000 takes jumbo frames). What only real hardware can show:

- The 82574L and I219 split one packet buffer between receive and transmit, and Linux's `e1000e_reset` changes `PBA` for
  jumbo so transmit can hold two full frames; some PCH parts need more setup. Not done here.
- The I350: its advanced descriptors with a frame split over buffers (with and without the timestamp header, which the
  code takes from the first buffer only) and whether its `RLPML` register needs setting. Not done here, and the QEMU
  model is an 82540, so none of the I350 path ran.
- Whether the lab's switch ports pass 9000.

Do not enable it on a machine until a `nic mtu` test frame has crossed that machine's real link.

### Jumbo frames on the AoE path (2026-11-24)

- **Receive.** `nicctl::drain_nic_to_fifo` receives into one `RX_FRAME_MAX` (9216-byte) buffer shared by all NICs
  (drains are serialised under the NIC lock), so a joined jumbo frame reaches the kernel's handlers. The tape FIFOs keep
  1600-byte slots: a larger frame no handler took is dropped and counted (tapes see at most 1518, `nic_max_frame_len`).
- **Target.** Each export's queue holds 64 frames of 9216 bytes (576 KiB), allocated with its first export; in .bss they
  would not fit the BIOS loader's kernel span. A request is answered with the MTU of the NIC it came in on (read once a
  batch), so Query Config advertises 17 sectors a frame on a 9000-byte NIC and 2 on a 1500 one.
- **Initiator.** Its reply queues (128 x 9216 bytes, 1.1 MiB) are allocated at an entry's first attach. The sectors per
  frame are fixed at attach from the target's figure and our NIC's MTU, so set the MTU first: `nic-mtu N SIZE` in
  `settings` is applied when the NICs come up, before `aoe-attach`; after `nic mtu` in the shell, detach and attach.
- **Shell.** `nic mtu [N [SIZE]]` on the server kernel (show, set). `writespeed diskN [MIB] [KIB] confirm` is the write
  twin of `readspeed`: raw writes from sector 0 (a pattern per request), then a flush, the rate with the flush included;
  never disk 1 or a disk with an AoE export.
- **RTL8168G/H.** `Rtl8168::set_mtu` (up to 9000; older chips stay at 1500): the first MTU above 1500 moves both rings
  to 9 KiB buffers (4.5 MiB) and resets the chip onto them, RMS follows the buffer, `MaxTxPacketSize` becomes 0x27 (Linux's
  `EarlySize` on these chips); Linux sets no jumbo enable bits for them (`rtl_jumbo_config`). A frame over the MTU or the
  caller's buffer is dropped whole (`jumbo_dropped`). Not run on hardware yet (QEMU has no RTL8168).
- **Checked** in QEMU (`make test-mf-aoe-initiator`): both guests at MTU 9000, 17 sectors a frame, a FOSFS file read back
  with its SHA-256, `writespeed` 8 MiB over AoE and its pattern read on both ends.
- **Not yet:** the I219's jumbo setup (Linux's `e1000_lv_jumbo_workaround_ich8lan`, PBA and watermarks; needed on aneby),
  the I350 path, and a real link at 9000.

## 8. The target (`crates/aoe`, done) and what is next

`crates/aoe` is the protocol and the target, `no_std`, no allocation, host-tested (14 tests, `make test`): Query Config
(read, test exact/prefix, set if empty, force set), IDENTIFY, READ/WRITE (LBA28 and LBA48), FLUSH CACHE (EXT), error
replies for unknown commands and versions, ATA aborts for out-of-range, oversize, short writes, read-only and disk
failures. `Target::handle(frame, &mut dyn Disk, out)` answers one frame; it never panics on a truncated one. MAC mask
lists and reserve/release (commands 2 and 3) are answered "unknown command". No queuing: each frame is answered before
the next is looked at.

The kernel side is `kernel-mf/src/aoetarget.rs` (server kernel only, which is also the only place it will run):

- `net.rs::frame_hook` takes 0x88a2 frames while a disk is exported and only queues them (it runs under the NIC lock; a
  disk read there would stall every interface and the audio drivers). The export's worker thread reads or writes the disk
  through `block::` under the disk lock, builds the reply with `crates/aoe` and sends it. A full queue drops the frame.
- Writes use `block::write_deferred`; the drive is flushed on FLUSH CACHE (EXT) and by a flusher thread after a pause in writing.
- Shell: `aoe` (list with counters), `aoe export N|@LABEL [slot N] [rw for MAC|rw confirm]`, `aoe export FILE [slot N]
  [rw for MAC|rw confirm]` (a FOSFS or ext file, "A file as an export" below), `aoe unexport SLOT`, `aoe shelf N`. A whole
  disk is slot N of the shelf unless `slot` says otherwise; a file takes the first free slot from 100. Disk 1 is never
  writable as a whole. Settings: `aoe-shelf N`, `aoe-export N|@LABEL|FILE [slot N] [rw for MAC]` (applied after boot, in
  order, with the shell's messages; docs/config.md).
- **One writer.** `rw for MAC` names the one machine that may write: the target compares the source MAC of every write
  and FLUSH (`aoe::Target::writer`). Another machine's write is aborted (an I/O error there), its FLUSH answered without
  flushing; its reads are still served. The first refusal is printed once per export ("aoe: slot N refused a write from
  MAC"), and `aoe` shows the writer and the count. `rw confirm` lets any machine write, as before; `settings` only takes
  `rw for MAC`, since nobody is at a settings line to confirm the other. The MAC is the initiator's NIC (`nic`, or
  `aoe discover` on that machine shows the target from it). A MAC, not a host name: names come from announces every 10 s,
  so right after a boot a named writer would have its writes refused. AoE's own Reserve/Release (a MAC list the
  initiator sets) is not used: Linux's `aoe` never sends it. The check is against mistakes, not an attacker: anyone on the
  segment can send a frame with another machine's MAC. It also does not stop this machine from mounting the disk.
- `@LABEL` names the disk that holds the volume with that label (FOSFS, FAT/exFAT or ext, in that order), and the whole
  disk is exported, partition table included, as with a number. Disk numbers follow discovery order and can change
  when a disk is added, but a label stays with its volume. The slot is the address initiators attach to (`e64598.2`),
  so give a label export a fixed `slot`; without one it takes the disk's number at boot. One disk is exported whole
  under one slot only: a second `aoe export` of it, by label or number, is refused.
- `make test-mf-aoe` (`tools/test_mf_aoe.py`): the host is the initiator over the QEMU multicast-socket network. Query
  Config, IDENTIFY, reads (first, middle, last sector) against the host's file, aborts (out of range, read-only write),
  the shell's refusals, a two-sector write that reaches the disk image after FLUSH.

**Threads, and what they can and cannot do.** Each export has its own queue and worker thread (a slow disk fills only
its own queue), and an `aoe-flush` thread flushes a written disk 50 ms after its last write (and `unexport` does).
Workers serve up to 8 frames per hold of the disk lock. Reading the drivers showed what threads cannot do:

- Every disk user in mf (shell, tapes, NETFS, KUP) takes the one `tapefs::lock()`, because the AHCI, NVMe and USB drivers
  keep their state in statics, poll and run one command at a time. The first version of the target did not take it, which
  could have raced with any other disk user on the same controller. It does now.
- So disk transfers are serial across all exports and everything else, however many workers there are. More workers
  would not read two disks at once, and several requests in flight to one disk is not possible with these drivers. Doing
  that means locks per controller in the drivers, a change in the core of the kernel, not in this module.

**Measured** (`tools/bench_mf_aoe.py`, 2026-11-22: QEMU without KVM on a 4-core host, a Python initiator with a window of
16 reads or writes of 2 sectors, MTU 1500; the host's `/dev/kvm` was not usable by the build user at that point, so these
first numbers are relative; the KVM numbers follow):

| What changed | idle, 1 outstanding | idle, window 16 | shell printing | shell reading the disk |
| --- | --- | --- | --- | --- |
| first version (threads pinned to CPU 0 by `sched::spawn`) | 98 frames/s | 3,100 | 150 | 0 |
| `sched::spawn_any` for the workers (committed) | 1,600 | 10,500 (10.7 MB/s) | 217 | 0 |
| plus a `net-rx` thread woken by the NIC interrupt (committed) | | 10,900 | 5,300 | 3,500 |
| (a thread that spun on the NICs, not kept: it starved the SysRq rescue) | | 10,900 | 10,400 | 10,300 |

- A round trip of 10 ms (98 a second with one frame outstanding) was the scheduler's tick: `spawn` pins a thread to CPU 0,
  where the service thread spins on `yield_now`. Unpinned workers run on an idle CPU at once. Writes behave like reads (10.8k
  idle). Not the disk lock, and not `SleepMutex` (it wakes at once).
- **Any shell command on the service thread stops AoE.** `service_thread` drains the NICs (`net::poll`) and also runs the
  shell's commands, so while a command prints or reads a disk nothing is drained and no frame reaches a worker. The same
  holds for every network service on mf (KUP, the net terminal, tapes' FIFOs) during a long command.
- **The `net-rx` thread** (`net::rx_thread`) drains the NICs when one interrupts. Each NIC driver's interrupt handler calls
  `fos_hal::net_event()` (a defaulted `Platform` method: the embedded kernel and the Pi ignore it) and mf's implementation
  wakes the thread. It holds the NIC lock only while draining, never while waiting; a thread that spun on the NICs
  gave the best numbers (10,400 and 10,300) but starved the SysRq rescue, which drains with `try_lock`.
  With it a shell command no longer stops the network: 5,300 frames/s while the shell prints (was 217) and 3,500 while it
  reads a disk (was 0). The rest of the gap to idle is the shell's own work competing for the two CPUs of the test guest
  (and TCG): not tuned. A guest with more CPUs than the host has free (`--smp 4` on this 4-core host) only measures the host.
- The service thread still drains too: a NIC that has no interrupts (polled) is only served there. Frame handlers
  (terminal, discovery, KUP) now run on either thread, always under the NIC lock.
- `make test-mf-sysrq` changed: with the service thread spinning, the receive thread answers SysRq (the dump says "service
  thread last went round N ms ago") instead of the timer interrupt. The rescue is still tested by the deadhang case (a CPU
  with interrupts off cannot take the NIC interrupt), and it remains the backstop when the drain is stuck.

**Under KVM** (same benchmark, same host, 2026-11-22, after the user joined the `kvm` group; guest CPUs 2, window 16):

| | idle | shell printing | shell reading a disk |
| --- | --- | --- | --- |
| reads | 6,455 frames/s (6.6 MB/s) | 6,388 | 5,848 |
| writes | 6,284 | 6,175 | 5,807 |

- With `net-rx` a busy shell now costs at most about 10 percent, not 97 or 100.
- One frame outstanding gives 4,530 frames/s: a round trip of about 220 microseconds, so the 10 ms tick is gone. From window 4
  up the rate stays at about 6,500 whatever the window (1 to 64) and whatever the guest's CPU count (2 or 4).
- **Where the time goes** (`aoe` prints per-worker counters, `aoe reset` clears them, `bench_mf_aoe.py --stats` shows them
  per run). At window 16 a worker never sleeps (one wake-up for 19,516 frames), `net-rx` runs within about 60 us of the
  interrupt, the disk lock costs nothing, and serving a frame costs 153 us: **121 us for the disk transfer and the reply,
  31 us for the send** (NIC lock and the e1000's doorbell). 1 / 153 us is the 6,500 ceiling. So the limit is one AHCI
  command per 1 KiB frame (in QEMU each command is several register writes that leave the guest), and to a lesser
  degree one doorbell per reply. Wake-ups, which I had guessed at, are not it.
- The counters are kept in TSC cycles and converted when printed. A first version read `time::micros()`, which under
  KVM's default CPU model (no invariant TSC) reads the ACPI PM timer through an I/O port: six reads a frame cost about
  40 us and pushed the ceiling down to 4,700.
- **One CPU: 100 frames/s with one frame outstanding**, a 10 ms round trip again (3,100 with window 16). With a single CPU
  every wake-up waits for the scheduler tick. A server has more CPUs; not looked at further.

**Batching** (`crates/aoe/src/cache.rs`, `aoe::cache::Cache`, host-tested): each worker puts a cache with two 64 KiB
buffers in front of the disk.

- A read that starts where the previous one ended fills the read buffer with one disk command of up to 64 KiB; the following
  reads are memory copies. Other reads go straight to the disk (random reads pay nothing extra). A write updates the read
  buffer where they overlap. The worker drops the buffer after sleeping 2 ticks or more (the bound on staleness when
  something else writes the disk) and when another export takes its slot.
- Consecutive writes are gathered into one disk command and acknowledged from RAM, as a drive's write cache would. The run
  is written out before an overlapping read, on FLUSH, when a write does not continue it, and at the end of every batch
  (32 frames) while the worker still holds the disk lock, so acknowledged data never outlives the batch and the flusher and
  `unexport` see everything. A run that fails after its frames were acknowledged is counted and makes the next FLUSH fail.
- Under KVM, window 16: **reads 6,500 -> 20,900 frames/s (21.4 MB/s), writes 6,300 -> 20,000 (20.5 MB/s)**; window 4 about
  19,000 for reads; one frame outstanding 4,700 -> 6,800 for reads (read-ahead now hits) and 4,600 -> 6,150 for writes
  (the reply goes out before the run reaches the disk). 61,421 read frames took 1,060 disk reads; 59,844 write frames 1,901
  disk writes. With the shell busy: 18,500 to 19,200 for reads, 16,200 to 17,800 for writes.
- What a frame costs now is the send: 44 us of 48 at window 16 (the NIC lock, which the spinning service thread and
  `net-rx` also take, and one e1000 doorbell per reply).

**One doorbell for several replies.** `E1000::send_deferred` queues a frame without writing the tail register (TDT), and
`kick` writes it once for all of them (`Nic::send_deferred`/`kick`, `nicctl::nic_send_deferred`/`nic_kick`; other
cards send at once). Any ordinary `send` writes the tail as well, which covers deferred frames, and `send_deferred` kicks
by itself after 32 unannounced frames so the ring never fills with them. The AoE worker queues each reply deferred and
kicks every 8 replies and at the end of each batch. Under KVM, window 16: **reads 20,900 -> 32,700 frames/s (33.5 MB/s),
writes 20,000 -> 29,500 (30.2 MB/s)**; serving a frame costs 15 to 19 us instead of 48. Windows 1 and 4 do not change:
they are bound by the round trip, not the send. Checked: `test-nic`, `test-jumbo`, `test-mf-aoe`, `-net`, `-netfs`,
`-smbd`, `-httpd`. Only real hardware can say what a TDT write costs on silicon (a posted PCIe write, far less than a
VM exit), so the gain there will be smaller.

**The Linux `aoe` module as initiator** (section 9, 2026-11-22, host kernel 7.3, AoE v85, KVM, 2 guest CPUs, MTU 1500):
discovery, content, the read-only refusal, raw writes, ext4 and `e2fsck` all pass (details in section 9). The module
picks 1024-byte data frames (2 sectors, as advertised) and keeps 16 frames in flight (`bufcount`): **reads 58 MB/s
(about 57,000 frames/s), writes 50 MB/s (about 49,000)**, against 32,700 and 29,500 from `bench_mf_aoe.py` at the same
window. So most of the window-16 ceiling seen above was the Python initiator. The guest's counters during that run: serve
13 us a frame (send 10-11 of it), 16-19 frames per worker wake-up, 64 read-ahead frames per disk command, 16 written
frames per disk write; the service thread still drained a third of the frames (`net-rx` the rest).

Not done, in the order I would do it:

1. **Per-controller locking** in the block layer, if measurements on hardware say serial disk access is the limit.
2. Jumbo frames on this path: done in QEMU ("Jumbo frames on the AoE path"); hardware needs the I219 setup.
3. `vblade` as a peer, and the Linux test on real hardware (a fos machine exporting to a Linux box over a real switch).
4. FAT and exFAT files as exports (FOSFS and ext files are done), and file exports in `settings`.

Small requests (128 KiB reads at 90 MB/s against 113 in 1 MiB ones) are done: read-ahead in the initiator, "Read-ahead"
below.

### A file as an export

```
aoe export FILE [slot N] [rw for MAC|rw confirm]    FILE: diskN/path, @LABEL/path, extN/path or a name on the boot volume
```

The same in `settings`: `aoe-export @data/vm.img slot 8 rw for 52:54:00:12:34:e2` serves the file from boot on, writable by
that machine only. An ext file on another disk needs that disk found before `aoe-export` runs (the end of boot, before
`aoe-attach`, so not a file on an AoE disk).

A disk image kept as a file (a VM's disk, a test volume, an installer image) can be served without giving the whole disk
away. FOSFS and ext2/3/4 files work; FAT, exFAT and remote (`//NAME/...`) files are refused for now.

- **Asked once.** At export time `tape_vfs::file_map` asks the file system where the file lies: FOSFS gives up to three
  extents, ext4 its extent tree (or ext2/3's block pointers) through `Fs::map`. The answer becomes a list of runs in file
  order: so many sectors at this disk sector, or so many that read as zeros (a hole, or an extent allocated but never
  written). Adjacent runs are merged (`filedisk::extend`): ext4 stores at most 32768 blocks per extent, so a large
  file in one piece arrives as many extents and is kept as one run.
- **Served like a disk.** The worker puts `aoe::filedisk::FileDisk` between the target and the block layer: a request is
  split where it crosses from one run into the next (found by binary search), zero runs read as zeros, and a write
  that touches one is refused as a whole. No file-system code runs per frame, and the worker's cache sits on top as
  for a disk. The export is the file's size in whole sectors; a partial last sector is not served (the shell says so).
- **`rw` only without holes.** Writing into a hole would need the file system to allocate, which an export cannot do, so
  a file with any is exported read-only or not at all ("has N sector(s) with no disk space behind them").
- **The guard.** The map is a snapshot, so it must not go stale: while a file is exported, `block::write`,
  `write_deferred` and TRIM refuse every request that touches its sectors (`aoetarget::guarded`, a sorted range list
  per export, one atomic load while there is none). Only the export writes them (`block::write_exported`). This holds
  for read-only exports too. Removing or replacing the file here still works on the file system, but its old sectors
  stay out of use: a file written onto them fails on the refused write ("aoe: refused a write to disk N sector S", and
  `aoe` counts them) instead of the export serving another file's data, or writing into it. `aoe unexport` lifts it.
- **Kept mounts.** An ext volume the tape calls keep mounted (`ext4vol::shared`) caches blocks; after a batch that wrote
  an ext file the worker drops that mount and its cache (`ext4vol::wrote_behind`), so a tape (an SMB server) reading
  the file here sees the new data. FOSFS keeps no data cache.
- A file export and a writable whole-disk export of the same disk refuse each other (two writers). A file on the boot
  disk may be exported `rw`: only its own sectors become writable.

### mkimage

```
mkimage FILE SIZE     a zero-filled image (SIZE: 64M, 20G, 1T, whole MiB) on a FOSFS or ext volume
mkimage               how far it is (MiB, %, MiB/s)
mkimage stop          stop it; the half-made file is removed
```

An image to export `rw` must have no holes (above), so `mkimage` writes every sector instead of making a sparse file:
the export's map then never changes, and the space is taken up front, so an initiator never meets a full volume in the
middle of its work. The cost is writing it once. It runs in a thread (an mf shell command cannot read the keyboard or the
network terminal while it runs), 1 MiB per hold of the disk lock, so AoE exports and other disk users go on meanwhile.

- FOSFS: one reserved extent of the whole size (`Volume::reserve`), so the image is one run. ext: the volume is asked for
  a region with room for the file (`Writer::place`, as the recorder does) before the first write; later blocks follow the
  file's last extent, so it is a few long runs (`aoe export` says how many). ext changes are committed every 256 MiB.
- Refused: an existing name, a size above the free space (ext) or the largest free run (FOSFS), FAT/exFAT and remote
  files (they cannot be exported), and a volume whose disk is exported whole and writable (another machine may be
  changing that file system; the fill also stops if such an export starts while it runs).
- While it is being made the file must not be removed or exported. The fill looks the FOSFS entry up before every chunk
  and stops if it moved; ext resolves the path on every write; and an export of the half-made file is guarded like any
  other, so the fill's next write is refused. A stopped or failed fill removes the file.
- An image made on Linux for this needs `dd if=/dev/zero`, not `fallocate` (whose extents are unwritten, which `rw`
  refuses as holes) or `truncate` (holes).

`make test-mf-aoe-file` (`tools/test_mf_aoe_file.py`): a FOSFS file on the boot volume read-only (size, reads, a refused
write, the partial last sector); an ext4 file `rw` (reads, a write that a tape then reads through the kept mount, and
the guest's `sha256` of the changed file; without `wrote_behind` the tape read the old data); a sparse ext4 file
(`rw` refused, holes read as zeros); and the guard (an exported file removed and a new one written by first fit onto
its sectors: refused, the export still serves the old data, writable again after `unexport`). `crates/aoe` has host
tests for the run splitting, zero runs, merging and a target serving a file through the cache.

Not checked on hardware yet; the per-frame path is the whole-disk one with a run lookup added.

### The initiator (`aoe::initiator`, `kernel-mf/src/aoedisk.rs`)

A disk another machine exports becomes one more `diskN` (`block::Backend::Aoe`), so `sector`, `format`, FOSFS, ext4 and
the tapes' `vfs_*` calls use it unchanged. Standard AoE only, so it works against this kernel's target and should against
`vblade`; the FOS dialect (section 3) is for later.

```
aoe discover                  Query Config on every NIC with a link; lists SHELF.SLOT, MAC, NIC, sectors/frame, bufcount
aoe attach SHELF.SLOT [rw] [MAC]  IDENTIFY, then a disk number; read-only unless rw; MAC picks the target's NIC
aoe detach SHELF.SLOT         requests fail from now on; attaching the same target again gives the same disk number
aoe                           also lists the attached disks: NIC, sectors a frame, window, requests, frames, resends,
                              read-ahead fetched and used, what the aoe-ahead thread did, and settings targets not found yet
```

**At boot**: `aoe-attach SHELF.SLOT [rw] [MAC]` in `settings` (one line per target, same words as `aoe attach`). The
service thread attaches them before the `start` file runs, so `start` and the services find the disks. A NIC's link takes
a few seconds after boot and the target may be booting as well, so it tries for up to 10 s. A target still missing then
does not hold up the boot: it is printed, `aoe` lists it as "waiting for", and an `aoe-attach` thread looks for it every
10 s until it answers (then it prints the disk number as `aoe attach` would) and ends when none is left. A refusal a
retry cannot change (this machine's own export, no free disk number) is printed once and dropped. `rw` here is the same
decision as `rw` on the command line, written down. `make test-mf-aoe-initiator` boots the initiator with one target that
answers (its `start` file's `disks` lists it) and one that does not, which it attaches once the target exports that slot.

- **The protocol half** is `crates/aoe/src/initiator.rs`, `no_std` and host-tested against the crate's own `Target` over
  a simulated link that loses requests and replies and reorders them (`initiator_tests.rs`). `Transfer` is one block
  request: it splits it into frames of `per_frame` sectors, keeps up to `window` in flight, matches replies by tag
  (`tag_base + frame index`, a fresh range per request so a late reply to an earlier one never matches), resends a
  frame with the same tag when it times out (doubling the wait) and fails the request after `MAX_TRIES` sends, on an
  ATA error status or on an AoE error. Its clock is the caller's.
- **No thread of its own.** A request runs in the caller's thread: it sends under the NIC lock (released at once) and
  sleeps until a reply or the next resend is due (`sched::wait_while_for`, new: a `Blocked` thread the timer tick also
  wakes at its `wake_at`). `frame_hook` hands frames with the R flag to the initiator and requests to the target; the
  initiator's part only copies the reply into the queue of the remote it is from and wakes it. With a polled NIC (no
  interrupt, so `net-rx` never runs for it) the waiter drains the NICs itself every tick.
- **Time** is in ticks (10 ms): first resend after 2 ticks, 8 sends in all, so a target that has gone fails a request
  after about 5 s (5.6 s measured) instead of hanging the caller. The window is the target's `bufcount`, at most 64;
  sectors per frame the least of what the target advertises, what our MTU carries and the 1600-byte reply buffer.
- **Safety.** Read-only unless `rw` (a write then fails here, before the wire). A target with one of this machine's own
  MACs is refused: that is our own export, whose worker needs the disk lock the waiting caller may hold. The target's
  `rw for MAC` keeps a second host from writing (its writes fail with an I/O error); with `rw confirm` nothing does, and
  the initiator's `rw` is where a person decides it. While a
  request waits on the network, its caller still holds `tapefs::lock()` if it took it, so local disk users wait too; two
  hosts that each read the other's disk while holding their own lock would wait on each other until the resends fail.

**Speed** (KVM, two guests on one QEMU multicast segment, 2 vCPUs each, on a 4-CPU host, 2026-11-22): `cp` of a 4 MiB
file onto a FOSFS volume on the attached disk **15.5 MiB/s**, `sha256` of it read back **14.4 MiB/s**; the Python
initiator against the same target in the same setup reaches 29,700 frames/s (30 MB/s) at window 16. The initiator's
counters (`aoe`): about 380 us from a burst sent to its first reply, about 55 us from a reply's arrival to the request
taking it.

**Window 32.** The requests are large (`cp` and `sha256` ask for 1 MiB, about 1000 frames), so running several at once
would gain little. The cost was that the two sides took turns: the initiator sent 16 frames, the target's worker woke
(about 300 us under KVM), served 16 and slept, and the initiator woke for the replies and sent 16 more. The target's
queue holds 32 frames per export but it advertised the crate's `bufcount` of 16; it now advertises 32, and the
initiator keeps up to 32 in flight (its reply queue holds 64), so the worker more often finds the next frames already
queued. With a 16 MiB file: window 16 wrote 17.0 and read 15.5 MiB/s (1908 worker wake-ups), window 32 20.9 and 19.5
(850), window 64 21.6 and 20.9 (256 wake-ups, but 4 frames resent and twice the queue memory), so 32 it is. Linux's `aoe`
module takes the advertised `bufcount` as its window too.

Measuring it found a **scheduler bug that cost every mf machine**: CPU 0 had no registered idle thread (its boot context
idled by convention only), so the wake IPI never went to CPU 0 and a thread pinned there that another CPU woke waited for
the next interrupt on CPU 0, often the 10 ms tick. The service thread, and so every shell command, is pinned to CPU 0:
the initiator waiting in `cp` saw each reply 1.4 ms after it arrived. `sched::become_idle` registers the boot context when
boot ends. Before and after on the same host: the initiator 3.9 -> 15.5 MiB/s writing, 3.6 -> 14.4 reading; the target
under the Python benchmark at window 16 14,700 -> 29,700 frames/s (the target's own `net-rx` ran 293 us after its
interrupt before, 34 us after). A brief spin before sleeping (300 us) was tried first and gained nothing once wake-ups
worked, so it is not in.

**On hardware** (2026-11-23): rack exporting its `kvart` disk (ext4 on SATA) read-only through its RTL8168H, aneby
attaching it over one gigabit switch. `sha256` of a 694 MB file on that volume: **16.9 s, about 41 MB/s**, the same digest
as rack's local `sha256` (7.7 s, about 90 MB/s, the disk's and hash's own ceiling). 678,533 frames, none resent or
dropped. Wake-ups cost little on real CPUs: the initiator took a reply 1 us after it arrived (55 us under KVM), the
target's worker ran 57 us after its frames were queued (about 400 us under KVM), 66 us from a burst sent to its first
reply. The worker served only 17 frames a wake-up and aneby took one interrupt per 2.5 frames, which looked like the
next limit; the raw reads below showed it was the window and the RTL8168 instead.

**Raw reads and the NICs** (`readspeed diskN [MIB] [KIB]`, new: the block layer only, no file system, no hashing).
Most of the 16.9 s above was not the network: rack's `kvart` reads 456 MB/s locally, so its 90 MB/s `sha256` was the
hash, and `sha256` reads and hashes in turn. Raw over AoE, 1 MiB requests:

| rack's NIC | window 32 | window 64 | per frame on rack (serve, of it send) |
| --- | --- | --- | --- |
| RTL8168H, doorbell per frame | 91 MB/s | | 8 us, 4 us |
| RTL8168H, deferred doorbell | 91 MB/s | 90 MB/s | 5 us, 1 us |
| RTL8168H, also max read request 4096 | | 113 MB/s | |
| I350 | 106 MB/s | 113 MB/s | 4 us, 0 us |

Gigabit carries at most about 118 MB/s of 1 KiB AoE payload, so through the I350 at window 64 the link is about 96% used,
with no resends. Interrupts and wake-ups cost little: aneby's `net-rx` ran in under 1 us and took a reply 1-2 us after
it arrived. What mattered:

- **The window.** 32 replies of 1 KiB are about 280 us of wire time, so every microsecond of round trip beyond that left
  the wire idle; 64 hides it. The target now advertises a 64-frame queue (it shares it between initiators).
- **The RTL8168's doorbell.** Its `TPPOLL` is an I/O-port write, non-posted on PCIe, about 4 us a frame. It now has
  `send_deferred` and `kick` like the e1000 family (one doorbell per burst, at most 8 frames held back; a frame that
  finds its slot still owned rings first). That freed rack's CPU but not the link, and neither did removing the
  card's virtual NICs (`vnic del`).
- **The RTL8168's PCIe read requests.** At 90 MB/s each 1 KiB frame took 11.25 us against 8.7 us of wire time: the
  card spends the difference fetching the frame by DMA, and the firmware had left Max_Read_Request_Size at 512 bytes
  (`pci 04:00.0`, Device Control 0x2010; `pci BB:DD.F` now dumps all 256 bytes, so the PCIe capability shows). The
  driver sets 4096, as Linux's r8169 did (`rtl_tx_performance_tweak`): 113 MB/s, the same as the I350. ASPM was already
  off (Link Control 0x0140).
- **The RTL8168's transmit ring**, 32 -> 256 descriptors (r8169's size): the rate is the same, but with 32 `send` found
  the ring full 493,366 times in 1.3 million frames and spun for the card with the NIC lock held; with 256, never
  (`nic` shows the count).
- `sha256` of the 694 MB file at window 64: 15.7 s through the I350, 14.6 s through the 8168H after the fixes (16.9
  before). `sha256` reads the whole file into memory and then hashes it, and aneby hashes at about 74 MB/s (a 338 MB
  local file in 5.3 s, 0.75 s of it reading), so about 9 s of it is hashing whatever the network does.

**Read-ahead** (`crates/aoe/src/readahead.rs`, host-tested, and an `aoe-ahead` thread per attached disk). A lone
transfer starts with an empty window (a round trip before the first reply) and ends by draining it, and nothing is on
the wire between two requests: 128 KiB reads made 90 MB/s through the 8168H against 113 in 1 MiB ones.

- A read that continues the previous one, or starts in what was read ahead, also asks for chunks past its end (one
  window, 64 KiB, each; at most 4, 256 KiB). The read's own frames go first and read-ahead takes the room left in the
  one shared window. The amount starts at twice the read and doubles with each read of the stream, as Linux's does, so
  the short sequential runs of `format` and FOSFS metadata fetch a few sectors, not 256 KiB.
- A read elsewhere (an inode, an extent block) is a plain transfer and leaves the stream alone. Our own writes and
  flushes drop it, and so does an idle stream after 2 ticks (20-30 ms; the target's cache has the same bound for another
  host's writes). Read-ahead that failed between reads is dropped and asked for again, not reported as the read's error.
- **Dropped read-ahead still occupies the target's queue.** Its frames wait in the 64-frame queue the target advertises
  as its window. A write burst sent straight after them overflowed that queue: under QEMU, 16 resends in
  `test-mf-aoe-initiator` where there had been none. Dropped transfers with frames in flight are now kept as orphans
  and still count against the window until their frames are answered or overdue, and a write waits for them first.
  The test is back to 0 resends, and 5,250 frames against 4,977 without read-ahead.
- **The `aoe-ahead` thread** drives the read-ahead while no request holds the disk, so a reader that works on each block
  (decoding, hashing) finds the next one already there. Two bugs found on aneby by counting what the thread did:
  - It was woken at the end of a request while the caller still held the disk's lock. It found the lock taken, slept to
    the next tick and missed the reader's pause, and sent a quarter of the read-ahead. It is now woken after the lock is
    let go.
  - When it found the lock taken it tried again at once, and took a CPU from the request it waited for (under QEMU,
    114 resends). It now sleeps until woken.

On hardware (aneby reading rack's `kvart` through the 8168H, `readspeed disk3 256 KIB [US]`, US = microseconds of
busy work after each request, new):

| Requests | Work after each | Without overlap | Before | With read-ahead |
| --- | --- | --- | --- | --- |
| 128 KiB | none | | 90 MB/s | 118 MB/s (2.28 s) |
| 1 MiB | none | | 113 MB/s | 115 MB/s |
| 128 KiB | 1 ms | 4.33 s | | 2.58 s |
| 128 KiB | 2 ms | 6.38 s | | 4.12 s (the work alone is 4.10) |
| 64 KiB | 1 ms | 6.38 s (2.28 reading) | | 4.14 s (the work alone is 4.10) |
| 1 MiB | 8 ms | 4.37 s | | 4.14 s |

Gigabit carries about 118 MB/s of 1 KiB payload, so 128 KiB reads now fill the link, and up to 256 KiB a request the
reading hides behind the reader's work. Larger requests see little gain: read-ahead covers only 256 KiB of each.
`sha256` is unchanged (14.4 s): it reads the whole file before hashing it, so nothing can overlap (above). `aoe` shows
how much was read ahead and used, and what the thread did. Not measured: a real tape reading in blocks (`play`, `smbd`
serving a file).

`aoe attach SHELF.SLOT [rw] MAC` picks which of a target's NICs to use; without it the first answer wins. Attach
matches a target by shelf.slot, so the same disk cannot be attached twice through two NICs, and it keeps its disk
number through another one.

The same run found a **stale ext4 mount**: the shell keeps one ext volume mounted between commands
(`ext4vol::shared`) and only re-reads it when the disk number changes. Re-attaching a target gives it its old number
on purpose, so a file rack had written meanwhile was missing on aneby until `disks` happened to remount. Attach and
detach now commit and drop that mount (`aoedisk::forget_mount`, under the disk lock).

`make test-mf-aoe-initiator` (two guests on one QEMU segment): discovery and its limits, attach and the size, raw sectors
read through AoE against the data image (first, last and two others), a refused write on a read-only attach, detach and
the same disk number on re-attach, `format` of a FOSFS v4 volume over AoE, a 192 KiB file copied on with the host's
SHA-256, read back after detach and attach, the same file found by the target on its own disk, and a target that stops
answering failing the read in 5.6 s; with the host's `mkfs.ext4`, an ext4 disk changed on the target and re-attached lists
the new file (it fails without the fix above). Only real hardware can say what the resend timing does on a busy switch.

## 9. Testing against Linux

The host's own `aoe` module as the initiator, against the server kernel in QEMU. Not automated: loading a kernel module
and reading `/dev/etherd` need root, so a person runs the `sudo` lines. `tools/aoe_linux.py` (`make run-mf-aoe-tap`) does
the rest: a scratch data disk of random bytes in `build/aoe-linux/data.img` with its SHA-256 in `data.sha256`, a boot image
that exports it read-only as shelf 5 slot 2, and QEMU (KVM) with that disk and an e1000 on a tap device, the guest shell on
the terminal (Ctrl-A X quits). No host block device is involved; the module is limited to the tap device so it never
broadcasts AoE on the lab network (`enp0s31f6`), where the fos machines are.

**Once, as root**: a tap device you own, and the module bound to it only.

```sh
sudo ip tuntap add dev fosaoe0 mode tap user $USER
sudo ip link set fosaoe0 up
lsmod | grep -q '^aoe ' && sudo rmmod aoe        # if it is loaded with other interfaces
sudo modprobe aoe aoe_iflist=fosaoe0
cat /sys/module/aoe/parameters/aoe_iflist        # expect: fosaoe0
```

**Terminal A**: `make run-mf-aoe-tap`. Expect `aoe: disk 2 exported read-only as shelf 5 slot 2.` before the prompt.

**Terminal B**: `tools/aoe_linux_host.sh` runs the steps below in order. It pauses once for the guest commands, writes only
after checking that the device is the scratch disk, and logs to `build/aoe-linux/host.log`. By hand, step by step (each
line says what to expect):

```sh
# 1. Discovery: the module broadcasts Query Config on fosaoe0, the guest answers.
echo | sudo tee /dev/etherd/discover
ls -l /dev/etherd/                               # e5.2 appears
sudo dmesg | grep -i aoe | tail -3               # a line naming e5.2 and 131072 sectors (64 MiB)

# 2. Size and content, read through AoE.
sudo blockdev --getsize64 /dev/etherd/e5.2       # 67108864
sudo sha256sum /dev/etherd/e5.2; cat build/aoe-linux/data.sha256    # the same digest

# 3. Read throughput (then `aoe` in terminal A shows the counters).
sudo dd if=/dev/etherd/e5.2 of=/dev/null bs=1M iflag=direct status=progress

# 4. The export is read-only: a write fails with an I/O error and changes nothing.
sudo dd if=/dev/zero of=/dev/etherd/e5.2 bs=4k count=1 oflag=direct    # expect an Input/output error
sudo blockdev --flushbufs /dev/etherd/e5.2; sudo sha256sum /dev/etherd/e5.2   # still the digest of step 2
```

In terminal A: `aoe unexport 2`, then `aoe export 2 rw confirm`. Back in terminal B:

```sh
echo | sudo tee /dev/etherd/discover             # in case the module gave up on it while it was gone

# 5. Raw writes: 8 MiB at 16 MiB, read back through AoE (buffers dropped first, so the read reaches the
#    target) and found in the disk image itself.
head -c 8M /dev/urandom > /tmp/aoe-pattern
sudo dd if=/tmp/aoe-pattern of=/dev/etherd/e5.2 bs=1M seek=16 oflag=direct conv=fsync status=progress
sudo blockdev --flushbufs /dev/etherd/e5.2
sudo cmp -n 8388608 -i 0:16777216 /tmp/aoe-pattern /dev/etherd/e5.2 && echo "read back: same"
sleep 1; cmp -n 8388608 -i 0:16777216 /tmp/aoe-pattern build/aoe-linux/data.img && echo "in the image: same"

# 6. A file system: ext4 made, used and checked through AoE.
sudo mkfs.ext4 -q -F /dev/etherd/e5.2
sudo mount /dev/etherd/e5.2 /mnt
sudo cp -r docs crates/aoe/src /mnt/ && sha256sum crates/aoe/src/lib.rs && sudo sha256sum /mnt/src/lib.rs   # the same digest
sudo umount /mnt
sudo blockdev --flushbufs /dev/etherd/e5.2; sudo e2fsck -fn /dev/etherd/e5.2               # clean

# 7. Write throughput (overwrites the file system: a scratch disk).
sudo dd if=/dev/zero of=/dev/etherd/e5.2 bs=1M count=32 oflag=direct status=progress
```

After quitting QEMU, `e2fsck -fn build/aoe-linux/data.img` (no root needed) checks what step 6 left, unless step 7 ran.

**Cleanup** (as root): `sudo umount /mnt` if still mounted, `sudo rmmod aoe`, `sudo ip link del fosaoe0`.

**What to bring back**: the `dmesg` lines from step 1 and any `aoe:` errors later, whether the digests and `cmp` agree, the
`dd` rates of steps 3 and 7, and the guest's `aoe` output after steps 3 and 7.

**Run on 2026-11-22** (host kernel 7.3.0, `aoe` v85, KVM): every step passed.

- Discovery: `aoe: e5.2: setting 1024 byte data frames`, `5254001234d5 e5.2 v0001 has 131072 sectors`.
- Content: the SHA-256 through AoE equals `data.sha256`; reads 58 MB/s.
- Read-only: the 4 KiB write fails with `EIO` and the digest is unchanged. The module logs `ata error cmd=34h stat=51h`
  four times (WRITE DMA EXT; DRDY, DSC, ERR), one per 1 KiB frame, and the guest counts 4 errors.
- The gap between `unexport` and `export 2 rw confirm` did no harm: the module kept the device and used it again.
- Raw writes read back the same through AoE and in `data.img` (50 MB/s with `fsync`); ext4 made, filled, remounted,
  compared and checked clean; writes 50 MB/s.
- `aoe: unknown ioctl 0x80280215` and `0x5395`: host programs (udev, blkid, mkfs) asking the block driver for things it
  does not implement. Local to the host; nothing reaches the wire.
- Nothing came back "unknown command": the module asks only Query Config and ATA.

## 10. Open questions

- RTL8168 jumbo: done for the 8168G and later (section 7); older generations stay at 1500.
- Does the recorder want the initiator on the embedded kernel too, or mf only? (Leaning mf only.)
- Claim timeout and what a crashed owner leaves behind (FOSFS journal-less; ext4 replays).
