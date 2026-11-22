# Sharing block devices over Ethernet

Status: design note (2026-11-22). Only the jumbo-frame groundwork in the e1000 driver exists (section 7, `make
test-jumbo`); no target or initiator yet.

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
- Shell: `aoe` (list with counters), `aoe export N [rw confirm]`, `aoe unexport N`, `aoe shelf N`. Disk N is slot N of the
  shelf. `rw` needs `confirm` on the same line, and disk 1 is never writable. Settings: `aoe-shelf N`, `aoe-export N`
  (read-only, applied after boot; docs/config.md).
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

Not done, in the order I would do it:

1. **One doorbell for several replies** (44 of 48 us a frame now): a send call that queues several frames before telling
   the card, and the NIC lock taken once per batch instead of once per reply.
2. **Per-controller locking** in the block layer, if the measurements say serial disk access is the limit.
3. Jumbo frames on this path: `nicctl` drains into 1600-byte buffers and the target assumes MTU 1500.
4. Interoperability with the Linux `aoe` module and `vblade` as a peer (the host has the module, version 85; loading it
   needs root, so a manual step).
5. The initiator: `block::Backend::Aoe`, a virtual disk like `Md`, with retransmit and a window.

## 9. Open questions

- RTL8168 jumbo (per-chip limits table, `RMS`, buffer size) is not done.
- Does the recorder want the initiator on the embedded kernel too, or mf only? (Leaning mf only.)
- Claim timeout and what a crashed owner leaves behind (FOSFS journal-less; ext4 replays).
