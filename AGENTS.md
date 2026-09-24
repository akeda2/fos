# fos development guide

## Project purpose

`fos` is a minimal educational operating system. The current supported path is
legacy BIOS -> 16-bit real mode -> 32-bit protected mode -> a `no_std` Rust
kernel. Boot stages emit explanatory messages to VGA text memory and COM1.
Keep changes small and explainable; avoid general frameworks or abstractions
that obscure hardware behavior.

## Current architecture

- `boot/stage1.S` is the 512-byte MBR loader. It loads stage two from LBA 2048.
  It must retain the partition table area at bytes 446-509 and the `0x55aa`
  signature at bytes 510-511.
- `boot/stage2.S` gathers E820 memory information, enables and verifies A20
  (trying, in order, the BIOS itself, the 8042 keyboard controller, and only
  then the "fast A20 gate" port `0x92` -- see the "Real hardware notes"
  section below for why), loads the kernel at physical `0x10000` using bounded
  multi-transfer BIOS EDD reads that never cross a 64 KiB transfer boundary,
  and enters flat 32-bit protected mode. The reserved
  `0x10000..0x60000` region, not the old single-transfer 127-sector
  convention, is the current kernel-size limit.
- `kernel/` is a freestanding Rust binary. `linker.ld` reserves the initial
  low-memory kernel region through `0x60000`; do not place loader, boot-info,
  or disk-trampoline data there.
- `kernel/src/entry.S` reserves a 32 KiB kernel stack with a low-end canary
  above an unmapped 4 KiB guard page. `kernel/src/gdt.rs` installs a
  double-fault task gate and emergency stack so an overflowed normal stack
  produces a diagnostic halt instead of a triple fault. Tapes run nested
  inside shell calls on that stack, so fixed packet/state buffers must leave
  headroom for the surrounding `term net` call chain; do not grow tape-local
  arrays casually without checking the stack budget.
- The packed version-3 `BootInfo` at physical `0x5000` is the loader/kernel ABI;
  its disk geometry fields are 64-bit so GPT-discovered LBAs can survive the
  boot handoff.
  Validate its magic, version, size, and count before using it. Use unaligned
  reads for packed multi-byte fields.
- `kernel/src/disk.rs` invokes the stage-two BIOS EDD trampoline. The
  trampoline itself can batch up to 126 sectors per real-mode call
  (`TRAMPOLINE_MAX_SECTORS`, matching the loader's own already-proven
  bounded-transfer size), but `disk.rs` only ever requests up to
  `MAX_TRANSFER_SECTORS` (16, an 8 KiB `CHUNK` buffer) at a time -- the
  smaller value exists purely to fit the kernel's `.bss` budget below the
  `0x60000` reserved-region ceiling, not any BIOS/hardware limit. FOSFS
  file reads/writes (`fosfs::cat`/`read`/`write_file_data`) loop over
  `MAX_TRANSFER_SECTORS`-sized chunks via `device_read_sectors`/
  `device_write_sectors` instead of one BIOS round trip per sector, which
  matters most for large tapes: legacy USB/BIOS INT13h per-call latency
  otherwise dominates a multi-hundred-sector transfer's total time far
  more than the underlying disk's real transfer speed. Every transfer,
  whether one sector or a full chunk, is issued only after validating the
  LBA and buffer constraints. Do not call BIOS interrupts directly from
  protected mode.
  Because the trampoline drops into real mode, and the kernel's own IDT/PIC
  remap would otherwise break BIOS's real-mode interrupt handling, `transfer`
  briefly restores the real-mode IVT (`IDTR` base 0) and the BIOS-conventional
  PIC vector mapping around each individual BIOS call, temporarily disables
  paging for that real-mode transition, then switches back. The trampoline
  reloads stage two's GDT, so `disk.rs` restores the permanent kernel GDT
  (including the double-fault TSS descriptors) before interrupts resume.
- `kernel/src/interrupts.rs` builds a 256-entry IDT (timer, keyboard,
  spurious-PIC, safe legacy-PCI IRQ gates, and vector 8's double-fault task
  gate), remaps the 8259 PIC clear of the CPU exception range, and arms the
  PIT at 100 Hz. NIC handlers only acknowledge/count/mark pending work;
  descriptor and protocol parsing remains foreground polling.
- `nic` status and ordinary foreground NIC traffic run a bounded watchdog,
  never an ISR or timer task. It only resets a backend after that driver
  latched a documented DMA/Tx fault, reuses its DMA allocations, restores
  interrupt/promiscuous state, and exposes progress/recovery counters.
- `kernel/src/pmem.rs` is a small bitmap-based physical-frame allocator (4 KiB
  frames) built from the E820 map; it reserves everything below the kernel's
  own low-memory region.
- `kernel/src/paging.rs` enables supervisor-only identity mappings for the
  first 2 GiB. The low 4 MiB uses 4 KiB PTEs to leave the kernel-stack guard
  page absent and make page-aligned kernel text/read-only data non-writable;
  the remaining low range uses 4 MiB PSE mappings. High PCI MMIO is absent
  until `map_mmio` registers a driver's exact BAR range, using at most three
  dynamic page tables. This retains physical-pointer compatibility for DMA
  and the BIOS trampoline while leaving unrelated high ranges unmapped for
  useful page-fault diagnostics; it is not userspace isolation.
  `make image-unsafe-memory` is the explicit teaching comparison: it builds
  a separate unpaged artifact, so it has neither those absent ranges nor the
  guard page and must not replace the default deployment image.
- `kernel/src/fosfs.rs` implements FOSFS version 3, a deliberately small
  flat custom filesystem: one superblock, eight directory sectors, 64 fixed
  64-byte entries, 64-bit contiguous multi-sector extents, and bounded
  first-fit reuse. Multi-extent files remain a future extension. File names
  use up to 16 lowercase letters, digits, `.`, `_`, or `-` (this project's
  own file-naming convention now uses lower case throughout, not just for
  FOSFS entries) -- enforced independently in three places that must be kept
  in sync: `fosfs::valid_name` (writes through the shell), `tape_fs_name`
  in `kernel/src/shell.rs` (every tape's own `fs_write`/`fs_read` ABI
  callback), and `tools/build_image.py`'s `valid_name` (preformatted image
  files). Individual tapes taking a file name argument (e.g. `tapes/ed`,
  `tapes/router`'s `dhcp-lease-file`) have historically duplicated this same
  check client-side and must be updated together with it.
- `kernel/src/shell.rs` runs an optional FOSFS `start` file after disk/NIC
  initialization and before the automatic `term net` wait. It accepts up to
  4096 bytes; non-empty lines not beginning with `#` use the ordinary command
  dispatcher. Keep a long-running router `tape run` last, because later
  script lines cannot run until it returns.
- `boot/netrom.S` is an optional, parallel boot path: a PCI expansion ROM that
  boots fos from a NIC's own boot ROM instead of disk/USB (`make netrom`,
  producing `build/netboot.rom`; never part of `make image`/`run`/`test`).
  It reuses `stage2.S`'s narration/E820/A20/GDT logic but must treat its own
  ROM image as read-only at run time (a shadowed option ROM cannot be
  written back to) -- see "Low-memory map" below for the scratch RAM this
  forces it to use instead.
- `tapes/tape-net/` is the shared no-heap protocol library for networking
  tapes. It builds on the kernel's raw NIC callbacks and implements the
  educational Ethernet/ARP/IPv4/ICMP/UDP/TCP layers described in
  `NETWORKING.md`; it is not part of the kernel image.
- `tapes/bridge/` is a raw layer-2 learning bridge tape. It uses the
  per-interface tape callbacks, enables promiscuous mode on selected ports,
  and forwards Ethernet frames without implementing IP routing.
- `tapes/router/` is a fixed-buffer layer-3 tape. It reuses the bridge's
  polling shape, answers ARP per configured interface, and forwards IPv4
  packets between directly connected subnets while updating TTL and header
  checksum. Its syntax is `tape run router [dhcp [NIC]]... [health A.B.C.D] [dhcp-server] [dhcp-lease-file NAME] [nat|no-nat] [forward tcp|udp EXTERNAL_PORT INTERNAL_IP [INTERNAL_PORT]] NIC IP/PREFIX ... [default
  NIC GATEWAY]` (legacy dotted `NIC IP NETMASK` and separate `NIC IP /PREFIX`
  forms remain accepted); NIC numbers are one-based, `dhcp` configures NIC 1
  (callback index zero), while `dhcp N` adds an ordered DHCP uplink. `health`
  requires both fresh gateway ARP and ICMP echo success, fails over after
  three missed bounded polls, and fails back on recovery. NAT and static
  forwards are limited to the active uplink, so existing flows may break on a
  change. Boot-time acquisition for a configured DHCP uplink is bounded but
  non-fatal to the whole router: an uplink that gets no usable offer within
  its own boot budget is left unconfigured and unhealthy rather than
  aborting startup, and its `DhcpClient` keeps retrying in the background
  (the same restart-on-`Expired`/`Failed` machinery that already drives
  ordinary lease renewal) until it does acquire a lease, at which point the
  ordinary failover machinery adopts it automatically. A router is only
  refused startup for an explicit operator abort or a NIC/callback-level
  error, never for one uplink alone failing to hear back from a DHCP
  server -- the entire point of configuring more than one uplink is that
  another one may still work. An explicit or active DHCP-supplied gateway handles unmatched
  destinations. NAT is enabled by default; `no-nat` disables it for
  deployments where upstream routing reaches the downstream subnets, while
  `nat` explicitly enables it. When enabled, TCP/UDP traffic leaving
  non-primary NICs through the active uplink is source-NATed through a 4 MiB
  physical-memory-backed table and reverse-translated for replies, with a
  32-slot fallback if allocation is unavailable, so upstream hosts do not
  need a route back to the downstream subnet. `dhcp-server` serves every
  configured interface
  except DHCP uplinks using an automatically derived subnet pool with one in-memory
  lease slot per usable address, and seven-day leases. Upstream DHCP DNS
  values are propagated to downstream clients; a `dns SERVER [SERVER]`
  directive overrides them. The server has no DNS implementation or
  persistence. It also accepts `tape run router @router.cfg`: the shell reads the lowercase
  FOSFS file and passes its contents through the existing startup-argument
  ABI; `#` comments and newlines are accepted, with a 4096-byte fixed limit.
  `dhcp-lease-file NAME` optionally persists bound DHCP assignments through
  the tape filesystem callbacks; omitting it keeps the router diskless. The
  tape deliberately has no dynamic routing,
  ICMP errors, fragmentation, or IPv6.

## Low-memory map

Real-mode code before the kernel's own memory management exists shares a
handful of fixed low-memory addresses across `stage1.S`, `stage2.S`, and
`netrom.S`. There is no single enforcing mechanism for this -- it is
convention only -- so treat this table as authoritative before adding any new
fixed address below `0x60000`, and update it in the same change that adds one:

| Address | Used by | Purpose |
| --- | --- | --- |
| `0x500`-`0x510` | `stage2.S` (`verify_a20`) | Two probe bytes used to detect whether the A20 line is enabled. |
| `0x600`-`0x613` | `netrom.S` | Scratch RAM for state a shadowed (read-only) option ROM cannot mutate in place: computed ROM segment/physical base, serial-availability flag, and the GDT/protected-entry descriptors used to enter protected mode. |
| `0x5000` | `stage2.S`, `netrom.S`, `kernel/` | The packed `BootInfo` struct: the loader/kernel ABI. |
| `0x5400`-`0x5447` | `stage2.S` BIOS disk trampoline | One EDD request/DAP plus saved protected-mode stack pointer and CR0, used while temporarily entering real mode. |
| `0x5500`-`0x5547` | `kernel/src/debug.rs` (`CRASH_RECORD_ADDR`) | A `CrashRecord` written by a fault handler and read back on the *next* boot, so a real-hardware operator can see what a previous fatal exception looked like without having watched it happen live. See "Debug mode" below. |
| `0x7a00` | `stage2.S`, `netrom.S` | Real-mode stack pointer during boot. |
| `0x7c00` | `stage1.S` | MBR load address (fixed by the BIOS). |
| `0x8000` | `stage2.S` | Stage-two load address (fixed by `stage1.S`). |
| `0x9f000` | `stage2.S`, `netrom.S` | Protected-mode stack pointer, just below the conventional End-of-Low-Memory / EBDA boundary. |
| `0x10000`-`0x90000` | `stage2.S`, `netrom.S`, `kernel/linker.ld` | The Rust kernel's load address and reserved low-memory region. |
| `0x100000`-`0x500000` | `kernel/src/pmem.rs` (`TAPE_BASE`/`TAPE_LEN`), any tape's own linker script | The fixed 4 MiB region every tape (`tape load disk`/`tape load net`) is always loaded into, carved out of `pmem`'s general frame pool at boot so `allocate`/`allocate_contiguous`/`palloc` can never hand these frames to anything else. See "Tapes: fixed load address, and writing one in Rust" below. |

## Educational verbosity

This project exists to be read and explained, not just to run. Boot stages
and kernel subsystems should narrate what they are doing and why in their
status messages (e.g. which BIOS interrupt is being called, what a PIC remap
or IDT load accomplishes), not just announce bare completion. When adding a
boot step or kernel subsystem, prefer an explanatory message over a terse one,
and keep `README.md`'s command table and "What happens at boot" walkthrough in
sync with the actual command surface and boot sequence. Inline documentation
is mandatory for new hardware-facing code and new educational examples:
explain the relevant ABI, register/port behavior, timing assumptions, and
why the implementation follows the project's constraints rather than leaving
those details implicit.

Every shell command's output should fit an 80x25 VGA screen without wrapping
or, where at all avoidable, scrolling: keep each `println!`/`print!` line to
at most 79 columns (not 80 -- a line landing exactly on column 80 wraps *and*
leaves a spurious blank row for its own trailing `'\n'`, as the `help`
rewrite below found the hard way), splitting a long message across two or
more `println!` calls rather than one long line. A command whose output is
inherently unbounded or can legitimately exceed 25 rows for its own good
reason (e.g. `ls` listing up to FOSFS's 64 directory entries, `nic sniff`'s
captured-frame dump, or a long `memmap`/E820 listing) may still scroll --
the console's own scroll logic (see `tapes/primes.S`'s scroll/cursor
handling above) already handles that correctly -- but scrolling should never
be the *incidental* result of an oversized fixed message that could easily
have been split to fit, and a command's normal/expected case should stay on
one screen whenever practical.

## Disk image and safety

`tools/build_image.py` creates `build/fos.img`, a 16 MiB raw image with a
protective MBR and primary/backup GPT metadata:

| Location | Contents |
| --- | --- |
| LBA 0 | MBR / stage-one loader |
| LBA 2048 | GPT boot payload partition |
| First 32 payload sectors (LBA 2048-2079) | Stage-two loader |
| LBA 2080 | One-sector A/B kernel-slot metadata block (see "Debug mode" below) |
| LBA 2081-2720 (640 sectors, slot 0) | Kernel slot 0 |
| LBA 2721-3360 (640 sectors, slot 1) | Kernel slot 1 |
| LBA 4096 | Dedicated 28639-sector GPT-discovered FOSFS data partition containing README and all compiled tapes |
| Final 33 sectors | Backup GPT entry array and header |

The image builder is the single source of truth for partition placement and
writes both GPT copies. The kernel validates the primary GPT and discovers the
FOSFS partition by its type GUID, with loader-provided geometry retained as a
compatibility fallback for older images. `tools/write_layout.py` emits
matching stage-loader constants from the actual kernel size. Never add host
block-device writes, automatic device selection, or commands that let FOSFS
write outside its own partition. Copying an image to a device is destructive
and must remain an explicit user action.

Only one of the two 640-sector kernel slots is ever the boot loader's actual
first choice at a time (`active_slot`, in the metadata block); the other
exists purely so a kernel update can be written into it without touching the
one currently running. See "Debug mode" below for the full state machine and
the network service that writes a fresh slot.

## Debug mode: unattended, remote-friendly development on real hardware

Real hardware has no debugger attached and no guarantee anyone is watching
the screen when something goes wrong -- the exact opposite of QEMU, where a
crash is easy to see and a bad build costs nothing to retry. Debug mode
exists to make iterating against a real machine over the network almost as
cheap as iterating against QEMU: instead of a fatal exception halting the
CPU and needing a physical hand on the reset button, the machine can reboot
itself and explain what happened; instead of a kernel update needing a
freshly written USB stick, a new kernel can be pushed over Ethernet and
tried automatically, with a built-in, automatic fallback if it turns out to
be broken.

Everything below is controlled by an optional `debug` file (the same kind
of per-machine config file `hostname`/`start` already are -- see
`kernel/src/debug.rs`'s `load_config`, checked on `disk1` first and then
every secondary disk in order) so an ordinary deployment with no such file
behaves exactly as it always has: a fatal exception halts the CPU, and the
network kernel-update service does not even acknowledge its own protocol's
frames. One line per option, `#` comments and blank lines ignored:

| Option | Effect |
| --- | --- |
| `reboot-on-crash` | A fatal CPU exception resets the machine instead of halting forever. |
| `reboot-delay N` | Seconds to print the crash details before actually resetting (default 3); gives a slow serial link time to flush. |
| `kernel-update` | Enables KUP, the network kernel-update service (see below). Off by default -- this is a powerful, write-to-disk-and-reboot capability, not something to leave enabled on every machine. |
| `crash-on-boot` | Test-only: crashes immediately at every boot, before the milestone that would otherwise reset the loop-guard counter (see below). Exists purely to exercise the loop guard itself in an automated test (`tools/test_debug_mode_e2e.py`) without needing a real hardware/driver bug; genuinely useful as a documented, supported option for anyone else who needs to test the same path, not something planned for removal. |

### Crash records and reboot-on-crash

`kernel/src/interrupts.rs`'s ordinary exception handlers and
`kernel/src/gdt.rs`'s double-fault task gate both call
`debug::handle_crash` (via `debug::record_crash`) instead of unconditionally
halting. `record_crash` writes a `CrashRecord` (vector, error code,
`CS:EIP`, `EFLAGS`, `SS:ESP`, and -- for a double fault only, since that's
the one case where the CPU's own hardware task switch actually saves them
-- the general-purpose registers) to a fixed physical address,
`CRASH_RECORD_ADDR` (`0x5500`; see "Low-memory map" above), not to disk:
writing it must be safe from inside a fault handler with no allocator and,
for a double fault, an already-exhausted normal stack. The *next* boot's
`shell::run` calls `debug::report_and_clear_previous_crash` early, which
prints the record (if its magic/version match) and clears it, so an
operator checking in later sees exactly what the last boot's fatal
exception looked like without having had to be watching when it happened.
This only survives a reset that happens to leave RAM intact -- true of
every real chipset and QEMU tested against, but not a guarantee this module
can make; a machine whose reset path clears RAM just shows no previous
crash on the next boot, same as if none had happened.

With `reboot-on-crash` enabled, `handle_crash` prints the crash details,
waits `reboot-delay` seconds, then performs the same 8042
keyboard-controller pulse reset the `reset` shell command uses, instead of
halting. With it disabled (the default), a crash still writes the crash
record (so `debug status`/the next boot's report both still work) but then
halts exactly as it always did.

A consecutive-crash loop guard (`MAX_CONSECUTIVE_CRASHES` = 5,
`crash_count` persisted in metadata byte 9 of the on-disk slot-metadata
sector, not in RAM, so it survives a RAM-clearing reset too) protects
against a kernel that crashes immediately after every reboot -- a bug
reboot-on-crash itself cannot fix -- from spinning forever, chewing power
and wearing out real hardware unattended. `debug::confirm_boot` (called
once `shell::run` reaches its own milestone, i.e. the point where a human
or a remote client could actually use the machine) unconditionally resets
`crash_count` to 0: reaching a real, working shell proves this boot is
fine, regardless of what an operator does with it afterward. This means
the guard can only ever be tripped by a crash that happens *before* that
milestone on every one of five consecutive boots -- exactly the scenario a
bad kernel update or a startup-time bug produces, not a manually typed
`debug crash` from an already-reached prompt (which has already reset the
counter for that boot). Once tripped, the machine halts with a clear
message instead of rebooting again.

Two hidden diagnostic commands (undocumented in `help`'s own listing, but
always available): `debug status` prints the current config
(`reboot-on-crash`/`reboot-delay`/`kernel-update`); `debug crash`
deliberately raises a real `int3` exception so `reboot-on-crash` (or its
absence) can be verified on purpose, cheaply, rather than waiting for a
real crash to happen; `debug slots` prints the current A/B slot metadata
(booted slot, active/pending slots, the fallback state machine's own
`attempted`/`confirmed` flags, and `crash_count`) -- mainly useful for
verifying a kernel update (or its fallback) actually did what it claimed.

### A/B kernel slots and automatic fallback

Two 640-sector kernel slots (see the disk layout table above) let a new
kernel be written without ever overwriting the one currently running. A
one-sector metadata block at LBA 2080 (`kernel/src/debug.rs`'s
`SlotMetadata`, byte-identical to `tools/write_layout.py`'s constants and
`boot/stage2.S`'s `select_kernel_slot`) records `active_slot` (0 or 1),
an optional `pending_slot`, and two flags: `attempted` (has the pending
slot been booted at least once) and `confirmed` (did that boot reach the
kernel's own milestone).

`select_kernel_slot` (`boot/stage2.S`), run before the kernel is loaded,
implements the actual fallback logic:
- No pending slot: boot `active_slot`, exactly as before this feature
  existed.
- A pending slot exists but hasn't been attempted yet: boot it, and mark
  `attempted = true` (a **single** try; see below).
- A pending slot was attempted and confirmed: promote it (`active_slot =
  pending_slot`, clear `pending_slot`/`attempted`/`confirmed`) and boot
  it -- covers the case where the kernel confirmed but reset before it
  could clear its own pending state.
- A pending slot was attempted but *not* confirmed (it crashed, hung, or
  the machine was power-cycled before reaching the milestone): give up on
  it, clear `pending_slot`/`attempted`/`confirmed`, and fall back to
  booting the unchanged `active_slot` instead. A bad kernel update gets
  exactly one try before this machine automatically reverts to the last
  known-good kernel, with no physical intervention required.

`debug::confirm_boot`, called once `shell::run` reaches its milestone, is
the only place that ever sets `confirmed = true` (or promotes an
already-confirmed pending slot immediately, for the reset-before-clearing
case above) -- this is a deliberate, narrow point in the boot sequence,
not "the kernel ran for a while without crashing."

### KUP: the network kernel-update service

KUP (`kernel/src/shell.rs`'s `serve_kup_put_*` functions,
`kernel/src/debug.rs`'s `install_kernel_update`/`inactive_slot`, and the
host-side `tools/send_kernel_update.py`) lets a new kernel image be pushed
to a running machine over plain Ethernet and tried automatically, with the
A/B fallback above as its safety net -- directly answering "would it be
possible to overwrite the kernel on a running machine from the network for
testing?". It only exists at all when the `debug` file's `kernel-update`
option is set; otherwise the frames below are never even acknowledged,
indistinguishable on the wire from "no such machine".

It shares `CHAT_ETHERTYPE` with `chat`/`nic discover`/`tape`/`term
net`/NETFS, and mirrors NETFS's own PUT flow almost exactly:

| Tag | Meaning |
| --- | --- |
| `KUP_TAG_PUT_REQUEST` (`0x3a`) | Client -> server. Payload: length (u32 LE), RFC 1071 Internet checksum (u16 LE) of the whole image. |
| `KUP_TAG_PUT_DATA` (`0x3b`) | Client -> server, one per chunk. Payload: sequence number (u32 LE) then up to 1024 bytes -- the same `TAPE_CHUNK_LEN`-shaped, sequence-numbered chunking `tape`'s own network transfer already uses, for the same reason (a binary image's own trailing zero bytes must never be confused with Ethernet's 60-byte minimum-frame padding). A gap or reorder aborts the transfer immediately; there is no retransmission. |
| `KUP_TAG_PUT_STOP` (`0x3c`) | Client -> server. Ends the transfer; triggers verification. |
| `KUP_TAG_PUT_ACK` (`0x3d`) | Server -> client. The image was verified and installed; the server will reset shortly to try it. |
| `KUP_TAG_PUT_ERROR` (`0x3e`) | Server -> client. One-byte code: `1` `TOO_LARGE` (bigger than one 640-sector slot, or the shared staging buffer), `2` `CHECKSUM_MISMATCH`, `3` `DISK_ERROR` (writing the slot, or reading metadata, failed), `4` `BUSY` (another update is already in flight). |

`ambient_discovery_poll` -- the same idle-prompt-loop hook that also serves
NETFS and ordinary discovery -- dispatches all three request-carrying tags,
gated behind `debug::kernel_update_enabled()`. **KUP is therefore only ever
served while a machine is sitting idle at its own local prompt**, exactly
like NETFS -- not inside `chat`, `nic discover`, `term net`, or a running
tape's own polling loop.

On `KUP_TAG_PUT_STOP`, the server verifies the reassembled image's checksum
before writing anything; a mismatch (or any other failure) leaves the
currently running kernel and `active_slot` completely untouched, replies
with the matching error, and the operator can simply try again. On success,
it writes the image into whichever slot is *not* `active_slot`
(`debug::inactive_slot`), records it as a fresh, unattempted `pending_slot`
(`debug::install_kernel_update` -- no `boot/stage2.S` changes were needed
for this, since its fallback state machine above already handles "a fresh
pending slot" as a first-class case), sends `KUP_TAG_PUT_ACK`, and resets
the machine roughly a second later so `select_kernel_slot`'s existing logic
gives the new kernel its one bounded try on the very next boot -- if it
never reaches its own milestone, the automatic fallback above reverts to
the previous kernel with no manual step at all.

Like NETFS's own PUT, a KUP transfer reuses the shared
`pmem::TAPE_BASE`/`TAPE_LEN` staging buffer -- the same accepted,
documented risk (a local `copy`/`tape load` command run concurrently on the
serving machine could corrupt an in-flight transfer) applies here too, but
is bounded: even a corrupted install can't brick the machine, since
`select_kernel_slot` never trusts an unconfirmed pending slot.

`tools/send_kernel_update.py` is the host-side sender, reusing
`tools/send_tape.py`'s discovery/peer-resolution/address-book helpers
directly (`--mac`, a peer number, or automatic single-reply discovery) so
it needs no separate addressing logic of its own; give it a `kernel.bin`
path and it sends the request/data/stop sequence above (paced the same way
`send_tape.py` paces a tape, to avoid outrunning a polling-only RTL8139
receive path) and reports the ACK/ERROR reply.

`tools/test_debug_mode_e2e.py` (`make test-debug-mode`) is the automated
regression test for everything in this section: reboot-on-crash reboots
and reports the previous crash; the same crash halts when reboot-on-crash
is off; the consecutive-crash loop guard trips after five automatic
pre-milestone crashes (using `crash-on-boot`); and KUP is silently ignored
when disabled, rejects a bad checksum with slot metadata untouched, and
installs, ACKs, actually reboots, and promotes the new slot to active on a
good push of the real `build/kernel.bin`.

`tools/test_realnet_e2e.py` (not part of `make test`/`make test-debug-mode`,
and depends on host machine state -- see its own docstring) is a second,
complementary debug-mode test proving the *client tooling* itself works,
not just the kernel/protocol side: unlike every other test script here, it
drives the real `tools/term_net.py`/`tools/send_kernel_update.py` CLIs as
subprocesses (through a `CAP_NET_RAW`-tagged `python3` copy, the same
`setcap` convention "Talking to a real fos machine from a sandboxed dev
environment" above already uses) over a genuine host tap interface bridged
to the QEMU guest's primary NIC, rather than reimplementing the wire
protocol inline over a raw UDP multicast socket the way
`test_debug_mode_e2e.py`/`test_kernel_update_e2e.py` do. It also
demonstrates the "exercise a new NIC driver as a second interface in QEMU
before ever touching physical hardware" workflow this section's debug-mode
design was originally motivated by: the guest's second NIC is a real,
QEMU-emulated `ne2k_pci` (NE2000-compatible), and `nic list` over the real
tap confirms both interfaces are correctly identified. (Intel's I350 has
no QEMU model at all -- see "Intel I350 support" above -- so this pattern
can't cover that specific driver, but it covers any NIC family QEMU does
emulate.) One non-obvious timing detail this test has to account for:
`term_net` runs automatically at every boot and needs no F12 escape at all
to reach with `term_net.py` (a session HELLO can connect directly to that
automatic wait); a local escape to the idle prompt is needed only
immediately before a KUP push, since KUP is served solely from the idle
loop's `ambient_discovery_poll`, never from inside an active `term_net`
session -- escaping earlier than necessary is actively harmful here, since
after a local escape the shell only restarts `term_net`'s own HELLO wait
after `TERM_NET_IDLE_TICKS` (60s) of local idle time, so a command sent
shortly after an unnecessary escape would see no listener at all until
that window elapses.

## Build and validation

The pinned toolchain is `nightly-2026-09-13`, with `rust-src`. Required host
tools are GNU Make, GNU `as`/`objcopy`, Python 3, and `qemu-system-i386`.

```sh
make image    # Produces build/fos.img with all compiled tapes. Plain `make` is equivalent.
make image-unsafe-memory # Produces build/fos-unsafe-memory.img without paging, then restores build/fos.img.
make run      # Boots it interactively in QEMU.
make run-unsafe-memory # Boots the optional unpaged teaching image.
make test     # Runs the serial-driven QEMU smoke test and VGA mode test.
make test-unsafe-memory # Smoke-tests the optional unpaged teaching image.
make test-stack-guard # Builds a fault-injection image and verifies its emergency #DF task.
make test-page-protection # Verifies CR0.WP and read-only kernel-text PTEs.
make test-nic-watchdog # Verifies foreground recovery from a latched NIC fault.
make test-start # Verifies an optional FOSFS start boot-command script.
make test-second-disk # Verifies the diskN selector against a second, attached disk.
make test-debug-mode # Verifies reboot-on-crash, the crash-loop guard, and KUP.
make all      # Builds everything: image and every test tape.
make tapes    # Builds all test tapes only (`build/tapes/*.bin`).
make help     # Lists every target with a one-line description.
```

Always run `make test` for boot, loader, kernel, shell, disk, or FOSFS changes.
Also run `make test-stack-guard` when changing paging, GDT/IDT handling,
exception paths, or kernel-stack layout.
Run `make test-page-protection` when changing page permissions, linker section
layout, or page-fault diagnostics.
Run `make test-start` when changing shell startup order, FOSFS startup-file
handling, or command dispatch.
Run `make test-second-disk` when changing secondary-disk discovery, the
`diskN` selector, or `disk.rs`'s device-aware trampoline calls (`make test`
already includes it, but it is faster to run alone while iterating).
Run `make test-unsafe-memory` when changing the legacy-memory feature or its
separate image target.
Run `make test-debug-mode` when changing `kernel/src/debug.rs`, the A/B
kernel-slot state machine in `boot/stage2.S`, `shell.rs`'s KUP handlers, or
anything else under "Debug mode" below -- it is not part of `make test`
since it performs several genuine QEMU guest resets (never `-no-reboot`)
and takes noticeably longer.
The smoke harness must use bounded waits, preserve serial output on failure,
and wait for the prompt before sending input.

Every new tape must include a focused test alongside its implementation, as
`tapes/test_beep_e2e.py` does for `tapes/beep/`. Tape tests should exercise
the tape through its documented load/run path and verify observable behavior
end to end; when hardware output cannot be captured directly, assert a
documented diagnostic or callback-visible result and verify control returns to
the shell. A successful build alone is not sufficient validation for a new
tape.

When debugging 16-bit real-mode code (`stage1.S`, `stage2.S`, `netrom.S`),
always disassemble with `objdump -d -M i8086 <object-file>`. Plain
`objdump -d` on these ELF objects defaults to 32-bit operand/address
decoding regardless of the source's `.code16` directives and silently
produces wrong instruction lengths and operands -- confusing, not just
unhelpful. `qemu-system-i386 -d int,cpu_reset -D <logfile>` (kept separate
from any `-serial file:...` log) is the other key tool: it dumps full CPU
register state, including segment/descriptor caches, at every exception or
reset, and pairs with `nm`/`objdump` on the *freshly rebuilt* object (stale
builds shift symbol offsets and will mislead you) to map a faulting address
back to source.

## Current command surface

System commands: `help`, `clear`, `vga [d|80x25|80x50|132x25]`, `about`,
`bootinfo`, `memmap`, `memory`, `cpuinfo`, `hexdump ADDRESS LENGTH`, `halt`,
`reset`, and `reboot [soft|hard]`.

### `reboot`: a soft-reboot path alongside the existing hardware reset

`reset` (and `reboot hard`, its explicit alias) has always meant an
unconditional 8042 keyboard-controller pulse reset -- a genuine hardware
CPU reset, indistinguishable from power-cycling as far as this kernel's
own state is concerned. `reboot`/`reboot soft` (`kernel/src/reboot.rs`,
`shell.rs`'s `reboot_command`) instead warm-restarts the machine by
jumping straight back into `boot/stage2.S`'s own `_start` (via a new
`BootInfo::soft_reboot` callback pointer, `soft_reboot_entry` in
`stage2.S`) without ever touching the CPU's hardware RESET line: BIOS
POST is skipped entirely, but every other step of an ordinary boot still
runs for real -- E820 memory-map collection, A20 verification,
`select_kernel_slot`'s full A/B fallback/confirm state machine, and a
fresh kernel load from whichever slot that state machine currently
selects. This is why a pending KUP-installed kernel (see "KUP" below) is
tried exactly the same way after a soft reboot as after a hardware one --
`select_kernel_slot`'s logic has no idea, and needs no idea, which kind
of reboot brought it here. `soft_reboot` mirrors `disk::call_trampoline`'s
own real-mode-drop discipline (`cli`, restore the real-mode IVT and
BIOS-conventional PIC mapping, *then* drop to real mode) for the same
reason: stage two's own BIOS disk reads need working BIOS timer-tick
delivery, and the currently running kernel's IDT/PIC state is otherwise
still loaded. If a given `BootInfo` predates this callback (or the
loader never populated it), `soft_reboot` falls back to a hardware reset
instead of transmuting a null pointer -- the same "cleanly unavailable,
not a fault" convention `disk.rs` already uses for a zero `bios_disk_io`
pointer.

**Deliberately not used by `reboot-on-crash`** (`debug::handle_crash`):
that path's entire point is recovering unattended no matter how corrupted
the running kernel's state is after a fatal exception, including the very
low-memory stage-two code and `BootInfo` a soft reboot depends on staying
intact. A hardware reset makes no assumptions about surviving state at
all; a soft reboot makes several (stage two's code at `0x8000` is
unmodified, `BootInfo` is still valid, the CPU can still safely drop to
real mode) -- appropriate only for a deliberate, operator- or
KUP-requested restart of an otherwise healthy kernel, never a crash
handler's own recovery path. `reset` remains available as an explicit,
always-usable fallback in case a soft reboot itself ever needs to be
ruled out (e.g. it hangs, or something about the running kernel's state
is suspect) -- exactly the situation the two debugging sessions below
were built to rule in or out.

#### Two soft-reboot bugs found and fixed during development, neither ever hit by a hardware reset

Soft reboot is a fundamentally new code path: it is the first time
`boot/stage2.S`'s real-mode boot sequence has ever run on a CPU that
*wasn't* freshly reset by BIOS POST moments before. Both bugs below are
exactly the class of surprise that predicts -- each one was invisible to
every existing test and to a hardware reset, and surfaced only because a
soft reboot leaves behind state (on disk, or in a CPU register) that a
real reset always wipes clean.

- **Stale slot-LBA/sector-count constants caused KUP to write a new
  kernel to the wrong disk location.** `kernel/src/debug.rs`'s
  `SLOT1_LBA`/`SLOT_SECTORS` constants are a copy of values
  `tools/write_layout.py` also computes and are meant to move together
  with it (see "Low-memory map"/A-B slot documentation for every other
  constant already known to need this) -- but this fifth location was
  missed when a past `KERNEL_RESERVED_END` growth (`0x60000` -> `0x70000`
  for ES1370 ADC support) updated `linker.ld`/`pmem.rs`/
  `write_layout.py`/`write_netrom_layout.py` together. The stale values
  (`SLOT1_LBA=2721`, `SLOT_SECTORS=640`, versus the current, correct
  `SLOT1_LBA=2849`, `SLOT_SECTORS=768`) meant `install_kernel_update`
  wrote a freshly pushed kernel image to the wrong LBA, while
  `select_kernel_slot` (using the current, correct constants baked into
  `build/layout.inc`) read the *right* LBA on the next boot -- landing on
  a stale, unrelated 128-sector region and executing whatever garbage
  bytes were there as code, producing a wild `#UD` a few instructions in.
  This was invisible to every pre-existing test because nothing before
  soft reboot ever exercised the very first KUP-installed-kernel boot in
  a way that also depended on `debug.rs`'s own copy of these constants
  being correct -- a hardware-reset-only KUP flow happened to never
  actually be run end-to-end with a genuinely mismatched value until this
  feature's own test coverage did. Fixed by correcting both constants and
  expanding `debug.rs`'s doc comment to explicitly document this as a
  fifth location that must be kept in sync, with the exact recipe
  (`python3 tools/write_layout.py --kernel build/kernel.bin --output
  /dev/stdout`) for recomputing them.
- **`collect_memory_map` cleared only the low 16 bits of `%ebp`, so a
  soft reboot's leftover register garbage made the E820 loop believe it
  was already done.** `boot/stage2.S`'s E820 collection loop used
  `xorw %bp, %bp` to zero its region counter, but then compared and
  stored using the full 32-bit `%ebp` (`cmpl $MAX_MEMORY_REGIONS, %ebp`,
  `movw %bp, BOOT_INFO + 10`). On a genuine BIOS cold boot this worked by
  incidental luck -- POST happens to leave `%ebp`'s upper 16 bits zero --
  but a soft reboot never resets the CPU at all: it is the same running
  processor jumping back into real mode from the still-executing 32-bit
  Rust kernel, which leaves `%ebp` holding whatever 32-bit stack-frame-
  pointer-sized value it was last using. The 32-bit comparison then saw a
  garbage value already `>= MAX_MEMORY_REGIONS` (32) on the very first
  loop iteration, exiting immediately and storing a region count of 0 (the
  low 16 bits were still correctly zero) -- producing exactly the observed
  `Physical memory: 0 frames (0 KiB) tracked above 0x70000` on every boot
  following a soft reboot, and, once `pmem`'s allocator had nothing to
  hand out, a cascading `No supported Ethernet controller found on the
  PCI bus.` once NIC driver initialization's own DMA-buffer allocation
  failed too -- one root cause producing two seemingly unrelated
  symptoms. Fixed by changing `xorw %bp, %bp` to `xorl %ebp, %ebp` so the
  full register is genuinely zeroed before the loop begins. Both bugs
  were caught by `tools/test_debug_mode_e2e.py`'s KUP-then-soft-reboot
  scenario, confirming the value of that test covering the exact
  code path (a real KUP install followed by an in-process soft reboot,
  not just a hardware-reset boot) most likely to expose this whole class
  of "assumes cold-boot register/state cleanliness" bug.

**A QEMU HMP debugging note from this investigation, worth keeping for
future monitor-driven repro scripts**: QEMU 10.2.1's `pmemsave` HMP
command requires the destination filename argument to be quoted if it
contains a `/` (e.g. `pmemsave 0x100000 4096 "/tmp/some/path.bin"`) --
an unquoted path with a slash is silently misparsed. Draining a QEMU
monitor UNIX-socket connection also needs a loop, not a single `recv()`
call, since a reply can arrive across more than one TCP/socket read.

`vga` shows the current and available local modes, and changes the mode when
an argument is supplied; it clears the framebuffer after a change. `d` and
`80x25` select the default mode. `80x25` is
the BIOS-compatible default, `80x50` uses the VGA 8x8-font mode, and `132x25`
uses experimental extended horizontal timing that is not guaranteed on every
VGA adapter or monitor. Serial and `term net` output are unchanged; existing
tapes continue to print through the kernel console callback.

Interrupt/memory commands: `ticks`, `uptime`, `irqstat`, `pmem`, `palloc`, and
`pfree ADDRESS`.

Storage commands: `diskinfo [diskN]`, `readsector [diskN] LBA`,
`format [diskN]`, `ls [diskN]`, `cat [diskN] NAME`, `write [diskN] NAME
TEXT`, `rm [diskN] NAME`, and `copy SRC_DISK NAME DST_DISK [NEW_NAME]`;
FOSFS is flat and supports contiguous multi-sector files. `disk1` (the boot
disk) is always the default and may be omitted; `disk2`, `disk3`, ... address
up to three additional BIOS hard-disk-range drives discovered at boot (see
"A second (non-boot) disk for config/log storage" below), each a flat,
unpartitioned FOSFS volume. `copy` is the one exception to `disk1`'s default:
since it moves a file between two disks, both `SRC_DISK` and `DST_DISK` must
be spelled out explicitly, never assumed. It stages the whole file through
the same shared `pmem::TAPE_BASE`/`TAPE_LEN` buffer `tape load disk`/`tape
load net` already use (see "Tapes: fixed load address..." below) rather than
adding a second large static buffer, and invalidates (and reports) any
previously loaded tape's now-stale bookkeeping afterward, since that buffer
no longer holds that tape's bytes.

Tape commands: `tape status`, `tape load disk NAME`, `tape load net`,
`tape save [diskN] NAME`, `tape send NAME PEER_MAC|PEER_NUMBER`, and
`tape run [ARGS...]`. Tapes are validated ELF32 `ET_EXEC` images or legacy
raw-flat binaries loaded into the fixed 4 MiB image region; `tape run`
supplies the append-only kernel callback ABI described below. `tape save`
writes whatever tape is currently resident (loaded via `disk`, `net`, or
even a prior `save`) back out to FOSFS unchanged -- the missing
counterpart to `tape load net`, which previously had no way to persist a
tape received over the network; a fresh transfer was the only way to run
it again after a reboot. It reads directly out of the loaded tape's own
buffer (`tape_get()`) via an ordinary `fosfs::write`, so -- unlike
`copy`, which stages through the same buffer as scratch space and always
invalidates whatever was loaded there -- the currently loaded tape's own
bookkeeping is left untouched and `tape run` keeps working immediately
afterward.

Serial commands: `serial` (lists COM1..COM4 via the BIOS Data Area, plus any
PCI-discovered serial controllers as `PCI0`, `PCI1`, ...) and
`term [COM1|COM2|COM3|COM4|PCI0|PCI1|...] [BAUD]` (raw terminal pass-through;
F12 exits).

Printer commands: `printer` (lists LPT1..LPT4 via the BIOS Data Area),
`term printer|LPT1|...|LPT4` (teletype-style live printer terminal; F12
exits, F2 abandons the line being typed and starts a fresh one), and
`print [diskN] NAME`/`print run
CMD...` (print a file, or a command's own output, to the first detected
LPT port).

`readsector` is read-only. `hexdump` is limited to the reserved kernel range
and at most 128 bytes. FOSFS names are limited to 16 lowercase alphanumeric or
`.`, `_`, and `-` characters. The fixed command buffer and contiguous
multi-sector file layout are intentional teaching constraints; the shell's
interactive command line is capped at 256 characters.

`term` defaults to COM1, reusing the same port most physical machines
already have rather than requiring a second UART. While active on a given
port, that port is exclusively the remote link: PS/2 keystrokes go out with
no local echo, and incoming bytes are shown via VGA only
(`console::write_vga_only`, which intentionally skips serial mirroring to
avoid retransmitting incoming bytes back over the same wire). The escape
key is F12 (scan code `0x58`) -- not a Shift/AltGr/Ctrl combination --
because function keys are layout-independent, satisfying keyboards (e.g.
Nordic) where such combinations are ambiguous or reserved. A serial-only fos
session loses its control channel upon entering `term` on COM1; this is a
documented, intentional trade-off.

`term COM2`/`COM3`/`COM4` (`console::Uart`, detected via `console::detect_ports`
reading the BIOS Data Area at physical `0x400`) accept an optional custom
`BAUD` argument, since those ports are never shared with the interactive
shell. COM1's baud rate is deliberately kept fixed at 115200 -- it is the
same port the shell itself may already be driven over, so changing its rate
mid-session would desync any attached terminal program. A requested `BAUD`
must evenly divide `console::UART_CLOCK` (115200) so the exact rate is
programmed rather than rounded; `term` reports and rejects invalid rates or
undetected ports rather than silently misconfiguring the link.

`term PCI0`/`PCI1`/... work the same way as COM2-4, but their I/O address
comes from `console::detect_pci_ports` (`kernel/src/pci.rs`) walking PCI
configuration space (Mechanism #1, ports `0xcf8`/`0xcfc`) for class `0x07`
subclass `0x00` ("simple communication controller" / "serial") devices and
reading each one's BAR0. This is not registered in the BIOS Data Area, so it
needs its own lookup, separate from `detect_ports`. Legacy ISA serial cards
need no such lookup: jumpered to a standard COM address, BIOS POST
enumerates them exactly like an onboard port, so they already appear via
`detect_ports`. `pci.rs` only implements enough PCI to serve this one
purpose (no bridges, no memory-mapped BARs, no other device classes) and
treats each matching PCI function as exactly one port, so multi-port cards
that multiplex several UARTs behind one BAR at internal offsets are not
specially decoded. Native USB (and thus USB-to-serial adapters) remains out
of scope; see "Deliberately deferred work" below.

Ethernet commands: `nic` (active status; initializes every supported backend
on first use, currently RTL8139, 3C905, or Intel e1000), `nic list`,
`nic use N`, `nic send` (one raw broadcast test frame), `nic recv`
(polls briefly for one incoming frame and hex-dumps it), `nic sniff`
(promiscuous-mode capture of every frame seen, until F12 stops it),
`nic discover`/`nic peers` (broadcast/list a numbered address book of other
listening fos machines), `chat [PEER_MAC|PEER_NUMBER]` (line-based
broadcast or unicast text chat; see below), and `term net` (a full remote
shell over Ethernet; see below).

`nic` is backed by the common `kernel/src/nic.rs` facade, which initializes
every polling backend instance that succeeds: `rtl8139.rs` for the
QEMU-default Realtek RTL8139, `threecom.rs` for selected PCI 3C905/B/C
variants, or `e1000.rs` for Intel's QEMU 82540EM model. The first
initialized interface remains active by default; `nic list` and `nic use N`
select the shell's active interface. Each card is found by exact vendor/device
ID through `pci::find_devices` because NIC register layouts are
vendor-specific. The 3C905 path uses the card's windowed command interface
and bus-master descriptor lists; it is not emulated by the installed QEMU
build and needs physical validation. The e1000 path uses a 32-bit memory BAR
and legacy descriptor rings; QEMU coverage makes it the first Intel backend
suitable for automated testing. PCI helpers enable the address-space type
required by each backend and bus mastering.

The 3C905 transmit descriptor's first word is revision-sensitive: original
905 cards use the packet length, while 905B/C cards require the transmit
indication and roundup-defeat flags. `threecom.rs` identifies that revision
from the PCI device ID and keeps transmit polling bounded, so a missing link
must not strand a polling bridge in a long wait.

The driver polls status registers rather than using the card's interrupt
line -- consistent with `interrupts.rs` leaving every IRQ but the timer and
keyboard masked at the PIC -- and asks `pmem` for real physical frames for
its DMA buffers rather than using `static` kernel memory, since a receive
ring large enough to be useful would not fit in the kernel's small reserved
low-memory region (`linker.ld`). Its receive ring must be one physically
contiguous block, which single-frame-at-a-time `pmem::allocate` cannot
guarantee, hence `pmem::allocate_contiguous` (this project's first caller to
need more than one contiguous frame). There is no protocol above raw
Ethernet frames (no ARP, no IPv4, no sockets) and no support for additional
NIC families beyond the RTL8139, selected 3C905 PCI backends, Intel e1000,
and NE2000-compatible clones (see "NE2000-compatible PCI clones" below);
3C509 ISA and e1000e/82574 support are separate future drivers.

### NE2000-compatible PCI clones

`kernel/src/ne2000.rs` is a fourth NIC backend, found by exact vendor/
device ID (`0x10ec`/`0x8029`, the Realtek RTL8029AS) the same way every
other backend is -- this is the chip almost every generic, unbranded PCI
card marked only "NetWare" (no vendor name at all) actually contains, since
NE2000 compatibility became a decades-long de facto standard for the
cheapest possible Ethernet silicon. Unlike every other driver here, it uses
no host-RAM DMA at all: the card has its own 16 KiB of onboard packet
memory, addressed through a remote-DMA port-I/O interface (`REG_RSAR0/1`
set a start address, `REG_RBCR0/1` set a byte count, then the data itself
is streamed through one port, `REG_DATA`) rather than a descriptor ring or
BAR-mapped buffer the driver reads/writes directly. `pmem::
allocate_contiguous` is therefore never called by this driver, unlike
every other one here.

The receive side is organized as an 8-entry ring of 256-byte pages
(`RX_START_PAGE`..`RX_STOP_PAGE`) inside that onboard memory, each entry
prefixed by a 4-byte header the card itself writes: byte 0 is the receive
status (matching `RSR`), byte 1 is the next entry's page number, and bytes
2-3 are a little-endian byte count. Getting that byte order backwards --
which this driver's first working version did -- silently drops every real
frame in a way that looks identical to "the card never received anything
at all": the swapped bytes make the ring's own sanity check compare a
receive-status value (typically `0x01`) against a page-number range
(`0x46..0x80`), which always fails, and this driver's next poll then
observes `recovery_pending` and performs a full software reset that erases
the very evidence needed to diagnose it (`CURR`/`BNRY`/`ISR` all revert to
their pristine post-reset values). This was only conclusively diagnosed by
using GDB against QEMU's own `hw/net/ne2000.c` (breakpoints on symbol-
relative offsets like `*ne2000_receive+0x331`, since raw absolute
addresses do not get PIE/ASLR-relocated by GDB, and since `ne2000_
ioport_read`/`ne2000_buffer_full` are `static` functions with no exported
symbol to break on at all) to prove QEMU's device model does fully write
and advance the ring before the fos driver ever sees it -- conclusively
isolating the bug to this driver's own header-field order, not QEMU, the
network layer, or the ring's page-range constants. `-object filter-
dump,id=ID,netdev=NETDEV_ID,file=PATH.pcap` (a QEMU netdev capture filter)
is a lower-effort complementary tool worth reaching for first in any
future "does the frame even arrive" investigation, since it proves or
disproves network-layer delivery without needing a debugger at all.

`Rtl8139::set_promiscuous` is a read-modify-write toggle of `RCR`'s `AAP`
("accept all packets") bit, layered on top of the accept-filter bits `init`
already sets in `RCR_CONFIG`, so it must not disturb them. `nic sniff`
(`shell.rs`) enables it, polls `receive` in a loop printing every frame
until `keyboard::poll_term_event()` reports the F12 escape -- the same
mechanism and rationale `term_loop` uses -- and always disables it again on
the way out, whether the loop ran to completion or was interrupted, so a
later `nic send`/`nic recv` is not left seeing unrelated traffic.

`tools/send_frame.py` is a standalone host-side script (not part of any
`make` target) that sends one raw Ethernet frame from the host machine via
an `AF_PACKET` raw socket, for testing `nic sniff` against real hardware.
It reads the destination MAC from `tools/mac`, a small `.gitignore`d file
(not checked in, since it names whichever physical test machine is at
hand) rather than a required argument. Keep this script dependency-free
(standard library only, matching the other `tools/` scripts) and keep its
default ethertype in sync with `shell.rs`'s `TEST_ETHERTYPE` if that ever
changes.

`chat` (`shell.rs`) is built from the same two pieces `nic sniff` and
`term` already demonstrate separately, merged into one full-duplex loop: a
receive-and-print loop (`Rtl8139::receive` + `CHAT_ETHERTYPE` filtering, in
`handle_incoming_chat_frame`) and a line-input-and-echo loop
(`keyboard::poll_term_event` for local keys, `console::read_serial_byte` as
an equally valid input source so `/quit` also works over a serial-only
session -- unlike `term`, `chat` never repurposes the serial console, so
there is no reason to restrict it to the keyboard). `CHAT_ETHERTYPE`
(`0x88b6`) is deliberately distinct from `TEST_ETHERTYPE` (`0x88b5`, `nic
send`'s), even though IEEE reserves both together for exactly this kind of
local experiment, so a `nic sniff`/`nic recv` capture can't confuse chat
traffic for test traffic. `chat AA:BB:CC:DD:EE:FF` (parsed by the local
`parse_mac`, which mirrors `tools/send_frame.py`'s Python function of the
same name) targets one peer directly instead of broadcasting; unicast
needed no driver changes at all, since `Rtl8139::send`/`receive` already
handle any destination address -- only the shell layer's addressing and
inbound source-MAC filtering needed to be added.

Every `CHAT_ETHERTYPE` frame carries a one-byte message tag right after the
Ethernet header (`CHAT_TAG_TEXT`/`CHAT_TAG_DISCOVER_REQUEST`/
`CHAT_TAG_DISCOVER_REPLY`), so the single ethertype can carry both chat
lines and the small peer-discovery exchange below without needing a third,
unreserved ethertype. A `CHAT_TAG_TEXT` payload is trimmed of trailing zero
padding (the same padding `Rtl8139::send` and `tools/send_frame.py` both
add to reach Ethernet's 60-byte minimum frame length) before being decoded
as UTF-8.

`nic discover`/`nic peers` add a small numbered peer table
(`PEERS`/`PEER_COUNT` statics, capped at `MAX_PEERS` = 16, deliberately not
dynamically sized since this kernel has no heap) so a peer can be named
`chat 1` instead of a full MAC. `nic discover` broadcasts one
`CHAT_TAG_DISCOVER_REQUEST` through every initialized NIC, re-sending it every
`DISCOVER_REBROADCAST_TICKS` (a plain `interrupts::ticks()` comparison, not a
timer interrupt) so a peer that only starts `chat`/`nic discover` partway
through is still found, and busy-polls every NIC's `receive` -- printing each
newly-seen peer as it arrives -- until `keyboard::poll_term_event()` reports
F12, the same escape convention `nic sniff`/`term`/`chat` all use; `nic peers`
just prints the table. A reply to an incoming probe is sent back through the
same local NIC on which that probe arrived, so discovery works across
different attached Ethernet segments rather than only on the primary NIC.
Whoever receives a `CHAT_TAG_DISCOVER_REQUEST` -- whether via `nic discover`
or a running `chat` session's own receive loop -- answers with one unicast
`CHAT_TAG_DISCOVER_REPLY` straight back to the requester's source MAC
(never broadcast, so replying doesn't also broadcast to every other
listener) and remembers the requester too, making discovery symmetric
after one round. Because this kernel has no interrupt-driven network
service (every receive is a polled call inside a foreground loop --
`nic_recv`, `nic sniff`, `chat`, or `nic discover` itself), a probe only
ever gets a reply from a fos machine that happens to be inside `chat` or
`nic discover` at that exact moment; this is a real, honestly-documented
limit of the polling design, not a bug to eventually
fix with a background listener.

`tape` (`shell.rs`) is a "load, then run" pair: one fixed-size physical
image buffer at `pmem::TAPE_BASE` through `pmem::TAPE_BASE + TAPE_LEN`
(4 MiB), plus explicit metadata for the loaded length, format, entry point,
and monotonically increasing handle. Loading a new tape overwrites that
reserved image region; it does not allocate or free a variable-sized tape
buffer from the general frame pool. The loader accepts validated fixed-address
ELF32 `ET_EXEC` images and legacy raw flat images, and rejects any image that
does not fit the reserved region.

`tape load net`/`tape send` share `CHAT_ETHERTYPE` with `chat`/`nic
discover`, adding three tags (`TAPE_TAG_START`/`TAPE_TAG_DATA`/
`TAPE_TAG_STOP`). Unlike `CHAT_TAG_TEXT`, which gets away with trimming
trailing zero padding because printable text never legitimately ends in
one, a genuine binary tape's own trailing zero bytes must never be
confused with Ethernet's 60-byte minimum-frame padding -- so `TAPE_TAG_START`
declares the transfer's exact total length up front, and every
`TAPE_TAG_DATA` frame's expected byte count is derived purely from its
sequence number and that declared length, never from the received frame's
own (possibly padded) length. A skipped or reordered sequence number aborts
that attempt and discards the partial image immediately, since this protocol
deliberately has no retransmission (consistent with `chat`/`nic discover`
also being simple, best-effort, unacknowledged protocols); it does not
retry or wait for a fresh start automatically, but `tape load net`'s own
loop keeps listening afterward, so a resend from the sender just starts a
fresh attempt. `tape load net` locks onto whichever sender's
`TAPE_TAG_START` it accepts first and ignores any other sender's frames
until that attempt finishes or aborts, the same reasoning `chat`'s unicast
mode filters incoming text by source MAC.

`tape send NAME PEER_MAC|PEER_NUMBER` reuses `chat`'s exact peer-addressing
(`parse_mac`/`parse_number`/`peer_by_index`) so a peer found by `nic
discover` needs no MAC typed out, and reuses `fosfs::read` (added
alongside `tape`, mirroring `fosfs::cat` but copying bytes into a caller
buffer instead of the console) to source its data. `tools/send_tape.py` is
the host-side counterpart, deliberately building the identical wire frames
by hand (rather than importing anything from the kernel side, which is not
possible across the language/target boundary) so either end can talk to
either implementation. Rather than requiring a `tools/mac`-style file up
front like `tools/send_frame.py` does, it can perform its own discovery --
broadcasting a `CHAT_TAG_DISCOVER_REQUEST` and listening briefly for
`CHAT_TAG_DISCOVER_REPLY` frames -- since a script only used for a single,
short transfer benefits less from a persistent MAC file than a script
meant to be re-run against the same test machine over and over. `--mac`/
`--mac-file` remain available to skip discovery entirely.

Both senders pace their frames because `tape load net` polls a finite RTL8139
receive ring rather than receiving through an interrupt-driven queue. The
kernel's `tape send` waits one 100-Hz PIT tick (about 10 ms) between frames;
`tools/send_tape.py` defaults to a 3 ms delay, which is the faster interval
verified reliable on the documented real-hardware setup. An unpaced burst can
overflow the receiver, causing the deliberately strict sequence check to
report a missing or unexpected chunk rather than load corrupt data.

`tape load disk NAME` reuses `fosfs::read` and now reads the complete
multi-sector file into the tape image buffer. New Rust tape files are ELF32
`ET_EXEC` images: the loader validates the i386 header and program-header
table, materializes only `PT_LOAD` segments, clears BSS tails, and records the
validated entry point. Legacy raw flat files remain accepted during migration.
The generated image preloads every compiled tape, so a fresh boot can run them
without a network transfer. `write NAME TEXT` remains the interactive
printable-ASCII path; binary files are installed by the host-side image
builder.

`tape run` uses a cdecl `call` (via `core::mem::transmute`) to the validated
ELF entry point (or the loaded buffer's address for a legacy flat file), then
passes the append-only tape callback ABI: console output, abort/input,
allocation, startup arguments, indexed NIC access, terminal position, and
FOSFS file operations. This kernel's flat, unpaged memory model lets the
tape image treat its physical address directly as a usable pointer -- the
same convention `rtl8139.rs`'s own DMA buffers rely on. A tape that ends with
a near `ret` returns control to the shell
cleanly (verified end-to-end with a real one-byte `0xc3` tape sent over a
live QEMU multicast netdev from a host-side Python script and executed);
anything else is trusted exactly as much as any other kernel code -- a bad
opcode is at least caught by the CPU exception handlers added earlier this
project (see "Real hardware notes" below) rather than corrupting silently,
but there is still no way back into a hung or corrupted kernel except a
reset, consistent with "Current boundaries" in README.md.

The reserved low-memory kernel region (`linker.ld`/`pmem::RESERVED_BELOW`/
`tools/write_layout.py`'s `KERNEL_RESERVED_END`) grew from `0x20000` to
`0x40000` alongside `tape`, then to `0x50000` when paging, NIC reliability,
and boot-script code consumed the remaining headroom. The feature's own code pushed the kernel
binary past the previous 64 KiB ceiling with essentially no headroom left
beforehand (the prior build was already only dozens of bytes under it), then
to `0x60000` when scaffolding the inert `kernel/src/i350.rs` driver module
(see "Intel I350 support" below) pushed `__kernel_end` to `0x51fb4` -- past
the old `0x50000` ceiling even with `i350::CANDIDATE_DEVICE_IDS` left empty,
since the module's per-instance static storage (`i350::NICS`) and its code
are linked in regardless of whether any real device ID ever matches at
runtime. All three constants must always match; see "Low-memory map" above for why
growing this region is safe (nothing else lives between `0x20000` and the
real-mode stack pointer's `0x9f000`).

Because a tape can write VGA memory and move the hardware cursor directly
(see `tapes/primes.S` below) with no way to tell `console.rs` it did
so, `tape_run` originally called a `console::resync_cursor()` immediately
after the tape returned -- it just re-read the CRTC cursor position the
same way `console::initialize()` already does once at boot, so the
shell's next line of output would continue from wherever the tape
actually left the cursor instead of overwriting the tape's own output
from a stale, pre-tape position. This whole mechanism (and the function
itself) was later removed once tapes stopped touching the CRTC directly
at all -- see the "tape print callback ABI" section further below, which
is the current design; the paragraphs immediately following this one
describe the tape as it worked *before* that change, for historical
context on the bugs that shaped `primes.S`'s scroll/cursor handling.

### A permanent `pci` device-dump command

`pci` (`shell.rs`) lists every discovered PCI function -- bus:device.func,
vendor:device ID, class/subclass/prog-if, header type, and BAR0 -- using a
new `pci::scan_devices()` that mirrors `find_serial_ports`'s own
multi-function-aware scanning (checking `HEADER_TYPE_MULTIFUNCTION` and
iterating functions 0..8 for a multifunction device, same as
`find_devices` already does for NIC discovery), capped at `MAX_PCI_DEVICES`
(64, comfortably above any real machine's populated slot count). Unlike
`ls`/`memmap`, it is allowed to scroll past one screen, since the number of
PCI functions on a real machine is inherently unbounded and not worth
truncating. This exists specifically so an unknown card's exact vendor/
device ID can be read directly from hardware -- the same information this
project would otherwise have to guess at from a chip's silkscreen marking
alone -- before deciding whether to add a new NIC (or other) backend for
it; see "Intel I350 support" immediately below for the motivating case.

### Intel I350 support: scaffolded, not yet enabled

`kernel/src/i350.rs` is a fifth NIC backend, structurally mirroring
`e1000.rs`'s driver shape (the same descriptor structs and
reset/configure/send/receive/watchdog pattern) but for Intel's I350
("Powerville") PCIe Gigabit Ethernet controller family -- a genuinely
different, newer chip generation than the `82540EM` QEMU emulates as
`e1000` (in Linux driver terms, I350 belongs to the `igb` family, not
`e1000`/`e1000e`). It was added in preparation for a physical dual-port
card (chip-marked `NHI350AM4 S621J51C 1621`, identified by its markings as
an I350 board with only 2 of the chip's 4 ports wired out) whose exact PCI
device ID had not yet been confirmed at the time this module was written
-- the owner's second card was offsite and its ID needed reading from a
different machine.

Three real register-layout differences from `e1000.rs`, documented in
`i350.rs`'s own doc comments as "unverified against real hardware,
best-effort transcription from the public 82576/82580/I350 register map,
same as Linux's `igb` driver" (mirroring this project's usual "flag it as
speculative until physical hardware proves or disproves it" convention --
see "Real hardware notes" below):

- Interrupt registers (`ICR`/`IMS`/`IMC`) moved from `0x00c0`/`0x00d0`/
  `0x00d8` to `0x1500`/`0x1508`/`0x150c`.
- Ring registers became per-queue arrays; queue 0 lives at `0xc000` (Rx) /
  `0xe000` (Tx) instead of the fixed `0x2800`/`0x3800` 82540EM uses, and
  needs an explicit `RXDCTL0`/`TXDCTL0` (`0xc028`/`0xe028`) "queue enable"
  bit (`1<<25`) set before the ring is considered live -- a step 82540EM
  never required.
- A `SWSM` (`0x5b50`)/`SW_FW_SYNC` (`0x5b5c`) software/firmware semaphore
  handshake (`acquire_swfw_sync`/`release_swfw_sync`) is needed before
  touching the PHY, to arbitrate with resident management firmware
  (AMT/ME) that I350 boards often carry -- again, not needed on 82540EM.

`i350::CANDIDATE_DEVICE_IDS` is deliberately an empty `&[u16]`: the module
is fully wired into `nic.rs`'s facade (`Nic::I350`, every match arm, and
`initialize()`'s backend loop) and compiles as ordinary, complete code, but
with no device IDs to match it can never actually claim a PCI device, so
`nic`/`nic list`/`pci` behave identically to before this module existed.
This is deliberate, not an oversight: `0x1520` is the I350 **Virtual
Function** (SR-IOV) device ID, with a completely different, mailbox-based
register interface that has no direct PHY/link access at all -- silently
matching it alongside a real physical-function ID would hang or corrupt
state, so no ID is enabled at all until the real card's own ID is read
back (e.g. via the new `pci` command above, or `lspci -nn` on a machine
that already has a working driver for it) and confirmed to be a physical
function, not this one. Enabling support for every plausible I350 sibling
ID at once was considered and rejected for the same reason: fiber/
backplane variants skip copper-PHY autonegotiation entirely, and treating
them identically to a copper port's link-bring-up sequence could hang
indefinitely waiting on a link state that variant will never report the
same way. The intended path is to add only the one confirmed, physically
tested ID first, then broaden deliberately to sibling copper IDs only once
each has actually been tested against real hardware -- never as a
speculative batch.

Like the 3C509 investigation and I350's own complete absence from QEMU
(`-device help` lists no I350 model at all, unlike RTL8139/82540EM/
NE2000), this is a standing, permanent real-hardware-only testing cost for
every future iteration on this driver, not a one-time bring-up expense --
see "ISA bus support and the 3C509" under "Deliberately deferred work"
below for the same tradeoff discussed at length for a different card.

Scaffolding this module alone (with no device IDs enabled, and therefore
no runtime behavior change at all) pushed `__kernel_end` past the
previous `0x50000` reserved-region ceiling, to `0x51fb4` -- the module's
own static per-instance storage (`i350::NICS`, sized like `e1000::NICS`)
and its driver code are linked into `kernel.bin` unconditionally,
regardless of whether `CANDIDATE_DEVICE_IDS` ever matches anything at
runtime. This required growing the reserved region again, to `0x60000`;
see the paragraph on that growth history further below.

`tapes/primes.S` is a worked, non-trivial test tape: a
Sieve-of-Eratosthenes prime counter up to 1,000,000, assembled the same
way as `boot/stage1.S`/`boot/stage2.S` (`as --32` then `objcopy -O
binary`, via the new `make tape-primes` target -- entirely separate from
`make image`/`run`/`test`) rather than compiled by `kernel/`'s own Cargo
build. Unlike the throwaway one-byte `0xc3` tape used to validate `tape`
itself, this one has real data (a 1,000,000-byte sieve array). It remains
genuinely position-independent as a teaching example, using the classic
`call get_base` / `pop %ebx` idiom and base-relative labels, although the
fixed tape address means that PIC is no longer required. The sieve array is
embedded as literal zero-filled space (`.fill N, 1, 0`) directly in the file,
which is why the assembled binary is roughly a megabyte. It prints through
the kernel's `putc` callback, so the normal VGA, serial, and `term net`
relay paths handle output; it measures elapsed time with a raw `rdtsc`
(reported as uncalibrated CPU cycles, not wall-clock time). The large image
is sent over the network in ~1000 chunks by `tools/send_tape.py`; FOSFS3
supports contiguous multi-sector files, so its preloaded copy is not limited
to one 512-byte sector. That is what lets this be checked end-to-end by a
disposable
serial-driven test harness (`tapes/test_primes_e2e.py`, a QEMU
multicast-netdev-based script in the same spirit as the one used to
validate `tape` itself) instead of needing a screen-capture/OCR pipeline
just for this one tape. It also saves and restores `%ebx`/`%esi`/`%edi`/
`%ebp` around its own body, since `tape_run` calls it as a plain
`extern "C" fn()` (cdecl), which requires those callee-saved registers to
come back exactly as they went in.

An earlier version of this tape's hand-rolled `print_char` tracked
`cursor_pos` as a flat `row*80 + col` offset but never checked it against
the 80x25 (2000-cell) screen, unlike `console.rs`'s own `newline()`. A
fresh QEMU boot happens to leave the cursor well within the visible
screen before `tape run`, so `test_primes_e2e.py` never caught this, but
real hardware's longer/different boot narration (or simply more commands
typed first) can easily leave the cursor already near the bottom row --
and on real hardware this tape's whole three-line summary silently
scrolled off into unaddressed-but-still-writable VGA memory and was never
seen at all, with the very next kernel-printed line afterward
(`Tape returned; back at the fos shell.`) then landing corrupted too,
since `tape_run`'s `console::resync_cursor()` inherited the tape's
now-out-of-range hardware cursor position. The fix mirrors
`console.rs`'s scroll logic in assembly: once a `'\n'` or an implicit
column-80 wrap would push `cursor_pos` to `VGA_WIDTH * VGA_HEIGHT`, a new
`print_char_scroll` routine `rep movsw`s every row but the first up by
one, `rep stosw`s the newly-exposed last row blank, and resets
`cursor_pos` to the start of that last row before the existing
CRTC-cursor-update code runs.

A second, unrelated bug surfaced immediately after fixing the first one:
with scrolling fixed, real hardware showed the three summary *labels*
("primes found:", "largest prime:", "elapsed:") but every digit was
missing. `print_decimal` was briefly repurposing `%ebx` -- this whole
file's PIC load-base register, on which every `print_char` call depends
to find `cursor_pos` (`(cursor_pos - get_base)(%ebx)`) -- as its own
pushed-digit counter across the very `call print_char` it makes per
digit. `serial_putc` is called unconditionally at the very top of
`print_char`, before any VGA/cursor work, so the COM1 mirror byte for
each digit was always correct regardless; only the on-screen VGA
placement broke, landing wherever `cursor_pos - get_base + <bogus small
counter value>` happened to point instead of the real cursor. This is
exactly why a serial-only test could never have caught either bug:
`tapes/test_primes_e2e.py` only reads QEMU's `-serial stdio`
stream, which both bugs left completely untouched. The fix was simply to
have `print_decimal` use `%edi` (free at every call site, and already
proven safe to hold across `print_char` -- `print_char_scroll` itself
saves/restores it) as its digit counter instead of `%ebx`.

Because both of these were VGA-only bugs invisible to the existing
serial-based test, `tapes/test_primes_vga_e2e.py` was added
alongside it: it runs the same tape-send/tape-run sequence but reads the
tape's actual summary back out of the live VGA text framebuffer
(physical `0xb8000`) via the QEMU monitor's `pmemsave`, exposed over a
throwaway local Unix socket (`-monitor unix:...`, since `-serial stdio`
already owns stdio). Anyone touching `print_char`/`print_decimal`/cursor
handling in `primes.S` should run this test, not just the serial-based
one, since only it actually checks what the tape's real screen would
show.

### Tape print callback ABI

The hand-rolled VGA/CRTC/COM1 output described above worked, but it had a
deeper cost than just being a source of subtle bugs: it wrote to
hardware directly, entirely bypassing `console::write_serial` -- the one
function `term_net`'s relay queue (`console::enable_relay`/`relay_push`)
taps into -- so a tape's own printed output could never reach a remote
`term net` client, only whatever wrapper messages `shell.rs` itself
printed around it (see "Remote terminal (`term net`)" below for the
`term_net`/tape interaction in general). Duplicating scroll and cursor
logic in assembly was also just inherently more bug-prone than reusing
the kernel's own, already-correct implementation, as the two bugs above
demonstrate.

The fix: `tape_run`'s entry type changed from a zero-argument
`extern "C" fn()` to `extern "C" fn(putc: extern "C" fn(u32))` (still
cdecl), and it now calls `entry(console::print_byte)` instead of
`entry()`. `console::print_byte` is a new, narrow `extern "C" fn(u32)`
wrapper around the existing `console::write_bytes(&[byte as u8])` -- the
same single function every other command's `println!` ultimately goes
through, so it already correctly handles VGA placement, scrolling, the
hardware cursor, the COM1 mirror, and relaying to `term_net` if active,
all in one place. The argument is widened from `u8` to `u32` specifically
so the calling convention at the assembly call site is unambiguous (a
full zero-extended 32-bit stack slot), rather than depending on how a
particular Rust/LLVM version happens to pass a sub-word argument under
cdecl.

`tapes/primes.S` was rewritten around this: its `_start` prologue
now captures the callback pointer from `4(%esp)` (the tape's one cdecl
argument) into a `putc_fn` data slot (replacing the old `cursor_pos`
slot) right after computing its own PIC base, `print_char` now just
zero-extends its byte into `%eax`, pushes it, and `call`s through
`putc_fn`, and the old hand-rolled `print_char_newline`/
`print_char_scroll`/`print_char_move_cursor`/`serial_putc` routines (and
the `VGA_*`/`CRTC_*`/`COM1_*` constants they used) are gone entirely --
none of it is needed anymore. `print_decimal`'s earlier `%ebx`-vs-`%edi`
fix (described above) remains necessary and correct under this design
too: `%ebx` is still this file's PIC load-base register, needed by every
`print_char` call to locate `putc_fn` itself (`(putc_fn - get_base)(%ebx)`),
so `print_decimal` still must not repurpose it as a digit counter.

Because `console::print_byte` is a plain Rust `extern "C" fn`, the tape
no longer needs to save/restore any of `%ebx`/`%esi`/`%edi`/`%ebp` around
the *callback* call specifically (Rust-generated code preserves its own
callee-saved registers automatically); it still saves/restores them
around its own body as a whole, since `tape_run` itself still calls the
tape's `_start` as a plain cdecl function.

This also made `tape_run`'s `console::resync_cursor()` call unnecessary:
a tape that only ever prints through the callback never touches the CRTC
directly, so the kernel's own cached `ROW`/`COLUMN` state is never stale
to begin with. That function and its call site were removed rather than
kept as dead weight -- nothing currently needs a "the tape misbehaved and
touched hardware anyway" recovery path, and one can be re-added if a
future tape ever needs it.

`tools/test_term_net_tape_e2e.py` (see "Remote terminal (`term net`)"
below) was updated to assert the opposite of what it asserted before this
change: the primes tape's own `78498`/`999983` digits now DO appear in
the burst of `TERM_TAG_DATA` frames relayed to a remote `term net` client
after `tape run` returns, proving the callback actually reaches the relay
queue end-to-end over the network, not just in the local VGA/serial view.

### Tapes: fixed load address, and writing one in Rust

Before this, `allocate_tape_buffer` (`kernel/src/shell.rs`) called
`pmem::allocate_contiguous`, the same general-purpose frame allocator
everything else uses -- so a tape's load address varied depending on
whatever contiguous run of free frames happened to be available that run.
That's exactly why `tapes/primes.S` had to be genuinely
position-independent: its `_start` prologue captures its own runtime base
with the classic `call get_base` / `pop %ebx` idiom, and every label is
addressed as an offset from that base, because the assembler/linker have
no way to know the load address ahead of time. That PIC requirement is
easy enough to hand-write in assembly, but neither Rust nor C has an easy
no-dynamic-linker PIC story on a freestanding `i686` target -- their
usual position-independent code relies on a dynamic linker computing
relocations at load time, which this kernel doesn't have and has no
reason to grow just for tapes.

The fix follows the exact same convention the kernel itself already
uses for its own load address: `kernel/src/pmem.rs` now reserves a fixed
physical region, `TAPE_BASE` (`0x100000`) through `TAPE_BASE + TAPE_LEN`
(4 MiB, comfortably above `primes.bin`'s ~1 MiB with headroom for future
tapes), carved out of the general frame pool once at boot via a new
`reserve_range` helper -- `mark_region_free`'s exact opposite, unmarking
those frames as free (and out of the tracked total) the same way frames
below `RESERVED_BELOW` never entered the pool to begin with. Because
this happens before `shell::run` (and therefore any command) can
allocate anything, `allocate`/`allocate_contiguous`/`palloc` can never
hand a tape's own memory to something else, and a tape never needs to
compete with other allocations for it.

With the address now always the same, `allocate_tape_buffer` shrank to a
bounds check (`length <= pmem::TAPE_LEN`) returning `pmem::TAPE_BASE`
unconditionally -- no more per-load frame accounting, and no more
`free_tape_frames`/`pmem::free` calls scattered through `tape_load_disk`/
`tape_load_net`'s abort paths, since there are no longer any dynamically
held frames to release: loading a new tape simply overwrites the same
fixed region in place. `tape_take` (which existed solely to clear `TAPE`
before freeing the old frames) became dead code and was removed along
with it.

This also means `tapes/primes.S` no longer *needs* to be
position-independent -- but it still works unchanged (a fixed address is
still a valid address for PIC code, it's just no longer required), so it
was left as-is, as the assembly reference example.

`tapes/hello-rs/` is the new Rust reference example, proving the
concept: a small `no_std`/`no_main` binary crate that reuses the kernel's
own custom target spec (`targets/i686-fos.json`) and `-Z build-std`
invocation verbatim (see its `.cargo/config.toml`), but with its own
`linker.ld` fixing the load address at `0x100000` (`pmem::TAPE_BASE`)
instead of the kernel's own `0x10000`. Two things make this simpler than
the kernel's own boot path:

- `tape_run` calls a tape with an ordinary cdecl `call` (the CPU is
  already fully set up -- protected mode, a working stack), unlike
  `boot/stage2.S`'s raw `jmp` into the kernel, which hands off the
  `BootInfo` address in a register rather than on the stack. So a tape
  written in Rust needs no register-to-stack bridging assembly like
  `kernel/src/entry.S` provides for `kmain` -- an ordinary
  `extern "C" fn(putc: extern "C" fn(u32))` already matches `tape_run`'s
  call site exactly.
- `tapes/hello-rs/src/entry.S` is consequently tiny: it exists
  solely to zero `.bss` before tail-`jmp`-ing into the real Rust entry
  point, `tape_main`, so `tape_main`'s own `ret` returns straight to
  `tape_run`. The ELF loader also clears each loadable segment's BSS tail;
  retaining the stub keeps the tape startup convention explicit and
  compatible with legacy flat images.

`hello-rs`'s `tape_main` mirrors `primes.S` in miniature -- prints a
greeting, sieves primes up to a much smaller 1,000 (168 primes, largest
997) using a `static mut` array rather than a stack buffer (same
reasoning as `primes.S`'s `.bss`-resident sieve: SIEVE_LIMIT bytes of
stack in a tape running on the kernel's own stack, with no guard page,
isn't worth the risk), and returns -- proving real, non-trivial compiled
Rust code runs correctly at the fixed tape address, not just a string
literal. It's built with `make tape-hello-rs` (producing
`build/tapes/hello-rs.bin`) and verified end to end over the network with
`tapes/test_hello_rs_e2e.py`, mirroring `test_primes_e2e.py`.

The same fixed-address convention would work identically for a C tape
(`gcc -m32 -ffreestanding -fno-pic` with a matching linker script) --
Rust was chosen here only because it reuses the kernel's own toolchain
and build-std setup with far less new scaffolding.

See `docs/rust-tapes-for-dummies.md` for a beginner-oriented, step-by-step
guide to writing, building, and running a new Rust tape from scratch --
this section explains the underlying design; that document is the
practical "how do I make one" walkthrough, and is what README.md's Rust
tape sections point readers at too.

### Tape ABI grows a second callback: `should_abort`, and `vga-colors`

Every tape up to this point ran once and returned: `primes.S`/`hello-rs`
compute something, print a summary through the one callback `tape_run`
passed in (`console::print_byte`, "the tape print callback ABI" above),
and `ret`. That's not enough for a tape meant to *loop* -- an animation,
a benchmark with a visible running total, anything a local operator
should be able to stop without rebooting -- since nothing in the
original one-callback ABI let a tape notice a keypress at all.

`shell::tape_run` now calls a tape as `extern "C" fn(putc: extern "C"
fn(u32), should_abort: extern "C" fn() -> u32)`, passing `keyboard::
tape_should_abort` as the second argument alongside `console::print_byte`
as the first. `tape_should_abort` (`kernel/src/keyboard.rs`) is a thin
wrapper around the exact same `poll_term_event`/`ESCAPE_SCANCODES` check
every other blocking shell loop (`term`, `term net`, `nic sniff`, `chat`,
`tape load net`) already uses to recognize "give up" (F12/F4/F6/F7/F8/
F10) -- a looping tape is just one more thing in that same family, not a
special case.

This is backward-compatible by construction, not by accident: under
cdecl, the caller (`tape_run`) both pushes arguments onto the stack and
cleans them up afterward, regardless of how many the callee actually
reads. `primes.S`/`hello-rs` still declare (and use) only the first
parameter, and neither needed a single line changed -- confirmed by
rerunning `tapes/test_primes_e2e.py`/`test_hello_rs_e2e.py`
unmodified against the rebuilt kernel before writing anything new.

The specific worry this raised -- "if a tape's own loop consumes a local
escape keypress to stop itself, could `term_net`'s own session loop
somehow also see that same keypress and disconnect the remote session
driving the tape?" -- turns out not to be a real risk, and
`tape_should_abort`'s own doc comment spells out exactly why: `interrupts
::pop_scancode`'s queue is a single, consuming FIFO shared by every
caller in this single-threaded kernel, and `term_net`'s own session loop
is paused for the entire time a nested command (like a tape) is running.
So a keypress a tape's loop consumes to stop itself is simply gone from
the queue by the time control returns to `term_net` -- it can never also
be read there afterward as "disconnect". The one loose end, a physical
key's paired *release* scancode left in the queue after the tape stops
polling, is harmless for the same reason `poll_term_event` already
documents elsewhere: release events decode to `None`, never `Escape`.

`tapes/vga-colors/` is the reference tape exercising this: same
Rust/`no_std`/fixed-address setup as `hello-rs`, but its main loop writes
several rows of text straight to VGA text memory (physical `0xb8000`,
the exact address `console.rs`'s own `VGA_BUFFER` uses) instead of
through `console::print_byte`, cycling their foreground color once per
frame, forever, until `should_abort` returns nonzero. Only one line at
the very start and one at the very end go through `putc` -- the
animation itself deliberately never touches serial or a `term net` relay
at all, which is the whole reason this tape bypasses the console
abstraction rather than reusing `print_byte` in a loop: a remote operator
watching a `term net` session would otherwise be flooded with a repaint
every frame, for output that only ever made sense on the local screen to
begin with. It also never touches the CRTC cursor ports, so the kernel's
own cached cursor position is exactly where it was before `tape run`
once the tape returns -- the colored rows simply stay on screen
underneath whatever the shell prints next.

Built with `make tape-vga-colors` (producing `build/tapes/vga-colors.bin`)
and verified with `tapes/test_vga_colors_e2e.py`, which goes beyond
the usual serial-stream check (there is deliberately very little to see
there): it reads the real VGA text framebuffer via the QEMU monitor's
`pmemsave` (the same technique `test_primes_vga_e2e.py` uses) to confirm
several distinct colors actually landed there, then uses the monitor's
`sendkey f12` to simulate a *local* physical keypress and confirms both
that the tape's loop stops (and control returns to the fos shell) and
that the exact same `term_net` session driving it remotely is still
alive afterward -- the concrete proof of the guarantee `tape_should_abort`
documents, not just a read of its doc comment.

#### `tapes/tape-rt/`: a shared runtime, so far only used by `vga-colors`

`tapes/hello-rs`, `tapes/vga-colors`, and `tapes/bigsieve` each used to
carry byte-identical copies of three things that have nothing to do with
any one tape's own purpose: `src/entry.S` (the tiny `_start` stub that
zeroes `.bss` and tail-calls into `tape_main`), a `#[panic_handler]` that
just spins forever, and a `print_bytes` helper wrapping the `putc`
callback (`hello-rs`/`bigsieve` also duplicated a `print_u32` decimal
formatter). `tapes/vga-colors` now instead depends on `tapes/tape-rt/`, a
small shared `no_std` library crate providing all of these once, added
as an ordinary Cargo path dependency (`tape-rt = { path = "../tape-rt" }`
in `vga-colors/Cargo.toml`) -- no new build-system machinery was needed:
`cargo`, `-Z build-std`, and the existing `targets/i686-fos.json`/
`linker.ld` setup all already handle a path dependency exactly like any
other crate in the dependency graph.

This mirrors, for ordinary 32-bit tapes, the exact same "shared plumbing
in one reusable place, swappable per-tape logic supplied externally"
split that `tapes/longmode64_shim.S` uses for 64-bit-excursion tapes (see
"A tape that visits 64-bit long mode and comes back" above): there, the
shim owns the CPU-mode-switch mechanics in assembly and calls out to an
external `tape64_body`; here, `tape-rt` owns the startup/panic/print
mechanics in Rust and a tape's own `src/main.rs` supplies `tape_main`.

`hello-rs` and `bigsieve` were deliberately *not* migrated to depend on
`tape-rt` too, even though both would benefit from the same deduplication
`vga-colors` got. This project's Rust tapes exist first as a teaching
ladder (see `docs/rust-tapes-for-dummies.md`), and a beginner working
through it is meant to read `hello-rs/src/main.rs` -- the very first Rust
tape -- top to bottom and see the *entire* thing that runs, with nothing
imported from an unfamiliar sibling crate. Introducing a shared dependency
into either of those two would trade that self-containedness for a small
amount of saved duplication -- a bad trade for a from-scratch walkthrough's
first two examples specifically. `vga-colors` was chosen instead to
demonstrate the *other* valid pattern -- extracting shared plumbing into
one place once duplicating it a third time starts to feel real -- so a
reader curious about that tradeoff has a working example to study,
without disturbing the two simpler ones. Any genuinely new future tape
(not a teaching-ladder example) should prefer depending on `tape-rt` the
way `vga-colors` does, rather than copying `entry.S`/the panic handler/
the print helpers a fourth time.

#### A local keypress isn't enough: remote abort via `poll_remote_tape_abort`

The above covers *local* escape stopping a tape started remotely, but
not the other direction: a remote `term net` peer had no way at all to
stop a looping tape like `vga-colors`, since `term_net`'s own session
loop -- the only thing that ever calls `nic.receive` -- is completely
paused for as long as a nested blocking command (a tape's own loop) is
running. `keyboard::tape_should_abort` only ever checks the *local*
PS/2 scancode queue, so a remote client sending anything at all over the
wire while a tape ran just sat unread in the network card's own Rx ring
until the tape eventually returned on its own.

`shell::tape_run` now passes a new `combined_tape_should_abort` as the
`should_abort` callback instead of `keyboard::tape_should_abort` directly:
it checks both, returning nonzero if *either* the local keypress check or
a new `shell::poll_remote_tape_abort` says stop. `poll_remote_tape_abort`
does nothing (a single `console::relay_enabled()` branch) unless a
`term_net` session actually started this tape; when one did, it polls
the network card itself, on `term_net`'s behalf, exactly the way that
loop's own `nic.receive` normally would -- recognizing the connected
peer's `TERM_TAG_DATA` frames (stopping the tape if `REMOTE_ABORT_BYTE`
(`0x1b`, plain ASCII Escape) appears anywhere in the payload, the whole
frame otherwise simply dropped rather than fed to the line editor, the
same "consumed, not typed" convention a local escape keypress already
gets) and `TERM_TAG_BYE` (stopping the tape *and* recording
`PENDING_REMOTE_BYE`, a new static `term_net`'s own `'session` loop
checks the moment it resumes, since the one `TERM_TAG_BYE` frame the peer
sent can only ever be received once -- by whichever loop happens to be
polling the network card at that instant). It still answers a discovery
probe from some unrelated third MAC along the way, the same courtesy
`term_net`'s own loop already extends, so a long-running tape doesn't
make the host briefly stop looking like a good network citizen.

Two new statics in `shell.rs`, `ACTIVE_TERM_PEER`/`ACTIVE_TERM_SOURCE`,
carry the connected peer's identity across this call boundary:
`poll_remote_tape_abort` runs many stack frames below `term_net`'s own
loop (inside `entry()`, the tape itself, calling back into
`combined_tape_should_abort`), with no argument of its own to thread the
peer through, so `term_net` sets these two right before entering its
session loop and clears them right after leaving it.

`tools/term_net.py --interactive` now puts its own stdin into cbreak
mode (see the `tty`/`termios` standard library modules) rather than
leaving it in the terminal's default line-buffered ("canonical") mode:
without this, a lone Escape keypress would sit in the *client's own*
input buffer until Enter was also pressed, arriving batched with
whatever was typed next instead of as a single `0x1b` byte on its own --
defeating `REMOTE_ABORT_BYTE`'s whole point. Cbreak (as opposed to full
raw mode) was chosen specifically because it leaves signal-generating
characters alone, so Ctrl-C still ends the client program the same way
it always did.

`tapes/test_vga_colors_e2e.py` was extended to prove this actually
works, not just compiles: after the existing local-`sendkey f12` half of
the test, it starts the tape a second time and sends a raw `0x1b` byte
inside an ordinary `TERM_TAG_DATA` frame over the same session (standing
in for a real `term_net.py --interactive` client's Escape keypress),
confirms the tape's loop stops the same way, and confirms -- again -- one
more command over that same session still gets a reply afterward.



`term net` is deliberately a full remote *shell*, not a byte pipe like
`term`: once a client connects, the real, live shell -- prompt, line
editing, and every existing command -- is handed over to it entirely. This
was made possible by two small refactors, not a parallel reimplementation
of shell logic:

- `run`'s local input loop was factored into a `LineEditor` struct
  (`feed(byte, info)`, holding the line buffer/length/carriage-return
  state that used to be loose locals in `run`). `run` itself now just
  reads a byte and calls `editor.feed`; `term_net` creates its own
  `LineEditor` and feeds it bytes arriving over the network instead,
  so command dispatch (`execute`), line editing, and echo are identical
  regardless of the byte's source -- no command needs to know it might be
  driven remotely.
- `console.rs` gained a small bounded relay queue
  (`enable_relay`/`disable_relay`/`drain_relay`), fed by a single
  `relay_push` call inserted at the top of `write_serial` -- already the
  common choke-point `Console::write_str` (backing `print!`/`println!`),
  `write_bytes`, `clear`, and `erase_last_character` all funnel through,
  so every one of them is captured for forwarding without touching any
  individual call site. `relay_push` runs unconditionally, before
  `write_serial`'s own `SERIAL_READY` check, so relaying still works on
  hardware with no COM1 detected. `write_vga_only` (used only by serial
  `term`'s incoming-byte display) is deliberately *not* hooked in, but
  since this is a single-threaded kernel where only one shell command runs
  at a time, serial `term` and `term net` can never be active
  simultaneously anyway -- this is a non-issue in practice, not a race
  requiring a guard.

`term_net` (`kernel/src/shell.rs`) waits for a `TERM_TAG_HELLO` frame
(sharing `CHAT_ETHERTYPE` with `chat`/`tape`/discovery; new tags
`TERM_TAG_HELLO`/`TERM_TAG_DATA`/`TERM_TAG_BYE` continuing the numbering
after `TAPE_TAG_STOP`), locking onto that peer's MAC for the session --
like `tape load net`'s `TAPE_TAG_START` locking, a second `HELLO` from
someone else while already connected is ignored, since only one remote
client can drive this single-threaded shell at a time. `TERM_TAG_DATA` (in
either direction) deliberately carries no sequence number, unlike `tape`'s
chunked transfer: term is a live, ongoing stream where an occasional
dropped frame is like a dropped keystroke or a torn screen update, not
corrupted data worth aborting over, so the extra bookkeeping tape's
protocol needs was not worth adding here. Both `TERM_TAG_DATA` payloads
trim trailing zero bytes on receipt, the same convention `CHAT_TAG_TEXT`
already uses (see above) and safe for the same reason: this is printable
shell traffic, which never legitimately ends in a genuine zero byte,
unlike `tape`'s binary payloads.

Once connected, `term_net`'s loop each iteration: drains the relay queue
and forwards whatever it finds as `TERM_TAG_DATA`; feeds any incoming
`TERM_TAG_DATA` payload bytes one at a time into its own `LineEditor` via
`feed`; and checks for local F12 (sends `TERM_TAG_BYE` to the peer, then
returns all the way to the local shell) or an incoming `TERM_TAG_BYE` (the
session ends without echoing one back, but `term_net` then loops back to
waiting for a fresh `TERM_TAG_HELLO` instead of returning -- see "`term
net` stays resident" below). Local PS/2/VGA input is not read at all while
connected -- this is an exclusive takeover, matching this project's
existing "one active mode at a time" convention (`chat`/`tape load
net`/serial `term` are all exclusive the same way), not a mirrored
session where a local operator and the remote peer both drive the shell
simultaneously. Like `tape_load_net`, it keeps answering
`CHAT_TAG_DISCOVER_REQUEST` throughout -- both while waiting for a `HELLO`
and during an active session -- so simply leaving `term net` running is
enough to make a host discoverable.

`tools/term_net.py` is the host-side client, reusing
`tools/send_tape.py`'s discovery/MAC-resolution/raw-`AF_PACKET`-socket
helpers rather than duplicating them (`import send_tape` and call its
`parse_mac`/`format_mac`/`discover`/`pad`/`default_interface`/
`interface_mac` directly). Its primary, expected-to-be-most-used mode is
non-interactive request/response: given one or more command lines (as
arguments or via `--stdin`), it sends each, waits for a quiet period on
the wire (`--settle`, default 0.5s) before printing accumulated output and
moving to the next command, then sends `TERM_TAG_BYE` and exits -- chosen
specifically so this tool (and, in turn, this assistant, which drives fos
primarily through the bash tool's call/response model rather than a live
interactive TTY) can script a fos session without needing a real terminal
attached to its own stdin/stdout. `--interactive` is a secondary,
genuinely interactive raw pass-through mode (reads/writes this program's
own stdin/stdout with no local echo, since the remote shell echoes
keystrokes itself) for a human operator's own manual use.

Verified end-to-end against a live QEMU multicast netdev (mirroring
`tapes/test_primes_e2e.py`'s approach; see
`tools/test_term_net_e2e.py`): `term net` accepted a `TERM_TAG_HELLO`,
relayed a couple of ordinary commands' full output (including their
prompts) back over the wire, and cleanly disconnected on `TERM_TAG_BYE`.

A follow-up worth noting for anyone extending `term net` further: since
`tape load net`/`tape run` are dispatched through the exact same
`execute()` as every other command, they already work unmodified from
inside a `term_net` session -- confirmed by `tools/test_term_net_tape_e2e.py`,
which connects over `term net`, remotely runs `tape load net`, has a
*third*, unrelated MAC send the tape (exactly as an independent
`send_tape.py` run elsewhere on the LAN would, with no awareness the
`term_net` session even exists), then remotely runs `tape run`. Both
commands' own blocking loops (`tape_load_net`'s frame-wait loop,
`tape_run`'s call into the tape) nest directly inside `term_net`'s outer
loop, which is what predicts -- and the test asserts -- two consequences
rather than surprises:
- The relay queue only gets drained by `term_net`'s own outer loop, which
  is paused for the entire time a nested command blocks. So every kernel-
  printed line around a tape (`Tape loaded: ...`, `Running N-byte tape...`,
  `Tape returned; back at the fos shell.`) reaches the remote client only
  as one burst once the nested command returns, never live.
- Since the "tape print callback ABI" change described above,
  `primes.S`'s own summary digits print through `console::print_byte`,
  the same combined VGA+serial+relay path every other command's output
  goes through -- so they now DO appear in that same relayed burst
  alongside the wrapper messages, not just on QEMU's raw serial stream
  (or a real machine's physical screen). A remote operator driving
  `tape load net`/`tape run` over `term net` now sees the tape's actual
  output too, just delayed until the nested command returns rather than
  streamed live. (This is a change from this feature's first version,
  when tapes still wrote VGA/COM1 directly and their output truly was
  local-only; `tools/test_term_net_tape_e2e.py` was updated accordingly
  once the callback ABI landed.)
Neither of these needed any code change to `term_net` itself to work
correctly -- they follow directly from `term_net`'s existing
single-threaded, nested-nothing design, combined with whatever a nested
command's own output actually goes through -- but are exactly the kind of
thing worth a regression test given how easy they'd be to silently break.

The one-burst-after-return behavior above has a real client-side
consequence worth calling out on its own: a long silence on the wire
while a slow blocking command runs is completely indistinguishable, from
`term_net.py`'s point of view, from the session having quietly died --
and both of its modes had (in `--interactive`'s case, still cosmetically
has, on purpose) to account for that so a working session doesn't *look*
frozen to an operator watching it.
- `run_noninteractive` originally used the same short `--settle` period
  (default 0.5s) as its *first* wait after sending every command, not
  just for detecting when a burst of output has finished. That's fine
  for quick commands, but a genuinely slow one like `tape run` produces
  nothing at all for as long as it runs -- so the first wait would
  time out, `drain_output` would report nothing received, and the
  client would move on to the next command (or send `TERM_TAG_BYE` and
  exit) having silently missed that command's entire output once it
  finally did arrive. Fixed by giving the *first* wait after each sent
  command its own, far more generous `--command-timeout` (default 30s),
  while `--settle` keeps its original, short-and-tight job of ending
  collection once output has actually started arriving and then goes
  quiet.
- `--interactive` has no analogous correctness bug -- it just keeps
  reading forever, so a slow command was always eventually going to
  produce its output -- but from a human's perspective, several seconds
  of a seemingly stuck cursor right after pressing Enter (for `tape load
  net`/`tape run` in particular) reads exactly like a crash. It now
  tracks the last time a line was submitted (a forwarded chunk
  containing `\r`/`\n`) and, if `STALL_WARNING_SECONDS` (1.5s) pass with
  no reply at all, prints a brief `...` heartbeat to stderr -- stderr
  specifically, so it can never get mixed into the remote shell's own
  terminal content on stdout, kept to three dots rather than a sentence
  so it reads as "still alive", not an alarming wall of text, and fires
  once per submitted line so it doesn't nag once a slow command is
  already known to be running.

### Tape ABI grows alloc/free: using the rest of the machine's RAM

Every tape up to this point was limited to its own fixed 4 MiB region
(`pmem::TAPE_BASE`/`TAPE_LEN`) for both code and data -- fine for a
`static mut` sieve up to a million or so, but no way to work with, say,
a genuinely large in-memory buffer on a machine with a real gigabyte of
RAM behind it. `shell::tape_run`'s ABI grows two more callbacks to fix
that: `TapeEntry` is now `extern "C" fn(putc: extern "C" fn(u32),
should_abort: extern "C" fn() -> u32, alloc: extern "C" fn(u32) -> u32,
free: extern "C" fn(u32, u32))`, with new `shell::tape_alloc`/
`shell::tape_free` functions as the third and fourth arguments. Exactly
like the `should_abort` callback before it, cdecl's caller-pushes/
caller-cleans convention means older tapes (`primes.S`/`hello-rs`,
one callback; `vga-colors`, two) still work unmodified even though
`tape_run` now always pushes four arguments -- the extra pushed bytes are
just stack space an older tape's shorter signature never reads, and
`tape_run` cleans up all four regardless of how many the tape declared.
The one hard rule (see `bigsieve`'s own doc comments): a tape may only
omit a *trailing* run of parameters it doesn't use, never one in the
middle -- a tape that wants `free` (fourth) but not `should_abort`
(second) must still declare `should_abort` in its signature (even if
unused, e.g. `_should_abort`) to keep every later parameter's stack
position aligned.

`alloc`/`free` are backed by a brand new, deliberately separate memory
pool in `kernel/src/pmem.rs`, not the existing frame allocator
`palloc`/`pfree`/disk/NIC code already use. That existing pool (`BITMAP`,
`TRACKED_BYTES` = 128 MiB, 4 KiB-per-frame granularity) was left
completely alone; a first attempt at this feature just grew its bitmap
to cover a full 1 GiB and hit a hard, easy-to-forget architectural wall
instead: at the time, `objcopy -O binary` baked a `.bss` array's *entire*
size into the shipped flat binary as zero bytes (there's no "reserved but
unbacked" concept once a `no_std` freestanding binary is flattened), so a
1 GiB bitmap (32 KiB, up from the original 4 KiB) directly inflated
`kernel.bin` by ~28 KB. **This specific mechanism no longer applies to the
current linker layout** -- see "`audio.rs`: a format-agnostic facade, and
network-fed playback" below, where a much later investigation found that
`.bss` (now `linker.ld`'s last section, with nothing loadable after it)
is *not* baked into `kernel.bin`'s raw file size at all; `entry.S`'s own
explicit zero-fill loop is what actually makes a large `.bss` static safe
at runtime today, and the real enforced limit is `linker.ld`'s
`ASSERT(__kernel_end < 0x60000, ...)`, not the flat file's byte count.
Whatever caused the ~28 KB growth described here was evidently something
other than pure `.bss`-truncation behavior at that point in this
project's history; take the mechanism explained below as the current,
verified one. Stage two now avoids the former single-transfer
127-sector ceiling by splitting kernel loading into bounded INT 13h
extended-read calls that respect each BIOS 64 KiB boundary. The remaining
limit is the reserved `0x10000..0x60000` low-memory region, currently
640 sectors, and `tools/write_layout.py` emits that limit.

The fix: a second allocator, the "big pool" (`BIG_BITMAP`,
`BIG_POOL_BASE` = `TRACKED_BYTES` through `BIG_POOL_END` = 2 GiB,
`BIG_CHUNK_SIZE` = 1 MiB granularity instead of 4 KiB), tracked by its
own tiny ~240-byte bitmap (1920 MiB / 1 MiB / 8) -- negligible compared to
the fine-grained pool's bitmap, so `kernel.bin` barely grows at all. The
coarser 1 MiB granularity costs nothing in practice: a tape asking for
`alloc` almost certainly wants one big contiguous block (a buffer, not
many small ones), and `alloc`/`free` round a request up to whole
`BIG_CHUNK_SIZE` chunks, same as `palloc`'s existing frame-granularity
rounding just at a coarser step. `pmem::initialize()` calls a new
`mark_big_region_free` for each usable E820 region (alongside the
existing `mark_region_free`) and its boot `println` reports the big
pool's tracked/usable MiB alongside the existing frame-pool line. Like
`palloc`/`pfree`, there is no auto-free-on-return: a tape that calls
`alloc` and never calls `free` leaks those chunks for the rest of that
boot, exactly as trusted and exactly as unforgiving as any other
kernel-level allocation in this project.

`tapes/bigsieve/` is the reference tape exercising this: it requests a
64 MiB buffer via `alloc` (sixteen times `pmem::TAPE_LEN`, so it could
never fit in the tape's own fixed region no matter how it was compiled),
zeroes it manually (unlike a `static mut` array, memory handed back by
`alloc` isn't pre-zeroed for you), sieves primes up to 67,108,864, prints
the results, and calls `free` before returning. Its sieve intentionally
does **not** reuse `hello-rs`'s algorithm unchanged: `hello-rs`'s bound
(1,000) is small enough that squaring every candidate all the way up to
the limit never overflows `u32`, but at 64 MiB `candidate * candidate`
would silently wrap around (Rust's `release` profile has overflow checks
off) for any candidate past 65,536, corrupting the sieve with bogus
composite markings far from where they belonged -- caught by this
tape's own e2e test asserting an exact, independently-computed prime
count. The fix is the standard sieve refinement: only compute
`candidate * candidate` while marking composites up to
`sqrt(SIEVE_LIMIT)`, then a separate linear pass counts unmarked bytes
across the full range. Built with `make tape-bigsieve` (producing
`build/tapes/bigsieve.bin`) and verified end to end over the network
with `tapes/test_bigsieve_e2e.py` (run with QEMU given `-m 256`, since
its 64 MiB request only succeeds once the machine actually has more than
128 MiB of RAM for the big pool to serve it from).

### Tape ABI grows again: `read_key` and startup arguments

Every tape so far was a pure "run once, print a summary, maybe loop
until told to give up" program -- none could take input, whether a
one-shot parameter (like an expression for a calculator) or ongoing
interactive keystrokes (like a calculator's own REPL). This surfaced
while sketching a genuinely new tape (a simple calculator) as a test of
the whole Rust-tape workflow itself: the very first design question --
"how does it get an expression to evaluate?" -- had no answer anywhere
in the existing ABI. `should_abort` could detect "give up" but nothing
else; there was no way to pass `tape run` any text, and no way for a
running tape to read a keystroke. Building the calculator was deferred
until this gap was closed, on the theory that the gap itself was the
more valuable finding: exactly the kind of blind alley a less-capable
model (or a person new to this codebase) would otherwise burn a lot of
effort discovering the hard way, mid-tape, with no clean way out.

Two additions close it, both purely additive to the existing four-
argument ABI (see "Tape ABI grows alloc/free" above):

- **Startup arguments.** `tape run` no longer requires an empty
  remainder -- `tape run 2 + 2` now runs the loaded tape exactly as
  before, but passes `"2 + 2"` through as two new trailing arguments,
  `args_ptr: u32`/`args_len: u32` (the address and length of that
  slice). This is deliberately the same one-shot, non-interactive
  parameter passing a Unix program's `argv` provides -- no parsing
  convention (words, quoting, ...) is imposed here at all; a tape
  receiving `args_ptr`/`args_len` gets the raw trailing bytes exactly as
  typed and decides for itself what they mean. The slice points directly
  into the command line `execute`/`LineEditor::feed` already parsed, and
  remains valid for as long as `tape_run`'s synchronous call lasts, so no
  copy is made. Every existing tape ignores both new arguments and is
  completely unaffected, the same cdecl "extra trailing arguments cost
  the callee nothing" guarantee `alloc`/`free`'s own addition relied on.
- **Interactive input.** A new fifth argument, `read_key: extern "C" fn()
  -> u32`, gives a tape actual keystrokes -- not just abort detection --
  from whichever source started it, local keyboard or an active
  `term_net` session, without needing to know or care which. Non-
  blocking, called once per polling iteration exactly like
  `should_abort`: `0` means nothing is waiting yet, `1`
  (`TAPE_EVENT_ABORT`, the exact same "give up" trigger `should_abort`
  reports as `true`) means stop, and `0x100` or above encodes an
  ordinary input byte (`value - 0x100` recovers it). A tape using
  `read_key` should not *also* call `should_abort`: both ultimately
  drain the same two underlying sources (`keyboard::poll_term_event`'s
  scancode queue, and a `term_net` session's own network frames), so
  calling both from the same loop races them for input and
  intermittently drops keystrokes to whichever polls first -- checking
  for `TAPE_EVENT_ABORT` already covers everything `should_abort` would
  have reported anyway.

  Implementing this surfaced one asymmetry between local and remote
  input worth calling out: a single `TERM_TAG_DATA` frame can carry many
  bytes at once (a whole pasted or fast-typed line), but `read_key`
  hands back only one decoded byte per call. `poll_remote_tape_abort`
  (the existing `should_abort`-only helper) never had to care, since it
  only ever looked for one specific byte and safely ignored the rest of
  a frame's payload -- but a general input callback can't discard bytes
  it hasn't been asked to. The new `poll_remote_tape_input` fixes this by
  queuing everything past the first byte of a frame into a small FIFO
  (`TAPE_INPUT_QUEUE`, 256 bytes), drained by later `read_key` calls
  before it ever polls the network again -- the same producer/consumer
  queue shape `console`'s own output-side `RELAY_QUEUE` already uses,
  just for input.

  Backspace (ASCII `8`) gets one more piece of special handling: `read_key`
  calls `console::erase_last_character()` itself before returning that
  event, regardless of whether the keystroke was typed locally or
  arrived over the network. This is the one thing a tape genuinely
  cannot do itself even with the `putc` callback -- plain
  `console::write_byte` does not special-case `0x08` the way
  `erase_last_character` does (rewinding the VGA cursor and emitting the
  `\x08 \x08` sequence a real serial terminal expects) -- so doing it
  here keeps a tape's own line-editing logic as simple as "decrement my
  buffer length," exactly mirroring how the ordinary shell prompt's own
  `LineEditor::feed` already handles backspace.

No tape in this repository uses either addition yet (none existed with
a reason to before this gap was found) -- this section exists to
document the ABI ahead of the calculator tape that motivated it, and any
future tape needing input. `docs/rust-tapes-for-dummies.md`'s tape ABI
section and `tapes/tape-rt/` (see "`tapes/tape-rt/`: a shared runtime,
so far only used by `vga-colors`" above) are the natural places a
`read_key`-based line-reading helper belongs once a tape actually needs
one, built the same way the shell's own `LineEditor` is: ordinary Rust
layered over one narrow, non-blocking primitive, not a second kernel
callback.

### `tapes/calc/`: the reverse-Polish-notation calculator this all was for

The calculator deferred while `read_key`/`args_ptr`/`args_len` above were
being designed now exists: `tapes/calc/`, a genuinely new tape (not a
`docs/rust-tapes-for-dummies.md` teaching-ladder example), depending on
`../tape-rt` the same way `vga-colors` does rather than duplicating
`entry.S`/the panic handler/print helpers a third time.

Reverse Polish notation (RPN) was chosen deliberately, not just as an
arbitrary demo choice: evaluating it needs only a stack and one
left-to-right pass over whitespace-separated tokens (a number pushes, an
operator pops two operands and pushes one result) -- no operator-
precedence table, no parentheses, no lookahead. That simplicity keeps
this tape's own logic (`eval_line`, `next_token`, `parse_i32` in
`tapes/calc/src/main.rs`) squarely focused on demonstrating the new ABI
additions, not on parser design.

It exercises both new additions, independently, exactly as each was
designed to be used:

- **`tape run 3 4 +`** (startup arguments): evaluates `"3 4 +"` once via
  `args_ptr`/`args_len`, prints `= 7`, and returns immediately -- no
  interaction at all.
- **Plain `tape run`** (interactive `read_key`): starts a small REPL,
  reading one line at a time from whichever source started the tape
  (local keyboard or an active `term_net` session, indistinguishably, as
  designed) until told to give up. This is also the first tape to
  demonstrate a *line-based* input pattern built entirely out of the
  kernel's one non-blocking `read_key` primitive, exactly as this
  section above predicted a future tape would: a fixed-size line buffer
  that grows on ordinary printable bytes, shrinks by one on a backspace
  event (the on-screen erase already happened inside `read_key` itself,
  so the tape only tracks its own length), and evaluates and resets on
  `\r`/`\n`. `TAPE_EVENT_ABORT` ends the REPL the same way it would for
  any other `read_key`-based tape.

`tapes/calc/src/main.rs` intentionally never calls `should_abort` --
consistent with this section's own warning, doing so alongside
`read_key` would race both against the same underlying input sources.
Its `EvalError` enum (stack underflow, division/remainder by zero,
`i32` overflow via `checked_add`/`checked_sub`/`checked_mul`/
`checked_div`/`checked_rem`, an unrecognized token, a full stack, or
leftover operands once input is exhausted) reports a friendly message
through `putc` for every failure mode rather than panicking or silently
producing a wrong answer -- deliberately, since a calculator that
crashes on `5 0 /` would defeat the point of a teaching example.

Verified end to end over the network with `tapes/test_calc_e2e.py`
(mirroring `tapes/test_hello_rs_e2e.py`'s one-continuous-`term-net`-
session structure): both startup-argument evaluation (`3 4 +` = `7`) and
an interactive session (`6 7 *` = `42`; `5 0 /` reports division by
zero; `2 2` with a backspace then `3 -` correctly becomes `2 3 -` = `-1`,
proving `read_key`'s backspace handling; a remote Escape byte stops the
REPL cleanly, the same mechanism `tapes/test_vga_colors_e2e.py` already
proves for a `should_abort`-based tape) all pass, and the same
`term_net` session remains responsive to an ordinary command
immediately afterward. Built with `make tape-calc` (producing
`build/tapes/calc.bin`).

### Tape ABI grows kernel-managed networking: `net-chat`

The tape ABI now has three trailing callbacks after `args_ptr`/`args_len`:
`nic_mac: extern "C" fn(u32) -> u32`, `nic_send: extern "C" fn(u32, u32)
-> u32`, and `nic_receive: extern "C" fn(u32, u32) -> u32`. They preserve
the same append-only cdecl rule as every earlier ABI extension: existing
tapes ignore the trailing stack slots unchanged, while a networked tape must
declare every earlier parameter to keep these three positions aligned.

The kernel retains ownership of every initialized NIC, including PCI
discovery, bus-master/DMA allocation, receive-ring accounting, and overflow
recovery. The original callbacks operate on the active interface;
`nic_mac` copies its six-byte address to a tape-owned buffer and returns one
on success; `nic_send` sends a complete tape-owned Ethernet frame;
`nic_receive` copies one complete frame into a tape-owned buffer and returns
its length, or zero when no frame is waiting or the operation cannot proceed.
A tape must call `nic_mac` first and report a zero return rather
than mistaking it for an idle network.

`tapes/net-chat/` is the reference consumer: it uses exactly the shell
`chat` ethertype/tags, supports broadcast and unicast chat plus discovery,
and combines the callbacks with `read_key` so local PS/2 and active
`term net` input both work. Because raw reception and remote `term net`
input share one NIC ring, the kernel preserves non-terminal frames for the
tape and queues terminal frames back for `read_key`; neither side silently
steals the other's traffic. `tapes/test_net_chat_e2e.py` proves the tape
exchanges messages with a second fos instance running the ordinary shell
`chat` command.

The ABI now appends two more callbacks after those raw Ethernet operations:
`nic_max_frame_len: extern "C" fn() -> u32` and
`ticks: extern "C" fn() -> u64`. The first reports the kernel's actual
maximum frame size so a tape library does not duplicate backend constants.
The second exposes the kernel's monotonic PIT tick count for retransmission
and connection deadlines; tapes must not reprogram the timer merely to obtain
protocol time.

Raw frames deferred while the `read_key`/networking callbacks share the
selected NIC receive path now lives behind a four-frame FIFO rather than one slot. A
frame is also retained when a tape polls with a too-small output buffer,
allowing it to retry using `nic_max_frame_len()` instead of silently losing
the packet. The kernel still does not parse ARP, IPv4, UDP, or TCP: those
protocols belong in the fixed-buffer `tapes/tape-net/` library, documented in
`NETWORKING.md`.

DHCPv4 is also protocol-library code, not a kernel service. Its client starts
with the required zero/broadcast IPv4 bootstrap addresses, uses the tape
`ticks` callback for bounded retries and lease renewal/rebind deadlines, and
returns explicit lease configuration to the application. A consuming tape
should expose `tape run dhcp` and a structured fallback such as
`tape run static IP/PREFIX GATEWAY`; dotted netmasks and a separate
`IP /PREFIX` remain accepted; argument parsing and post-configuration
policy belong to that tape, not to `tape-net`.

The ABI now appends five multi-interface callbacks after `ticks`:
`nic_count`, `nic_mac_at`, `nic_send_at`, `nic_receive_at`, and
`nic_set_promiscuous_at`. Their indices are zero-based, unlike the shell's
one-based `nic list`/`nic use N` commands. The final callback toggles
receive-all filtering for one interface; it is required by
`tapes/bridge/`, which must receive frames addressed to hosts behind a
different port. A bridge tape should enable it only for its selected ports
and restore normal filtering before returning.

`tapes/ip-chat/` is the reference application for this split. It uses
ARP/IPv4/UDP from `tape-net` to implement a fixed-buffer line chat, accepts
`static LOCAL_IP NETMASK GATEWAY [PEER_IP]` or `dhcp [PEER_IP]` startup
arguments, and keeps `/to`, `/broadcast`, `/peers`, and `/quit` policy in the
tape. Its focused `tapes/test_ip_chat_e2e.py` test uses two static-IP QEMU
guests; the multicast fixture has no DHCP server, so DHCP is covered by the
library/application unit tests rather than that integration test.

`tapes/telnet/` is a single-connection Telnet client using `tape-net` for
ARP, DHCP, and TCP. Its incremental parser negotiates the basic Telnet verbs
plus an 80x25 `xterm` terminal type and NAWS window size, handles
subnegotiation across TCP boundaries, doubles literal `IAC` bytes, and
forwards UTF-8 as bytes without decoding it in the tape. The kernel's local
VGA callback interprets a bounded VT100 subset and answers `CSI 5n`/`CSI 6n`
using its 1-based software cursor position, while serial and `term net`
retain raw control sequences. Keep authentication, DNS, and
multi-connection support out of scope; the focused
`tapes/test_telnet_e2e.py` test uses an unprivileged multicast QEMU fixture
plus a small host-side protocol peer.

### Live output relaying for `read_key`/`should_abort`-based tapes over `term net`

Building `tapes/calc` (the first genuinely interactive, potentially
long-running tape) surfaced a real usability bug the batching design
described just above never had to face: a remote `term_net` client
driving the calculator's interactive REPL saw *no* output at all --
no banner, no prompt, no echoed input, no evaluated result -- until the
tape finally exited, even though keystrokes typed in the meantime were
clearly reaching the tape (the REPL visibly responded to them on the
*local* screen). This is a stronger version of the batching behavior
documented above for `primes.S`/`tape_load_net`: those are one-shot,
bounded operations where "everything arrives in one burst once the
nested command returns" is a minor, acceptable delay; an interactive
REPL that may run for as long as an operator keeps typing has no
natural "returns soon" to wait for, so batching there meant a remote
session looked completely frozen, not merely a little behind.

The root cause was a real asymmetry between this feature's input and
output sides. `poll_remote_tape_abort`/`poll_remote_tape_input`
(`kernel/src/shell.rs`) already poll the network card directly, on
`term_net`'s behalf, so a nested blocking command's `should_abort`/
`read_key` calls keep working correctly even while `term_net`'s own
outer loop -- the *only* place that used to drain
`console`'s relay queue and forward it via `send_term_data` -- is
completely paused. Nothing equivalent existed for output: a tape's
`putc` calls still reached `console::relay_push` exactly as before, but
that queued data just sat there, undrained, for as long as the nested
command kept running.

The fix is a small, shared helper, `relay_pending_tape_output` (`kernel
/src/shell.rs`), doing exactly what `term_net`'s own loop already does
each iteration -- `console::drain_relay` into a `TERM_DATA_CAPACITY`
buffer, then `send_term_data` if anything came out -- called from both
`poll_remote_tape_abort` and `poll_remote_tape_input` before either
does its own network-card polling. `poll_remote_tape_input` in
particular drains relay output *unconditionally*, even on the fast path
where a byte is already sitting in `TAPE_INPUT_QUEUE` and would
otherwise let the function return immediately without ever reaching the
network-polling code below -- otherwise a quickly-typing remote peer
could keep this function short-circuiting on queued input for several
calls in a row, needlessly delaying whatever the tape had already
printed (e.g. its next prompt). This required no tape ABI change and no
per-tape opt-in: any `read_key`- or `should_abort`-polling tape gets
live output relaying automatically, purely because both polling
functions now happen to also drain output as a side effect.

`tapes/test_calc_e2e.py` was extended with `drain_term_data_for`, which
actually listens on the simulated wire for `TERM_TAG_DATA` frames
addressed to the remote test client (something no earlier `term_net`
e2e test did -- they only ever checked the *local* fos-side QEMU serial
log, which always looked correct regardless of this bug, since local
VGA/serial output was never affected). It asserts the interactive mode's
banner and first prompt reach the wire *before any input is even sent*
(proving they could only have arrived via live relaying, not as a
side effect of the tape returning, since nothing sent yet could have
made it return) and that an evaluated result reaches the wire live too.
Confirmed both ways: reverting the fix and rerunning this test
reproduces exactly the reported bug (the assertion fails, with the
banner/prompt only appearing in a captured buffer after the escape byte
that ends the tape -- i.e. only once `term_net`'s own loop resumes), and
with the fix restored the same test passes.

This does **not** change the batching behavior documented above for
non-polling commands: `tapes/primes.S`'s summary and `tape_load_net`'s
own wrapper messages still arrive in one burst on return, confirmed
unchanged by rerunning `tools/test_term_net_tape_e2e.py` unmodified --
`primes.S` never calls `should_abort`/`read_key` at all (it has no
input, and always runs to completion), so neither polling function
above ever executes during its run, and there is nothing new to drain
on its behalf. The "relay queue only gets drained by `term_net`'s own
outer loop" claim above is now only accurate for a nested command that
never calls `should_abort`/`read_key`; any tape that does gets the
benefit of this fix for free.

### `poll_background_work`: one shared hook instead of four duplicated ones ("Tier 0" of the scheduler discussion)

Investigating what a real preemptive/cooperative scheduler would cost
this project (per-task stacks, a context-switch primitive, and -- the
expensive part -- auditing every "this kernel is single-threaded"
assumption, most critically `disk.rs`'s BIOS real-mode trampoline, which
cannot tolerate any concurrent execution during its brief real-mode
window) surfaced a much cheaper, already-half-built alternative: this
kernel's several long-running blocking loops (`term_net`'s own session
loop, `nic sniff`, `nic discover`, `chat_command`, and the tape-callback
polling functions `poll_remote_tape_abort`/`poll_remote_tape_input`
described just above) already each poll hardware/keyboard state once per
iteration -- so "background work" here doesn't need real concurrency at
all, just a handful of small, cheap, non-blocking checks called from
every one of those loops instead of only some of them. This is
explicitly scoped to kernel-native recurring work only, not to
backgrounding a *tape*: a tape is structured as one big imperative
blocking loop with its state on the stack, and making one of those
(e.g. `tapes/router`) genuinely resumable without a real per-task stack
would mean hand-rewriting it into an explicit coroutine/step function --
harder, not easier, than just giving it a real stack and one `yield()`
call once a full scheduler exists. No tape or tape ABI changed for this
work.

Auditing the four independent places this ad hoc pattern had grown
(`relay_pending_tape_output`/`poll_remote_tape_abort` for `term_net`'s
own relay+network polling, `poll_remote_tape_input`'s near-identical
copy of the same for `read_key`, and `term_net`'s own session loop
inlining the identical relay-drain a third time) also surfaced a real,
previously invisible functional gap: `nic.watchdog()` -- the mechanism
that notices and recovers a latched, documented adapter fault (see
"`nic` status and ordinary foreground NIC traffic run a bounded
watchdog" above) -- was, before this change, only ever invoked from
`nic`/`nic list`/`nic watchdog`'s own command handlers, never from
inside any of these long-running loops. A fault occurring mid-capture,
mid-chat, mid-discovery, or mid-`term_net`-session (including while
still idle waiting for a client to connect) simply sat unrecovered until
that loop happened to return and a human thought to run `nic status`
afterward.

`poll_background_work(nic: Option<&nic::Nic>)` (`shell.rs`) is the one
shared hook consolidating all of this: it drains the print-relay queue
(`relay_pending_print_output`, a no-op unless a printer session is
actually active), relays queued `term_net` output to the connected peer
if this call is running nested inside an active session
(`console::relay_enabled()` plus `ACTIVE_TERM_PEER`/`ACTIVE_TERM_SOURCE`,
exactly the gating `poll_remote_tape_abort`/`poll_remote_tape_input`
already used), and calls `nic.watchdog()`. It is cheap enough to call
unconditionally on every iteration of every loop it was added to:
`watchdog()` is a handful of register/atomic reads unless a fault is
actually latched (see `Rtl8139::watchdog`), and each relay check is a
single peek against an otherwise-empty queue.

It is now called from: `term_net`'s outer "waiting for a `HELLO`" loop
and its inner `'session` loop (replacing that loop's own inlined
relay-drain duplicate); `nic_sniff`'s capture loop; `nic_discover`'s
probe/listen loop; `chat_command`'s input/receive loop; and
`poll_remote_tape_abort`/`poll_remote_tape_input` (replacing their own
bespoke relay-plus-network-poll logic with one call to this same hook,
called unconditionally before either function's own
`console::relay_enabled()`-gated logic runs -- so a tape's watchdog
recovery and print-relaying now also work when the tape was started
*without* an active `term_net` session, which the old, `term_net`-only
gated logic in those two functions never covered). No externally
observable behavior of the tape ABI (`should_abort`/`read_key`
semantics, or any tape's own output) changed; the tape e2e suite
(`tapes/test_calc_e2e.py`, `tools/test_term_net_tape_e2e.py`,
`tapes/test_net_chat_e2e.py`, `tools/test_term_net_e2e.py`) and `make
test` all pass unmodified after this refactor, confirming the
consolidation is behavior-preserving apart from the intended new
watchdog coverage.

### `term net` stays resident after a remote disconnect

Originally, *any* session-ending event -- local F12, a remote
`TERM_TAG_BYE`, or the remote end simply going away -- caused `term_net`
to print a message and `return`, falling all the way through to the local
shell. Real-hardware testing exposed the problem this caused: once the
automatic boot-time `term net` wait's first session ended for any reason,
the machine dropped to the local prompt and stayed there -- a remote
operator had no way back in without a local operator physically retyping
`term net`, defeating the point of an unattended, screen/keyboard-less
machine being reachable by default.

`term_net` was restructured so the entire "wait for `HELLO`, then run a
session" body lives inside an outer `loop`. Each iteration tracks whether
*local* escape (not a remote `TERM_TAG_BYE`) is what ended the session,
via a `local_escape` flag set only in that one code path. Local escape
still sends `TERM_TAG_BYE` to the peer and `return`s out of the whole
function, exactly as before -- this is the only way to reach the local
shell now, whether at boot or from a manually typed `term net`. Every
other way a session can end (a remote `TERM_TAG_BYE`, or a fresh `HELLO`
implicitly superseding a stale one) instead prints
`"Remote terminal disconnected; waiting for a new one to connect ..."`
and loops back to the top of the outer `loop`, re-entering the
"waiting for a `HELLO`" state with no local action required. Both call
sites (`run`'s automatic boot-time call, and `term_command`'s manually
typed `"term net"` dispatch) share this one function, so the fix applies
uniformly to both.

One consequence worth flagging: since a serial-only operator has no way
to trigger "local escape" (there is deliberately no serial equivalent,
same limitation covered below), the old workaround this project's own
tests and a serial-only human operator both relied on -- have any peer
connect and then disconnect, which used to fall through to the local
prompt -- no longer works, since that disconnect now just re-arms the
wait instead. A serial-only machine's only ways back to a local prompt
are now driving it entirely over `term net` itself, or briefly attaching
a local keyboard to press one of the "give up" keys once. The affected
test scripts (`tapes/test_primes_e2e.py`,
`tapes/test_hello_rs_e2e.py`, `tapes/test_primes_vga_e2e.py`,
`tools/test_term_net_tape_e2e.py`) were all updated to drive their
commands over one continuous `term net` session (via `TERM_TAG_DATA`
frames) instead of relying on that trick -- console output always mirrors
to local VGA/serial regardless of relay state (`console::write_bytes`
writes both unconditionally), so their existing `read_until()` assertions
against QEMU's local serial stream kept working unchanged.

### `term net` at boot, and more "give up" function keys

`shell::run` now calls `term_net` once, automatically, before it ever
enters the ordinary interactive prompt loop -- so every fos machine waits
for a remote terminal client by default at every boot, with no manual
step required. This matters most for a machine with no attached
screen/keyboard: it is reachable over the network the moment it finishes
booting. `term_net` already tolerated "no NIC" gracefully (it just prints
a message and returns via `nic_or_report`), so this needed no change to
`term_net` itself, only one new call site in `run`. A local operator
watching a real screen who wants the ordinary prompt immediately, without
waiting, presses one of the "give up" keys below; if a remote client
connects first instead, it does *not* fall through to the local shell on
disconnect (see "`term net` stays resident after a remote disconnect"
above) -- only local escape does that, whether during this automatic wait
or a manually invoked `term net` session.

There is deliberately no serial equivalent of "give up" for this
automatic wait (mirrored from `term_net`'s own pre-existing convention):
reading one byte here to mean "give up" would silently eat the first
character of whatever a serial operator was about to type. A serial-only
operator can no longer fall back on "have any peer connect and then
disconnect" either, now that `term_net` re-arms itself instead of
returning on a remote disconnect -- see the note above about this
project's own e2e tests (`tapes/test_primes_e2e.py`, `tools/
tapes/test_primes_vga_e2e.py`, `tapes/test_hello_rs_e2e.py`, and
`tools/test_term_net_tape_e2e.py`) being updated to drive their commands
over one continuous `term net` session instead of relying on that trick.

Separately, `keyboard::ESCAPE_SCANCODES` (`kernel/src/keyboard.rs`) grew
from a single F12 scancode to six: F12, F4, F6, F7, F8, and F10. F12 was
always a deliberate choice for being a function key (same physical
position/scancode on every keyboard layout this project targets,
unreachable through Shift/AltGr, so nothing about the current locale
matters) rather than an ordinary printable character that would need
distinguishing from real typed text. The problem in practice: some
terminal emulators, KVMs, and remote-desktop clients intercept F12
themselves before it ever reaches a guest OS, so a fos operator using one
of those had no working "give up" key at all. Recognizing several
function keys instead -- whichever one actually reaches the guest works,
with nothing to remember or configure -- fixes this without weakening the
original rationale for using a function key in the first place. F1/F3/
F5/F9/F11 were deliberately left out since some terminals bind those to
their own emulator-level features (new tab, fullscreen, etc.); F12 stays
first in the list and in every user-facing message since it remains the
most commonly documented, most muscle-memory choice. F2 was later given
its own distinct meaning -- see "A physical printer port" below -- rather
than being added to this "give up" list, since it needs to mean something
narrower ("abandon the line I'm typing without executing it," only inside
`term_printer`) than every other key here.


### A physical printer port

`kernel/src/parallel.rs` is a new, deliberately narrow legacy-ISA
parallel/Centronics port driver, discovered via the BIOS Data Area
(`0x408`-`0x40E`, LPT1..LPT4) the same way `console::detect_ports` finds
COM1..COM4 -- there is no PCI parallel-card discovery to mirror
`console::detect_pci_ports`, matching this project's existing "ISA only,
PCI parallel cards out of scope" convention already established for the
3C509 investigation. It implements only the original IBM PC "SPP"
(Standard Parallel Port) register set -- Data/Status/Control at
`base+0`/`+1`/`+2` -- with no bidirectional/EPP/ECP modes. `Printer::
write_byte` writes a byte, pulses Strobe, and polls the Busy status bit
for up to `BUSY_POLL_ATTEMPTS` (200,000) reads before giving up and
reporting a `PrinterFault` (`NotSelected`/`PaperOut`/`ErrorLine`/`Busy`)
rather than blocking forever -- the same bounded-poll-then-caller-retries
shape every other polling loop in this kernel already follows.

**Status/control bit polarity is an explicit, documented real-hardware
caveat**, exactly the same class of risk as the RTL8139's TCR
interframe-gap field and CR/RCR/TCR write-order surprises (see "Real
hardware notes" below): several Centronics signals (`nBusy`, `nAck`,
`nError`) are active-low on the physical cable, and different references
disagree on exactly how an ISA bus buffer presents them back to software.
The polarities implemented follow the most commonly cited convention
(matching Linux's historic `parport_pc`/`lp` driver headers) but have
only been checked against QEMU's `-parallel` emulation, not a real
Centronics cable -- `kernel/src/parallel.rs`'s own module doc comment
flags this prominently. The user's physical Panasonic KX-P1083 dot-matrix
printer is the intended real-hardware validation target; until that
happens, treat the fault classification (which bit means what) as
unconfirmed.

**Real-hardware follow-up: a false "Printer out of paper" report, resolved
by a BIOS parallel-port fix.** The user's KX-P1083 reported "out of
paper" with paper loaded, the printer online, and its own out-of-paper
indicator light off. A first theory -- that the printer's DIP-switch-
selected emulation mode (IBM Proprinter vs. Standard/Epson-compatible)
changes how the Paper-Out *status line* is driven -- turned out to be the
wrong lead on reflection and has been retracted: emulation mode governs
how a printer's firmware interprets incoming escape codes for text
formatting, not how its hardware drives the Centronics status signals
back to the host, which come directly from a physical paper sensor.

Rather than guess again, `printer` was extended to print each detected
port's raw status byte plus a per-bit decode
(`status=0xNN: busy=B paper_out=P select=S error=E`), so the exact
real-hardware register value could be read directly instead of inferred.
Real hardware showed `status=0xff` -- literally every bit set, including
ones that should reflect a genuine "ready, selected, no error" printer.
That is the signature of a status register not backed by real hardware at
all, not a single mis-wired polarity bit in this driver. The actual cause
was a dead CMOS battery that had disabled/reset the BIOS parallel port
configuration entirely; setting it back to base `0x378` + Normal (SPP)
mode (not ECP, which repurposes the register layout this driver assumes,
and not Bi-Directional, which is unneeded since this driver never reads
the Data port back) resolved it -- confirmed by the printer working
correctly afterward. This driver's status-bit polarity constants remain
otherwise unconfirmed against real hardware, since this particular bug
turned out to be a hardware/BIOS configuration issue, not a software one.

`shell.rs` adds `term printer`/`term LPT1`..`LPT4` (an exclusive terminal
mode, mirroring `term COM1`/`term_net`'s existing "one active mode at a
time" convention) and a `print` command pair (`print [diskN] NAME`,
`print run CMD...`), on top of two new pieces of shared plumbing:

- **`keyboard.rs` gained a sixth recognized key, F2 (`KILL_LINE_SCANCODE`
  = `0x3c`, `KeyEvent::KillLine`)**, chosen for the same reason F12 was
  originally chosen (a function key is layout-independent, unlike a
  Shift/AltGr-modified character a Nordic keyboard might put somewhere
  unexpected) -- this avoided adding Ctrl-modifier tracking to the
  keyboard driver, which does not exist at all today. It means "discard
  the line I'm typing" only inside `term_printer`'s own typing loop;
  every other blocking loop in this kernel (`term_loop`,
  `combined_tape_read_key`, `nic sniff`, `nic discover`, `tape_load_net`,
  `chat_command`) either already has a catch-all match arm or was given
  one, so adding this variant needed no behavior change anywhere else.
- **`console.rs` gained a second, fully independent relay queue**
  (`PRINT_RELAY_QUEUE`/`enable_print_relay`/`disable_print_relay`/
  `print_relay_active`/`peek_print_relay`/`discard_print_relay`),
  structurally identical to the existing network relay queue
  (peek/discard rather than drain, so a fault mid-print never silently
  loses a byte that was already "read" from the queue but not yet
  accepted by the printer) and fed from the exact same single tap point,
  `relay_push` (called from `write_serial`) -- generalized to feed both
  queues unconditionally, so any command's ordinary `println!`/`print!`
  output is captured for the printer automatically, with no per-call-site
  instrumentation needed. `print_relay_push_raw` bypasses this tap
  entirely (queuing raw bytes with no VGA echo of its own); it remains in
  use only by `print_file`, which queues a whole file's raw contents this
  way rather than routing them through `println!`.

**Typed input is echoed live, teletype style, not buffered until
Enter.** Unlike the original design (buffer locally, echo to VGA only,
and queue nothing for the printer until Enter commits the line),
`term_printer` now writes every typed character to the physical printer
the instant it is typed -- via a new `shell::write_to_printer_with_retry`/
`print_bytes_live` pair, not the print-relay queue -- exactly mirroring
`term_net`'s own live, byte-at-a-time echo, just to a printer instead of a
network peer. This was a deliberate reversal of the earlier design once
real-hardware testing showed the buffered approach didn't read as a
teletype at all: a real teletype/telex terminal always inks a character
the instant a key is struck, with no "not yet committed" state. F2
(`KeyEvent::KillLine`) can therefore no longer act as a genuine undo --
already-inked characters cannot be un-printed -- so it instead just
abandons the local input buffer (clearing the on-screen echo the same way
as before) and prints a fresh `"\r\n" + "fos> "` break to the paper,
mirroring how a real teletype operator would simply start a fresh line
after a visible mistake rather than pretend it never happened. Backspace
remains screen/buffer-only and is still never itself sent to the printer,
so backspacing over an already-printed character leaves that character
visible on the physical paper -- an intentional, documented limitation of
live echo, not a bug. Enter still triggers the same enable-queue-execute-
drain-disable cycle as before for a committed command's own *output*
(unchanged, since that output is only known once the command actually
runs) -- only the typed input side changed. One `last_fault:
Option<PrinterFault>` variable is now held for an entire `term_printer`
session and threaded through both the live per-character writes and
`drain_print_relay` (whose signature grew a matching `&mut
Option<PrinterFault>` parameter), so an ongoing fault is reported once
across the whole session rather than being re-announced the moment
output-draining begins right after a fault was already being reported
during typing.

**`help` refuses to run while `PRINTER_SESSION_ACTIVE`** (a small flag
set only for the duration of a `term_printer` session's own `execute`
call), to avoid a full-screen listing burning a whole sheet of paper by
habit -- the user should consult the manual instead for routine reference.
`print run help` is a deliberate, always-available exception: `print_run`
temporarily clears `PRINTER_SESSION_ACTIVE` around its own nested
`execute` call and restores it afterward, so a genuine one-off paper copy
of `help` works even when typed from inside an already-active
`term_printer` session, not only from the ordinary local prompt.

**`print run CMD...`/`print [diskN] NAME` are nesting-aware.** Both were
originally written assuming they always own the full
enable-queue-drain-disable cycle themselves, which is correct when either
is typed at the ordinary local prompt -- but when typed as a command
*from inside* an already-active `term_printer` session, that session's
own committed-line flow has already called `enable_print_relay` and
queued that line's own transcript before `execute` ever reaches
`print_command`. Naively calling `enable_print_relay` again there resets
the queue (discarding the transcript still waiting to be drained), and
draining/disabling independently ends the outer session's print job
early -- splitting what should be one physical print job (the transcript,
plus this command's own "Running ...""/output/"Done." messages) into two.
`console::print_relay_active()` is checked first specifically to detect
this nesting: when already active, `print_file`/`print_run` only queue
their own bytes (the file's raw contents, or the command's captured
output) and leave every bit of enabling, draining, and disabling to the
outer `term_printer` loop, so it all lands on one continuous piece of
paper as one job. This was caught by an interactive QEMU-monitor-driven
manual test (`sendkey`-based, mirroring `tapes/test_vga_colors_e2e.py`'s
technique) exercising `term printer` -> `print run diskinfo` end to end
against QEMU's `-parallel file:PATH` emulation, which showed the bug
directly: only `diskinfo`'s own two output lines reached the captured
file, with the surrounding transcript and wrapper messages silently
missing, before this fix.

No automated `make test`-style coverage exists yet for this feature (the
QEMU `-parallel file:PATH` interactive check above was ad hoc, not a
committed test script) -- a `make test-printer`-style smoke test using
that same QEMU flag remains a reasonable follow-up, with the same
limitation every such test would have: it can prove the happy-path byte
sequence but not the real Busy/Select/Error handshake or fault recovery,
which only a real Centronics cable can exercise. Real-hardware validation
against the user's Panasonic KX-P1083 is the outstanding confirmation
step before this feature's fault/polarity assumptions can be considered
settled, consistent with this project's established pattern for
hardware-facing features (see "Real hardware notes" below).

**Real-hardware follow-up: dot-matrix printers cannot show true
character-by-character teletype output.** With the port fixed, live
per-character echo (above) was confirmed to genuinely reach the printer
one byte at a time -- but a moving-print-head dot-matrix printer (the
user's KX-P1083) buffers a full line internally and only physically inks
it once flushed by CR/LF/FF or an internal buffer limit, so typed
characters visibly appear only once the line commits, not as each key is
struck. This is inherent to the printer's own mechanism, not something
any universal SPP/Centronics command can change; true live
character-at-a-time inking needs a genuine teletype/TTY or a
daisy-wheel/typewriter-style printer instead. No code change was made or
is possible here.

**Real-hardware follow-up: an interactive tape's own output was batched
until it exited.** `print run tape run` against an interactive,
`read_key`/`should_abort`-polling tape (e.g. `tapes/calc`'s REPL) showed
no printed output at all until the tape returned, even though its
banner/prompts/results were clearly being produced (and echoed to VGA)
throughout. This is the exact same class of bug already fixed for
`term_net` (see "Live output relaying for `read_key`/`should_abort`-based
tapes over `term net`" above): the print-relay queue was only ever
drained by the outer `print_run`/`term_printer` code, which is paused for
as long as a nested tape's own blocking call is running -- there was no
equivalent of `relay_pending_tape_output` for the *print* relay queue.

The fix follows that fix's pattern exactly. Two new statics,
`ACTIVE_PRINT_BASE`/`ACTIVE_PRINT_FAULT` (`shell.rs`), carry the active
printer's port and ongoing fault state across the call-stack boundary
into a nested tape's own polling callbacks, the same reason
`ACTIVE_TERM_PEER`/`ACTIVE_TERM_SOURCE` exist for `term_net`. A new
`relay_pending_print_output()` drains whatever the print-relay queue
currently holds to that printer -- but, unlike `drain_print_relay` (used
once a command finishes, where blocking retry-until-clear is the correct
"pause, don't abort" behavior), it never blocks on a fault: it writes
what it can, stops the instant a fault is hit (reporting it once via
`ACTIVE_PRINT_FAULT`, exactly like every other fault-reporting path in
this feature), and leaves the remainder queued for the next poll --
blocking here would freeze the *tape* itself, not just its printer
output. It is called from both `combined_tape_should_abort` and
`poll_remote_tape_input`, so any `read_key`- or `should_abort`-polling
tape gets live print-relaying automatically, with no per-tape opt-in,
mirroring exactly how the `term_net` equivalent fix works.

`term_printer` and `print_run` (when `owns_relay` is true, i.e. not
itself nested inside an already-active `term_printer` session) now set
`ACTIVE_PRINT_BASE`/reset `ACTIVE_PRINT_FAULT` before running a command
and clear both afterward, and use a fresh, transient `&mut` into
`ACTIVE_PRINT_FAULT` (via a new `active_print_fault_mut()` helper using
`core::ptr::addr_of_mut!`, never a reference held across the nested
`execute()` call itself) for their own direct
`print_bytes_live`/`drain_print_relay` calls, rather than each keeping a
separate local `last_fault` variable -- so a fault surfaced live during a
running tape is not redundantly re-announced the moment the tape returns
and ordinary post-`execute()` draining resumes. `print_file` (which never
calls `execute()`, so no nested tape can run during its own work) keeps
its own local `last_fault` unchanged, since no cross-call-stack sharing
is needed there. Verified with a QEMU-monitor-driven manual test
(`-parallel file:PATH`, `sendkey`-based, in the same spirit as the
nesting-aware fix's own manual test above): `print run tape run` against
`tapes/calc` showed its `= 42` result already present in the captured
print file before F12 ended the tape, confirming the fix (reverting it
reproduces the batched-until-exit symptom exactly).


## Real hardware notes

QEMU is this project's primary validation environment, but a few behaviors
have only ever surfaced on physical machines; both are recorded here so a
future contributor doesn't have to rediscover them from scratch:

- **VGA cursor and boot narration** -- writing to the `0xb8000` text
  framebuffer does not move the blinking hardware cursor by itself; the
  kernel drives the CRTC cursor-position ports (`0x3d4`/`0x3d5`) explicitly
  after every write, and continues from wherever the real-mode loader's own
  BIOS-teletype printing left the cursor rather than clearing the screen at
  boot, both confirmed necessary on a real Pentium 4 machine booted from
  USB (see `README.md`'s "Prompt" section).
- **A20 enable order** -- `enable_a20` (`boot/stage2.S`, duplicated in
  `boot/netrom.S`) used to only write the "fast A20 gate" system control
  port (`0x92`). That worked fine in QEMU, and this reordering is a real,
  worthwhile robustness fix regardless, but it turned out *not* to be the
  cause of the AMD Athlon 64 X2 reset loop below -- trying the BIOS
  (`INT 15h`, `AX=2401h`) and the 8042 keyboard-controller gate before
  falling back to port `0x92` (re-verifying A20 after every attempt rather
  than trusting any method's own reported success) is still worth keeping,
  since some real chipsets have side effects on writes to that port beyond
  toggling A20 that a pure emulator won't reproduce -- it just wasn't
  what was resetting *this particular* machine.
- **PIC/PIT command timing** -- `program_pic`/`program_pit`
  (`kernel/src/interrupts.rs`) used to send their ICW/OCW command bytes to
  the 8259/PIT back-to-back. QEMU's emulated 8259 accepts that fine, but a
  real chipset can be slower to latch one command byte before the next
  arrives, silently leaving the PIC mis-programmed (there's no error
  status to check). The fix inserts a short `io_wait()` (a throwaway write
  to port `0x80`) after every PIC/PIT command byte, the standard technique
  for this exact class of bug. This did not turn out to be the cause of
  the AMD Athlon 64 X2 reset loop below (see the spurious-IRQ entry), but
  is still worth keeping as a general robustness fix.
- **Protected-mode IRQ window during BIOS disk calls** -- `disk::transfer`
  (`kernel/src/disk.rs`) must keep interrupts disabled after loading the
  real-mode IVT and BIOS PIC mapping, until `boot/stage2.S`'s
  `bios_disk_real` has actually switched the CPU to real mode. An earlier
  version executed `sti` while the processor was still in protected mode
  with `IDTR=0`; any timer or keyboard IRQ in that window interpreted the
  IVT's 4-byte far pointers as protected-mode IDT gates and triple-faulted
  back to reset. This was especially reproducible on the AMD machine during
  boot and during the many-sector disk load of `primes.bin`. The trampoline
  now enables interrupts only after entering real mode, where BIOS's timer
  tick and disk-timeout logic can run safely, and executes `cli` before
  returning to protected mode.
- **CPU exception handlers as a diagnostic tool** -- added alongside the
  PIC/PIT fix above, since that fix couldn't be confirmed without the
  original hardware: all 32 CPU exception vectors (`kernel/src/interrupts.rs`,
  `install_exception_handlers`) now have real IDT gates instead of being
  left `missing()`. Each prints the vector number, its name, the
  CPU-pushed error code if the vector has one, and the faulting `CS:EIP`,
  then halts. Verified in QEMU by deliberately executing `int3` right
  after `interrupts::initialize()` and confirming the diagnostic printed
  and the machine halted cleanly instead of resetting (then reverting
  that temporary trigger). This is exactly what let the AMD Athlon 64 X2
  reset loop below get diagnosed instead of staying a silent reset.
- **Spurious IRQ7/IRQ15 -- confirmed cause of the AMD Athlon 64 X2 reset
  loop** -- with the exception handlers above in place, the real hardware
  that had been resetting instead printed
  `CPU EXCEPTION 13 (General Protection Fault), error code 0x0000013b, at
  0008:0001218f` right after the "Physical memory:" line. The error code
  decodes (bit0 EXT=1, bit1 IDT=1, bits3-15 index = `0x13b >> 3` = `0x27`)
  to "IDT gate 0x27 is not a valid gate descriptor" -- vector `0x27` is
  `IRQ_BASE + 7`, i.e. IRQ7 on the master 8259. This kernel only ever
  unmasks IRQ0/IRQ1, and nothing is wired to IRQ7, but the 8259 hardware
  itself uses IRQ7 (and IRQ15 on the slave) as its own "spurious
  interrupt" signal: a device can assert then withdraw its interrupt
  request before the PIC latches it (electrical noise, a glitchy
  peripheral, ...), and the 8259 reports that as IRQ7/IRQ15 regardless of
  masking. QEMU's idealized 8259 essentially never generates one, so this
  never surfaced there. Because `IdtEntry::missing()` zeroes the
  `type_attributes` byte -- an invalid gate type, not just "not present"
  -- the CPU's own gate-type check (which runs before the present-bit
  check) raised `#GP` rather than `#NP`, and with no `#GP` handler
  previously installed, that cascaded into the double-/triple-fault reset
  loop. The fix gives IRQ7 and IRQ15 real IDT gates
  (`spurious_master_interrupt`/`spurious_slave_interrupt`) that read the
  relevant PIC's in-service register before deciding whether to send an
  EOI at all, per the standard spurious-IRQ protocol -- a genuine EOI-less
  spurious IRQ7 is now a normal, silent, roughly-never-noticed event
  instead of a fatal exception. `irqstat` reports how often it happens.
  **Confirmed fix for the originally reported bug.**
- **RTL8139 transmit interframe gap** -- `Rtl8139::init` (`kernel/src/rtl8139.rs`)
  used to program `TCR` (Transmit Configuration Register) as all zeros,
  leaving the Interframe Gap Time field (bits 25:24) at its minimum. This
  looked fine by every signal the driver itself checks (`nic send` always
  reported success, since the card's `TOK` status bit only reflects its
  own FIFO/abort state, not whether anything downstream accepted the
  frame), and QEMU's virtual NIC/hub happily ignored the too-short gap
  too -- but on real hardware, no other machine ever received anything a
  real fos host sent, including another fos host's own `nic sniff`, even
  though that same fos host's `nic sniff` correctly captured ordinary
  external traffic (confirming the receive path, RCR config, and ring
  buffer parsing were all fine). This is a well-known RTL8139 gotcha --
  Linux's historic `8139too.c` driver forces the same field to the
  standard IEEE 802.3 gap for the same reason. Fixed by writing
  `TCR_STANDARD_IFG` (`0b11 << 24`) instead of `0`. Found via real
  hardware bisection across three physical machines after ruling out the
  PIC/IRQ fixes above as the cause (the pre-AMD-work baseline commit had
  the identical bug -- it just had never been tested host-to-host on real
  NICs before). **Confirmed correct in principle (matches Linux's own
  constant/comment) but did not, on its own, resolve the reported
  symptom** -- see the next entry.
- **RTL8139 CR/RCR/TCR write order** -- still investigating the same
  "nothing a real fos host sends is ever received by anything" report
  after the interframe-gap fix above. Comparing against Linux's reference
  `rtl8139_hw_start` (`8139too.c`) turned up an explicit comment: "Must
  enable Tx/Rx before setting transfer thresholds!" -- it writes `CR`
  (`CmdRxEnb`/`CmdTxEnb`) *before* `RxConfig`/`TxConfig`, not after. This
  driver's `init` did the opposite: it wrote `RCR`/`TCR` (which carry
  FIFO/DMA threshold fields alongside their other bits) while Tx/Rx were
  still disabled, then enabled them afterward. QEMU's emulated RTL8139
  tolerates either order, which is why this was never caught by any
  QEMU-based testing this session. Fixed by moving the `CR` write (enable
  Tx/Rx) before the `RCR`/`TCR` writes. **Not yet confirmed against real
  hardware** -- if the AMD/P4/dev-host trio still can't exchange frames
  after this, the next thing to check is the exact hex bytes an external
  `tcpdump`/Wireshark capture sees leaving a fos host during `nic send`
  (a capture that's never actually been attempted yet in this
  investigation), since that would show directly whether the frame
  reaching the wire is well-formed, garbled, or simply never arrives at
  all -- narrowing between "chip mis-programmed" and "frame data itself
  is wrong" far more precisely than blind register-level guessing can.
- **RTL8139 receive ring could wedge permanently on one bad frame** --
  found while diagnosing "`tape load net` prints its `Receiving a
  N-byte tape from...` line and then never receives another frame again
  (and `nic discover` stops finding anyone either, in the same session)"
  on real hardware. `Rtl8139::receive` reads a 4-byte header out of the
  ring for the frame at the current read offset; if the card's own
  "receive OK" status bit was clear (a real CRC/alignment/runt error --
  much more likely to actually occur under a fast, back-to-back burst of
  frames like `tape`'s DATA chunks than the occasional single `chat`/
  `nic discover` frame), the old code returned `None` *without* advancing
  `rx_offset`/`CAPR` past that entry at all. Every later call then read
  the exact same bad header again, forever -- silently, with no error
  printed, since returning `None` is also the ordinary "nothing waiting
  right now" result. Because the ring offset lives in the one persistent
  `Rtl8139` instance rather than being reset per-command, one single bad
  frame during *any* network command permanently wedges receiving for
  the rest of the session, explaining why an unrelated command
  (`nic discover`) also stopped working once this happened. QEMU's
  virtual NIC essentially never reports a receive error, so this never
  surfaced in any QEMU-based testing. Fixed by always advancing past the
  ring entry using its header's length field whenever that length itself
  looks sane, regardless of the "receive OK" bit, and only skipping the
  copy-out (still returning `None` to the caller) when the frame itself
  was bad -- resynchronizing a *nonsensical* length field is still out of
  scope for this teaching driver (there is no sane byte count to advance
  past), but a bad-but-plausible header must never be left blocking the
  ring forever. **This fix alone was not sufficient** -- see the next
  entries for the actual root cause of the reported stall.
- **`tape load net` never answered discovery probes** -- found alongside
  the ring-wedge investigation above: a fos machine sitting in
  `tape load net` never replied to `CHAT_TAG_DISCOVER_REQUEST`, unlike
  `nic discover`/`chat`, because `tape_load_net`'s receive loop
  (`kernel/src/shell.rs`) only matched the `TAPE_TAG_*` tags and silently
  ignored everything else. This forced `tools/send_tape.py` to always be
  given an explicit `--mac` while a receiver waited in `tape load net`.
  Fixed by adding a `CHAT_TAG_DISCOVER_REQUEST` match arm mirroring
  `handle_incoming_chat_frame`'s (`remember_peer` + a unicast
  `CHAT_TAG_DISCOVER_REPLY`), so this host stays discoverable the whole
  time it waits for a tape, not just before/after a transfer.
- **RTL8139 Rx buffer/FIFO overflow -- the actual cause of the "stuck
  after `Receiving a N-byte tape from...`" reports** -- confirmed by
  reproducing against real hardware directly from a sandboxed dev
  environment that turned out to share the same LAN (raw `AF_PACKET`
  sockets need `CAP_NET_RAW`; granted via `setcap` on a dedicated
  `python3` copy rather than system-wide, see `tools/send_tape.py`'s
  usage). Sending `tapes/primes.bin` (~1 MB, 978 chunks) with
  `send_tape.py`'s default (no `--delay`) reliably reproduced the stall;
  the same tape with `--delay 0.003` (3 ms between chunks) reliably
  succeeded -- isolating the cause to the receive ring overflowing under
  an unpaced burst, something a real NIC sending at its own wire speed
  can do far more easily than QEMU's virtual NIC ever does (QEMU's own
  end-to-end test for this same tape, `tapes/test_primes_e2e.py`,
  needed the identical 3 ms pacing to avoid "Gap in tape data" failures
  even inside emulation). `Rtl8139::receive` never checked `ISR`'s
  `RXOVW`/`FOVW` bits at all, so once the ring overflowed, its read/write
  pointers were left desynchronized with nothing to resynchronize them.
  Three things changed in `kernel/src/rtl8139.rs` to address this:
  1. `RCR`'s Rx Buffer Length field (`RBLEN`, bits 12:11) was left at its
     reset default, the smallest ring size (8 KiB). Grown to 32 KiB
     (`RCR_RBLEN_32K`) -- **not** the largest option, 64 KiB: that was
     tried first and confirmed, in QEMU testing, to actually corrupt
     frame data, because at 64 KiB the ring already spans `CAPR`'s entire
     16-bit addressable range, leaving `RCR_CONFIG`'s `WRAP` bit (which
     works by letting a packet spill a little past the ring's nominal end
     into reserved slack space) with no well-defined place left to spill
     to. A bigger ring only reduces, and does not eliminate, how often an
     unpaced sender can overflow it.
  2. Added overflow detection and recovery (`recover_from_overflow`,
     called at the top of every `receive`): the first attempt -- disable
     `CR_RE`, reset `CAPR`/`rx_offset`, rewrite `RBSTART`/`RCR`, re-enable
     `CR_RE` -- looked sufficient in QEMU but, on real hardware, left the
     card completely unable to receive anything at all afterwards, in a
     way that outlasted leaving and re-entering `tape load net` and
     needed a full reboot to clear. Recovery was changed to a full
     software reset (the same `CR_RST` sequence `init` uses, factored out
     as `soft_reset`/`program_registers` so both share it) instead of a
     lighter register-level fixup; confirmed on real hardware that a
     paced (`--delay 0.003`) send now succeeds immediately after an
     unpaced send has overflowed the ring, with no reboot in between.
  3. `tape_load_net` now prints a periodic (~2s) progress line (bytes
     received so far vs. the declared total, plus
     `Rtl8139::overflow_count`) while a transfer is in flight, so a
     transfer that is still making progress is visibly distinguishable
     from one that has stalled, and a nonzero/growing overflow count
     points straight at this class of problem instead of leaving it a
     silent, undiagnosable hang.

  **Confirmed on real hardware**: an unpaced `send_tape.py` run against
  the fixed driver no longer hangs forever -- it now either completes, or
  cleanly aborts with a reported "Gap in tape data" once frames are
  actually lost to an overflow the bigger ring couldn't absorb, since
  this protocol has no retransmission (see `tape_load_net`'s own doc
  comment) -- and a paced (`--delay 0.003`, matching
  `test_primes_e2e.py`'s own pacing) run reliably succeeds, including
  immediately after a prior unpaced run has triggered the overflow path,
  without needing a reboot in between. **A polling-only driver with no
  interrupt and no hardware descriptor ring beyond one 32 KiB buffer
  fundamentally cannot keep up with a full wire-speed, completely unpaced
  burst from a real host NIC** -- this is an intentional teaching
  boundary (see "Deliberately deferred work" below), not a bug still to
  be chased further; pacing sends of any nontrivial tape remains expected
  practice, exactly as `send_tape.py`'s own `--delay` flag already
  documented before this investigation. `send_tape.py`'s `--delay`
  default was later changed from 0 to 0.003 (matching every value
  confirmed reliable above), since real hardware needs it essentially
  always, not just for unusually large tapes -- passing `--delay 0`
  explicitly still opts back into the old unpaced behavior.
- **RTL8139 receive ring could also wedge permanently on a nonsensical
  length field, with no recovery trigger at all -- suspected cause of a
  router NIC never rejoining fail-over after a cable reconnect** -- found
  while diagnosing a real-hardware report against `tapes/router`: after
  pulling and reconnecting one uplink's cable, the router's health check
  correctly failed over to the other uplink, but the reconnected NIC
  never recovered even after being physically reconnected for well over
  a minute -- no ARP reply from it was ever recognized again, so it never
  rejoined the fail-over rotation, and it also then failed a fresh DHCP
  attempt outright once it became the router's only uplink (after the
  other cable was pulled to test the reverse direction). The router's own
  health-check logic (`start_health_round`/`expire_health_timeouts`/
  `advance_uplink_health`, `tapes/router/src/main.rs`) was reviewed and
  confirmed to probe every uplink every cycle regardless of its current
  active/healthy status, using consistent tick units throughout -- so
  this is not a router-logic bug. `Rtl8139::receive` reads a 4-byte
  header out of the ring at the current read offset; the entry
  immediately above this one already fixed the case where the "receive
  OK" status bit is clear, but a *separate* branch -- reached when the
  header's own length field is nonsensical (`frame_len < 4` or greater
  than the ring size) -- still just returned `None` with nothing
  advanced and no fault flagged, exactly the same silent-forever-stuck
  shape the "receive OK" bug had. A link down/up cycle plausibly deposits
  link-training noise into the ring at its current offset, landing
  exactly there. Fixed the same way the two entries above already fixed
  their own trigger conditions: this branch now sets `recovery_pending`
  before returning `None`, so the very next `receive()` call's
  unconditional `recover()` performs the existing, already real-hardware-
  proven full soft-reset (not a lighter register fixup, per the overflow
  entry above) instead of leaving the ring wedged. **This fix alone did
  not resolve the reported symptom** -- see the next entry for the
  confirmed root cause.
- **RTL8139 never resumes receiving after a link recovery on its own --
  confirmed root cause of the router fail-back bug** -- real-hardware
  testing isolated the entry above's fix as insufficient by process of
  elimination: an all-3C905 router setup fails over *and back* correctly
  on a cable pull/replug, but swapping NIC 1 for an RTL8139 reproduces
  the original symptom exactly (fails over fine, never rejoins after the
  cable is reconnected, needs a reboot). The ring-wedge fix above only
  helps once garbled bytes actually land in the ring; with the link
  genuinely down, nothing lands there at all, so that fix's trigger
  condition never fires during a real cable pull. This driver never read
  the card's Media Status Register (`MSR`, offset `0x58`) at all, so it
  had no way to even notice a link recovery happened, let alone react to
  one. Linux's historic `8139too.c` documents the identical requirement:
  it polls this exact same `LinkB` bit and performs a full re-init
  (`rtl8139_hw_start`) on a down-to-up transition, rather than trusting
  the card to resume Rx by itself -- this chip does not. Fixed by adding
  `check_link_recovery`, called from both `receive` (the router/tape
  polling path) and `watchdog` (the shell's own path): it reads `MSR`
  each call, and on a down-to-up transition flags `recovery_pending` the
  same way the ring-wedge and overflow/TX-failure conditions already do,
  so the very next call performs the existing, already real-hardware-
  proven full soft-reset. **Pending final real-hardware confirmation**
  from the user repeating the cable-pull/reconnect sequence against the
  rebuilt image with an RTL8139 as the affected uplink.
- **`reboot soft` (see "reboot: a soft-reboot path..." above): not yet
  tested on real hardware, and several of its assumptions have never been
  exercised on real silicon before** -- every prior BIOS/A20/PIC code path
  in this project always ran exactly once per genuine power-on; a soft
  reboot re-enters `boot/stage2.S`'s `_start` without ever asserting the
  CPU's hardware RESET line, so this is the first time any of the
  following gets asked to run a *second* time against the same,
  never-power-cycled chipset state. None of this is confirmed broken --
  it is a predicted risk list to check first if `reboot soft` behaves
  differently on real hardware than in QEMU, the same way the historical
  A20/PIC/spurious-IRQ entries above were themselves predictable in
  hindsight from reading the code closely enough:
  - **A20 enabled a second time, with no reset in between.** `enable_a20`
    tries BIOS (`INT 15h`, `AX=2401h`), then the 8042 keyboard controller,
    then port `0x92`, re-verifying after each. Every one of those three
    mechanisms has now run once already, earlier in the very same power-on
    session -- toggling port `0x92`'s A20 gate bit (or pulsing the 8042's
    output port) a second time on real silicon it already succeeded on
    once is a genuinely new scenario for this codebase, not merely a
    repeat of a previously-tested path. If `reboot soft` fails specifically
    at "Enabling the A20 line..." on real hardware, suspect a chipset that
    tolerates this fine once per boot but not twice, and consider whether
    the *first* mechanism to succeed should be remembered and retried
    first on a soft reboot, rather than probing all three again from
    scratch.
  - **The kernel's own 100 Hz PIT is still running throughout the
    trampoline, not restored to BIOS's conventional ~18.2 Hz rate.** This
    is not new to soft reboot -- every ordinary post-boot BIOS disk call
    (`disk::call_trampoline`) already runs with the same faster PIT rate
    for the same reason -- but `load_kernel`'s BIOS EDD reads inside
    `soft_reboot_real`'s re-entered `_start` are the first place this
    project depends on BIOS's own disk-timeout logic (which polls the BDA
    tick counter IRQ0 advances) working correctly *immediately at boot*,
    on potentially slower or flakier real USB boot media, rather than
    mid-session against an already-proven-working disk path. A real BIOS
    whose timeout logic reads that counter as advancing ~5.5x faster than
    it expects could time out a disk read prematurely in a way QEMU's own
    BIOS emulation may not reproduce.
  - **Sibling bugs of the `%bp`-vs-`%ebp` E820 bug, elsewhere in
    `stage2.S`/`netrom.S`/`disk.rs`.** That bug existed for a long time
    unnoticed because a cold BIOS POST happens to leave a register's upper
    bits zero by luck; a soft reboot is the first code path that leaves
    genuinely different (Rust-kernel-derived) garbage in registers a cold
    boot never would. Any other "clear only the low 16 bits of a 32-bit
    register, then later compare/store the full 32 bits" pattern in this
    real-mode code is a plausible sibling, worth a deliberate audit pass
    rather than waiting to be tripped over the same way this one was.
  - **NIC/PCI device state is never power-cycled** (`reboot soft` never
    asserts PCIRST#, unlike a real hardware reset). A device's own
    re-`init()` should fully reprogram it regardless of prior state, but
    an interrupt latched, or a DMA transfer in flight, at the exact
    instant of the jump has never been exercised on real silicon this way
    before; if a NIC ever comes up in a stuck or half-configured state
    specifically after a soft reboot (but not after `reboot hard`), start
    here.
  - **Real BIOS/USB-boot-media tolerance of a second, POST-less round of
    EDD disk calls.** Every BIOS disk call this project has ever made
    before now happened exactly once per genuine power-on cycle (BIOS's
    own POST always ran first). `reboot soft` is the first time BIOS's
    disk services get invoked cold a second time without an intervening
    reset -- plausible territory for a real BIOS/USB-controller
    combination to hold some one-time-per-power-on internal state that
    does not expect this, even though nothing in this project's own code
    changes between the two calls.
  - Minor/cosmetic, not expected to cause a real failure: a stray PS/2
    scancode or a partially transmitted UART byte from the very keystroke
    that submitted `reboot soft` is not drained before the jump, so it
    could resurface oddly in the freshly-booted kernel's input queue.

If real hardware ever produces a boot loop, a screen that fails to update,
or output that only appears over one of VGA/serial, suspect this class of
gap first -- QEMU's emulated devices are close to real hardware but not a
perfect model of the more idiosyncratic chipset-level behavior above.

### Talking to a real fos machine from a sandboxed dev environment

`tools/send_tape.py`, `tools/term_net.py`, and `tools/send_frame.py` all
need raw `AF_PACKET` sockets, which normally require `CAP_NET_RAW` --
in practice, running as root. A sandboxed environment (such as this
assistant's own bash tool) is unlikely to have interactive/passwordless
`sudo`, and running an agent as root for an entire session is not
something to reach for just to send one Ethernet frame. The workaround
used throughout this project's real-hardware verification instead grants
the capability to one dedicated binary, not system-wide:

```sh
cp "$(command -v python3)" ~/.local/bin/python3-netraw
sudo setcap cap_net_raw+eip ~/.local/bin/python3-netraw
```

The `setcap` command itself needs a real, interactive `sudo` prompt, so a
human has to run it (or at least approve it) once -- an agent should
never attempt this silently on the user's behalf. After that one-time
setup, verify it stuck with `getcap ~/.local/bin/python3-netraw`, then
invoke any of the scripts above through that binary instead of the
system `python3`:

```sh
~/.local/bin/python3-netraw tools/send_tape.py --iface wlp2s0 \
    --discover-timeout 5 build/tapes/hello-rs.bin
~/.local/bin/python3-netraw tools/term_net.py --iface wlp2s0 \
    --mac 00:20:18:89:4a:d6 "tape status" "tape run"
```

Nothing about the copied binary's capability leaks into the ordinary
`python3` on `PATH` or into any other process -- `cap_net_raw+eip` is
scoped to that one file's inode, not the user, so this cannot be used to
raw-socket from any other script or shell command without going through
`python3-netraw` explicitly.

Two things specific to this kind of environment tripped this up in
practice, worth checking first if a raw-socket script mysteriously can't
find the target machine:

- **Interface selection.** `send_tape.py`'s `default_interface()` picks
  the first "up" interface in alphabetical order; a sandboxed dev box can
  easily have an unrelated VPN-style interface (alphabetically ordered
  before the real LAN NIC) that a real fos machine is obviously never
  reachable through. Always pass `--iface <name>` explicitly (e.g.
  `--iface wlp2s0`) rather than relying on the default when more than one
  interface is up.
- **`send_tape.py`'s `--delay` default.** As covered above, real hardware
  needs pacing between chunks (`--delay 0.003` is the current default);
  without it, a real fos machine's polling-only RTL8139 driver can
  silently overflow its receive ring on a large tape sent from a fast,
  unpaced sender.

### A numbered peer address book for `send_tape.py`/`term_net.py`

Both tools already broadcast the same `CHAT_TAG_DISCOVER_REQUEST` probe
`nic discover` sends and pick the destination automatically when exactly
one fos machine replies. With more than one on the wire, though, each
was previously a dead end: print every MAC found and ask for `--mac`
spelled out in full -- every single run, since a short-lived Python
process has nowhere of its own to remember "peer 2 was that RTL8139 at
`52:54:00:...`" the way the fos kernel's own `PEERS` table
(`kernel/src/shell.rs`) does for a whole boot session, letting `chat`/
`tape send` accept a small `PEER_NUMBER` instead.

`send_tape.py` now keeps a small on-disk address book mirroring that
table -- `load_known_peers`/`save_known_peers`/`remember_known_peer`/
`resolve_peer`/`print_known_peers`, all reused by `term_net.py` (which
already imports `send_tape` for its wire-format helpers) and the new
`tools/peers.py`. It's a JSON file, by default `~/.cache/fos_peers.json`
(`PEERS_CACHE_FILE`, overridable via the `FOS_PEERS_FILE` environment
variable so tests can point it somewhere disposable instead of a real
user's cache): every discovery either tool performs merges newly seen
peers into it in first-seen order, exactly like `remember_peer`, and
`--mac` on either tool now accepts either a full MAC address or a
1-based index into this list, exactly like `chat`/`tape send` accept
`PEER_MAC|PEER_NUMBER`.

`tools/peers.py` is the standalone equivalent of `nic peers`/
`nic discover`: no arguments just prints the cached list (no network
traffic); `--discover` broadcasts one probe, merges replies in, and
prints the resulting list; `--forget` empties it. The one real
difference from the kernel's own table is persistence itself: the
kernel's starts empty every reboot, so a stale entry there is
self-limiting, while this one survives indefinitely across separate
tool invocations and can go stale (a MAC that answered yesterday might
be a different machine, or offline, today) -- hence `--forget`, so a
person notices and resets it deliberately rather than either tool
trying to guess when an entry has gone stale on its own.

### `tapes/longmode.S` verified on real hardware

Confirmed working on two real machines, not just QEMU: a Pentium 4 (no
x86-64 support at all) correctly took the graceful CPUID early-return
path and printed "CPUID reports no long-mode support on this CPU;
nothing to do" without touching CR0/CR3/CR4/EFER/GDTR/EFLAGS, and an AMD
Athlon 64 X2 (the same machine behind the A20/PIC/PIT/spurious-IRQ
entries above) completed the full round trip -- reaching 64-bit mode,
writing its proof value to VGA, and returning cleanly to a still-
responsive shell. Real hardware is exactly where this tape's "restore
everything, `cli` for the whole excursion" discipline matters most: a
real CPU, unlike QEMU, offers no forgiving default behavior for a
half-restored GDT or a stray interrupt firing against an invalid IDT
mid-transition, so this is a meaningfully stronger confirmation than the
QEMU-only (`qemu-system-x86_64` vs `qemu-system-i386`) testing described
in AGENTS.md's "A tape that visits 64-bit long mode and comes back"
section above.

### Hostname-based NIC discovery, and an always-on ambient discovery loop

Every peer this project's discovery mechanisms (`nic discover`/`nic
peers`, `chat`'s own discovery, `tools/peers.py`'s address book) tracked
was previously identified only by MAC address -- functional, but not
memorable across a LAN with more than a couple of fos machines on it.
`kernel/src/shell.rs` now loads an optional `hostname` file the same way
`locate_start_file` looks for `start`, but checking *every* attached
disk rather than stopping at the first one with a FOSFS volume:
`load_hostname` tries `disk1` first, then every secondary disk in order
(`0..disk::secondary_count()`), since a hostname is just as likely to
live on a small config disk as a boot-time script is, and any error
short of "found a valid, differently-sized file" is folded into "no
hostname here," mirroring `locate_start_file`'s own `disk2`-is-commonly-
blank reasoning. The file's contents are trimmed of trailing whitespace
(`trim_trailing_whitespace`, distinct from the wire-payload
`trim_trailing_zero` below) and capped at `HOSTNAME_CAPACITY` (16 bytes);
an oversized file is reported and ignored rather than silently
truncated. A machine with no `hostname` file anywhere behaves exactly as
it always has -- this is purely additive.

Because one machine can have several NICs, and a bare hostname could not
tell two interfaces on the same host apart, `interface_hostname(index,
out)` formats the configured base name with a 1-based interface number
appended (`myhost1`, `myhost2`, ...) -- consistent with every other
1-based NIC numbering already in this project's UI (`nic list`/`nic use
N`). This formatted, per-interface name is what actually gets sent and
displayed everywhere below; the bare on-disk `hostname` file content is
only ever a template for it. `nic list` prints a machine's own
`hostname: NAME` line under each configured interface once a hostname
exists.

**Carrying a hostname over the wire.** `send_chat_control` (the one
function behind every `CHAT_TAG_DISCOVER_REQUEST`/`REPLY` frame this
project sends -- confirmed to have no other caller) grew an
`interface_index` parameter and now appends that interface's formatted
hostname (or nothing, if unconfigured) as a payload right after the
existing tag byte. This is safe on the wire for the same reason
`CHAT_TAG_TEXT`/`TERM_TAG_DATA` payloads already are: neither
`tools/send_tape.py`'s `discover()` nor `tapes/net-chat`'s frame handler
validate an exact/maximum frame length, only the tag byte itself, so an
older sender's shorter (no-payload) discovery frame and a newer
receiver's longer one interoperate unchanged in both directions. The
receiving side trims trailing zero padding (`trim_trailing_zero`,
matching the identical convention already used for `CHAT_TAG_TEXT`) since
Ethernet's 60-byte minimum-frame padding must never be mistaken for part
of the name.

**The peer table now remembers names, not just MACs.** The former
`PEERS: [[u8; 6]; MAX_PEERS]` table became `[Peer; MAX_PEERS]`, `Peer`
holding a MAC plus an optional fixed-size hostname buffer/length.
`remember_peer(mac, hostname)` only *fills in* a hostname for an entry
that does not have one yet -- it never overwrites an already-known
hostname with a later, possibly different one from the same MAC. This
was a deliberate simplicity choice (first-seen name wins for the rest of
that boot), not a live-rename feature; a machine whose `hostname` file
changes mid-session will not update an already-recorded peer's displayed
name until that peer entry is naturally re-created (e.g. after a
reboot). `print_peers`/`nic peers` show `MAC (name)` once a hostname is
known, falling back to a bare MAC otherwise, exactly like before.

**Always-on ambient discovery, not just the dedicated `nic discover`
command.** The original ask was "can discovery just run all the time?",
which this project's single-threaded, no-scheduler, busy-polling
architecture (see "Scheduler" discussion elsewhere in this document)
answers narrowly: there is no background execution at all, only
"whichever loop happens to be running right now can also poll for this."
Every existing long-running command loop (`chat`, `term net`, `nic
discover`, a tape's own `should_abort`/`read_key` polling) already
replied to an incoming discovery probe unconditionally -- those needed no
change beyond the hostname-payload plumbing above. The one genuine gap
was the *idle local prompt loop* in `run()`: it never touched the NIC at
all between commands, so a fos machine sitting at its own prompt (the
most common steady state) was invisible to discovery and could not
receive one either, unless an operator happened to be running `nic
discover`/`chat` at that exact moment.

`poll_background_work()` -- the existing Tier-0 unification point shared
by many short command loops -- was deliberately *not* extended to
auto-broadcast a probe, since doing so would make every one of its many
callers start transmitting extra frames, which was not the ask. Instead,
a new `ambient_discovery_poll()` is called only from the idle prompt
loop, once per iteration. Two independent behaviors, arrived at after
explicit user steering across several iterations of this design:

- **Reply and remember, always, silently.** Every iteration, it drains
  and answers any incoming `CHAT_TAG_DISCOVER_REQUEST`/`REPLY` on every
  interface exactly like `nic_discover`'s own loop does, but with no
  `println!` output at all -- unlike the dedicated command, this runs
  underneath an interactive prompt display and must never corrupt it.
- **Self-announce on a schedule, not on every idle iteration.** The
  first design considered ("broadcast a probe every idle iteration") was
  rejected by the user as too noisy; the next ("only reply, never
  broadcast") was rejected in turn since two machines that both only
  reply and never initiate would never discover each other at all
  without a human running `nic discover` on at least one side. The
  design actually built: broadcast one immediate self-announce the
  first time the idle loop starts (fast convergence right after boot),
  then continue re-announcing every `AMBIENT_BOOT_BROADCAST_TICKS` (~3s,
  the same cadence `nic_discover`'s own rebroadcast already uses) for
  `AMBIENT_BOOT_BURST_TICKS` (~30s) after that first call -- explicitly
  to cover two machines booting around the same time but not perfectly
  in sync, so each one's boot-time burst window has a good chance of
  overlapping the other's -- and settle to a much slower
  `AMBIENT_STEADY_BROADCAST_TICKS` (~60s) afterward, since a machine that
  has been up and idle for a while has little need to keep re-announcing
  aggressively. `nic_discover`'s own command keeps its separate,
  unmodified, faster fixed-interval rebroadcast loop -- the two are
  intentionally not unified, since a human explicitly running `nic
  discover` should still get its existing fast convergence regardless of
  ambient discovery's own slower steady-state cadence.

Verified end to end with a new, dedicated two-guest test,
`tools/test_hostname_discovery_e2e.py` (not part of `make test`,
mirroring `tapes/test_net_chat_e2e.py`'s structure): each guest is built
from its own tiny image carrying a distinct `hostname` file (`alpha`/
`beta`) via `build_image.py --file hostname ...`, both are driven to
their local idle prompt (F12 out of the automatic boot-time `term net`
wait, never running `term net`/`nic discover` themselves), and after a
short wait for a couple of ambient-discovery rounds, `nic peers` on each
side shows the other's formatted name (`(beta1)`/`(alpha1)`) -- proving
the whole chain (hostname file -> wire payload -> peer table -> display)
works purely through the idle loop's own ambient polling, with no
dedicated discovery command ever invoked by either guest.

### NETFS: a simple network filesystem over discovered peers

Once every machine on a LAN segment reliably shows up in `nic peers` by
name, the natural next question was whether a file could be fetched
straight from a peer's own disk without first bouncing it through a
disk that both machines share -- motivated directly by the target use
case `copy hostname1:disk2/file.name disk2`. NETFS is the small,
deliberately unauthenticated protocol this project ended up with, reusing
`CHAT_ETHERTYPE` (the same one `chat`/`nic discover`/`tape`/`term net`
already share) and the same design philosophy as every other protocol in
this codebase: no retransmission, no security, and an honestly documented
accepted risk where corners were cut, rather than hidden complexity to
paper over them.

**Wire protocol.** Ten new tags continue the numbering after
`TERM_TAG_BYE`: `NETFS_TAG_GET_REQUEST`/`GET_START`/`GET_DATA`/
`GET_STOP`/`GET_ERROR` and `PUT_REQUEST`/`PUT_DATA`/`PUT_STOP`/`PUT_ACK`/
`PUT_ERROR`. A GET request carries a one-byte remote disk number plus the
file name; the server replies with a `_START` frame declaring the exact
length and a 16-bit checksum, then a `TAPE_TAG_DATA`-shaped burst of
`_DATA` frames (same sequence-number-and-declared-length discipline as
`tape load net`'s own chunked transfer, for the same reason: a binary
file's own trailing zero bytes must never be mistaken for Ethernet's
60-byte minimum-frame padding), then `_STOP`. A PUT request instead
carries the length and checksum up front (the client already has the
whole file in memory before sending), followed by the same `_DATA`/
`_STOP` shape, and the server replies with `_ACK` or a one-byte
`_ERROR` code. Nine error codes cover not-found, disk-unavailable,
too-large, invalid-name, directory-full, data-full, a checksum mismatch,
a generic disk error, and `BUSY` -- the last one new to this feature: a
peer already mid-PUT rejects a second concurrent `PUT_REQUEST` outright
rather than letting two transfers corrupt the one shared staging buffer
(see below).

**The checksum.** Every transfer computes the standard RFC 1071 16-bit
Internet checksum (`internet_checksum`, the same algorithm
`tapes/tape-net`'s IPv4/UDP/TCP layers already use) over the *entire*
file, verified only once the whole thing has been reassembled -- there is
no partial-frame verification, matching this protocol's "no
retransmission, abort cleanly on any detected problem" philosophy
throughout. GET carries the checksum in `_START`, since the server only
knows it once the file has actually been read from disk; PUT carries it
in `_REQUEST`, since the client already holds the whole file in memory
up front. A mismatch on either side aborts before any write happens (a
GET never installs a corrupt tape or writes a corrupt file locally; a PUT
server never calls `fosfs::write` with data that failed verification).

**Peer references never accept a raw MAC address.** `resolve_peer` (used
by NETFS's `parse_remote_host_token`/`parse_remote_token`) accepts only
an `nic peers` index number or a known hostname -- deliberately not a
colon-separated MAC address, unlike `chat`/`tape send`'s existing peer
resolution, because a MAC's colons would be genuinely ambiguous against
`HOST:diskN`'s own colon. This is a one-line, deliberate scope narrowing
for NETFS specifically, not a change to `chat`/`tape send`'s own
resolution.

**Command syntax**, layered onto every existing command that already
moves a whole file through the shared `pmem::TAPE_BASE`/`TAPE_LEN`
staging buffer:

```
copy HOST[:diskN]/NAME DST_DISK [NEW_NAME]     remote source (NETFS GET)
copy SRC_DISK NAME HOST[:diskN] [NEW_NAME]     remote destination (NETFS PUT)
tape load HOST[:diskN] NAME                    remote source, straight into the tape buffer
tape save HOST[:diskN] NAME                    remote destination, from the loaded tape
tape run @HOST[:diskN]/NAME                    remote source, as a startup-argument file
```

A remote *source* is always written as one `HOST[:diskN]/NAME` token
(the same reasoning `tape run`'s pre-existing `@diskN/NAME` already
established: FOSFS names never contain `/`, so it's an unambiguous
separator), because it needs to carry a file name alongside the host. A
remote *destination* has no file name of its own to combine with -- the
name always comes from the source side (or an explicit `NEW_NAME`) -- so
it's just `HOST[:diskN]`, the same shape as a local `DST_DISK`/`diskN`
selector; `parse_copy_destination`/`parse_save_destination` each try a
local `diskN` selector first, falling back to a peer reference.
`tape run`'s existing `TapeArgumentSource` enum gained a `RemoteFile`
variant fetched into the small stack-resident `file_buffer`
(`TAPE_ARGUMENT_FILE_CAPACITY`, 4096 bytes) that argument already used
-- deliberately *not* `pmem::TAPE_BASE`, since a tape is already resident
there by the time `tape run` parses its arguments, and clobbering it
mid-fetch would corrupt the very tape about to run. `copy`/`tape load`'s
remote-GET call sites, which have no such conflict, pass the full
`pmem::TAPE_BASE`/`TAPE_LEN` region as `netfs_get`'s destination buffer
instead -- `netfs_get` takes that buffer as a caller-supplied slice
specifically so both call sites can share one function with no
special-casing; it learns the real length from the `_START` frame and
errors out before writing anything if it would overflow the caller's
buffer. Attempting `copy HOST1:.../NAME HOST2:...` (two remote endpoints
at once) is explicitly rejected with a message to copy through a local
disk in two steps instead -- NETFS moves a file to or from *this*
machine's own disk, never directly between two other peers.

**Serving is scoped to the idle prompt loop only**, mirroring ambient
discovery's own scoping decision above and for the same reason: this is
the one loop already proven safe to extend with extra, silent background
polling without corrupting an interactive display. `ambient_discovery_poll`
now also recognizes and dispatches the four request-carrying tags
(`GET_REQUEST`/`PUT_REQUEST`/`PUT_DATA`/`PUT_STOP`) to
`serve_netfs_get_request`/`serve_netfs_put_request`/`serve_netfs_put_data`/
`serve_netfs_put_stop` -- unlike routine discovery traffic, NETFS activity
*is* printed, since it is a much rarer, more consequential event than an
ordinary discovery probe/reply. A GET is served synchronously within one
`ambient_discovery_poll()` call (read the file, compute the checksum,
send the whole paced burst) -- an accepted blocking cost, the same
trade-off `tape send` already makes. A PUT is necessarily stateful across
many idle-loop iterations instead (one frame arrives at a time), tracked
in a small `NetfsPutTransfer` held in a `static mut` -- a second,
concurrent `PUT_REQUEST` while one is already in flight is rejected with
`BUSY` rather than silently clobbering it. **A fos machine must be
sitting idle at its own local prompt to serve a NETFS request at all** --
not inside `chat`, `nic discover`, `term net`, or a running tape's own
polling loop, all of which keep working exactly as they did before this
feature and simply do not answer NETFS traffic.

**Accepted shared-buffer collision risk.** NETFS serving reuses the same
`pmem::TAPE_BASE`/`TAPE_LEN` staging buffer that a locally typed
`copy`/`tape load` command already uses. Because a PUT transfer can span
several real seconds across many idle-loop iterations, a local operator
typing a `copy`/`tape load` command on the *serving* machine at the same
moment could corrupt both. This is an explicit, documented limitation of
fos's single-threaded, no-locking design (see `NetfsPutTransfer`'s own
doc comment in `kernel/src/shell.rs`) -- not something this feature
guards against, consistent with how little else in this kernel guards
against concurrent misuse of shared state.

Verified end to end with `tools/test_netfs_e2e.py` (not part of `make
test`, mirroring `test_hostname_discovery_e2e.py`'s two-guest structure,
each guest additionally carrying its own blank second disk and the
prebuilt `calc.bin` tape): a remote GET (`copy HOST:disk2/NAME disk2`), a
remote PUT (`copy disk1 NAME HOST:disk2`), `tape load HOST NAME`, `tape
run @HOST:diskN/NAME` (evaluating `3 4 +` through the `calc` tape to
prove a fetched startup-argument file reaches a real tape), and a clean
`NOT_FOUND` error for a nonexistent remote file all pass.

## Real-time audio streaming: the ES1370 driver

Raised by a real request: could fos support realtime audio streaming
between two dedicated machines, each with its own audio interface card,
talking over plain layer-2 Ethernet? The answer settled on after
discussion is "yes, as a genuine teaching-grade real-time exercise,"
provided two project-shaping decisions are kept: **audio and `router`
never run on the same machine** (this project's "do one thing at a time
and do it well" philosophy, applied literally -- each dedicated machine
gets its own single-purpose image), and this kernel's single-threaded,
polling-only architecture is *not* replaced with a general preemptive
scheduler just for this. A real scheduler was investigated and rejected
for exactly the reasons "Cooperative/preemptive scheduling" above already
gives: the single most disruptive cost (auditing/rewriting every place
that assumes single-threaded execution, especially `disk.rs`'s BIOS
real-mode trampoline) buys nothing once audio and router are guaranteed
never to compete for one CPU. What audio *does* need, and does not yet
have anywhere else in this kernel, is a real ISR that does non-trivial,
time-critical work instead of this project's usual "ack, count, flag,
defer to a foreground poll" convention -- see `kernel/src/es1370.rs`'s own
module doc comment for why that departure is deliberate and scoped
narrowly to this one driver.

**ES1370 was chosen as the first target the same way RTL8139 was chosen
as the first NIC:** it is QEMU's own emulated sound device
(`-device ES1370,audiodev=snd0`), so this driver is testable in CI right
now, without needing physical hardware for every iteration. It is also
directly relevant to real hardware this project expects to run on:
Creative's early "Sound Blaster PCI" cards (SB PCI64/128) are commonly a
rebranded Ensoniq ES1370 or its near-identical successor ES1371 -- a
Sound Blaster Live!/Audigy card is a *different*, unrelated chip
(Creative's own EMU10K1/EMU10K2), not part of this family, so a physical
card's exact identity should always be confirmed with `pci` first, the
same caution this project already applies to every other NIC/serial/
printer card.

Every register offset, bit, and DMA semantic in `kernel/src/es1370.rs`
was cross-checked directly against QEMU's own `hw/audio/es1370.c`
emulation (itself transcribed, with attribution, from Linux's historic
`drivers/sound/es1370.c`), the same rigor this project applies to every
other register-level driver after the RTL8139 lessons above. DAC1 (one
stereo 16-bit playback channel) and ADC (one stereo 16-bit recording
channel) are both implemented -- see "ES1370 ADC/capture support" below
for the ADC half; DAC2, the MIDI UART, and the joystick port are all
left untouched, and this driver does not program the external
AK4531-style mixer chip real hardware also carries (reachable through
the indirect `CODEC` register) -- QEMU's emulated backend has no such
gating, so this gap does not block testing here, but a physical card
may need its master/DAC (and, once a real microphone is attached, its
mic/ADC) volume unmuted through a separate path before audio is actually
audible or captured. All of this is, as of this writing, **unverified
against real hardware** -- expect real-hardware fixups the same way every
other PCI driver here needed them (see "Real hardware notes" above).

The driver programs a small double-buffered DMA ring (two periods of
1024 stereo 16-bit frames each, roughly 23 ms/period at 44.1 kHz) and
lets the hardware loop through both halves continuously once started;
`Es1370::handle_interrupt` -- installed as this device's legacy PCI IRQ
handler via the existing `interrupts::register_pci_irq` facade every
other PCI backend already uses -- refills whichever half just finished
playing each time `STATUS`'s `STAT_DAC1` bit is raised. A bare
`Cell<u32>`/`AtomicU32` pair (`tone_phase`, `interrupt_count`/
`periods_serviced`) is all the state this needs, precisely because the
work done per interrupt is small and bounded by construction (exactly
`PERIOD_FRAMES` samples, never a variable amount), unlike NIC frame
parsing.

### A genuine interrupt storm, not a missing interrupt -- confirmed via QEMU's own `-d int` trace

The first working version of `handle_interrupt` served the very first
period correctly (proven by a `-audiodev wav` capture containing real,
continuous, correctly-shaped test-tone audio) but reported **zero**
interrupts ever having fired, on every subsequent check, via a raw
counter (`interrupts::pci_irq_count`) incremented unconditionally in
this kernel's shared PCI-IRQ dispatch path before any device-specific
handler runs at all. Those two observations flatly contradict each
other under the assumption "no interrupt fired" -- and turned out to
both be true simultaneously, for a much stranger reason.

`STAT_DAC1` is a **level**, not an edge, and -- unlike RTL8139's `ISR`,
which a driver acknowledges by writing back the status bits it just
handled -- `STATUS` on this chip is entirely read-only. QEMU's own
`es1370_write` has no case for it at all. The *only* documented way to
lower an already-set `STAT_DAC1` is the specific transition ES1370's
datasheet (and QEMU's `es1370_maybe_lower_irq`) requires:
`SERIAL_CONTROL`'s `P1INTEN` bit going from 1 to 0. The original driver
never did this, so the first period's genuine, correct interrupt raised
the level and then left it stuck asserted forever. The instant that
handler's `iret` restored `EFLAGS.IF`, the still-asserted legacy PCI
`INTx#` line refired *immediately* -- before any other instruction,
including one that could print a counter or read a keystroke, ever ran
again. This was confirmed directly, not inferred: `qemu-system-i386 -d
int -D logfile` showed the identical faulting `EIP` re-entered over
188,000 times within a couple of seconds -- a genuine interrupt storm
that consumed effectively all CPU time from the moment DAC1 first
activated, starving the very shell command that would have reported a
nonzero count. QEMU's own `-trace enable=es1370_*` output corroborated
this from the device side: a real `irq=1` transfer trace event on
(roughly) every period, immediately followed by a repeating
`es1370_lost_interrupt` trace event -- QEMU's own internal detection
that the device wanted to re-assert an already-asserted level.

The fix, `Es1370::acknowledge_dac1`, toggles `P1INTEN` off then
immediately back on (rather than leaving it permanently disabled) right
after each period's refill -- clearing the level for that period while
keeping the next period's interrupt enabled, exactly mirroring the real
driver convention this chip's own datasheet documents. Confirmed fixed
three ways: the `-d int` storm disappeared entirely (the shell/keyboard/
timer stayed fully responsive throughout a multi-second test), the raw
and per-device interrupt counters agreed and incremented steadily (one
period roughly every 23 ms, matching `PERIOD_FRAMES`/44.1 kHz), and a
fresh WAV capture came back 100% non-zero with no discontinuities,
confirming glitch-free, continuously-refilled playback end to end.

This is a genuinely new failure class for this project's driver work,
worth naming plainly for future debugging: **a level-triggered interrupt
source that the driver never acknowledges does not look like "no
interrupt" from a symptom-only view -- it can look exactly like one**,
because the resulting storm can starve the very code path that would
otherwise reveal it. Every other PCI backend here (`rtl8139.rs`,
`threecom.rs`, `e1000.rs`) already acknowledges its own status register
on every interrupt as a matter of course, so this exact trap could not
recur there; ES1370 is simply the first device in this kernel whose
"how do I clear this" is a register *transition* instead of a
write-back, and that shape is worth checking explicitly for the next
driver that has anything resembling a persistent, level-style status
bit.

### Auditing every `cli`-disabled section in the kernel for audio-safe duration

Once ISR-driven refill was confirmed correct, the next real risk to
glitch-free playback is anything else in this kernel that disables
interrupts (`cli`) for long enough to make the CPU miss a whole DAC1
period (~23 ms at the current `PERIOD_FRAMES`) -- if that happens, the
hardware loops back and replays a stale half of the ring rather than
waiting, audibly. Every runtime `cli` site in `kernel/src/*.rs` (boot-time
`cli`s in `boot/*.S` and `kernel/src/entry.S` all execute before
interrupts are ever enabled, so they cannot affect a running audio
stream and are out of scope here) was reviewed for its worst-case
duration:

- **`disk::call_trampoline` (`kernel/src/disk.rs`) is the one real risk,
  by a wide margin.** Its `cli`-disabled span covers the *entire* BIOS
  `INT 13h` round trip through `boot/stage2.S`'s real-mode trampoline --
  fundamentally unbounded from this kernel's point of view, since it is
  however long real BIOS/firmware takes to complete one genuine disk
  transfer (easily tens of milliseconds on real spinning or USB media,
  dwarfing one 23 ms audio period many times over). Worse, this window's
  IDT is temporarily the *real-mode* IVT with the PIC remapped to
  BIOS-conventional vectors (see `call_trampoline`'s own doc comment) --
  even if `EFLAGS.IF` is set for part of this window, this kernel's own
  ES1370 interrupt gate is not reachable at all while it lasts, so any
  DAC1 interrupt that fires during a BIOS disk call is simply not
  serviced by this driver until the call returns, no matter how the real
  BIOS's own default interrupt stub handles it. A native ATA/AHCI driver
  would remove this entirely but is explicitly deferred elsewhere in this
  document, for unrelated reasons of its own scope. **The actionable
  mitigation is architectural, not a code change**: the dedicated audio
  machine must never perform disk I/O while a stream is active. This is
  exactly what "do one thing at a time and do it well" already implies in
  practice -- the eventual audio-streaming tape (see the project's own
  todo list) must load any configuration it needs *before* entering its
  streaming loop, and never touch FOSFS/disk again once streaming starts.
- **`interrupts::register_pci_irq`'s own `cli`/`sti` pair, `scancode_queue_stats`/`pop_scancode`'s
  (`interrupts.rs`), and `read_u64_atomically`'s** are all a handful of
  register/array reads or one PIC port write apiece -- sub-microsecond,
  negligible next to a 23 ms budget regardless of how many times any of
  them run.
- **`keyboard::set_caps_lock_led` (`kernel/src/keyboard.rs`) is the one
  other real, if much smaller, finding.** It holds `cli` across up to two
  bounded 8042-controller polling loops (`wait_for_input_empty`/
  `wait_for_output`, `100_000` iterations each, the same
  bounded-poll-then-give-up shape `parallel.rs`'s `BUSY_POLL_ATTEMPTS`
  already uses elsewhere) -- normally a handful of microseconds in the
  overwhelmingly common case where the 8042 responds immediately, but
  theoretically up to on the order of 100 ms in the pathological case
  where the controller never responds at all. This is deliberately left
  as a documented, accepted, rare edge case rather than a code change:
  it only triggers on an actual Caps Lock keypress, and this project's
  own single-purpose-machine philosophy already implies a dedicated
  audio-streaming machine is not expected to have an operator typing on
  it interactively while a stream is live.

No code changes were needed for the second and third bullets; the first
is recorded here as a hard design constraint on the eventual
audio-streaming tape rather than a kernel change, consistent with this
project's preference for a small, explained trade-off over speculative
complexity elsewhere in this document (e.g. the floppy `A`/`B` naming
collision, the `DISK2`-named-file collision).

### The `audio` shell command

`audio` (bare, no arguments, mirroring `pci`'s own shape) reports
whether an ES1370 (or compatible) controller was found and its I/O base,
lazily performing PCI discovery/DMA-buffer allocation/IRQ registration
and starting DAC1 playback (currently a fixed ~441 Hz test tone -- see
`Es1370::fill_period`'s own doc comment -- standing in for a real network
audio source until the streaming transport below is wired in) on its
first call. It also prints the raw and DAC1-recognized interrupt counts
(`interrupts::pci_irq_count`/`Es1370::interrupt_stats`), the count of
periods refilled so far, and the raw `STATUS` register value -- the same
diagnostic depth `pci`/`irqstat` already provide, and exactly what made
the interrupt-storm bug above possible to root-cause from a serial-only
test harness with no audio hardware of its own to listen to.

### `audio.rs`: a format-agnostic facade, and network-fed playback

`kernel/src/audio.rs` mirrors `nic.rs`'s own shape deliberately: a small
enum (`Audio`) wrapping every supported backend (currently one,
`Audio::Es1370`), a lazy `initialize()`/`get()` pair, and a stable,
format-agnostic contract both the tape ABI and the `audio` shell command
build on -- so a second codec driver (an AC97 or HD-Audio card, neither
implemented yet) can slot in the same way a second NIC backend already
does, without either caller changing. This split exists specifically
because ES1370 is a genuine *silicon* ceiling, not a driver choice: its
DAC1 rate generator only offers four hardwired rates (5512/11025/22050/
44100 Hz) and its sample-format field only encodes 8- or 16-bit PCM (see
`es1370.rs`'s own register doc comments) -- this project's real Sound
Blaster PCI hardware target shares that ceiling. A future 24-bit/48 kHz
card should not need `audio.rs`, the ring buffer, or the tape ABI's
`audio_submit` callback to change at all; a tape must always ask the
active device its real format (`AudioFormat`, via the ABI's
`audio_sample_rate`/`audio_channels_and_bits` callbacks) and produce
exactly that format -- this kernel does no sample-rate conversion or
bit-depth requantization of its own, the same "small kernel API,
protocol/format logic stays in tape code" split this project already
applies to NIC frame parsing.

The tape ABI grew three more trailing callbacks (append-only, cdecl,
after the existing `nic_*`/`ticks` set): `audio_sample_rate() -> u32` (0
if no device), `audio_channels_and_bits() -> u32` (channels in the low
byte, bits-per-sample in the next byte, 0 if no device -- packed into one
u32 deliberately, to avoid a fourth trailing parameter for what is really
one small format fact), and `audio_submit(data: u32, length: u32) -> u32`
(pushes raw PCM bytes into the network-fed ring, returning how many bytes
were actually accepted -- a short count means "ring full, retry the
remainder shortly," mirroring `nic_send`'s own short-write convention).
Calling `audio_submit` at all permanently switches the card from its
default built-in test tone into network-fed mode (`Es1370`'s
`network_mode` flag) -- there is no way back to the test tone without a
reboot, the same one-way switch `tape_load_net`'s "first sender locks the
attempt" convention uses elsewhere in this project.

**The network-fed ring buffer, and a real sizing pitfall worth
remembering.** `PcmRing` (`audio.rs`) is a fixed-capacity, lock-free,
single-producer/single-consumer byte ring -- exactly the same shape and
safety argument as `console`'s own `RELAY_QUEUE`/`interrupts`'s scancode
queue: monotonically increasing `write_pos`/`read_pos` counters, a
`Release` store of `write_pos` only after every byte is written, and an
`Acquire` load of it before the consumer (the DAC1 ISR's own period
refill, `Es1370::fill_period`) touches any byte it implies is valid.

A first attempt sized `RING_CAPACITY_BYTES` generously against the
*worst* format this project expects to ever support (24-bit stereo at
48 kHz, 384,000 bytes/sec) at 64 KiB -- and that measurably does not fit.
Investigating why turned up two separate, easy-to-conflate effects,
worth spelling out precisely since a future contributor sizing another
large `.bss` array here could otherwise draw the wrong lesson from either
one alone:

- **`objcopy -O binary` does not bake `.bss` into `kernel.bin` at all,
  contrary to an earlier (now corrected) claim in this document's own
  size-budget history above.** `.bss` is `linker.ld`'s last section, with
  nothing loadable after it; `objcopy`'s raw-binary output simply
  truncates right after `.data` and never extends out to `.bss`'s full
  virtual size, regardless of how large any `.bss` static declares itself
  to be. This is *why* `kernel.bin`'s own file size stayed byte-identical
  across many different `RING_CAPACITY_BYTES` values during bisection --
  a red herring that has nothing to do with the array being "dead" or
  "live." `kernel/src/entry.S` is what actually makes a large `.bss`
  array safe at runtime: it zero-fills the whole `__bss_start`..
  `__bss_end` range with an explicit loop before any Rust code runs, the
  same way any freestanding kernel's startup stub must, precisely because
  the loaded image itself never contains those bytes.
- **The real, enforced budget is the link-time `ASSERT(__kernel_end <
  0x60000, ...)` in `linker.ld`**, which *does* depend on `.bss`'s true
  linked size (the address arithmetic that computes `__kernel_end`, not
  the raw file's byte count) and *does* depend on whether `--gc-sections`
  actually keeps a large unreferenced static's section at all. Bisecting
  through the real `make kernel` recipe (not a bare `cargo build
  --release`, which uses different `-Z build-std` flags and gives
  misleading numbers here) found a genuine, reproducible LLVM/linker
  cliff for a `[0u8; N]` initializer this large: `N = 32768` links,
  `N = 40960` does not -- confirmed identical whether `submit`/
  `audio_submit`/`audio_status` are genuinely reachable, live code paths
  or not, so (unlike a first, since-retracted theory) this is not
  actually a dead-code-elimination artifact at all; it is a real codegen
  threshold for how large a zero-initializer LLVM will still fold away
  cheaply before it has to be emitted as a real, full-sized `.bss`
  region. `RING_CAPACITY_BYTES` was set to 16 KiB -- comfortably inside
  that threshold, covering ~93 ms of jitter tolerance at ES1370's current
  44.1 kHz/16-bit/stereo format (176,400 bytes/sec), and ~43 ms at the
  hypothetical 24-bit/48 kHz future ceiling. A future codec driver
  needing more headroom at that higher rate should re-measure this same
  real link budget rather than assuming today's 32 KiB/40 KiB cliff still
  holds once other kernel code has grown or shrunk around it.

`audio_status()` (the `audio` shell command's handler) was rewritten to
go through this facade (`audio::get()`) instead of calling `es1370::get()`
directly, so it keeps working unchanged once a second backend exists, and
now also reports the network-fed ring's current backlog
(`Audio::ring_backlog()`/`Es1370::ring_backlog()`, in bytes out of
`audio::RING_CAPACITY_BYTES`) and underrun count
(`Audio::underrun_count()`) -- useful for telling a healthy, steadily-fed
stream apart from one about to glitch, without waiting for an actual
underrun to happen first.

No dedicated audio-streaming tape exists yet to actually drive
`audio_submit` end to end over the network (see "Deliberately deferred
work" -- this is the next planned increment); this section documents the
kernel-side facade, ABI, and sizing investigation only.

### `tapes/audio-net/`: network-fed playback, and a file-replay stand-in for live capture

`tapes/audio-net/` is the dedicated tape driving `audio_submit` end to
end over the network -- the increment the previous section's closing
note flagged as not yet built. It shares `CHAT_ETHERTYPE` (`0x88b6`) with
`chat`/`nic discover`/`tape`/`term net`/NETFS/KUP, adding three new tags
continuing that numbering: `AUDIO_TAG_START` (`0x40`, declares the
stream's sample rate/channel count/bits-per-sample so a format mismatch
against the local device can be rejected up front rather than played as
noise), `AUDIO_TAG_DATA` (`0x41`, one PCM chunk), and `AUDIO_TAG_STOP`
(`0x42`, ends the stream cleanly). Unlike `CHAT_TAG_TEXT`/`TERM_TAG_DATA`,
which trim trailing zero padding on receipt, `AUDIO_TAG_DATA` carries an
explicit 16-bit length prefix instead: legitimate 16-bit signed PCM
silence is genuinely all-zero bytes, and must never be confused with
Ethernet's own 60-byte minimum-frame padding the way printable text safely
can be. Like `TERM_TAG_DATA` (and unlike `TAPE_TAG_DATA`'s strict,
abort-on-gap sequence numbering), there is deliberately no sequence number
at all: this is a live, ongoing best-effort stream where an occasional
dropped frame should read as a brief audio glitch, not a fatal transfer
error worth aborting over.

The tape has two modes. `tape run` (no arguments) is **listen mode**: it
queries the local device's actual format via `audio_sample_rate()`/
`audio_channels_and_bits()`, prints it,
then waits for an `AUDIO_TAG_START` from any peer -- locking onto the
first sender whose declared format matches (a mismatched stream is
rejected with a clear message, but the tape *keeps waiting* rather than
exiting, so a misconfigured first sender doesn't force a reboot/rerun
just to try a corrected one) -- and forwards every `AUDIO_TAG_DATA`
payload into `audio_submit`. Because `audio_submit` has short-write
semantics (mirroring `tape_nic_send`'s own convention: it returns bytes
actually accepted, which may be less than requested if the ring is
momentarily full), `listen_mode` retries the remainder in a bounded
64-spin loop and only counts genuinely undelivered bytes as dropped once
that bound is exceeded, rather than blocking indefinitely. `tape run send
NAME [PEER_MAC|PEER_NUMBER]` remains a stand-in for live capture: even
though `kernel/src/es1370.rs` now implements real ADC/recording support
(see "ES1370 ADC/capture support" below) and exposes it through the tape
ABI's `tape_audio_capture` callback, this tape has not yet been given a
mode that reads it -- `send` still replays a
pre-recorded, headerless raw PCM file already on FOSFS (read whole via
`fs_file_length`/`fs_read` into an `alloc()`-backed buffer, capped at 16
MiB, since the FOSFS tape callback ABI has no partial/offset read) at a
fixed 44100 Hz/2ch/16-bit format -- this project's real ES1370 ceiling,
not an arbitrary choice -- pacing `AUDIO_TAG_DATA` chunks (1024 bytes
each) against `ticks()`'s 100 Hz PIT count using a computed absolute
target-tick schedule (`start_ticks + offset * PIT_HZ / bytes_per_sec`)
rather than a fixed per-chunk sleep, to avoid cumulative drift over a
long transfer. A genuine live-capture send mode (reading
`tape_audio_capture` in a loop instead of a file) is the natural next
step toward the user's "ADC to DAC, and eventually full-duplex" goal,
but remains unbuilt as of this writing.

Verified end to end with `tapes/test_audio_net_e2e.py` (not part of
`make test`, mirroring every other tape's own `test_*_e2e.py` harness):
a single QEMU guest carries both an RTL8139 (the ordinary multicast
Ethernet segment) and a QEMU-emulated ES1370 captured to a host-side WAV
file via `-audiodev wav,id=snd0,path=...`. The test script itself acts as
the sending peer -- constructing and sending the exact
`AUDIO_TAG_START`/`DATA`/`STOP` wire frames directly, the same protocol a
real `tools/send_audio.py` or the tape's own `send` mode would use --
streams a short synthetic 440 Hz tone into the tape's listen mode, and
confirms both the tape's own byte-count summary (received == sent,
dropped == 0) and, the stronger proof, that the captured WAV file
actually contains real, non-silent audio matching the sent tone (a peak
amplitude comfortably above a silence-noise floor), not just that the
tape's own counters looked right.

### A real bug this test found: shared legacy PCI IRQ lines silently broke a second device's `init()`

Building the test above surfaced a genuine kernel bug, not a test
artifact: the ES1370 device consistently failed to initialize
(`audio_status()` reporting "An audio controller was found but could not
be initialized") the moment an RTL8139 NIC was also attached to the same
QEMU guest. `info pci` at the QEMU monitor showed why: QEMU's i440fx/
PIIX3 IRQ router assigned *both* devices to the same legacy line, IRQ 11
-- normal, expected PCI behavior (a real motherboard's fixed slot wiring
can do the same), not an emulator quirk. `interrupts::register_pci_irq`
(`kernel/src/interrupts.rs`) only ever supported one handler per IRQ
line: a second, different device registering on an already-claimed line
was rejected outright, `es1370::initialize()` treated that rejection as
a fatal `Status::InitFailed`, and the whole device came up unusable --
silently, with nothing about the failure pointing at the real cause.

This was a real, previously-undiscovered gap, not specific to ES1370:
*any* two PCI backends this project has (two NICs, a NIC plus ES1370, a
future I350 alongside anything else) could hit the identical failure the
moment QEMU's (or a real motherboard's) IRQ router happened to assign
them the same line -- which audio-plus-networking, the two device
classes this project's whole current priority is built around, makes a
routine combination to hit, not a rare edge case.

The fix: `PCI_IRQ_HANDLERS` grew from one `Option<IrqHandler>` slot per
line to a small fixed-size array (`MAX_SHARED_HANDLERS` = 4 -- the same
bounded-fixed-array convention this kernel already uses everywhere else,
comfortably covering any realistic sharing scenario this project's small
number of PCI backends can produce). `register_pci_irq` now searches for
an existing matching handler (idempotent re-registration, unchanged),
then an empty slot, and only fails if the line's handler capacity is
genuinely exhausted; `dispatch_pci_irq` now calls every handler
registered on that line instead of at most one. This is safe by
construction, not just by observation: every backend's own
`handle_interrupt()` already checks its own device's status register
before doing anything (`nic.rs`'s documented "ack/count/flag, nothing
else" design, and ES1370's own `handle_interrupt` checks `STAT_DAC1`
the same way), so a handler that isn't the actual interrupt source
simply finds its own status bit clear and no-ops -- calling every
handler on a shared line costs nothing beyond that one harmless check.
`pci_irq_registered` (used by `nic`/`audio`'s own diagnostics to decide
whether to report raw IRQ fire counts) was updated to mean "at least one
handler is registered on this line," preserving its existing meaning for
every caller. Confirmed fixed by rerunning the same reproduction (`info
pci` showing IRQ 11 shared between the RTL8139 and ES1370, `audio`
reporting successful discovery afterward) and by rerunning `make test`,
`make test-nic-watchdog`, `make test-stack-guard`, and
`make test-page-protection` -- all pass unmodified, confirming this is a
targeted fix with no regression to the existing single-handler-per-line
cases those tests already exercise.

### ES1370 ADC/capture support: implemented

The user's stated project goal, once `tapes/audio-net/` above proved the
network-fed playback path works end to end: real microphone input (ADC
-> network -> speaker output, DAC), and eventually full-duplex
(simultaneous capture and playback) as a stretch goal. `kernel/src/
es1370.rs` now implements DAC1 (playback) and ADC (recording) as two
independent, simultaneously running DMA channels sharing one chip; DAC2,
the MIDI UART, and the joystick port remain untouched. Every ADC register
offset, bit, and DMA/interrupt semantic was re-derived from QEMU's own
`hw/audio/es1370.c`, the same source and rigor used for DAC1: ADC's
`FRAMEADR`/`FRAMECNT` registers alias DAC1's own `0x30`/`0x34` offsets,
distinguished only by which page (`PAGE_ADC` = 0x0d vs. `PAGE_DAC` =
0x0c) is selected in `MEM_PAGE` before writing them; `CTRL_ADC_EN =
1<<4`; `CTRL_PCLKDIV` (bits 28:16) is a clock divider shared between ADC
and DAC2, precomputed here as a fixed `CTRL_PCLKDIV_44100` constant for
the driver's one supported rate (`divider = ((1411200 + rate/2)/rate) -
2`); `SCTRL_R1INTEN = 1<<10`/`SCTRL_R1FMT_STEREO16 = 0b11<<4`; and
`STAT_ADC = 1<<0` is an independent, level-triggered status bit distinct
from `STAT_DAC1 = 1<<2` -- both can be set simultaneously on one
interrupt, so `handle_interrupt` checks each unconditionally rather than
treating them as an if/else.

**The one real bug this surfaced, caught before it ever reached real
hardware:** `CONTROL` and `SERIAL_CONTROL` are genuinely *shared*
registers -- `CONTROL`'s `CTRL_PCLKDIV` field and DAC-vs-ADC enable bits,
and `SERIAL_CONTROL`'s per-channel format/interrupt-enable bits, all live
in the same 32-bit register regardless of which channel a given bit
belongs to. The DAC1-only version of `start_dac1`/`acknowledge_dac1`
wrote a fixed, hardcoded bit pattern to each register -- harmless with
only one channel ever touching them, but it would have silently
clobbered whichever channel's bits it didn't intend to set the moment a
second channel existed. Every one of these functions
(`start_dac1`/`start_adc`/`acknowledge_dac1`/`acknowledge_adc`) was
rewritten to read-modify-write instead: read the current value, flip
only the bits that function owns, write it back. Confirmed correct by
booting with both channels enabled and observing DAC1's own period count
keep advancing at its expected rate the entire time ADC is also running
(and vice versa) -- if either write path had clobbered the other
channel's enable/format bits, one channel would have silently stalled.

`audio.rs`'s facade gained a read-direction counterpart to
`audio_submit`: `Audio::capture`/`capture_overrun_count`/
`capture_backlog`/`capture_periods`, backed by a second, independent
`PcmRing` (`CAPTURE_RING`) and a new `PcmRing::pop_upto` (a variable-
length "however much is ready" pop, unlike `pop_into`'s all-or-nothing
shape, mirroring `tape_nic_receive`'s own convention). The tape ABI grew
a matching `tape_audio_capture(out, capacity) -> u32` callback (the
final, append-only `TapeEntry` cdecl argument), and `audio_status()` now
reports the ADC's own period-captured/overrun counts and capture-ring
backlog alongside DAC1's existing diagnostics.

**Design choice: ADC runs unconditionally, exactly like DAC1.** `Es1370::
initialize()` calls `start_adc()` right after `start_dac1()`,
unconditionally -- there is no "only start recording once a tape asks
for it" gate. This mirrors DAC1's own always-on design (silence plays
continuously even with nothing feeding `audio_submit`) for the same
reason: a second, always-running DMA channel costs nothing observable
when unused, and it means `audio_status()`'s ADC diagnostics are always
meaningful immediately after boot, not only after some tape has opted
in. Captured bytes nobody ever reads via `tape_audio_capture` simply
overflow `CAPTURE_RING` and increment its overrun counter once full --
the same harmless "recording into a room nobody is listening to"
behavior a real always-on ADC would have, not a fault condition.

**Reserved low-memory region grew again, to `0x70000`.** ADC's second
DMA capture buffer and its associated code pushed the kernel past the
previous `0x60000` ceiling (see "Low-memory map" for why all four
constants -- `kernel/linker.ld`, `kernel/src/pmem.rs`'s
`RESERVED_BELOW`, `tools/write_layout.py`'s `KERNEL_RESERVED_END`, and
`tools/write_netrom_layout.py`'s `MAX_KERNEL_BYTES` -- must always move
together). One non-obvious wrinkle surfaced while sizing this: a naive
`size`-command `text+data+bss` sum undercounts the real footprint,
because `linker.ld` 4096-byte-aligns the `.text`/`.rodata`/`.data`
section boundaries, adding padding that sum doesn't reflect -- `readelf
-S`'s real final `.bss` end address is the number that actually matters
against the `ASSERT` ceiling. `tools/write_layout.py`'s `SLOT_SECTORS`
(and therefore `tools/build_image.py`'s A/B kernel slot sizing) is
derived automatically from `KERNEL_RESERVED_END`, so growing that one
constant alone was sufficient -- no separate disk-layout constant needed
updating.

**Verified with `tapes/test_audio_capture_e2e.py`** (not part of `make
test`, mirroring every other tape's own harness): using QEMU's `none`
audiodev backend (bidirectional -- it opens both an out and an in voice
-- but silence-only), the test confirms DAC1 and ADC periods both
advance simultaneously and independently (proving the shared-register
read-modify-write fix above actually works, not just compiles), and that
`CAPTURE_RING` -- deliberately never drained in this test, since no tape
calls `tape_audio_capture` -- correctly fills to capacity and then
reports a steadily incrementing overrun count rather than wedging or
corrupting state. **This cannot verify real, non-silent captured audio
content**, unlike the DAC1/`tapes/test_audio_net_e2e.py` test, which can:
QEMU's own `audio/wavaudio.c` (confirmed by reading its source directly)
implements only `HWVoiceOut` in this project's installed QEMU version
(10.2.1) -- there is no `in.path`/file-sourced fake-microphone input
option at all, despite that combination appearing in some online
examples written against other QEMU versions or forks. Verifying actual
captured sample values therefore needs either a host environment with a
real, usable audio input backend (ALSA/PulseAudio/PipeWire, generally
unavailable in a sandboxed dev/CI container) or real hardware, the same
honestly-documented gap this project already accepts for other
QEMU-model-limited features (e.g. the 3C509/I350 real-hardware-only
testing cost documented above).

Full-duplex (a tape both feeding `audio_submit` and reading
`tape_audio_capture` in the same loop) is now mechanically unblocked --
both channels run independently and simultaneously. `tapes/audio-net/`
now has three modes dispatched from its startup argument: bare
`tape run` (listen mode, unchanged), `tape run send NAME [MAC]` (the
pre-recorded-file replay stand-in described above), and
`tape run capture [MAC]` -- the genuine live-capture send mode this
section's closing note originally called for, described next.

### `tape run capture`: genuine live-microphone streaming

`capture_mode` (`tapes/audio-net/src/main.rs`) is the ADC counterpart to
`listen_mode`: it reads the local device's real format via the same
`audio_sample_rate`/`audio_channels_and_bits` callbacks `listen_mode`
already used, sends one `AUDIO_TAG_START` declaring it, then loops
pulling bytes out of `tape_audio_capture` (non-blocking, "however much
is ready" semantics -- see "ES1370 ADC/capture support" above) and
forwarding them as `AUDIO_TAG_DATA` frames via `nic_send`, exactly
mirroring `send_mode`'s own pacing/chunking but sourced from the live
capture ring instead of a FOSFS file. `should_abort()` is checked
unconditionally at the top of every loop iteration, before the
non-blocking capture call, so a remote or local abort is expected to be
noticed within one iteration regardless of whether any audio happens to
be flowing that moment.

Verified correct end to end using QEMU's `none` audiodev backend (no
real audio device at all -- `audio_capture` simply returns 0 bytes most
iterations): streaming starts, `nic_send`s real (silent) PCM frames at
the expected 44.1 kHz/stereo rate, and a remote abort byte sent partway
through is detected and handled in the same call, printing
`"audio-net: capture stopped locally after N byte(s) sent."` and
returning cleanly to the shell prompt -- proving the ABI wiring,
argument parsing, and abort-polling logic here are all correct. See
the next section for what happened -- and did not happen -- when this
same mode was pointed at a genuine, physical microphone on a real host
machine, and why that gap was not chased further.

### Real-microphone testing on a dev machine: QEMU audiodev backend pitfalls (host-environment, not a fos bug)

Answering "can a real microphone on this dev machine feed `tape run
capture`?" surfaced two host-side QEMU audiodev quirks, neither of which
is a fos kernel or tape bug -- both are recorded here since they cost
real time to isolate and would otherwise be rediscovered the hard way by
the next person trying the same thing on a similarly PipeWire-based
Linux dev machine.

**`-audiodev alsa,...` deadlocks completely against a PipeWire-provided
ALSA compatibility layer.** On this machine (PipeWire providing the
system's ALSA plugin, as is now the Linux desktop default), pointing
QEMU's `alsa` audiodev backend at the "default" device -- for either
direction (`in.dev`/`out.dev`) independently -- hung the entire guest
indefinitely: not just the audio subsystem, the whole VM stopped
responding to anything, including plain keypresses, for 120+ seconds
with zero progress. Plain host-side `aplay`/`arecord` against that same
"default" device in the same shell session completed instantly,
confirming the underlying audio pipeline was healthy and the hang was
specific to QEMU's own `alsa` backend implementation, not this machine's
audio configuration. This is a known category of bug: QEMU's `alsa`
backend's own polling/threading assumptions don't fully match the
semantics PipeWire's ALSA-compatibility shim presents, unlike a
genuine, unmediated ALSA driver talking directly to hardware. **Fix**:
use QEMU's native `-audiodev pipewire,id=snd0` backend instead --
property names differ (`in|out.name=<sink/source>`, not `in.dev`/
`out.dev`, which are `alsa`-backend-specific); omitting them entirely
uses PipeWire's own default routing. With `pipewire`, the `audio` shell
command returned instantly and reported correct ES1370 register state.

**The originally reported "`tape run capture` hangs on real audio +
remote abort" was not a real bug at all -- it was two bugs in the ad hoc
test harness's own output-parsing logic.** A first pass genuinely
reproduced what looked like a 40+ second hang and, lacking `strace`/`gdb`
access (see below for why that belief was itself wrong), that was
tentatively attributed to a QEMU/PipeWire lock-ordering deadlock. Once
`strace`/`gdb` were confirmed usable after all (see below), re-running
the exact same scenario -- both a single guest launched directly under
`strace -f`, and the full two-guest demo, several times each -- never
reproduced any hang at all: every remote-abort round trip completed in
under a second. Instead, the demo script itself had two bugs, both
instances of a general class worth watching for in any `term_net`
byte-at-a-time-relayed test harness:
1. A checkpoint-ordering race: one guest's output-buffer length was
   captured as a search starting point *after* a slow round trip on the
   *other* guest, whose output kept streaming into that same buffer the
   whole time in the background -- so the checkpoint ended up placed
   *after* the very text it was meant to search for, and the subsequent
   bounded `read_until` call simply timed out looking in the wrong
   place.
2. A marker-prefix race: a `read_until` marker
   (`b"capture stopped locally"`) that is itself a strict prefix of the
   full line (`"capture stopped locally after N byte(s) sent.\n"`,
   assembled from three separate `print_bytes`/`print_u32`/`print_bytes`
   calls, each relayed one byte at a time over `term_net`) can match
   before the rest of the line has actually arrived, so a later regex
   against the still-partial buffer fails. This is the exact same bug
   class already documented above for the `"audio"` command's own marker
   (fixed there by using `b"Raw STATUS register"` instead of `b"ADC:"`).
   **The general lesson: a `read_until` marker must always be the true
   tail of the line a subsequent regex will parse, never merely an
   early-appearing prefix or substring of it**, or the exact same class
   of intermittent, timing-dependent failure can recur in any future
   `term_net`-driven test.

Both bugs are now fixed in
`tapes/test_audio_net_real_mic_e2e.py`, committed but deliberately kept
outside `make test` -- a manual, environment-dependent demo, since its
"real microphone" half only works with a physical audio input device
actually present and reachable on the host; see below.

**A second, more interesting false positive surfaced once the harness
bugs above were fixed: the demo could report "PASS: real, non-silent
audio flowed" even when the ADC channel had captured zero real bytes.**
`kernel/src/audio.rs`'s DAC1 channel runs its own default idle test tone
continuously and unconditionally regardless of network activity (see
"The `audio` shell command" above) -- so a captured WAV file being
non-silent proves nothing about whether real network-relayed audio ever
reached it; the demo's own before/after `audio` status check (`ADC: 0
periods captured; overruns (ring full): 0` before *and* after an 8-second
capture attempt, while `DAC1` periods kept advancing) revealed that on
this specific sandboxed dev machine, the ADC channel was capturing
*literally nothing at all*, not even digital silence (silence would
still count as nonzero captured bytes, since the ADC produces a steady
sample stream regardless of amplitude). `wpctl status` confirmed the
root cause directly: PipeWire on this host has **no audio Source node
registered at all** -- only a dummy/virtual sink for output -- because
the system is configured to expect a specific USB audio CODEC input
device that is not actually connected in this sandboxed environment.
QEMU's `pipewire` audiodev backend degrades silently in this situation
(no error, just an ADC channel that never produces a single sample),
which is exactly what made the false positive possible. `tapes/test_audio_net_real_mic_e2e.py`
was fixed to assert `bytes_sent > 0` and `bytes_received > 0` from the
tapes' own reported network byte counts before ever trusting the WAV
file's amplitude -- a captured WAV's non-silence is only meaningful
*after* confirming real bytes actually flowed over the network, not on
its own.

**Root cause of the missing PipeWire Source, precisely identified: this
sandbox's SSH session is not the seat0 session.** This dev machine is a
physical laptop, normally accessed by SSH rather than its own screen/
keyboard; after a host power outage, its physical desktop session had
not been logged back into again. `loginctl list-sessions`/
`seat-status seat0` showed `seat0`'s only active session was still
`gdm-greeter` (the login screen itself, class `greeter`, on `tty1`) --
every `akeda` session (including this one) showed `SEAT -`, i.e. not
seat-assigned at all, because an SSH login is never attached to a
physical seat the way a graphical desktop login is. This matters
because the laptop's real built-in microphone device node
(`/dev/snd/pcmC0D0c`, "HDA Intel PCH Headphone Mic") is udev-tagged
`uaccess` -- systemd-logind grants read/write ACL on `uaccess`-tagged
device nodes only to whichever session is the *active* one on a given
seat, confirmed directly: `getfacl /dev/snd/pcmC0D0c` showed an ACL
entry for `gdm-greeter` and none at all for `akeda`. So this SSH
session's own PipeWire user instance (`akeda`'s systemd `--user`
manager) never receives access to the real hardware capture device at
all, regardless of how QEMU's `pipewire` audiodev backend is
configured -- it correctly shows only a dummy/virtual sink and no
Sources, because from logind's point of view `akeda` genuinely has no
seat-granted device access right now. `wpctl status`'s "Default
Configured Devices" entry naming a USB audio CODEC input is just
WirePlumber's cached last-known configuration from whenever someone
last logged into the desktop with that device attached -- it does not
mean the device is currently reachable, and is not itself proof of a
missing microphone (check the live `Audio -> Sources` list instead, as
this section's earlier `wpctl status` output demonstrates).

**Fix**: once someone physically logs into (or is auto-logged into) the
laptop's own desktop session -- making an `akeda` session the *active*
one on `seat0` instead of `gdm-greeter` -- logind will grant that
session's PipeWire instance the same `uaccess` ACL on the real hardware,
and the whole `tapes/test_audio_net_real_mic_e2e.py` pipeline should be re-run at that
point to get the final, genuine non-silent-signal confirmation this
section has been building toward. This is a same-machine, purely local
prerequisite (nothing to do with SSH access, network config, or fos
itself) -- re-run `wpctl status` after logging in and confirm a real
device (not just the dummy sink) appears under `Audio -> Sources`
before re-attempting the demo.

**Practical takeaway**: the tape/kernel-side capture ABI and logic
(`capture_mode`'s `should_abort`/`audio_capture`/`nic_send` loop,
`Es1370`'s ADC DMA channel, remote-abort handling) are all proven
correct -- confirmed with QEMU's `none` backend (silent digital data,
correct byte rate, instant remote-abort response) and now also with
`pipewire` (correct instant remote-abort response; zero hang ever
reproduced across many runs). What remains unverified is a **genuine
non-silent captured signal**, blocked purely by the seat0/logind
device-ACL issue just diagnosed above, not by any fos or QEMU bug --
tracked as the `audio-real-mic-verify` follow-up until someone logs
into the laptop's desktop session in person.

**Corrected: `strace`/`ptrace` access in this sandbox.** An earlier
version of this section claimed this sandbox lacks `CAP_SYS_PTRACE`
entirely and so cannot use `strace`/`gdb` at all -- that claim was
tested directly and found wrong. The real, verified picture: `strace`
uses the `ptrace` system call to intercept and log every syscall a
process makes (e.g. showing a `futex(...)` call it's blocked in, or a
`poll`/`read` waiting on a file descriptor); `gdb` also uses `ptrace`,
to pause a process and inspect its actual call stack/thread state.
**Launching a target process directly under either tool
(`strace -f -o log <command>`, `gdb --args <command>`) works today with
zero privilege changes** -- this is ordinary parent-traces-child
`ptrace`, which Linux's Yama LSM always permits regardless of scope
setting, confirmed by directly running `strace -f -o log
qemu-system-i386 ...` in this environment with no error. Only
*attaching* to an already-running, unrelated process
(`strace -p <pid>` / `gdb -p <pid>`) is blocked here, confirmed via a
live test (`ptrace(PTRACE_SEIZE, ...): Operation not permitted`),
because `/proc/sys/kernel/yama/ptrace_scope` is `1` ("restricted":
ptrace only between a direct parent/child, or if the target opts in via
`PR_SET_PTRACER`) -- a normal, unprivileged-process posture
(`CapEff` empty, `CapBnd` nearly full), not a container-specific
restriction. To also enable attach-mode tracing, in descending order of
convenience: prefer direct-launch tracing when the target process can be
started that way at all (covers the overwhelming majority of cases, as
it did here); `sudo sh -c 'echo 0 >
/proc/sys/kernel/yama/ptrace_scope'` to relax to classic same-uid
ptrace rules (needs interactive `sudo`, resets on reboot); or
`sudo setcap cap_sys_ptrace+eip <binary>` to grant the capability to one
specific tool binary, mirroring this project's existing
`python3-netraw` convention (see "Talking to a real fos machine from a
sandboxed dev environment" above).

### The `audio-real-mic-verify` follow-up, resolved: a genuine non-silent capture, and a QEMU TCG/BQL timing artifact found along the way

The prior section's blocker was purely a seat0/logind device-ACL issue on
one specific laptop, tracked as `audio-real-mic-verify` until someone
could test from a machine with a real, reachable microphone. That
happened on a different physical dev machine ("bengt"): a genuine active
seat0 session (not just SSH) and a real PipeWire `Audio -> Sources` entry
were confirmed present via `wpctl status`, closing out that
prerequisite.

**A real, unrelated bug in the test script itself surfaced first.**
`tapes/test_audio_net_real_mic_e2e.py` resolved `sys.path`/`ROOT`/`IMAGE`
using `Path(__file__).resolve().parent` (pointing at `tapes/`) instead of
`.parent.parent` (the repo root) -- every sibling `test_*_e2e.py` script
uses `.parent.parent` for this exact reason, since the script itself
lives one level below the repo root. This immediately raised
`ModuleNotFoundError: No module named 'send_tape'` before the test could
even start a guest. Fixed by correcting the tools-path insert and
`ROOT`/`IMAGE` construction to use `.parent.parent`, keeping a bare
`.parent` only for importing the sibling `test_primes_e2e` module (which
does live alongside this script in `tapes/`).

**With that fixed, the test ran but reported the mic guest sending 0
captured bytes -- even though the microphone itself was confirmed
genuinely working** (`arecord` captured real, non-silent audio directly
on the host; a standalone diagnostic running the plain `audio` shell
command showed the ES1370 ADC ring filling and overflowing normally while
the guest sat idle, proving the pipewire-backed device model itself
receives real mic input correctly). The bug only appeared once the
`audio-net` tape's actual `capture` mode ran its polling loop -- a code
path `tapes/test_audio_capture_e2e.py` had never exercised, since that
existing regression test only checks the `audio` command's own counters
and never drains the ring via the tape ABI's `tape_audio_capture`
callback.

Isolating it: a diagnostic running `tape run capture` for a fixed 4
seconds showed only 4 raw PCI IRQ fires / 0 recognized ADC periods for
the entire window, versus roughly 275 raw fires / 169 ADC periods over
the same 4 seconds with no tape running at all (an idle shell loop) --
the correct rate for 1024-frame periods at 44.1 kHz (~23 ms/period, ~43/s).
This was exactly, deterministically reproducible across repeated runs,
which argued against ordinary host scheduling noise. A second diagnostic
running `tapes/bigsieve` instead (a tape with a pure CPU-bound loop and
*no* I/O port polling of any kind) reproduced the identical starvation
pattern -- proving this is not specific to `audio-net`'s own
`should_abort`/`nic_send` polling, but a general property of *any* tight,
uninterrupted guest busy loop under QEMU's software (TCG) execution
mode: the continuously-running vCPU thread holds the Big QEMU Lock
almost the entire time, starving the separate host-thread-driven audio
timer callback that the `pipewire` audiodev backend needs serviced on
real wall-clock schedule to pull genuine samples through. **This is a
QEMU/TCG emulation-fidelity limitation of this specific test
methodology, not a fos kernel or tape bug** -- real hardware's audio
interrupt is asserted by silicon regardless of what the CPU happens to
be executing at that instant, so no equivalent starvation is possible on
a real machine.

**Fix confirmed**: adding `-accel kvm -cpu host` (hardware-virtualized
execution instead of interpreted) to the exact same reproduction fully
resolved it with no other change -- 24 raw fires / 15 DAC1 periods / 13
ADC periods captured / 9 overruns over the same 4-second window, and the
tape correctly forwarded all bytes it captured. `tapes/
test_audio_net_real_mic_e2e.py` now auto-detects `/dev/kvm` accessibility
(`kvm_available()`, a plain `os.open`/`os.close` probe) and adds
`-accel kvm -cpu host` to both guests' QEMU invocations when available,
printing a clear warning and falling back to plain TCG (with capture
byte counts expected to read 0, for the reason above, rather than a real
failure) when it is not -- this is the first script in this repository to
request KVM acceleration at all; every other `test_*_e2e.py`/
`qemu_smoke.py` script uses default TCG, since exact real-time device
timing has never mattered for any of those tests before ES1370.

**Final result, `audio-real-mic-verify` now genuinely closed**: with both
fixes applied, the full test passes end to end -- mic -> ES1370 ADC ->
network -> `audio-net` capture/listen tapes -> ES1370 DAC1 -> a captured
WAV file containing real audio with peak amplitude 8000 (of 32767 max)
and RMS 5449.1, clearly a real signal well above a silence noise floor,
not the false-positive "PASS" the earlier section's `bytes_sent == 0`
guard was added specifically to prevent.

### Multi-instance ES1370: more than 2 channels via several physical cards

Raised directly by the user's stated goal: could fos support more than
ES1370's hard 2-channel silicon ceiling (see "Real-time audio streaming:
the ES1370 driver" above) -- 4 or 8 channels, say -- if bandwidth allows?
ES1370's `SERIAL_CONTROL` format field only ever encodes mono/stereo
8-/16-bit PCM (re-confirmed against QEMU's own `hw/audio/es1370.c`), so
more channels from *one* card is not on the table; two options were
weighed instead: combine several independent physical ES1370-family
cards (the user's own box of assorted Sound Blaster PCI cards, each an
independent stereo pair), or build a driver for a genuinely
multi-channel codec family (Intel HDA). The user chose to build the
multi-card path first, deferring HDA to a later session.

**The driver/facade conversion mirrors `nic.rs`/`rtl8139.rs`'s existing
multi-instance pattern exactly, not a new design.** `kernel/src/
es1370.rs`'s singleton `static mut CARD: Option<Es1370>` became
`CARDS: [Option<Es1370>; MAX_INSTANCES]` (`MAX_INSTANCES = 4`, chosen the
same way `rtl8139.rs`'s/`nic.rs`'s own small fixed caps are -- comfortably
above what any one real machine is likely to carry, without an unbounded
or dynamically-sized array this kernel's no-heap design can't support
anyway) plus a `CARD_COUNT`; `initialize()` became `initialize_all() ->
Result<usize, Status>`, walking every PCI match `pci::find_devices`
returns (up to `MAX_INSTANCES`) and calling a new `init_card(address)`
helper (the same per-card bring-up `initialize()` used to do once,
unchanged in substance) for each; `get()` became `get(index: usize) ->
Option<&'static Es1370>`; and `handle_pci_irq` now iterates every card
instead of checking one, exactly mirroring `nic.rs`'s own IRQ dispatch --
safe for the identical reason the ES1370+RTL8139 shared-IRQ-line fix
already established (every backend's own `handle_interrupt()` checks its
own status register first and no-ops harmlessly if it isn't the actual
source, so calling every card's handler on a shared line costs nothing
beyond that one check). Each card's own DAC1/ADC ring buffers
(`RINGS`/`CAPTURE_RINGS`) became `[audio::PcmRing; MAX_INSTANCES]`
arrays too, indexed by a new `instance: usize` field stored on each
`Es1370` -- so every ring-touching method (`submit`, `capture`,
`ring_backlog`, `capture_backlog`, `fill_period`,
`drain_capture_period`) now reaches its own card's own ring, never
another card's.

`kernel/src/audio.rs`'s facade mirrors this identically:
`DEVICES: [Option<Audio>; MAX_DEVICES]`/`DEVICE_COUNT` replace the former
`ACTIVE`, `initialize()` now calls `es1370::initialize_all()` and
populates every discovered slot, and new `get_at(index)`/`count()`
functions join the existing `get()` (kept meaning "device 0", purely for
backward compatibility with the non-indexed tape ABI callbacks that
predate this feature). **Deliberately, and unlike `nic.rs`, there is no
"active device" selection concept at all** (no `set_active`/
`ACTIVE_INDEX`) -- the entire point of driving several cards is to use
all of them *simultaneously* as independent stereo pairs for one
combined multichannel stream, not to pick one card at a time the way
`nic use N` picks one NIC. A tape wanting genuine multichannel output is
expected to address every device by index, not switch between them.

**The tape ABI grew five more trailing callbacks**, following the exact
same append-only-cdecl convention as every prior ABI growth in this
document: `tape_audio_count() -> u32` (how many devices were found) and
four index-based counterparts to the existing non-indexed audio
callbacks -- `tape_audio_sample_rate_at`,
`tape_audio_channels_and_bits_at`, `tape_audio_submit_at`, and
`tape_audio_capture_at` (all taking a 0-based device index as their first
argument, matching `nic_mac_at`'s own indexing convention). Every
existing tape ignores all nine audio callbacks (five new, four
pre-existing) exactly like any other trailing cdecl argument; the
existing `tape_audio_sample_rate`/`tape_audio_channels_and_bits`/
`tape_audio_submit`/`tape_audio_capture` callbacks keep meaning "device
0" unchanged.

**`audio` (the shell command) was rewritten to report every discovered
device, not the single one it used to assume.** It now prints an overall
`N audio device(s) found:` line, then one indexed block per device
(`[0] ES1370 at I/O base 0x...`, followed by that device's own
IRQ/DAC1/ADC/backlog/underrun/overrun diagnostics, each indented two
spaces) -- deliberately allowed to scroll past one screen once several
real cards are attached, the same `pci` precedent already established
for an inherently unbounded hardware listing. This changed the exact
text `audio` prints (the former single-device `"ES1370 found: I/O base
0x...` line is gone), which broke two pre-existing tests' own marker
strings (`test_audio_capture_e2e.py`'s `b"ES1370 found"` check and
`test_audio_net_e2e.py`'s identical one) purely because they searched for
that literal, now-removed substring -- both were updated to look for
`b"[0] ES1370"` instead, confirmed passing again afterward. No other
test needed a change; `test_audio_net_real_mic_e2e.py`'s own marker
(`b"Raw STATUS register"`, chosen specifically to be a true line-tail
rather than a prefix -- see that section's own note on this exact class
of bug) still appears verbatim, just now indented two spaces under its
device's own block.

**Reserved low-memory region grew again, from `0x60000` to `0x80000`.**
Growing `MAX_INSTANCES`-many copies of every per-card static (DMA
buffers, ring arrays, `CARDS`) pushed `__kernel_end` well past the
previous ceiling. Measuring the real overflow needed the same technique
this document's own audio-ring-sizing investigation above used:
temporarily raise the linker `ASSERT` to something absurd (`0x200000`),
build, then read `.bss`'s real start/size back out of `readelf -S`
directly, since the `ASSERT` failure itself reports no numeric detail --
this measured `.bss` starting at `0x37000`, sized `0x41fb4`, giving
`__kernel_end = 0x78fb4`. `0x80000` was chosen as the new ceiling
(roughly 28.75 KB of headroom above that measurement) and, as this
project's "Low-memory map" section requires, all four constants that
must always move together were updated in the same change:
`kernel/linker.ld`'s `ASSERT`, `kernel/src/pmem.rs`'s `RESERVED_BELOW`,
`tools/write_layout.py`'s `KERNEL_RESERVED_END`, and `tools/
write_netrom_layout.py`'s `MAX_KERNEL_BYTES` -- plus a sixth,
undocumented-until-now spot this same grep sweep turned up:
`boot/stage2.S`'s soft-reboot page-touch/resync loop had its own
hardcoded `0x70000` bound that also needed updating to `0x80000`, since
it is not derived from any of the four "official" constants at all.

**A fifth dependency this project's own soft-reboot documentation
already warned about, checked and confirmed necessary again**:
`kernel/src/debug.rs`'s `SLOT1_LBA`/`SLOT_SECTORS` constants (used by the
A/B kernel-slot machinery) are not automatically derived from
`KERNEL_RESERVED_END` and had to be manually recomputed (`python3
tools/write_layout.py --kernel build/kernel.bin --output /dev/stdout`)
and copied in by hand -- `SLOT1_LBA` moved `2849 -> 2977`, `SLOT_SECTORS`
moved `768 -> 896`. `debug.rs`'s own doc comment (which already
documents an earlier real crash caused by exactly this drift) was
extended with this session's growth as a second confirming data point.
Re-running `tools/write_layout.py` after every later shell-only ABI
change (which does not change kernel size at the sector-rounding
granularity) confirmed these two values did not need to move again for
those later, smaller edits -- only the ring-array-driven growth above
actually crossed a sector boundary.

**Verified with `tapes/test_audio_multi_e2e.py`** (not part of `make
test`; run via the new `make test-audio-multi` target): two independent
QEMU-emulated ES1370 devices, each with its own `none` audiodev backend
(chosen for the same environment-portability reason
`test_audio_capture_e2e.py` already gives), are confirmed discovered at
two distinct I/O bases (not the same device reported twice), both
report the expected 44100 Hz/2ch/16-bit format independently, and both
devices' DAC1 period counts keep advancing independently and
simultaneously over a 1.5-second window -- the concrete proof that the
array-of-instances refactor genuinely drives two separate cards rather
than aliasing one card's state onto the other. This test deliberately
scopes itself to the driver/facade/shell-command plumbing only, not the
new tape-ABI callbacks themselves (`tape_audio_count`/`..._at`); those
remain to be exercised by a future update to `tapes/audio-net` (the
actual multichannel streaming payoff), not yet built as of this writing.
`make test` (the full existing regression suite) was re-run after this
whole refactor and passed unmodified, confirming no regression to any
existing NIC/audio/disk/boot behavior from either the driver conversion
or the reserved-region growth.

`tapes/audio-net`'s own move to genuine multichannel streaming --
designing a wire format that declares a device/channel count and
interleaves PCM across several independent stereo devices, and extending
`AUDIO_TAG_START`/`AUDIO_TAG_DATA` to carry it -- is the next planned
increment, not yet started as of this writing.

### `tapes/audio-net`'s wire protocol grows a per-device tag, instead of interleaving

The multichannel-streaming increment flagged above as "not yet started"
raised a real design fork: should one combined multi-card stream be a
single frame format interleaving every device's samples together (e.g.
`[dev0-L, dev0-R, dev1-L, dev1-R, ...]` per frame), or should each
device keep its own independently tagged sub-stream sharing one session?

**Tagged per-device sub-streams were chosen, not interleaving**, for the
same reason this project's other wire protocols already tolerate
occasional loss as "a glitch, not an abort" (see `AUDIO_TAG_DATA`'s own
original doc comment, and `TERM_TAG_DATA`'s identical reasoning above):
an interleaved frame's channels are only meaningful relative to each
other, so one dropped frame -- routine on this project's best-effort,
no-retransmission protocol family -- would desynchronize *every*
channel's byte alignment permanently, with no way to recover it later.
A dropped frame in a tagged sub-stream, by contrast, only glitches that
one device; every other device's own sub-stream is completely
unaffected. Tagging is also the more natural fit for genuinely
independent multi-card hardware (as opposed to one hypothetical
multichannel codec): each physical Sound Blaster card's sub-stream can
be sent, dropped, or resent independently, arrive over a different NIC,
or have its sender join/leave the session at a different time --
`listen_mode`'s "first `AUDIO_TAG_START` from any device index locks the
peer" design (below) depends directly on this independence.

**Wire format, extended, not replaced:**

| Tag | Payload (was) | Payload (now) |
| --- | --- | --- |
| `AUDIO_TAG_START` | `sample_rate:u32 LE, channels:u8, bits:u8` (6 bytes) | adds a trailing `device_index:u8` (7 bytes) |
| `AUDIO_TAG_DATA` | `length:u16 LE` + PCM | adds a leading `device_index:u8` before the existing `length:u16 LE` + PCM (`DATA_HEADER_LEN` grew 2 -> 3 bytes) |
| `AUDIO_TAG_STOP` | no payload | unchanged -- still ends the *entire* session, not one device's sub-stream |

This is a breaking wire-protocol change, not an additive one -- accepted
deliberately, since (unlike, say, NETFS/KUP) nothing outside this
project's own paired sender/receiver ever speaks this exact protocol, so
there is no interoperability cost to changing the payload layout
outright rather than growing it compatibly. `MAX_DEVICES = 4` is a new
tape-side constant deliberately mirroring `kernel/src/es1370.rs`'s
`MAX_INSTANCES`, duplicated rather than shared (no header mechanism
crosses the kernel/tape boundary, the same situation every other
tape-side mirrored constant in this project is already in).

**`listen_mode` was rewritten for per-device demultiplexing.** It now
queries `audio_count()` (clamped to `MAX_DEVICES`) and prints every
locally attached device's own format up front, then tracks
`local_formats`/`device_active` arrays across the session. The *first*
`AUDIO_TAG_START` received from any peer, for any device index, locks
that peer's MAC as the session's `source` (mirroring `tape_load_net`'s
existing "first sender wins" convention) and resets every device's
active/byte-counter state for the new session; any *later* `AUDIO_TAG_
START` from that same locked peer, naming a different device index, is
then accepted as an additional sub-stream within the same session
(printed as `"device N sub-stream active"`) rather than being rejected
or re-locking the source. Each sub-stream's declared format is validated
against that specific local device's own real format independently --
one device's stream can be rejected as a mismatch while another
device's stream is accepted in the same session, since resampling is
still out of scope exactly as it always was. `AUDIO_TAG_DATA` frames are
demultiplexed by their leading `device_index` byte to the matching
local device's `audio_submit_at(index, ...)`; an out-of-range or not-yet-
activated device index is silently dropped, mirroring the existing
"drop what can't be understood" tolerance this protocol family already
has elsewhere. Byte counters (`bytes_received`/`bytes_played`/`bytes_
dropped`) remain one running session-wide total across every device's
sub-stream combined, not broken out per device -- this matches the
existing single-device summary's own granularity and was not considered
important enough to change.

**`capture_mode` was rewritten to round-robin across local devices.**
It queries `audio_count()`, prints every device's format, sends one
tagged `AUDIO_TAG_START` per local device up front (each carrying that
device's own real format and index), and its main loop now polls
`audio_capture_at(index, ...)` for every attached device once per pass,
sending a tagged `AUDIO_TAG_DATA` frame for whichever device(s)
produced a non-empty result that pass -- so one slow or silent device's
capture never blocks another device's sub-stream from being forwarded
promptly. `send_mode` (the pre-recorded-file replay stand-in for live
capture) is unaffected in spirit -- it still replays exactly one file at
the fixed 44100 Hz/2ch/16-bit format -- but now tags every frame it
sends with a new `SEND_DEVICE_INDEX = 0` constant, since the wire format
requires a device index on every frame regardless of how many devices
are actually involved.

**Verified with a new, dedicated e2e test, `tapes/test_audio_net_multi_
e2e.py`** (not part of `make test`, mirroring every other tape e2e
harness): two QEMU-emulated ES1370 cards, each with its own `wav`
audiodev backend, are driven through one `audio-net` listen-mode
session carrying two independently tagged sub-streams at once -- device
0 gets a real, non-silent 440 Hz tone, device 1 gets genuine PCM
silence (all-zero samples), sent interleaved frame-by-frame over the
same session. The test deliberately does not just check "some audio
played somewhere": it asserts device 0's captured WAV contains the real
tone (peak amplitude comfortably above a silence floor) *and* device 1's
captured WAV settles to near-zero amplitude once its own sub-stream
starts submitting silence -- if `device_index` demuxing were broken
(e.g. both sub-streams aliased onto the same local device, or one
silently swapped for the other), this is exactly the check that would
catch it. One real measurement subtlety surfaced while writing this
test and is worth remembering for any future audio-capture-verifying
test: a device's own captured WAV file spans its *entire* guest
lifetime, including the boot-time narration before any tape ever ran --
during which every device still plays its default ~441 Hz idle test
tone (see `network_mode`'s own doc comment in `kernel/src/es1370.rs`:
"there is no way back to the test tone without a reboot" once a
device's first `audio_submit`/`audio_submit_at` call flips it into
network-fed mode) -- so a naive whole-file peak check on the
"expected silent" device would spuriously fail on that earlier idle
tone. The fix was to check only the *tail* of that device's capture
(after a deliberate `time.sleep(1.0)` once the streaming session's byte
counts are confirmed, giving the device time to settle into pure
post-submission silence) rather than the whole file -- the device-with-
a-real-tone's check does not need this same care, since its own
sub-stream's tone dominates the peak regardless of when in the capture
window it is measured.

The pre-existing single-device test, `tapes/test_audio_net_e2e.py`, was
updated (not replaced) to speak the new 7-byte `AUDIO_TAG_START`/tagged
`AUDIO_TAG_DATA` payloads with a fixed `DEVICE_INDEX = 0`, and its
`"Local audio device: ..."` marker was updated to match `listen_mode`'s
new per-device-list printed format (`"Local audio device(s): N.\n  [0]
..."`). `tapes/test_audio_net_real_mic_e2e.py` needed no changes at all,
since it drives both peers through the real compiled tape end to end
(never constructing raw wire frames itself) and its own markers
(`"Streaming live ADC input"`, `"capture stopped locally"`) are
unaffected text that only moved position within `capture_mode`'s
rewritten body. `make test` (the full existing regression suite),
`tapes/test_audio_capture_e2e.py`, and `tapes/test_audio_multi_e2e.py`
were all re-run after this change and passed unmodified, confirming no
regression to the underlying driver/facade work from the prior session.

### Intel HD Audio ("HDA"): a discover-only first increment

Genuine Intel HDA support -- the second hardware family the user's own
box of sound cards includes, considered as a follow-up once ES1370
multi-device streaming was working -- now has a first, deliberately
narrow increment: `kernel/src/hda.rs` and the new `hda` shell command
reset an Intel HDA controller and enumerate whatever codec(s) respond,
reading back each one's root-node Vendor/Device ID. **There is no
playback or capture yet** -- this step proves controller discovery and
codec enumeration work, nothing more, the same incremental approach
`i350.rs`'s empty `CANDIDATE_DEVICE_IDS` and the very first ES1370
increment (test-tone-only DAC1, before ADC/network-fed streaming
existed) both already established for this project.

**HDA's controller/codec split is architecturally different from every
prior audio/NIC backend here.** An "HDA controller" (the PCI device this
driver discovers) is a generic DMA/verb-transport engine, not itself an
audio device -- the actual codec(s) (DAC/ADC/mixer widgets) sit behind it
on a private, controller-owned "CODEC" bus addressed by a small integer
("codec address", `cad`, 0-14) and are discovered, not assumed, by
reading a bitmask register after reset. This is a real architectural
split ES1370 (one PCI function *is* the whole audio device) has no
equivalent of.

**The Immediate Command interface was used instead of the full CORB/RIRB
DMA ring buffers**, specifically because it needs zero guest-memory setup
at all -- the spec's mandatory command/response path otherwise requires
allocating and programming two ring buffers (Command Output Ring Buffer,
Response Input Ring Buffer) before a single verb can be sent. Confirmed
this synchronous path is real, not a QEMU stub, by reading QEMU's own
`intel_hda_set_ics`/`intel_hda_corb_run`/`intel_hda_response`
(`hw/audio/intel-hda.c`, tag v10.2.1, this project's installed QEMU
version): `IC` (offset `0x60`) holds the 32-bit verb to send; writing
`IRS_BUSY` (bit 0 of `IRS`, offset `0x68`) submits it; polling `IRS_BUSY`
back to clear and `IRS_VALID` (bit 1) set indicates a response is ready
in `IR` (offset `0x64`); writing 1 to `IRS_VALID` clears it for the next
command, per spec. Every register offset/bit here was cross-checked
directly against QEMU's own `intel-hda-defs.h`, the same rigor this
project applies to every other register-level driver after the RTL8139
lessons (see "Real hardware notes" above) -- **all of it remains
unverified against real hardware**, exactly like every other freshly
scaffolded driver here before its first physical test.

**The 32-bit verb encoding** (validated against QEMU's own
`hda_audio_command` decode logic in `hw/audio/hda-codec.c`): a command
word is `(cad << 28) | (nid << 20) | (verb << 8) | payload`, where `nid`
is the target widget/node (`AC_NODE_ROOT = 0x00` for this step, since it
only reads the root node's own Vendor/Device ID parameter) and `verb`
here is `AC_VERB_PARAMETERS = 0xf00` (get-parameter) with payload
`AC_PAR_VENDOR_ID = 0x00`. This single formula covers both the spec's
12-bit "short" verbs and 4-bit "long" verbs uniformly, since their bit
ranges never overlap -- no special-casing needed, confirmed against
several different verb constants in QEMU's decode path.

**`GCTL_CRST`'s reset polarity is the opposite of an intuitive "write 1
to reset" convention** -- bit 0 of `GCTL` (offset `0x08`) reads 0 while
the controller is held in hardware reset and reads/must be written 1 for
normal operation; it is not a self-clearing pulse. `reset_controller`
clears it, polls until it reads back 0, sets it again, and polls until
it reads back 1 -- both transitions bounded (`IMMEDIATE_COMMAND_POLL_
ATTEMPTS`-style spin ceilings, this project's usual "poll then give up
cleanly" convention, not a real timeout value from the spec) -- then
waits a short, fixed number of PIT ticks (`CODEC_ENUMERATION_WAIT_
TICKS`) before reading `STATESTS`, since the spec allows codecs a brief
settle period after reset before they assert their own presence bit.

**Only QEMU-confirmed PCI IDs are registered for now** -- Intel
`0x8086:0x2668` (`intel-hda`, ich6) and `0x8086:0x293e`
(`ich9-intel-hda`) -- following the same "don't guess real-hardware IDs,
confirm via `pci` first" convention `i350.rs`'s empty candidate list
already established; the user's actual Sound Blaster PCI cards will need
their real vendor/device IDs read via the existing `pci` command before
being added here. **Single-instance for this step** (only the first
matching controller found is driven), a deliberate scope-narrowing
documented in `hda.rs`'s own module doc comment, not a hard ceiling --
mirroring how `es1370.rs` itself started single-instance before growing
`MAX_INSTANCES` once multi-card streaming became a real, motivated need.

**QEMU's synthetic codec identity, confirmed empirically, not just from
reading source**: `hda-output`/`hda-duplex`/`hda-micro` all report
Vendor ID `0x1af4` (QEMU's own convention for synthetic devices, shared
with virtio) -- a real `-device intel-hda -device hda-duplex,audiodev=...`
QEMU boot reported Device ID `0x0022` for `hda-duplex` specifically,
confirming the `0x1af4:0x00xx` family predicted from reading
`hw/audio/hda-codec-common.h` alone.

Verified end to end with `tapes/test_hda_e2e.py` (not part of `make
test`, mirroring every other tape/driver e2e harness's `term_net`-driven
structure): boots a guest with `-device intel-hda -device
hda-duplex,audiodev=none`, drives the `hda` shell command over a live
`term net` session, and asserts the reported vendor ID matches `0x1af4`
and that a second, immediately following `hda` call reports the exact
same codec without re-probing (the command is idempotent once
`initialize()` has already succeeded, matching every other lazy-init
backend's `Option<Backend>`-cached convention here).

CORB/RIRB DMA-ring support, real node-graph widget enumeration (walking
each codec's Audio Function Group and its DAC/ADC/pin-complex widgets),
stream-descriptor/BDL-based playback and capture, and format negotiation
are all deliberately deferred to a later phase -- this increment proves
only that the controller and codec(s) can be found and talked to at all.

### Intel HDA, step two: CORB/RIRB command transport and real node-graph enumeration

The natural next increment after discover-only: `kernel/src/hda.rs`
switched entirely from the optional Immediate Command interface to
CORB/RIRB (Command Output Ring Buffer / Response Input Ring Buffer), the
HDA spec's mandatory command transport, and added a real node-graph
walk -- root node -> Audio Function Group -> that AFG's own child
widgets (DAC/ADC/pin complexes) -- replacing the prior step's "only ever
read the root node's Vendor/Device ID" scope. Still no playback or
capture; this is a diagnostic/plumbing increment, mirroring how ES1370
grew in stages (test tone, then ADC, then network-fed streaming) rather
than attempting stream DMA in one step.

**Why CORB/RIRB despite Immediate Command already working.** Immediate
Command is genuinely optional per spec -- real hardware is not required
to implement it -- while CORB/RIRB is the transport every real
controller must support, and it is also the same DMA-ring shape (a
host-memory ring, a hardware-maintained write pointer, a
software-maintained read pointer) actual stream playback's Buffer
Descriptor Lists will need later, so building it now doubles as
practice for that shape rather than being throwaway plumbing.

**Every offset, bit, and DMA read/write-pointer semantic here was
cross-checked directly against QEMU's own `hw/audio/intel-hda.c` (tag
v10.2.1) source, not just header constants** -- specifically
`intel_hda_corb_run`/`intel_hda_response`/`intel_hda_set_corb_wp`/
`intel_hda_set_rirb_wp`, and the `regtab[]` register-definition table's
`reset`/`wmask` fields for `CORBSIZE`/`RIRBSIZE`/`CORBRP`/`RIRBWP`. One
practical lesson from this cross-referencing worth recording for future
large-source-file investigations: `web_fetch` silently truncated at
least one of these files with no truncation notice, giving an
incomplete (and briefly misleading) picture; downloading the full file
with `curl` and then `grep`/`sed`-ing it directly is the reliable way to
get complete, verifiable reference source.

**The CORB/RIRB read/write-pointer convention is a real gotcha, not
obvious from the register names alone.** Both `CORBWP` and `RIRBWP` hold
the index of the *last* written entry, not "the next free slot" --
confirmed directly from `intel_hda_corb_run`'s `d->corb_rp = (d->corb_rp
+ 1) & 0xff` (reading the verb at that *pre-incremented* index) and
`intel_hda_response`'s identical pre-increment before writing a
response. So posting a new verb means: compute `(current_CORBWP + 1) &
0xff`, write the verb into *that* ring slot, then update `CORBWP` to
match -- writing the verb at the *current* `CORBWP` index (the
intuitive-but-wrong reading) would silently clobber whatever slot the
hardware last consumed, or race a still-in-flight command. `RIRBWP`
mirrors this exactly on the read side, with one further wrinkle:
**software has no RIRB read-pointer register at all** -- there is no
`RIRBRP`, only `RIRBWP` (hardware's own last-written index) -- so this
driver tracks its own "last consumed" index in a `Cell<u16>` on `Hda`
(interior mutability, since every `send_corb_command` call takes only
`&Hda`) and polls `RIRBWP` for it to advance to `(tracked + 1) & 0xff`
before reading that slot.

**`CORBSIZE`/`RIRBSIZE` are read-only in practice, not negotiated.**
QEMU's own `regtab[]` gives both registers no `wmask` at all, and both
reset to `0x42` (bit 6 set, "256-entry size supported"; bits[1:0] = `10`,
"256 already selected"). The HDA spec does allow smaller ring sizes (2
or 16 entries) on some real controllers, selected by writing bits[1:0],
but since 256 is both the maximum and QEMU's only supported size,
`setup_corb_rirb` simply reads each register back after programming the
base-address registers and treats anything other than the 256-entry
selection as unsupported, rather than attempting to write a particular
size -- the safer assumption for real hardware that might likewise
silently ignore an unsupported write.

**`CORBRP`'s reset is a two-phase handshake; `RIRBWP`'s is a
self-clearing pulse -- these are genuinely different, not two
descriptions of the same thing.** `CORBRP_RST` (bit 15) must be set,
polled until the readback confirms it, cleared, and polled again until
the readback confirms *that* -- mirroring Linux's own `azx_reset`.
`RIRBWP_RST`, by contrast, self-clears instantly per
`intel_hda_set_rirb_wp`'s handler (`if (d->rirb_wp & ICH6_RIRBWP_RST)
d->rirb_wp = 0;`), needing no poll at all. Getting these confused (e.g.
polling `RIRBWP` for a bit that already cleared itself) would either
hang or silently skip the intended reset.

**`RINTCNT` must be set to its maximum (`0xff`) at setup, not left at
its reset default**, because `intel_hda_corb_run` stalls verb processing
once `rirb_count == RINTCNT` until software clears `RIRBSTS` -- a
signal this driver, which only ever polls `RIRBWP` and never touches
`RIRBSTS` at all, would never satisfy. Missing this would manifest as
"the first handful of verbs work, then the controller silently stops
answering" -- a real, easy-to-hit trap for a polling-only driver like
this one.

**Node-graph walk.** `find_audio_function_group` reads the root node's
own `NODE_COUNT` parameter (start nid + count, packed as bits[23:16]/
bits[7:0]) and checks each subordinate node's `FUNCTION_TYPE` for
`AC_GRP_AUDIO_FUNCTION`, rather than assuming the first subordinate node
is always the AFG -- a real codec could expose more than one function
group (e.g. audio plus modem). `enumerate_afg_widgets` then reads the
AFG's own `NODE_COUNT` for its child-widget range and, per widget, reads
`AUDIO_WIDGET_CAP` (widget type via bits[23:20], stereo via bit 0) plus,
conditionally, `PIN_CAP` (for a `Pin` widget, giving output/input
capability) and `CONNLIST_LEN`/`GET_CONNECT_LIST` (for any widget
reporting `AC_WCAP_CONN_LIST`, giving its first upstream connection).
Only the short-form (one byte per entry, up to four entries) connection
encoding is read -- sufficient for QEMU's synthetic codecs, which only
ever expose one connection per widget in short form; a real codec's
long-form (16-bit) entries or more than four connections would need a
fuller parser, out of scope for this step.

**Confirmed against QEMU's actual `hda-duplex` layout, not just
predicted from source**: root (nid 0) -> AFG (nid 1) -> DAC (nid 2,
`audio output`, stereo) -> line-out pin (nid 3, `pin complex`,
output-capable, connected to nid 2) -> ADC (nid 4, `audio input`,
stereo, connected to nid 5) -> line-in pin (nid 5, `pin complex`,
input-capable). Note the ADC's connection points *at* its pin (nid 4 ->
nid 5), the reverse direction of the DAC/output-pin pair (nid 3 -> nid
2) -- an input path's converter reads *from* its pin, while an output
path's pin reads *from* its converter, exactly the direction each
signal actually flows; `tapes/test_hda_e2e.py`'s own regex assertions
initially got this backwards and had to be corrected once the real
output made the actual topology obvious.

Verified end to end with the extended `tapes/test_hda_e2e.py`: beyond
the prior step's vendor/device-ID and idempotency checks, it now also
asserts the AFG nid, the DAC/line-out-pin pair (nid 2 -> nid 3
connection), and -- when the guest is configured with `hda-duplex`
(as this test's fixture is) -- the ADC/line-in-pin pair (nid 4 -> nid 5
connection) all appear correctly in the `hda` command's output. `make
test` (unrelated to HDA) was re-run unmodified and continues to pass,
confirming no regression from this rewrite.

Stream-descriptor/BDL-based playback and capture (`SD_FORMAT`, a Buffer
Descriptor List, `SET_STREAM_FORMAT`/`SET_CHANNEL_STREAMID` verbs,
`AC_PINCTL_OUT_EN` to actually enable a pin's output, and starting DMA
via `SD_CTL`) remain the next, larger phase -- not yet scoped in detail.

### Intel HDA, step three: more than one controller

`kernel/src/hda.rs`'s own doc comment on the (then-)single-instance
`MAX_CODECS` constant had already flagged this exact limitation as
deliberate-but-revisitable, explicitly citing `es1370.rs`'s own
instance-count growth (see "Multi-instance ES1370 audio driver" below)
as the precedent to follow "once a real need for it appears" -- it
appeared directly, from the user owning a box of several different
physical sound cards and asking whether more than one HDA controller
could be handled at once (e.g. one onboard controller plus a discrete
add-in card).

The conversion mirrors `es1370.rs`'s established shape exactly:
`initialize()` became `initialize_all() -> Result<usize, Status>`
(idempotent, returns the discovered count on success/repeat calls,
`Status::NotPresent`/`InitFailed` unchanged otherwise), the single
`static mut CONTROLLER: Option<Hda>` became `static mut CONTROLLERS:
[Option<Hda>; MAX_INSTANCES]` plus a `CONTROLLER_COUNT` counter, and the
former index-less `get()` became `get(index: usize)` (0-based, matching
`es1370::get`'s own convention, not the shell's 1-based `nic use N`). A
new `count()` accessor lets a caller loop `0..hda::count()` without
needing to know `MAX_INSTANCES` itself. The per-address bring-up logic
(PCI enable, MMIO BAR map, controller reset, CORB/RIRB ring setup, codec
discovery) was extracted into a new `init_controller(address, instance)`
helper -- mirroring `es1370::init_card`'s own split -- so one
controller's bring-up failure does not prevent an already-discovered
earlier one from being usable; `initialize_all` simply skips a failed
address and continues. `Hda` gained one new field, `instance: usize`
(mirroring `Es1370`'s own field of the same name and purpose), purely so
`hda_status`'s per-controller output can print "Controller N: ..." from
the struct itself rather than needing to be threaded through separately.

**No structural change was needed to `Hda` itself, `CodecInfo`,
`WidgetInfo`, `discover_codecs`, `find_audio_function_group`,
`enumerate_afg_widgets`, `reset_controller`, or any register/verb
constant** -- every one of those already operated on one controller's
own `mmio_base`/`corb_base`/`rirb_base`, so they needed nothing beyond
being called once per discovered address instead of once total. This is
exactly why the conversion was safe and small: `paging::map_mmio` is
already keyed off each controller's own distinct PCI BAR, so two
controllers' MMIO windows, CORB/RIRB rings, and register state can never
collide with one another simply by construction, with no extra
bookkeeping required to keep them apart.

**`MAX_INSTANCES` was set to `2`, not `es1370.rs`'s `4`.** The two
drivers' growth motivations are genuinely different: ES1370's `4` was
chosen to reach a target *channel count* (each ES1370 card is a fixed
one-DAC-per-card stereo pair, so more instances is the only way to add
more channels once playback exists). HDA has no playback implemented
yet, and when it does, a single HDA codec can already expose several
independent DAC/pin pairs on its own node graph -- so this ceiling only
needs to cover a plausible number of physically installed
*controllers*, not multiply out to a channel-count target. `2` also
already matched this driver's pre-existing `find_devices` match-array
size (sized for "at most a couple of these on one machine" even before
this change), so no companion constant needed to grow alongside it.

`kernel/src/shell.rs`'s `hda_status()` was split into an outer loop
(calling `initialize_all()`, printing a `"N Intel HD Audio
controller(s) discovered:"` summary line, then iterating
`0..hda::count()`) and a new `hda_status_one(&Hda)` printing one
controller's own MMIO base/IRQ/stream-count/codec-node-graph block,
prefixed `"Controller N: ..."` -- reusing exactly the same per-codec/
per-widget printing logic the prior single-controller version already
had, unchanged. This follows this driver's existing "no arguments
needed, just show me everything" diagnostic style (matching `pci`'s own
shape) rather than adding `nic use N`-style index-based selection
syntax, since HDA has no persistent "active controller" concept to
select yet (no playback/capture exists to direct at one card over
another).

Verified end to end with a new, dedicated `tapes/test_hda_multi_e2e.py`
(not part of `make test`, mirroring `tapes/test_hda_e2e.py`'s own
structure): a guest configured with *two* independent `intel-hda`
controllers (`hda0` carrying an output-only `hda-output` codec, `hda1`
carrying a full-duplex `hda-duplex` codec, each on its own `-audiodev
none` backend) confirms both are discovered (`count == 2`), reset
independently with genuinely distinct MMIO bases, and each one's own
codec is enumerated correctly and independently -- controller 0's block
has no ADC/line-in widgets at all (proving `hda-output`'s narrower
topology wasn't accidentally inherited from the other controller),
while controller 1's block has both a DAC/line-out and an ADC/line-in
pair, exactly like the existing single-controller test's `hda-duplex`
fixture -- and that a second `hda` call remains idempotent, still
reporting both controllers with no re-probe. `tapes/test_hda_e2e.py`
(the original single-controller test) was re-run unmodified and
continues to pass unchanged, confirming this conversion is backward-
compatible for the single-controller case. `make test` (unrelated to
HDA) was also re-run and continues to pass.

### Intel HDA, step four: real stream-DMA playback

The prior two increments got as far as codec/node-graph *enumeration*;
this one makes an Intel HDA controller actually produce sound, joining
`audio.rs`'s facade (and, through it, `audio-net`) alongside ES1370 with
no tape-side changes at all -- `tapes/audio-net/src/main.rs` already
iterated every device generically via `audio_count()`/`audio_sample_rate
_at()`/`audio_submit_at()`, so an `Audio::Hda` variant slots in exactly
the same way a fifth NIC backend joins `nic::initialize`'s own loop.

**Register/verb semantics were re-derived from QEMU's own emulation
source** (`hw/audio/intel-hda.c`/`intel-hda-defs.h`/`hda-codec.c` at the
pinned `v10.2.1` tag, fetched via `raw.githubusercontent.com` since this
sandbox has no local QEMU source tree and `apt-get source` has no
`deb-src` configured) -- the same rigor and reasoning this project
already applied to ES1370's own register work. Two points were load-
bearing:
- **The verb-encoding formula has two shapes**, matched by QEMU's own
  `hda_audio_command`: a 12-bit-verb/8-bit-payload form when
  `(data & 0x70000) == 0x70000` (used by the pre-existing discovery
  code's `verb()` helper), and a 4-bit-verb/16-bit-payload form
  otherwise (a new `verb16()` helper), needed for
  `SET_STREAM_FORMAT`/`SET_AMP_GAIN_MUTE`, whose payloads don't fit in
  8 bits.
- **`SD_CTL` (per-stream control) and `SD_STS` (per-stream status)
  alias the same 32-bit register**: the low 24 bits are real control
  bits, but the top byte *is* `SD_STS` -- so every `SD_CTL` write must
  mask `& 0x00ff_ffff` or it silently acknowledges a completion status
  the driver never meant to touch. `write_sd_ctl`/`read_sd_ctl` wrap
  this masking once so every call site can't get it wrong.

**DMA is codec/controller-pulled, not driver-pushed**, the same shape
ES1370's own DAC1 ring already uses: `setup_playback` allocates a
physically contiguous 2-period DMA buffer (`pmem::allocate_contiguous`)
plus a one-page Buffer Descriptor List (`pmem::allocate`), writes two
16-byte BDL entries (`addr:u64`/`len:u32`/`flags:u32`, only the
interrupt-on-completion flag used), performs the documented
stream-reset handshake, programs `SD_CBL`/`SD_LVI`/`SD_FORMAT`/
`SD_BDLPL`/`SD_BDLPU`, sends the codec verbs that route a fixed stream
tag to the discovered DAC/pin pair and unmute it
(`SET_STREAM_FORMAT`/`SET_CHANNEL_STREAMID`/`SET_AMP_GAIN_MUTE`/
`SET_PIN_WIDGET_CONTROL` -- sent defensively, non-fatal if a codec
doesn't answer, matching the existing discovery step's tolerance),
pre-fills both periods with silence, registers a legacy PCI IRQ handler
(`handle_pci_irq`, mirroring ES1370's own shared-line-safe pattern:
check this device's own `SD_STS_BCIS` bit before doing anything, so
sharing an INTx line with an unrelated device costs nothing beyond one
harmless register read), and finally sets `SD_CTL`'s stream tag/IOCE/RUN
bits and unmasks the controller's global/controller/per-stream
interrupt-enable bits in `INTCTL`. `find_playback_target` walks the
already-enumerated node graph for a DAC (`AudioOutput` widget) and an
output-capable `Pin`, preferring a pair the graph already reports as
connected and falling back to the first plausible pair otherwise --
exactly the same "prefer confirmed, fall back to first-plausible"
tolerance this driver's discovery step already uses elsewhere. A
controller whose graph has no usable DAC/pin pair, or no free output
stream, is left with `has_playback() == false` and is not added to
`audio.rs`'s device list at all -- `hda status` remains a separate,
always-available way to inspect such a controller's node graph
regardless, unchanged from the previous two increments.

**Format is a fixed 44100 Hz / 2ch / 16-bit ceiling** (`PLAYBACK_FORMAT
= 0x4011`), the same ceiling QEMU's synthetic codec's converter widgets
appear to support and, not coincidentally, identical to ES1370's own
hardwired ceiling -- true per-codec-negotiated multichannel/higher-bit-
depth support is deferred, undocumented and untested against this
project's installed QEMU version, the same "flag it as speculative
until real hardware or a richer emulation proves otherwise" convention
already applied to I350's empty `CANDIDATE_DEVICE_IDS`.

**`audio.rs`'s facade grew an `Audio::Hda(&'static hda::Hda)` variant**
alongside the existing `Audio::Es1370` one: every method gained a
matching arm, `MAX_DEVICES` became `es1370::MAX_INSTANCES +
hda::MAX_INSTANCES` (both backends share one flat `DEVICES` array,
mirroring `nic.rs`'s own multi-backend facade), and `io_base()`'s return
type widened from `u16` to `u32` since HDA's MMIO physical base doesn't
fit in 16 bits (ES1370's own arm now just casts its I/O port number
`as u32`) -- `shell.rs`'s `audio_status()` print format string was
updated from `0x{:04x}` to `0x{:08x}` to match, and its header line's
wording changed from "I/O base" to the backend-neutral "base" for the
same reason. `initialize()` no longer early-returns the moment ES1370
reports `NotPresent`/`InitFailed`; it now falls through with `found = 0`
so HDA discovery still runs on a machine with no ES1370 at all, then
adds each HDA controller that has `has_playback() == true` after the
ES1370 loop. Capture-side methods (`capture`/`capture_overrun_count`/
`capture_backlog`/`capture_periods`) all return `0`/no-op for
`Audio::Hda`, since this driver's initial scope is playback-only --
mirroring ES1370's own DAC1-before-ADC history, and a candidate for a
future increment the same way ES1370's ADC was.

`kernel/src/shell.rs`'s `hda_status_one` was updated to match: the
stale "node graph, no playback yet" wording is gone, and a controller
with `has_playback() == true` now prints its active format, IRQ/period/
interrupt counts, underrun count, and network-fed ring backlog --
exactly the same diagnostic depth `audio_status()` already provides for
ES1370, just placed under the existing `hda`/`hda status` per-controller
block instead of `audio`'s own per-device one (a controller can be
inspected either way: `audio` for the format-agnostic facade view every
backend shares, `hda` for the full node-graph-plus-playback view unique
to this driver).

**The reserved low-memory region grew again, from `0x80000` to
`0x90000`**, across all four required constants (`kernel/linker.ld`'s
`ASSERT`, `kernel/src/pmem.rs`'s `RESERVED_BELOW`, `tools/
write_layout.py`'s `KERNEL_RESERVED_END`, `tools/write_netrom_layout.py`'s
`MAX_KERNEL_BYTES`) -- diagnosed the same way this region's every prior
growth was: temporarily raising the linker `ASSERT` far past any
plausible real ceiling to get a linkable binary, then reading `.bss`'s
real end address via `readelf -S -W` (`0x83fb4`, about 16 KiB over the
prior `0x80000` ceiling) rather than trusting a naive `size`-command
`text+data+bss` sum, which undercounts `linker.ld`'s own 4096-byte
section-alignment padding.

Verified end to end with a new `tapes/test_hda_playback_e2e.py` (not
part of `make test`, mirroring `tapes/test_audio_net_e2e.py`'s own
structure almost exactly, just with `-device intel-hda,msi=off -device
hda-output,audiodev=snd0` in place of `-device ES1370,audiodev=snd0` --
`msi=off` is required since `kernel/src/hda.rs` only ever registers a
legacy PCI IRQ handler, and QEMU defaults a new-enough HDA controller to
MSI unless told otherwise): `audio` reports the discovered controller
with the expected fixed format, `hda` reports active playback with a
matching format line, `audio-net`'s listen mode accepts a locally
generated 440 Hz tone streamed over the wire exactly like the ES1370
test does, and the captured WAV file contains real, non-silent audio
(peak amplitude comfortably above a silence floor) -- proving the whole
chain (codec/DAC/pin discovery, BDL programming, the stream-reset
handshake, and IRQ-driven period refill) actually produces sound, not
just that the tape's own byte counters look right. `tapes/
test_hda_multi_e2e.py`, `tapes/test_audio_multi_e2e.py` (whose own
regex needed the same "I/O base" -> "base" wording update above),
`tapes/test_audio_net_e2e.py`, and `tapes/test_audio_capture_e2e.py`
were all re-run afterward and continue to pass unmodified (beyond that
one regex fix), confirming this is additive and does not regress
ES1370's own playback/capture paths. `make test`,
`make test-nic-watchdog`, `make test-stack-guard`, and
`make test-page-protection` were also re-run and continue to pass,
confirming the reserved-region growth and the shared-PCI-IRQ-line/
paging code this driver touches remain correct.

Real, per-codec multichannel negotiation (beyond this fixed 2-channel
ceiling), HDA capture (ADC) support, and validation against a real
physical HDA-compatible sound card all remain open, deferred follow-ups
-- consistent with this project's existing "confirmed in QEMU, flagged
as unverified against real hardware" convention for every other
register-level driver here.



- Per-process paging/isolation, heap allocation, filesystems beyond FOSFS,
  userspace, scheduling,
  native USB, further hardware IRQ drivers beyond the timer/keyboard, robust
  fault recovery, any protocol above raw Ethernet frames (no ARP, no IPv4,
  no sockets), and support for NIC families beyond the current RTL8139,
  selected 3C905, Intel e1000, and NE2000-compatible backends.
- A native ATA/AHCI disk driver. The current protected-mode-to-real-mode BIOS
  trampoline is educational and intentionally narrow, not a scalable disk
  architecture.
- Broad physical-machine support. The documented target requires legacy BIOS,
  BIOS EDD/LBA, E820, VGA text, COM1, and PS/2-compatible input. QEMU is the
  primary validation environment.

### MSI/MSI-X and the local APIC: investigated, not yet built

Raised while scoping PCIe/I350 support: fos's entire interrupt story today
is the legacy 8259 PIC (`interrupts.rs`) fed by INTx lines -- `pci::
PciDevice::interrupt_line()` reads the config-space interrupt-line byte,
and `register_pci_irq` just unmasks that IRQ on the 8259. There is no local
APIC, no I/O-APIC, and `pci.rs` does not walk the PCI capability list at
all, so MSI (capability ID `0x05`) and MSI-X (`0x11`) support does not
exist in any form.

MSI replaces a physical interrupt wire with an ordinary-looking memory
write: the device writes a small "message data" value to a "message
address" (architecturally `0xfeennnnn`, intercepted by the CPU's local
APIC rather than landing in RAM) whenever it wants to interrupt. Adding
real support would need, at minimum: a local APIC driver (CPUID detection,
mapping its MMIO window, programming the spurious-vector and
task-priority registers -- a genuinely new subsystem running alongside,
not replacing, the existing PIC code, since BIOS/legacy devices may still
want PIC-routed IRQs); a PCI capability-list walk in `pci.rs` to find the
MSI/MSI-X capability and program the device's message address/data
registers; and a new IDT vector range for MSI-delivered interrupts,
separate from the PIC's remapped `IRQ_BASE` range. MSI itself needs no
I/O-APIC, since it bypasses I/O-APIC redirection entirely -- that could be
scoped independently of full I/O-APIC-based INTx routing.

This is *not* expected to be a performance win for fos as currently built,
and should not be undertaken as one. MSI's usual real-world benefits --
avoiding shared-IRQ-line contention, skipping a status-register read to
de-assert a level-triggered line, and (the largest one) giving each queue
its own vector routed to its own CPU core for per-core affinity/RSS --
mostly do not apply here: fos is single-core with no per-core IRQ affinity
concept at all; every backend's `handle_interrupt()` already does nothing
but ack-count-and-flag before returning to a foreground polling loop
(`nic.rs`'s documented design), so there is no expensive status-register
read in the hot path for MSI to remove; and nothing in fos blocks or
sleeps waiting to be woken by an interrupt -- `nic recv`/`tape load
net`/`chat` etc. are busy-polling loops regardless of whether an
interrupt ever fires, so lower interrupt latency has nothing downstream
to speed up. The one real fos-relevant benefit worth keeping in mind: some
PCIe devices legitimately leave the legacy `interrupt_line`/
`interrupt_pin` config-space bytes meaningless (implementing only MSI/
MSI-X), so a card encountered in the future that never asserts INTx at all
is a plausible, not just theoretical, real-hardware scenario -- but
fos's polling-first design already tolerates an interrupt line that never
fires without any correctness impact, exactly as it tolerates today's
already-documented PCIe INTx-routing quirks.

Worth revisiting if fos ever grows either genuine multi-core scheduling
(where per-queue MSI-X vector affinity would become a real win) or a
polling loop redesigned to actually `hlt`-and-wake on interrupt instead of
spinning (where MSI's lower, non-shared delivery latency would then
matter) -- neither depends on the other, and either alone would be enough
to justify this work. Until one of those lands, treat it the same as the
deferred x86-64 path below: a worthwhile future architectural exercise,
not a prerequisite for any NIC backend (including I350) working
correctly.

### Cooperative/preemptive scheduling: investigated, not yet built

Raised as "what would a real scheduler cost, and what would it buy us?"
while designing `poll_background_work` above (which is deliberately the
much cheaper alternative, scoped to kernel-native recurring work only --
see that section). A genuine scheduler, cooperative or preemptive, would
need several pieces this kernel has none of today:

- **Per-task stacks and a small, fixed-size task-control-block table.**
  No heap exists (see "Deliberately deferred work" below), so this would
  be a fixed array of TCBs, each owning a statically-sized stack --
  mirroring how `kernel::entry.S`'s single kernel stack and guard page
  already work, just multiplied per task, not a dynamically sized pool.
- **A context-switch primitive** (save/restore the full register set plus
  stack pointer, the same category of state `gdt.rs`'s double-fault task
  gate and `tapes/longmode64_shim.S`'s mode-switch save/restore already
  demonstrate this project handling carefully elsewhere) and a scheduling
  policy on top of it -- even a trivial round-robin needs somewhere to
  decide "whose turn is it," which does not exist at all right now.
- **Cooperative vs. preemptive** is the first real fork: cooperative (a
  task runs until it explicitly `yield()`s) needs no timer-interrupt
  integration and is far simpler to reason about, but only helps once
  something in the kernel actually calls `yield()` at a useful point --
  preemptive (forcibly switching tasks on the existing 100 Hz PIT tick)
  would need every switch point audited for what state is safe to
  interrupt, a much larger and riskier change.
- **By far the most expensive part, though, is auditing every place this
  kernel currently assumes single-threaded execution**, not building the
  mechanism itself. `disk.rs`'s BIOS real-mode trampoline is the sharpest
  example: it briefly disables paging and restores the real-mode IVT/PIC
  mapping to make one BIOS call, a window during which absolutely nothing
  else -- no interrupt handler, no other task -- can safely run without
  corrupting CPU/segment state (see "Protected-mode IRQ window during
  BIOS disk calls" above for what happens when even an ordinary IRQ fires
  during that window). A scheduler would need this (and every other
  similarly single-threaded assumption throughout `shell.rs`/`nic.rs`) to
  either run with scheduling explicitly disabled/pinned, or be redesigned
  entirely -- a large, project-wide review, not a contained addition.

**Benefits weighed against that cost, for this project specifically:**
genuine concurrency would let a long-running foreground task (e.g. a
`router` tape) share CPU time with an interactive shell instead of
occupying it exclusively, and would let a future blocking primitive
("wait for a frame" instead of "poll for a frame") avoid busy-spinning.
Neither is a pressing need today: `poll_background_work` above already
gets the low-cost share of that benefit (recurring kernel-native
housekeeping alongside any blocking loop) with none of the single-
threading-audit cost, and no tape currently needs to run "in the
background" while the shell keeps working -- `tape run` already fully
occupies the CPU by design, and F12/remote-abort already let an operator
reclaim it. **Conclusion: revisit a real scheduler only if a future tape
or workload genuinely needs concurrent execution (not just recurring
background polling, which `poll_background_work` already covers); until
then this remains a deliberately deferred, larger architectural project,**
not a prerequisite for anything currently planned.

### ISA bus support and the 3C509: investigated, not yet built

Every NIC backend today (`rtl8139.rs`, `threecom.rs`, `e1000.rs`) is found
through `pci.rs`'s PCI configuration-space walk (Mechanism #1, ports
`0xcf8`/`0xcfc`); this project has no ISA bus code at all. A 3Com 3C509 was
considered as a next NIC target and investigated, but deliberately not
started, because it splits into two pieces of very different size:

- **Card discovery is the genuinely new, harder half.** A 3C509 in its
  default ISA Plug-and-Play state has no PCI-style enumerable configuration
  space to walk; it must instead be woken from sleep with an LFSR-derived
  32-byte key sequence written to an ID port, then walked through a serial-
  identification/CSN-assignment protocol to have software assign it an I/O
  base and IRQ. That is a real, standalone subsystem (`isa.rs`, unrelated to
  `pci.rs`'s mechanism) this project has never needed before. The
  alternative -- jumperless-configuring a card once via the old DOS
  `3c5x9cfg.exe` utility (or its EEPROM default) to always answer at one
  fixed, known I/O port -- sidesteps ISA PnP entirely and shrinks discovery
  to "probe one known address, read the card's ID," at the cost of a one-
  time manual setup step per physical card that this project's existing
  drivers never require.
- **The register-level driver itself would actually be simpler** than the
  PCI backends already built: the 3C509 has no bus-mastering or descriptor
  rings at all -- it is plain programmed I/O through a window-based FIFO
  (well documented; Linux's original `3c509.c` is short), comparable to
  `threecom.rs`'s own window-register conventions but without any of its
  DMA-buffer bookkeeping.

No ISA machine was available to validate discovery either way at the time
of this investigation, so nothing was implemented; a future attempt should
decide up front which discovery path to take, since that choice (not the
Ethernet register work) is what actually determines the size of the task.

QEMU's `-device help` lists `ne2k_isa` (a fixed-port, non-PnP ISA NE2000)
but no 3C509 model at all, unlike every PCI backend this project has built
so far. A 3C509 driver would therefore have no automated `make test`
coverage ever -- every iteration, on both the discovery code and the
register-level driver, would need real hardware, permanently, not just at
the end. This is a standing cost the PCI backends never had, and is worth
weighing at least as heavily as the discovery-protocol choice above when
deciding whether/when to build this driver.

### Floppy drive support: investigated, not yet built

Raised by the lab team as a possible next storage target. This project's
entire disk stack -- `boot/stage2.S`'s loader reads and `kernel/src/
disk.rs`'s BIOS trampoline alike -- is built exclusively around INT 13h's
*extended* (`AH=42h`) LBA read/write service, and around one fixed drive
(`boot_drive`, captured from `DL` at boot, always `0x80`+ for a hard disk).
Real floppy drives are a real complication here, on two independent axes:

- **The BIOS call convention itself differs.** Real BIOS floppy support is
  legacy CHS (`AH=02h`/`03h` read/write, `AH=08h` get drive
  parameters/geometry) -- the extended `AH=41h`/`42h` LBA service this
  project already relies on is a hard-disk-era addition that most real
  BIOSes do not implement for the floppy drive number (`0x00`/`0x01`) at
  all. None of `disk.rs`'s existing LBA-based transfer logic could be
  reused unmodified; a floppy path needs its own CHS conversion and its
  own bounded-transfer logic (the same "never cross a 64 KiB boundary"
  discipline `stage2.S`'s multi-transfer kernel load already follows, just
  derived from CHS geometry instead of LBA math). The real-mode-trampoline
  *mechanism* itself (`disk::transfer`'s protected-to-real-mode drop, IVT/
  PIC save-restore, GDT reload) is fully reusable as-is, though -- only the
  BIOS call semantics change, not the surrounding plumbing.
- **There is no second-device concept anywhere above the trampoline.**
  `fosfs.rs` and every disk-facing shell command assume the one boot
  drive/GPT-discovered partition; adding a floppy would need either a
  separate, filesystem-agnostic raw command surface (mirroring
  `readsector`/`hexdump`'s existing pattern, e.g. a new `floppy` namespace)
  or an actual multi-device parameter threaded through `fosfs.rs` -- the
  latter is a real, if not huge, refactor of code that has never needed to
  address more than one device.

Unlike the 3C509 investigation above, QEMU emulates this well
(`-device help` lists `floppy`/`isa-fdc`, and `-fda` is a standard, widely
documented QEMU option) -- so, refreshingly, this is one legacy-hardware
feature that keeps full `make test`-style automated coverage, not a
permanent real-hardware-only cost.

**Follow-up scoped to secondary storage only (no floppy boot path), and to
the lab's actual two physical drives -- a 3.5" 1.44 MB and a 5.25" 360 KB
unit:**

- **The "two floppy types" concern turns out to be mostly a non-issue.**
  `AH=08h` (get drive parameters) reports the real, current drive's own
  geometry (heads/sectors-per-track/cylinders) -- for a 1.44 MB 3.5" drive
  that is 2 heads x 18 sectors/track x 80 cylinders; for a 360 KB 5.25"
  drive it is 2 heads x 9 sectors/track x 40 cylinders. A single generic
  CHS driver that queries this once per drive and caches it, then does
  ordinary LBA<->CHS math against whatever came back, handles both
  transparently -- there is no need for two separate code paths, just one
  path plus a per-drive-number geometry cache. The one real caveat: real
  BIOS resolves each floppy *drive number* (`0x00`=A:, `0x01`=B:) to a type
  via CMOS Setup, not by inspecting whatever medium happens to be in the
  drive at boot -- so this only works cleanly if CMOS is actually
  configured with drive A as the 1.44 MB type and drive B as the 360 KB
  type (or vice versa), a one-time BIOS Setup step on that physical
  machine, not something software here can query and set for itself.
- **The trampoline needs a drive-number parameter it does not have today.**
  `disk::transfer`'s Rust-side call into the loader's trampoline only ever
  passes direction/LBA/buffer; the actual BIOS drive number is baked in on
  the assembly side, read once from `BOOT_INFO+8` (the boot drive DL
  captured at boot) and never varied. Adding a second device means
  extending that call ABI with an explicit drive-number argument (and a
  CHS-vs-extended mode flag, since the boot disk keeps using `AH=42h`/
  `43h` while a floppy uses `AH=02h`/`03h`/`08h`), not just adding a new
  Rust-side wrapper around the existing call.
- **`disk.rs` itself only needs a modest generalization, not a rewrite.**
  Today every function (`read_sector`/`write_sector`/`transfer`) implicitly
  means "the boot disk," using `BootInfo` for both the trampoline address
  and the sector-count bound. A floppy is a genuinely separate device with
  its own drive number, its own queried geometry, and its own bound (2880
  or 720 sectors) that has nothing to do with `BootInfo`'s disk-geometry
  fields -- so the natural shape is a small, explicit device
  parameter/enum threaded through these functions (boot disk vs. floppy
  unit 0/1), not a multi-device abstraction layered underneath every
  existing call. Because this kernel is single-threaded with no
  concurrent disk access, the existing single static sector buffer can
  still be shared sequentially across devices with no locking concerns.
- **`fosfs.rs` does not need to change at all for this scope.** Since this
  is secondary storage for raw sector access/education, not a second
  bootable filesystem, the floppy path can mirror the existing
  `readsector`/`hexdump` raw-access commands instead of extending FOSFS to
  understand more than one device. Putting an actual second FOSFS instance
  on a floppy is a real, separable follow-up (FOSFS's superblock/directory
  logic would need a device parameter threaded through it too), not part
  of this scope.
- **Naming: keep the existing boot-disk commands exactly as they are.**
  `diskinfo`/`readsector`/`format`/`ls`/`cat`/`write`/`rm` are unprefixed
  today because there has only ever been one disk; renaming all of them
  under an explicit `disk` prefix to mirror a new `floppy` namespace would
  be a disruptive, unforced command-surface break (every existing test
  script, `start` file, and README passage that already types
  `readsector`/`ls`/`cat` unprefixed would need updating for no functional
  gain). The lower-disruption shape mirrors this project's own `nic`/
  `tape` convention instead: add `floppy` as a wholly new namespace
  (`floppy status`, `floppy readsector UNIT LBA`, `floppy write UNIT LBA
  ...`, one/zero-based unit selecting drive A/B) alongside the untouched
  existing boot-disk commands. A bare `disk` alias that simply forwards to
  today's existing commands could still be added purely for naming
  symmetry if wanted, but it is cosmetic, not a functional requirement.

**Accepted UX design** (for this lab's actual two-drive setup; nothing
below is implemented yet):

```
floppy                          Show both drives: detected type/geometry, or "not responding"/"no disk"
floppy readsector [A|B] LBA     Raw sector read + hexdump (unit-scoped readsector clone)
floppy write [A|B] LBA TEXT     Raw sector write, printable ASCII, zero-padded to 512 bytes
tape load floppy [A|B] NAME     A third `tape load` source alongside `disk`/`net`
```

The `[A|B]` unit letter is optional whenever exactly one floppy is
attached (mirroring `nic`'s own "single interface stays active by default,
`nic use N` only needed once there is more than one" convention), and
mandatory -- with a clear, narrated error rather than a silent guess --
whenever both are. Parsing rule: the first token after the subcommand is
a unit selector if and only if it case-insensitively equals `A` or `B`;
otherwise it is the real argument (`LBA`/`NAME`), and the drive is
inferred, which only succeeds with exactly one floppy present. This works
because a bare `A`/`B` letter can never also parse as a valid LBA number.
`tape load floppy NAME` and `tape load floppy A NAME` are unambiguous by
the same reasoning `readsector`/`write` are: `tape load floppy` never
takes more than one argument besides an optional unit, so one remaining
word is always `NAME` and two are always `unit NAME`.

One accepted, undoctored edge case: a FOSFS tape file literally named `A`
or `B` (single-letter names are technically legal) would collide with the
unit-letter shorthand on a single-drive machine (`tape load floppy A`
reads as "drive A, missing NAME"). This is a one-line documented
limitation -- don't name a floppy tape file a bare `A`/`B` -- not something
worth solving with extra parsing cleverness, consistent with this
project's existing preference for small, explained trade-offs (e.g.
`term`'s COM1 lock-out) over cleverness.

Errors throughout are narrated, matching this project's existing
"explain the hardware state" convention (`nic`, GDT/PIC messages) rather
than surfacing a bare BIOS status byte: "no disk in drive," "write-
protected," "drive not responding," and (for the two-drives-present,
no-letter-given case) "ambiguous: both drive A: and drive B: are
attached; specify which one."

A native FDC/8237-DMA driver bypassing BIOS entirely (à la the deferred
native ATA/AHCI work) was also considered and set aside for the same
reason ATA/AHCI is deferred: the floppy controller's own timing/DMA/motor
quirks are a well-known source of fragile, fiddly driver code, and the
BIOS trampoline this project already leans on for the boot disk sidesteps
all of it for comparable effort. Booting fos *from* a floppy is a separate,
larger question again: the current 16 MiB GPT image (with its FOSFS
partition holding every compiled tape) does not come close to fitting in a
1.44 MB floppy's 2880 sectors, so that would mean a distinct, trimmed-down
boot layout in the spirit of `boot/netrom.S`'s own parallel/optional path,
not a change to the existing `image`/`run`/`test` layout.

### A second (non-boot) disk for config/log storage

Raised by the lab team: their test machines boot from an ordinary USB
thumb drive, and the actual motivation here is to stop needing to bake
config into the build (today, e.g., `tapes/router/router.cfg` only
reaches a running router via `tools/build_image.py --file router.cfg` at
build time, or the 4096-byte `tape run router @router.cfg` startup-
argument path) -- a second, persistent, live-editable disk sitting in the
machine would let config and logs be read/written directly without
touching the build pipeline or the boot medium at all.

**This is genuinely easier than the floppy work above, and shares most of
its plumbing.** A second hard disk (BIOS drive `0x81`, `0x82`, ...) needs
no CHS/geometry translation at all -- real BIOS EDD extensions (`AH=42h`/
`43h`) work identically for any hard-disk-range drive number, not just the
boot one. So the one change floppy support already requires --
parameterizing `disk::transfer`'s trampoline call with an explicit drive
number, instead of the hardcoded single value read once from `BOOT_INFO+8`
-- is *all* the register-level work a second disk needs; no new BIOS call
variant, no per-drive geometry cache.

What is new, specific to a second disk:

- **Discovery.** The boot drive arrives for free via `DL` at boot; nothing
  else does. A second disk needs to be probed for at kernel init -- try
  `AH=41h` ("check extensions present") against `0x81`, `0x82`, ... and
  treat a success response as "this disk exists," then `AH=48h` ("extended
  read drive parameters") for its sector count.
- **No GPT/MBR needed on the second disk.** The boot disk's whole MBR +
  primary/backup-GPT dance (`tools/build_image.py`) exists purely so it can
  also be a legitimate, standards-compliant *bootable* image. A disk that
  is never booted from has no such requirement -- it can be a single flat
  FOSFS volume starting at LBA 0, formatted once (by `format disk2`, see
  below) and left alone across every later `make image` rebuild of the
  boot medium, which is the entire point.

**Implemented design: option 2 -- a device argument on the existing FOSFS
commands, not a parallel command namespace.** `ls`/`cat`/`write`/`rm`/
`format`/`diskinfo`/`readsector` all gain an optional leading `diskN`
argument (`disk1` is the boot disk -- what these commands already mean
today -- `disk2` the first additional disk, `disk3` a third if one is ever
attached, and so on):

```
ls [diskN]
cat [diskN] NAME
write [diskN] NAME TEXT
rm [diskN] NAME
format [diskN]
diskinfo [diskN]
readsector [diskN] LBA
```

Omitting `diskN` keeps meaning exactly what it means today (the boot
disk) -- every existing script, `start` file, and test stays byte-for-byte
valid with zero migration cost. This also means `disk1` is never mandatory
to type; it exists purely so the numbering is uniform and a config/log
file can unambiguously say "disk2" without also needing a special word for
"the first one."

Parsing follows the same two rules already established for `floppy`/
`tape load floppy`: a leading `diskN` token is recognized as a device
selector only when it matches that strict pattern *and* names a disk that
is actually attached (never guessed); and for commands that take exactly
one further argument (`cat NAME`, `rm NAME`, `readsector LBA`), remaining-
word count disambiguates `NAME`-only from `diskN NAME` the same way `tape
load floppy NAME`/`tape load floppy A NAME` already do. The same class of
accepted edge case applies too: a FOSFS file literally named `DISK2` (a
syntactically legal name) would collide with the selector once a second
disk is actually attached -- another one-line documented limitation, not
worth solving with cleverness, consistent with the floppy `A`/`B` case.

**Scope of the `fosfs.rs` refactor.** `fosfs.rs` today holds no global
state of its own -- every function (`format`/`list`/`cat`/`write`/`remove`/
...) already takes `&BootInfo` purely to reach `disk::read_sector`/
`write_sector` and `disk::data_partition_lba`/`data_partition_sectors`.
Threading a device parameter through it is a mechanical, if broad, change:
every one of those function signatures gains a device argument that flows
down into `disk.rs`'s own (already-planned, for floppy) device-parameter
support, and a disk2-style device supplies its geometry from the `AH=48h`
probe above (start LBA 0, whole-disk sector count) instead of from
`BootInfo`'s boot-disk-specific GPT-discovered fields. No in-memory
directory cache or other per-device state exists to duplicate -- FOSFS
reads its superblock/directory fresh from disk on every operation already.

**As built.** `boot/stage2.S`'s `bios_disk_io` trampoline call gained a 5th
cdecl argument (the BIOS drive number, validated to the `0x80`-`0xff`
hard-disk range) and a third direction value (`2` = probe), which runs
`AH=41h` then `AH=48h` and copies back an 8-byte sector count. One real bug
surfaced and was fixed here: the probe path's AH=48h result buffer (at
`DISK_REQUEST+16`) overlaps the read/write DAP's own offset/segment fields
at `DISK_REQUEST+20`/`+22` (used to build the caller's ES:DI copy
destination) -- `AH=48h`'s "number of cylinders" output field lands on
those exact same two bytes, silently corrupting the destination pointer
right before it was used, which surfaced as `disk2: found (..., <huge
garbage number> sectors)` instead of the real sector count. The fix
stashes a second copy of that offset/segment at `DISK_REQUEST+42`/`+44`
(untouched by the probe's own buffer) immediately after computing it, and
the probe path reads back from there instead.

`kernel/src/disk.rs` gained a `Device` enum (`Boot`/`Secondary(usize)`), a
4-entry secondary-disk table populated by `discover_secondary_disks`
(probing `0x81`-`0x83` at boot, right after `disk::initialize`), and
device-aware `device_read_sector`/`device_write_sector`/
`device_data_partition_lba`/`device_data_partition_sectors`/
`device_sector_count` functions; `read_sector`/`write_sector` remain as
thin `Device::Boot`-only wrappers so every pre-existing call site outside
the seven commands above (tape ABI file callbacks, the `start` file,
`tape run router @router.cfg`, `tape load disk`, `tape send`) needed no
change and stays boot-disk-only by design. Every `fosfs.rs` function
threads a `disk::Device` parameter exactly as scoped above. `shell.rs`
gained `strip_disk_selector`/three arity-specific `parse_disk_*` helpers
implementing the disambiguation rule described above, wired into
`cat`/`write`/`rm`/`readsector`/`diskinfo`/`format`/`ls`.

Verified end to end (`make test-second-disk`, folded into `make test`):
booting with a second, blank 4 MiB QEMU drive attached is discovered as
`disk2` (8192 sectors), `format disk2`/`write disk2 NAME TEXT`/`ls
disk2`/`cat disk2 NAME`/`rm disk2 NAME` all work, and disk1's own files are
completely unaffected by any of it (this project's real hardware is
`0x80`+ USB thumb drives, which BIOS enumerates the same as this test's
attached drive). A secondary disk's practical size ceiling is BIOS/
firmware LBA support, not FOSFS -- the flat format itself has no meaningful
capacity limit; the fixed 64-entry directory limits file *count*, not
capacity, on any FOSFS volume regardless of size.

**Follow-up: `copy SRC_DISK NAME DST_DISK [NEW_NAME]`.** Copying a file
directly between two disks (e.g. `disk1` -> `disk2`) needed no host round
trip and no new on-disk mechanism, since `fosfs::read`/`fosfs::write` were
already binary-safe, device-parameterized functions operating on plain
`&[u8]` buffers -- `write`'s "TEXT" naming is historical, not a real
restriction. Unlike the seven commands above, `copy_command` requires
*both* disk selectors to be spelled out explicitly; letting either one
default to `disk1` would be genuinely ambiguous about which side it meant
once two disks are actually in play. `copy_file` stages the whole file
through the shared `pmem::TAPE_BASE`/`TAPE_LEN` buffer that `tape load
disk`/`tape load net` already use as scratch space (`allocate_tape_buffer`,
already bounds-checking and error-reporting), rather than adding a second
large `.bss` array that would inflate `kernel.bin` for no benefit -- this
also means a copy is bounded by the same 4 MiB the tape subsystem already
budgets for. Because that buffer is shared, `copy_file` never calls
`install_tape` (the copied bytes are an arbitrary file, not necessarily a
valid tape) but does explicitly clear the `TAPE` static and print a note
once the copy succeeds, if and only if a tape had actually been loaded
there before -- otherwise a later `tape run`/`tape status` would silently
misreport or attempt to execute whatever file was just copied through,
exactly the same hazard `tape load disk`/`tape load net` avoid by always
calling `install_tape` for themselves. Verified end to end by extending
`make test-second-disk`'s own scenario: `copy disk2 config.txt disk1`,
`copy disk1 note2.txt disk2 note2-copy.txt`, confirming byte-for-byte
content on each destination, and confirming a tape loaded earlier in that
same run reports "No tape loaded" (with the reused-buffer note printed)
immediately after a copy.

**Follow-up: `@diskN/NAME` for `tape run`'s file-argument syntax.** Once a
file like `router.cfg` could live on a secondary disk (via `copy` above),
`tape run router @router.cfg` -- previously hardcoded to `disk::Device::
Boot` in both `tape_argument_source` and `tape_run` -- had no way to reach
it there. `TapeArgumentSource::File` now also carries a `disk::Device`,
resolved by checking for a `/` inside the `@`-prefixed reference: a
`diskN` prefix before the `/` selects that disk (reusing the existing
`strip_disk_selector`, so an unattached or malformed `diskN` cleanly falls
through to the same `Invalid`/usage-message path as any other malformed
`@` reference, rather than silently defaulting); no `/` at all keeps
meaning the boot disk, exactly as before. A literal `/` is a safe,
unambiguous separator for this precise reason: FOSFS names may only
contain letters, digits, `.`, `_`, and `-`, so a real file name can never
collide with it. This works for *any* tape's `@FILE` argument, not just
`router`'s, since the `@`-detection logic already applied uniformly before
this change. Verified by extending `make test-second-disk`: `tape run
@disk2/expr.txt` (reading `"3 4 +"` written to disk2) runs the already-
loaded tape without a "not found"/usage error, while `tape run
@disk9/expr.txt` (no such disk attached) cleanly prints the usage message
instead of crashing or guessing.

**Follow-up: `tape load diskN NAME`.** The tape itself -- not just its
config/startup-argument file -- can now come from a secondary disk too.
`tape_load_command`'s `kind` word (`net`/`disk`) already only ever meant
"where does the tape come from"; `disk` was really always `disk1` spelled
the historical way. Rather than inventing a `diskN/NAME`-style prefix
(the `/` convention above exists specifically because `tape run`'s file
reference is embedded inside one `@`-prefixed token; `tape load`'s `kind`
and `NAME` are already separate words, so no such trick is needed here),
any other `diskN` word is now recognized as a `kind` in its own right via
the existing `strip_disk_selector`, dispatching to the same
`tape_load_disk`, now itself taking a `disk::Device` parameter instead of
hardcoding `disk::Device::Boot`. `disk` alone is kept as a permanent
alias for `disk1`, not deprecated, since it predates secondary disks
entirely and every existing script/test already uses it. An unattached or
malformed `diskN` (or any other unrecognized `kind` word) falls through to
a `"Usage: tape load [net|disk NAME|diskN NAME]"` message rather than
being misread as a file name. Verified by extending `make
test-second-disk`: `tape load disk2 config.txt` loads a plain-text file
from disk2 as a "legacy flat" tape (never `tape run` on it, since it is
not real code -- only proving the disk2 read path itself works), and
`tape load disk9 config.txt` (no such disk) prints the usage message and
leaves the previously loaded tape's state untouched.

**Follow-up: disk2 fallback for the boot-time `start` script, plus an
interactive `start` command.** The `start` file (see "Boot-time `start`
script" in README.md) previously had to live on `disk1`, forcing any
change to it through a rebuild of the boot medium -- exactly the
inconvenience the second disk exists to avoid. `shell.rs` gained a small
`StartFileLookup` enum (`Found(disk::Device, u64)` / `Missing` /
`Error(disk::Error)`) and a `locate_start_file` helper: check `disk1`
first, and only if it has no `start` file, check `disk2`
(`disk::Device::Secondary(0)`). The two checks are deliberately not
symmetric in how they treat an error: `disk1` always has a valid FOSFS
volume (it is the boot disk), so a real read failure there is reported;
`disk2` is commonly blank/unformatted (the default state for a machine
not yet using a config disk), and `fosfs::validate` on an unformatted
volume always returns `Err(disk::Error::FirmwareFailure)` -- indistinguishable
at this level from a genuine I/O fault -- so *any* error checking `disk2`
is folded into `Missing` rather than surfaced, avoiding a scary, spurious
error message on every ordinary boot with an unused second disk attached.

The former single `run_start_file` function was split into three: the
silent, boot-time `run_start_file` (unchanged behavior when no script
exists anywhere), a new interactive `start_command` (explicitly reports
`"start: no start file found on disk1 or disk2."` when absent, since a
human typing `start` deserves to know why nothing happened), and a shared
`execute_start_file(info, device, length)` doing the actual line-by-line
dispatch, parameterized by whichever device `locate_start_file` resolved.
`start` (no arguments) was added to `execute()`'s dispatch and to the
"no arguments accepted" error group. The main use case: after F12-ing out
of a long-running tape the script launched (e.g. `router`), `start` puts
the machine back into the same state a fresh boot would, without an
actual reset. Verified by extending `make test-start`'s existing scenario
with an interactive rerun (confirming the script executes a second time)
and by extending every other `qemu_smoke.py` scenario (which never
preload a `start` file) to confirm the "not found" message, including
`--second-disk`'s still-unformatted secondary disk correctly falling
through to `Missing` rather than an error.

**Follow-up: a real double fault from `start`, traced to inlining and a
too-small stack, not a logic bug.** Running the boot script's exact
commands manually always worked, but retyping them via the interactive
`start` command reliably produced "DOUBLE FAULT: switched to the
emergency task stack" whenever the script's `tape run` line was something
heavy, e.g. `tape run router @router.cfg` with several NICs, DHCP, health
checks, and NAT -- reproduced with a QEMU-monitor-driven script that
confirmed the exact same real `router.cfg` crashed a pre-fix kernel every
time and no longer did after the fix below, both via the real interactive
path, not just an approximation of it.

The root cause is two compounding effects, both invisible from the
source code alone:
- `kernel/Cargo.toml`'s release profile (`opt-level = "z"`, `lto = true`,
  `codegen-units = 1`) is aggressive enough that every command handler
  `execute()`'s `match` calls (`tape_run`, `nic_command`, `term_command`,
  ...) was being inlined directly into `execute` itself, even though only
  one handler's code path is ever live per call. That collapsed
  `execute`'s own stack frame to the *sum* of every command's stack
  needs (confirmed via `nm`/`objdump` on a debug build override,
  `--config profile.release.strip=false`, since the shipped profile
  strips symbols: `execute`'s frame was 5288 bytes before the fix, most
  of it dead weight for any one call). `#[inline(never)]` was added to
  every large handler function `execute` calls, shrinking `execute`
  itself to a thin ~1.7 KB dispatcher and letting each handler pay for
  its own frame only when it is genuinely on the call stack.
- That alone was not enough. The interactive `start` command
  unavoidably nests one real level of `execute` deeper than the boot-time
  path (`execute("start")` -> `start_command` -> `execute_start_file` ->
  `execute(script line)`, versus boot's `run` -> `run_start_file` ->
  `execute(script line)`, which never re-enters `execute` at the top
  level at all) -- and a heavy tape running at the bottom of that extra
  layer is a real, unavoidable added stack cost, not an artifact of
  inlining. Confirmed by re-running the same real-`router.cfg` repro
  against the kernel with the `#[inline(never)]` fix alone: it still
  double-faulted. `kernel/src/entry.S`'s `kernel_stack` was doubled from
  32 KiB to 64 KiB to give this path real headroom; `__kernel_end` sits
  tens of KiB below `linker.ld`'s `0x50000` ceiling even after this
  change, so the extra reservation is safe. Re-running the identical
  repro against the doubled-stack kernel completed the router script a
  second time via `start` with no fault.

Both changes were necessary together: the inlining fix alone measurably
shrunk `execute`'s frame but the two genuinely nested `execute` ->
`tape_command` -> `tape_run` chains (each now a real, separate call
sequence rather than one inlined blob) still summed to more than the
inlining fix alone saved; only adding the larger stack on top actually
closed the gap for the heaviest real-world case. `make test-stack-guard`
(unaffected -- it targets the separate emergency double-fault task stack,
not this ordinary kernel stack) and every other `make test*` target were
re-run after this fix with no regressions.

**Follow-up: `help` output was wrapping and scrolling itself out of
view.** Several `help` lines were longer than 80 columns, so VGA text
mode wrapped them onto an extra row each; combined with the original
line count, the full listing took over 30 rows once wrapped -- more
than an 80x25 screen holds, so its own earlier lines scrolled out of
view before the listing finished printing. Rewritten to 11 lines, each
kept to at most 79 columns, not 80: a `println!` whose text lands
exactly on column 80 wraps *and* leaves a spurious blank row for its
own trailing `'\n'`, so 79 is the true safe maximum. All the same
commands remain listed, just more tersely phrased (full syntax stays
in this document and README.md). `tools/qemu_smoke.py`'s matching
assertion string was updated to match; verified with a QEMU boot
printing `help` (each line lands as exactly one row, confirmed against
the real serial output) and by rerunning `make test` and
`tools/test_term_net_e2e.py`.

**Follow-up: `diskN/NAME` for the tape filesystem callback ABI (`ed`,
`router`).** `tapes/ed` and `tapes/router` are the only two tapes using
the kernel's `tape_fs_file_length`/`tape_fs_read`/`tape_fs_write`/
`tape_fs_remove` callbacks, and all four were hardcoded to
`disk::Device::Boot` -- so neither tape could read or write a file on
`disk2`, even though the shell's own `cat`/`write`/`rm`/`copy` commands
already could. Adding a `device` parameter to these callbacks was
possible under the append-only cdecl ABI rule, but would have required
updating every tape using this ABI (`ed` and `router`) to pass it, for a
change that is really only about what the existing `name` argument can
*mean*. Instead, `tape_fs_name` was replaced with `tape_fs_reference
(pointer, length) -> Option<(disk::Device, &'static [u8])>`, which parses
an optional leading `diskN/` prefix out of the name string via the
already-shared `strip_disk_selector` -- exactly the same trick `tape
run`'s `@diskN/NAME` argument uses (see above), just applied to this
callback's string instead. No `name` with no `/` still means the boot
disk, so no existing caller changes behavior. This means `tapes/router`'s
lease-file argument (`dhcp-lease-file NAME`, previously unrestricted)
transparently gained disk2 support for free, with zero code changes on
the tape side, though this has not yet been separately tested or
documented for `router` specifically.

`ed` itself needed one small change to avoid rejecting a legitimate
`diskN/` prefix before it ever reaches the kernel: its own `valid_name`
check (which rejects `/`, among other things) is now only applied to the
part *after* an optional `diskN/` prefix, via new `valid_reference`/
`is_disk_selector` helpers. `ed` never resolves the disk itself -- it just
validates the reference's shape and passes the whole string through to
the kernel callbacks unchanged, which is why its `save`/`written ...`
message reports the full reference (e.g. `written disk2/config.txt`), not
just the bare file name. Verified by extending `tapes/test_ed_e2e.py`
with a second, blank QEMU drive and a full `format disk2` / `write` /
`tape run ed disk2/NAME` / edit / save / `cat disk2 NAME` cycle.

### A tape that visits 64-bit long mode and comes back

`tapes/longmode64_shim.S` + `tapes/longmode.S` together are a hand-written
tape that jumps the running 32-bit kernel into 64-bit long mode, proves
it got there, and jumps back -- entirely inside one `tape run` call,
leaving nothing about the kernel's own GDT, page tables (fos never
otherwise enables paging), or IDT touched or replaced once it returns.
This is deliberately not the same thing as "Deferred optional x86-64
path" below: that section is about a permanent, parallel 64-bit boot
*target* built from the ground up; this tape is a small, one-shot, fully
reversible demonstration that the mode switch itself works, run from
inside the existing 32-bit kernel, using CPU features stage2's own
GDT/paging setup never touches.

The design is deliberately split into two files with a narrow, documented
contract between them, so the privileged mode-switch mechanics can be
reused by any future "visits 64-bit mode" tape without rewriting them:

- **`tapes/longmode64_shim.S`** owns everything privileged and reusable:
  the `tape_main` entry point the kernel calls, the CPUID long-mode
  check (with its own early-return path -- see below), the identity-
  mapped page tables, the private GDT, the full CR0/CR3/CR4/EFER/GDTR/
  EFLAGS save-and-restore, and both directions of the actual mode
  transition. Its only job besides that plumbing is calling one external
  symbol, `tape64_body`, once it has safely landed in 64-bit mode with
  interrupts off -- and restoring everything the instant that call
  returns.
- **`tapes/longmode.S`** (now much shorter than before this split) is
  just one *implementation* of `tape64_body`, written in hand-assembled
  `.code64`: it loads a 64-bit-only immediate (`0xdeadbeefcafebabe` -- a
  value no 32-bit `mov` could load in one instruction) and hex-dumps it
  directly to VGA text memory, exactly like `tapes/vga-colors` writes
  VGA, as proof that 64-bit-encoded instructions genuinely ran (this
  can't go through the `putc` callback: calling 32-bit code from 64-bit
  mode would need a whole second nested mode transition for one debug
  print, so it isn't done).

The `tape64_body` contract is intentionally minimal: an ordinary SysV64
`extern "C" fn()` -- no arguments, no return value -- called only after
the shim has already entered 64-bit mode (paging on, CR4.PAE/EFER.LME/
CS.L set, the shim's private GDT loaded, interrupts masked), free to do
whatever it wants, and expected to return via a plain `ret`. Because
SysV64 requires `%rsp` 16-byte-aligned at the point of `call` (unlike the
cdecl the shim's own 32-bit code otherwise follows), the shim saves its
pre-alignment `%rsp` in `%r15` (callee-saved, and otherwise unused)
immediately before the `call` and restores it immediately after --
critical, since leaving `%rsp` misaligned by even a few bytes would
corrupt the later 32-bit register-restore epilogue and the tape's final
`ret` back to the kernel. Any body -- hand-written assembly or, as below,
compiled from Rust -- just needs to define one global `tape64_body`
symbol matching this contract and link against `longmode64_shim.o`.

Mechanically, the shim builds its own tiny identity-mapped page tables
(three static 2 MiB pages covering physical 0..6 MiB -- the kernel's own
code/stack, 0x10000..0x60000; VGA text memory, 0xb8000; and the tape's
own 4 MiB image, `pmem::TAPE_BASE`..+`TAPE_LEN`) and its own two-
descriptor private GDT (a 64-bit code descriptor at selector 0x08, and a
flat 32-bit code descriptor at selector 0x10 used only as the landing pad
immediately after leaving 64-bit mode), then: `cli`s, saves CR0/CR3/
CR4/EFER/GDTR/EFLAGS, clears CR0.PG before changing the kernel's non-PAE
page-table mode, sets CR4.PAE, loads its own page tables into CR3, sets
EFER.LME, sets CR0.PG (entering IA-32e compatibility submode), swaps in its
private GDT, and far-jumps into 64-bit code, where it calls
`tape64_body` as described above. A `push`+`push`+`lretq` (the standard,
SDM-documented way to leave 64-bit mode for a fixed target, since a
plain far jump with an immediate target isn't encodable there) returns
to the compatibility-mode landing pad, which restores every saved
register in reverse order -- it first clears CR0.PG, restores CR4
(clearing PAE), CR3, EFER via `wrmsr` (clearing LME), the original GDTR,
and a far jump back onto the kernel's own code selector to reload CS;
only then does it restore CR0 (re-enabling the kernel's original
non-PAE paging) and EFLAGS (re-enabling interrupts last of all) -- before
printing a wrap-up message through the ordinary `putc` callback and
returning to the shell exactly like any other tape.

Two things make this materially different from every other tape in this
repository:

- **Interrupts stay off for the whole excursion, not just as a nicety.**
  `kernel/src/interrupts.rs` keeps the PIT (100 Hz, IRQ0) and PS/2
  keyboard (IRQ1) firing live the entire time the shell runs, including
  during a tape. That IDT is a 32-bit-format IDT, invalid to service an
  interrupt against while CR0/CR3/CR4/EFER/GDTR are mid-transition or
  while this tape's own private GDT is loaded -- so the `cli` above is a
  hard correctness requirement, and EFLAGS is restored (re-enabling
  interrupts) only after every other piece of state is back to normal.
- **The build needs a real `ld` link step, unlike `tapes/primes.S`.**
  x86-32 ELF relocations are REL-style: GAS bakes the resolved address
  directly into the instruction bytes at assemble time, so `primes.S`'s
  bare `as --32` + `objcopy -O binary` pipeline (no linker at all) works
  correctly. x86-64 ELF relocations are RELA-style instead: the resolved
  address lives only in a separate `.rela` section that a linker-free
  `objcopy -O binary` throws away entirely, leaving zeroed placeholders
  everywhere this file's `.code64` region references a `.code32` label
  or vice versa. `make tape-longmode` therefore assembles both
  `tapes/longmode64_shim.S` and `tapes/longmode.S` with `as --64`
  (needed for their 64-bit-context relocations, even though most of the
  shim's instructions are still ordinary 32-bit-encoded `.code32` code),
  links both objects together with `ld -Ttext=0x100000
  --oformat=elf64-x86-64 -e tape_main` (matching `pmem::TAPE_BASE`, so
  every label resolves to its real, final runtime address, and picking
  the shim's `tape_main` -- not the body's `tape64_body` -- as the entry
  point), and only then runs `objcopy -O binary`.

If CPUID (leaf 0x80000001, EDX bit 29) reports no long-mode support, the
shim prints a short message and returns immediately, without ever
calling `tape64_body` or touching CR0/CR3/CR4/EFER/GDTR/EFLAGS at all.
This matters for testing: `qemu-system-i386`'s TCG backend cannot emulate
the long-mode CPUID bit at all (confirmed: it prints `warning: TCG
doesn't support requested feature: CPUID[eax=80000001h].EDX.lm [bit
29]` and reports the feature absent), so this tape's real 64-bit
excursion can only be exercised under `qemu-system-x86_64` -- confirmed
to boot this same 32-bit `fos.img` identically otherwise.
`tapes/test_longmode_e2e.py` is one of the two e2e tests in this
repository that launch `qemu-system-x86_64` instead of
`qemu-system-i386`; it asserts on the shim's own serial-visible messages
and that the shell keeps responding to ordinary commands afterward (the
"64-BIT MODE REACHED" VGA text itself is only visible on the real screen
-- `make run` with this tape loaded -- since it bypasses the serial-
mirrored `putc` callback entirely). Built with `make tape-longmode`,
producing `build/tapes/longmode.bin`.

#### A second `tape64_body`, written in Rust: `tapes/longmode-rs/`

`tapes/longmode-rs/` is a second implementation of exactly the same
`tape64_body` contract described above, written in ordinary `#![no_std]`
Rust (`src/lib.rs`) instead of hand-assembled `.code64` -- proof the
shim/body split above is a real, reusable boundary, not just a
refactor for its own sake. It performs the identical VGA hex-dump proof
as `tapes/longmode.S`'s body, just via safe(-ish) `write_volatile` calls
to `0xb8000` instead of raw `mov`s, and is linked against the *exact same*
`tapes/longmode64_shim.S` object file -- no shim changes needed at all to
support a compiled-language body.

Unlike the ordinary Rust tapes in this repository (`tapes/hello-rs`,
`tapes/vga-colors`, `tapes/bigsieve` -- each a whole self-contained ELF32
binary compiled as a `bin` crate with its own linker script), `longmode-rs`
is a `staticlib` crate (see its `Cargo.toml`): it has no `_start`, no
entry point, and no linker script of its own, since it exists only to
provide one function that gets linked into a larger image alongside the
shim. `make tape-longmode-rs` builds it with `cargo`, then links
`build/tapes/longmode64_shim.o` against the resulting `.a` archive with
the same `ld -Ttext=0x100000 --oformat=elf64-x86-64 -e tape_main` recipe
`tape-longmode` uses -- the linker pulls only the one object it actually
needs (the one defining `tape64_body`) out of the archive automatically.

It also needs its own new custom Rust target, `targets/x86_64-fos-tape.json`
-- the first genuinely 64-bit target in this repository, unlike
`targets/i686-fos.json` every other Rust tape uses. Modeled on that file
but for `x86_64`, it additionally disables SSE/MMX/AVX and sets
`"rustc-abi": "softfloat"`: fos's kernel never sets CR4.OSFXSR/OSXMMEXCPT
(bits 9/10), since it never uses floating point, so any SSE-encoded
instruction -- including ones LLVM might silently emit for something as
mundane as a struct copy, not just explicit float math -- would fault
with `#UD` the instant it executed. (Confirmed necessary: an earlier
attempt without `"rustc-abi": "softfloat"` failed to build at all, with
`error: target feature sse2 is required by the ABI but gets disabled in
target spec`.) It deliberately uses `"code-model": "small"`, not the
`"kernel"` code model rustc's own builtin `x86_64-unknown-none` target
defaults to: `"kernel"` assumes high, negative-canonical addresses like
`0xffffffff80000000`, which is wrong for this tape's low, positive
`0x100000` load address (`pmem::TAPE_BASE`).

`tapes/test_longmode_rs_e2e.py` is the second (and, along with
`tapes/test_longmode_e2e.py`, only other) e2e test in this repository
that launches `qemu-system-x86_64`; it is a near-duplicate of that
script -- same shim-emitted serial messages, same shell-still-alive
check, a distinct multicast group/port so both can run concurrently --
proving the shim's mode-switch mechanics behave identically no matter
which language implemented the body that runs while briefly parked in
64-bit mode. Built with `make tape-longmode-rs`, producing
`build/tapes/longmode-rs.bin`.

## Deferred optional x86-64 path

Do not treat x86-64 as a compiler-target switch. BIOS begins in real mode; the
64-bit path needs CPUID long-mode detection, a 32-bit transition, PAE, identity
mapped early page tables (a 1 GiB 2 MiB-page map is a good initial teaching
layout), EFER.LME, paging enablement, and a far transfer to long mode.

Implement it as a parallel, optional boot path that retains the working
32-bit kernel as fallback. Add a separate `x86_64-unknown-none` target,
64-bit linker/entry code, and an explicitly versioned or architecture-neutral
boot ABI. Keep VGA, serial, PS/2, shell, FOSFS format, and command policy
portable while separating port I/O, CPU setup, interrupt entry, and pointer
conversions into architecture modules.

The 32-bit BIOS trampoline cannot be called unchanged by a long-mode kernel.
For an initial x86-64 milestone, make post-boot FOSFS access read-only or defer
it; prefer a native disk driver before attempting a 64-bit-to-real-mode
transition that preserves full 64-bit state.

Could this eventual path be prototyped as a tape first, the way
`tapes/longmode.S` above proves the mode switch itself? For early,
incremental work, plausibly yes, and it may be the better place to
start: a tape needs no boot loader, disk image, or partition layout
changes at all to iterate on, and most of what a real 64-bit path needs
early on -- VGA, serial, and PS/2 drivers re-hosted as native 64-bit
code, a 64-bit interrupt/exception setup, more generous page tables --
is just port I/O and MMIO with no real-mode dependency, exactly the kind
of thing that translates cleanly into a long, ambitious tape built on
`tapes/longmode.S`'s pattern. Two real limits stop that pattern from
covering the *whole* eventual path, though. First, disk I/O fundamentally
needs BIOS EDD calls, which need real mode: a tape wanting real FOSFS
access from 64-bit code would need its own, more elaborate 64-bit ->
32-bit -> 16-bit -> BIOS -> back-up trampoline (a taller version of the
one `kernel/src/disk.rs` already has for 32-to-16), or a native
AHCI/ATA driver -- already listed above as deferred work either way.
Second, and more fundamentally: a tape that *never returns* -- that
becomes the environment for the rest of the boot, instead of a bounded
excursion back to the 32-bit shell -- stops being "just a tape" in any
meaningful sense and starts being exactly this section's "parallel,
optional boot path", just entered via `tape run` instead of the boot
loader's partition layout. That may still be a genuinely useful way to
begin (no disk image to touch while the shape of things is still
changing), with promotion to a real, dedicated boot entry point coming
later, once/if this path becomes serious enough to be more than a demo.
