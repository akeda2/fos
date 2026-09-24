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
  until `map_mmio` registers a driver's exact BAR range, using at most
  three dynamic page tables -- kept at three rather than bumped for the
  local APIC's own fixed MMIO window (a fourth `map_mmio` consumer, see
  "MSI/MSI-X and the local APIC" below) because the reserved low-memory
  kernel region has no spare room for another whole 4 KiB page table right
  now; exhausting this pool just means MSI is unavailable that boot
  (every driver falls back to its legacy PIC IRQ), not a fault. This
  retains physical-pointer compatibility for DMA and the BIOS trampoline
  while leaving unrelated high ranges unmapped for useful page-fault
  diagnostics; it is not userspace isolation.
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
no QEMU model at all (see the Intel I350 discussion below) -- so this pattern
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

Two soft-reboot bugs were found and fixed during development, neither ever
hit by a hardware reset (both are exactly the class of surprise a warm
re-entry into `boot/stage2.S` predicts, since it's the first time that code
has ever run on a CPU that wasn't freshly reset by BIOS POST): a fifth,
previously undocumented location (`kernel/src/debug.rs`'s `SLOT1_LBA`/
`SLOT_SECTORS` constants) had drifted out of sync with the real disk layout,
making KUP write a freshly pushed kernel to the wrong LBA; and
`collect_memory_map` zeroed only the low 16 bits of `%ebp` (`xorw %bp, %bp`)
while comparing/storing the full 32-bit register, which is harmless on a
cold BIOS boot (POST happens to leave the upper bits zero) but made a soft
reboot's leftover register garbage abort the E820 loop immediately, reporting
zero tracked memory and cascading into "No supported Ethernet controller
found." Full root-cause detail, including the QEMU HMP debugging notes from
this investigation, is in `docs/history/development-notes.md`.

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
the very evidence needed to diagnose it. This was conclusively diagnosed by
attaching GDB to QEMU's own `hw/net/ne2000.c` to prove the device model
fully writes and advances the ring before the fos driver ever sees it,
isolating the bug to this driver's own header-field order. (`-object
filter-dump,id=ID,netdev=NETDEV_ID,file=PATH.pcap`, a QEMU netdev capture
filter, is a lower-effort complementary tool worth reaching for first in
any future "does the frame even arrive" investigation.)

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
(the Intel I350 scaffolding, discussed below) pushed `__kernel_end` to `0x51fb4` -- past
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
it; the Intel I350 discovery below is the motivating case.

`kernel/src/i350.rs` is a fifth NIC backend, structurally mirroring
`e1000.rs`'s driver shape but for Intel's I350 ("Powerville") PCIe Gigabit
Ethernet controller family -- a genuinely different, newer chip generation
than the `82540EM` QEMU emulates as `e1000`. `i350::CANDIDATE_DEVICE_IDS` is
deliberately an empty `&[u16]`: the module is fully wired into `nic.rs`'s
facade and compiles as ordinary, complete code, but with no device IDs to
match it can never actually claim a PCI device -- `0x1520` (the I350
Virtual Function/SR-IOV ID) is deliberately excluded, since it has a
completely different, mailbox-based register interface with no direct
PHY/link access; enabling every plausible I350 sibling ID at once was also
rejected, since fiber/backplane variants skip copper-PHY autonegotiation
entirely. The intended path is to add only the one confirmed, physically
tested ID first (read via the `pci` command below), then broaden
deliberately to sibling copper IDs only once each has actually been tested.
Like the 3C509 (see "Deliberately deferred work" below), this is a standing,
permanent real-hardware-only testing cost, since I350 has no QEMU model at
all. Full detail (the three register-layout differences from `e1000.rs`,
scaffolding this module's effect on the reserved low-memory region) is in
`docs/history/development-notes.md`.

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

`shell::tape_run` calls a tape as `extern "C" fn(putc, should_abort: extern
"C" fn() -> u32)`, passing `keyboard::tape_should_abort` -- a thin wrapper
around the same `poll_term_event`/`ESCAPE_SCANCODES` check every other
blocking shell loop uses -- as the second argument. This is
backward-compatible by construction: cdecl's caller-pushes/caller-cleans
convention means `primes.S`/`hello-rs` (declaring only the first parameter)
needed no changes. `tapes/vga-colors/` is the reference tape: it writes
directly to VGA text memory in a loop, cycling colors until `should_abort`
returns nonzero, verified with `tapes/test_vga_colors_e2e.py` (QEMU monitor
`sendkey f12` to simulate a local escape).

`tapes/tape-rt/` is a small shared `no_std` library crate (an ordinary
Cargo path dependency) providing the startup stub, panic handler, and
print helpers that `hello-rs`/`vga-colors`/`bigsieve` used to duplicate;
only `vga-colors` depends on it, since `hello-rs`/`bigsieve` are meant to
stay fully self-contained for `docs/rust-tapes-for-dummies.md`'s teaching
ladder.

A *remote* `term net` peer had no way to stop a looping tape, since
`term_net`'s own session loop -- the only thing that calls `nic.receive` --
is paused for as long as a nested tape's loop is running.
`combined_tape_should_abort` fixes this by also checking
`shell::poll_remote_tape_abort`, which polls the network card directly on
`term_net`'s behalf whenever a `term_net` session started the tape
(tracked via two statics, `ACTIVE_TERM_PEER`/`ACTIVE_TERM_SOURCE`),
recognizing `REMOTE_ABORT_BYTE` (`0x1b`, ASCII Escape) inside a
`TERM_TAG_DATA` payload or an incoming `TERM_TAG_BYE`. `tools/term_net.py
--interactive` puts its own stdin into cbreak mode so a lone Escape
keypress reaches the wire as a single byte rather than waiting for Enter.
Full detail (including the exact statics/queueing involved) is in
`docs/history/development-notes.md`.

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

An interactive tape's `putc` output used to reach `console::relay_push`'s
queue but sit there undrained for as long as the tape's own blocking loop
ran, since only `term_net`'s own outer loop used to drain it -- making a
remote client watching an interactive tape (e.g. `tapes/calc`'s REPL) see
no output at all until the tape finally exited. The fix,
`relay_pending_tape_output` (`shell.rs`), does what `term_net`'s loop
already does (drain `console::drain_relay`, forward via `send_term_data`)
and is called from both `poll_remote_tape_abort` and
`poll_remote_tape_input` before either does its own network polling --
so any `should_abort`/`read_key`-polling tape gets live output relaying
automatically, with no tape ABI change and no per-tape opt-in. This does
not change the batching behavior for non-polling commands like
`primes.S` or `tape_load_net`, which still arrive in one burst on return.
Full detail (including the exact test methodology proving the fix) is in
`docs/history/development-notes.md`.

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
its own distinct meaning (see the parallel-port driver discussion below)
rather than being added to this "give up" list, since it needs to mean something
narrower ("abandon the line I'm typing without executing it," only inside
`term_printer`) than every other key here.


`kernel/src/parallel.rs` is a deliberately narrow legacy-ISA
parallel/Centronics port driver, discovered via the BIOS Data Area
(`0x408`-`0x40E`, LPT1..LPT4) the same way `console::detect_ports` finds
COM1..COM4 -- there is no PCI parallel-card discovery, matching this
project's "ISA only" convention for the 3C509. It implements only the
original IBM PC "SPP" (Standard Parallel Port) register set (no
bidirectional/EPP/ECP modes); `Printer::write_byte` writes a byte, pulses
Strobe, and polls the Busy status bit for up to `BUSY_POLL_ATTEMPTS`
(200,000) reads before giving up and reporting a `PrinterFault`
(`NotSelected`/`PaperOut`/`ErrorLine`/`Busy`). Status/control bit polarity
is an explicit, documented real-hardware caveat: several Centronics signals
are active-low, and this driver's polarities follow the most commonly cited
convention but have only been checked against QEMU's `-parallel` emulation.

`shell.rs` adds `term printer`/`term LPT1`..`LPT4` (an exclusive terminal
mode, mirroring `term COM1`/`term_net`) and a `print` command pair
(`print [diskN] NAME`, `print run CMD...`), backed by a second, independent
print-relay queue in `console.rs` (`PRINT_RELAY_QUEUE`) fed from the same
`relay_push` tap point the network relay queue already uses. Typed input in
`term printer` is echoed live, teletype style (written to the physical
printer the instant it is typed, not buffered until Enter); F2
(`KeyEvent::KillLine`) abandons the local input buffer and prints a fresh
line break, since already-inked characters cannot be un-printed. `help`
refuses to run while a `term printer` session is active (to avoid burning
paper), except via `print run help`. `print run CMD...`/`print [diskN] NAME`
are nesting-aware: `console::print_relay_active()` is checked first so a
command run from inside an already-active `term_printer` session doesn't
split one physical print job into two. A tape's own live output also
reaches the printer via `relay_pending_print_output()`, called from
`combined_tape_should_abort`/`poll_remote_tape_input`, mirroring the
identical `term_net` fix. Real-hardware follow-ups (a false "out of paper"
report traced to a dead CMOS battery resetting the BIOS parallel-port
config; dot-matrix printers cannot show true character-by-character
teletype output since they buffer a full line internally; an interactive
tape's output was initially batched until it exited) are recorded in full,
along with the user's physical Panasonic KX-P1083 as the real-hardware
validation target, in `docs/history/development-notes.md`.

## Real hardware notes

A handful of behaviors have only ever surfaced on physical machines, not in
QEMU: VGA cursor/CRTC handling and boot narration order; A20 enable
ordering (BIOS, then 8042, then port `0x92`, re-verifying after each); PIC/
PIT command timing needing an `io_wait()` between command bytes; keeping
interrupts disabled during the entire protected-mode-to-real-mode BIOS disk
trampoline window; spurious IRQ7/IRQ15 needing real IDT gates instead of
`missing()` (the confirmed cause of one AMD Athlon 64 X2 reset loop); the
RTL8139's transmit interframe-gap field, CR/RCR/TCR write order, several
receive-ring-wedge conditions (a bad-status frame, a nonsensical length
field, and no link-recovery detection via `MSR`), and receive-ring
buffer/FIFO overflow at unpaced wire speed; and a predicted risk list for
`reboot soft` (A20 enabled a second time with no reset in between, the
kernel's own faster PIT rate during the trampoline, sibling bugs of the
`%bp`-vs-`%ebp` E820 bug, NIC/PCI state never power-cycled, and real BIOS/
USB-boot-media tolerance of a second, POST-less round of EDD calls).

Full root-cause detail for every one of these (including the QEMU `-d int`/
`-trace`-based diagnosis techniques used) is in
`docs/history/real-hardware-notes.md`.

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

### HLT-on-idle: letting the CPU actually rest at the idle prompt

Every polling loop in this kernel busy-spins by design -- there is no
scheduler, so "poll hardware, then loop" is how anything gets noticed at
all. But the one loop that is *always* running whenever nothing else is --
`shell::run`'s own idle prompt loop -- was spinning the CPU at full speed
even while genuinely idle (the common steady state, especially for a
debug-mode "unattended, always up" machine sitting at its own prompt
between remote sessions). `arch::x86::halt_until_interrupt` (distinct from
the existing `halt_forever`, which `cli`s and never returns -- this
executes exactly one `hlt` and returns once any interrupt wakes it) lets
that loop stop burning cycles/power/heat between polls, called only when a
whole iteration found nothing at all to do.

The reasoning for *not* needing the textbook `sti; hlt` atomic idiom here
(which exists to close the race where a wakeup becomes pending between the
caller's last check and `hlt` itself executing): every caller reaches this
function with interrupts already unconditionally enabled for the entire
life of the loop, and the PIT (IRQ0) is never masked and fires at a fixed
100 Hz regardless of anything else -- so even a "missed" wakeup right at
the call boundary only costs one more PIT tick (~10ms), never an
indefinite hang. This bounded worst case is a property of this specific
loop's environment, not a general guarantee `halt_until_interrupt` makes
on its own; a caller anywhere PIT ticks could ever be masked, or that
might reach this function with interrupts off, would need the full
`sti;hlt` idiom instead.

This is deliberately scoped to *only* `shell::run`'s idle loop, not
`term_net`'s session loop, `nic_sniff`, `nic_discover`, `chat_command`, or
any tape network-transfer loop (`tape_load_net`, `netfs_get`/`put`,
`nic_discover`'s own loop). Every one of those is actively polling NIC
receive with no interrupt backing at all (every NIC backend's own ISR only
acks/counts/flags a diagnostic `work_pending` nothing actually waits on --
`receive()` always polls hardware directly regardless), so none of them
are "idle" in the sense this feature targets, and halting there would add
the same latency for zero benefit.

The one loop this does apply to also happens to be the *only* one that
serves NETFS GET/PUT and KUP traffic (`ambient_discovery_poll`, see
"NETFS" above and "KUP" under "Debug mode" below), which arrives in paced
chunks only a few milliseconds apart (`tools/send_kernel_update.py`'s
default `--delay 0.003`). Halting the instant a single iteration finds
nothing would halt right after draining one chunk while the next is still
in flight, adding up to one PIT tick of latency *per chunk* instead of
just once per idle stretch -- a real, roughly 3x slowdown of an
already-working feature, not a rounding error. `ambient_discovery_poll`
therefore returns whether it actually received a frame this call, and
`run`'s loop only halts once `AMBIENT_IDLE_GRACE_TICKS` (2 ticks, ~20ms --
comfortably above that ~3ms chunk pacing) have passed with no NIC activity
*and* no local keyboard/serial byte. An active transfer keeps refreshing
that deadline faster than it can elapse, so the loop never halts
mid-transfer; only genuine idle time triggers it.

One accepted, bounded caveat: COM1 (serial input) has no interrupt behind
it at all (IRQ4 is never unmasked), so once the machine is truly idle and
halting, a serial-typed byte can incur up to one PIT tick (~10ms) of added
latency before it's noticed, versus effectively 0ms before this change.
Verified with `make test` (its bounded waits are 10-second-scale, utterly
unaffected) and `make test-debug-mode` (KUP is served from this exact
loop), plus a manual re-run of `tools/test_netfs_e2e.py` and
`tools/test_hostname_discovery_e2e.py` to confirm no throughput regression
in the paced-transfer traffic this change is riskiest against.

## Real-time audio streaming: the ES1370 driver, and Intel HD Audio

`kernel/src/es1370.rs` and `kernel/src/hda.rs` are two independent PCI
sound-card drivers, exposed to tapes through a shared, format-agnostic
facade (`kernel/src/audio.rs`, mirroring `nic.rs`'s own multi-backend
shape) and the `audio`/`hda` shell commands. ES1370 (Ensoniq/Creative
"Sound Blaster PCI", QEMU's `-device ES1370`) implements DAC1 (playback,
one always-on stereo 16-bit channel with a default test tone) and ADC
(recording) as two independent, simultaneously running DMA channels, with
up to `MAX_INSTANCES` (4) cards supported at once. Intel HDA ("HD Audio",
QEMU's `-device intel-hda` + a codec such as `hda-duplex`) is a genuinely
different architecture -- a generic CORB/RIRB DMA command-transport engine
talking to one or more codecs on a private bus, each exposing a node graph
(root -> Audio Function Group -> DAC/ADC/pin-complex widgets) -- with
stream-descriptor/BDL-based playback and capture, real per-codec channel-
count negotiation (`AC_WCAP_CHAN_CNT_EXT`, up to `MAX_NEGOTIATED_CHANNELS` =
8), and up to `MAX_INSTANCES` (2) controllers supported at once.

The tape ABI's audio callbacks (trailing, append-only cdecl, after the
`nic_*`/`ticks` set): `audio_sample_rate`/`audio_channels_and_bits`/
`audio_submit`/`audio_capture` (device 0 only, kept for backward
compatibility) and their indexed counterparts `audio_count`/
`audio_sample_rate_at`/`audio_channels_and_bits_at`/`audio_submit_at`/
`audio_capture_at`. `audio_submit`/`_at` push raw PCM into a lock-free,
network-fed ring (`PcmRing`, 16 KiB per direction per device -- a real,
measured link-time codegen ceiling, not an arbitrary choice) that the
DAC's own ISR drains each period; calling it at all permanently switches
that device from its default test tone into network-fed mode.
`tapes/audio-net/` is the reference tape: `tape run` (listen mode, receiving
one or more per-device-tagged network sub-streams and forwarding them to
local playback), `tape run send NAME [MAC]` (replays a pre-recorded FOSFS
file), and `tape run capture [MAC]` (genuine live-microphone streaming,
round-robining every local device via `audio_capture_at`).

Both drivers are, as of this writing, unverified against real hardware
(every register offset/bit/DMA semantic was cross-checked directly against
QEMU's own emulation source, the same rigor applied to every other driver
here after the RTL8139 lessons in "Real hardware notes" above) -- the
user's box of physical Sound Blaster PCI/HDA cards is the intended
validation target. A real, previously-undiscovered kernel bug (two PCI
devices sharing one legacy IRQ line silently broke whichever device's
`init()` ran second) was found and fixed while testing this feature; see
"shared PCI IRQ lines" above. Real-microphone testing on a dev machine
surfaced two host-side QEMU/PipeWire pitfalls and one seat0/logind
device-ACL gotcha, unrelated to fos itself.

The full chronological development history -- the ES1370 interrupt-storm
bug (a level-triggered status bit the driver never acknowledged, causing an
interrupt storm that looked exactly like "no interrupt fired"), the
`.bss`/link-budget investigation behind `PcmRing`'s sizing, multi-instance
ES1370, the audio-net wire-protocol's per-device tagging design, and every
HDA increment (discover-only, CORB/RIRB + node-graph enumeration,
multi-controller, stream-DMA playback, capture, and multichannel
negotiation) -- is in `docs/history/audio-hda.md`.

## MSI/MSI-X support: a local APIC, and Intel HDA as the first consumer

`kernel/src/lapic.rs` is a minimal local APIC driver, added specifically
to receive Message Signaled Interrupts (MSI) -- fos's interrupt story was
previously the legacy 8259 PIC alone (`interrupts.rs`), fed by INTx lines
via `pci::PciAddress::interrupt_line()`/`register_pci_irq`. The PIC keeps
running exactly as before for every existing backend (RTL8139, the
selected 3C905 variants, Intel e1000, the NE2000-compatible clone, and
ES1370, which predates MSI and has no such capability to find); the local
APIC is purely additive, used only to receive MSI-delivered interrupts,
never to relay legacy INTx lines (its LINT0/LINT1 Local Vector Table
entries are deliberately left masked/as firmware set them), and there is
still no I/O-APIC anywhere in this kernel, since MSI bypasses I/O-APIC
redirection entirely and needs none.

`lapic::initialize()` runs once, unconditionally, from
`interrupts::initialize()` (right alongside the PIC/PIT setup already
there): it checks CPUID.1:EDX bit 9 (reusing the existing `cpu::features()`
wrapper), reads `IA32_APIC_BASE` (MSR `0x1b`, via new `arch::x86::rdmsr`/
`wrmsr` helpers -- the first MSR access anywhere in this kernel) to find
the LAPIC's physical MMIO base (architecturally `0xfee00000`), maps that
one 4 KiB window with the existing `paging::map_mmio` (the same mechanism
every BAR-mapped driver already uses), and programs the Spurious-Interrupt
Vector Register with a dedicated vector (`lapic::SPURIOUS_VECTOR`, `0x60`)
-- mirroring the PIC's own IRQ7/IRQ15 spurious-vector convention for
continuity, though a genuine spurious *local APIC* interrupt needs no EOI
at all, unlike the PIC's ISR-check dance. Every real chipset since the
Pentium era, and every CPU model QEMU emulates (even single-core, even
the default `i440fx` machine type), has always had a local APIC, so this
is a defensive check rather than an expected failure path; if it ever
does fail (no APIC, or `map_mmio` has no free dynamic page table slot --
see below), `lapic::initialize()` returns `false`, the boot narration
says so, and every MSI-capable driver transparently falls back to its
legacy PIC IRQ path exactly as if the feature didn't exist.

`pci.rs` gained a capability-list walk (`PciAddress::find_capability`,
bounded to a small fixed hop count against a malformed list) -- Status
register bit 4, then the linked list at the Capabilities Pointer, offset
`0x34` -- and `PciAddress::enable_msi(vector, destination_lapic_id)`,
which finds the MSI capability (ID `0x05`; MSI-X, a different, table-based
capability with ID `0x11`, is not implemented -- no current device needs
it), writes the Message Address (`0xfee00000 | (destination_lapic_id <<
12)`, physical destination mode, no redirection hint -- this single-core
kernel only ever has the one destination) and Message Data (delivery mode
fixed, the vector in the low byte -- MSI is always edge-triggered per the
PCI spec, so there is no trigger-mode bit to set, unlike a legacy PIC IRQ
gate), then sets the Message Control register's MSI-enable bit. Only the
simplest case is handled: one vector, a 32-bit message address; a
64-bit-capable device still works correctly with just its Message Upper
Address register zeroed, since every address this kernel ever targets
fits in 32 bits regardless.

`interrupts.rs` reserves a new vector range, `MSI_VECTOR_BASE` (`0x50`)
through `0x5f` -- clear of the CPU exception range, the PIC's remapped
`0x20`-`0x2f`, and the new LAPIC spurious vector `0x60` (`gdt::
DOUBLE_FAULT_TSS_SELECTOR`, `0x30`, is a GDT *selector*, a different
namespace entirely, not a conflict) -- with `allocate_msi_vector`/
`register_msi_irq`/`dispatch_msi_irq`. Unlike `register_pci_irq`'s
`PCI_IRQ_HANDLERS`, which deliberately holds up to `MAX_SHARED_HANDLERS`
handlers per legacy line (see "A real bug this test found: shared legacy
PCI IRQ lines..." in `docs/history/audio-hda.md`), `MSI_HANDLERS` holds
exactly one handler per vector: MSI's entire point is exclusive, unshared
ownership, unlike legacy INTx sharing, which is a motherboard-topology
fact no driver controls. Each generated MSI IDT gate calls
`lapic::send_eoi()` -- a genuinely different acknowledgment mechanism
from the PIC's own `send_eoi` (an MMIO register write instead of an `outb`
command sequence) -- and the two are never mixed for one interrupt.

`hda.rs` is the first (and, for now, only) consumer: `init_controller`
tries `lapic::is_available()` -> `interrupts::allocate_msi_vector()` ->
`address.enable_msi(vector, lapic::id())` before falling back to its
previous `address.interrupt_line()` + `register_pci_irq` path, storing
whichever succeeded as `Hda::msi_vector`/`irq`. `handle_pci_irq` matches
an incoming byte against either field (`irq` values 0-15 and MSI vectors
`0x50`-`0x5f` never overlap, so this is unambiguous) -- the existing
"every controller checks its own status register, so a non-matching call
is a harmless no-op" reasoning from the shared-legacy-line fix applies
unchanged. `hda status` reports `"MSI enabled, vector 0x.."` with a live
fire count when MSI is active, or the previous legacy-IRQ wording
otherwise; ES1370 is completely untouched, since `find_capability(0x05)`
naturally returns `None` for a device that predates MSI, and takes the
same legacy path it always has.

`kernel/src/pci.rs`'s `pci` command output also gained an `msi=yes/no`
flag per discovered function (capability *presence*, not whether this
kernel actually enabled it for that device) -- read-only, at no writing
cost, useful for confirming a card the same way `pci`'s vendor/device ID
dump already helps identify unknown hardware.

**Accepted low-memory constraint**: the local APIC's own `map_mmio` call
is a fourth potential consumer of `paging.rs`'s dynamic-page-table pool
(alongside e1000/i350/HDA), but that pool was deliberately *not* widened
from 3 to 4 slots -- confirmed by trying it, not assumed: even one more
static 4 KiB page table pushed `__kernel_end` past `linker.ld`'s
`0x90000` ceiling, meaning this kernel's low-memory budget currently has
essentially no spare room at all. If this pool is ever genuinely
exhausted (3 *other* high regions already mapped when the LAPIC tries to
map its own), the failure is graceful: no local APIC, no MSI that boot,
every driver on its legacy path -- not a fault. Growing the pool (and the
reserved region alongside it, in the same synchronized way this
document's low-memory growth history already describes) is the right fix
if a real scenario ever needs 4 simultaneous high MMIO mappings; nothing
here needs it pre-emptively.

Verified end to end by dropping the `-device intel-hda,msi=off` workaround
from `tapes/test_hda_playback_e2e.py`/`tapes/test_hda_capture_e2e.py`
(previously forced off specifically because this feature didn't exist yet
-- see those files' own doc comments) and asserting the new `"MSI
enabled, vector 0x.."` narration instead: both tests still produce real,
non-silent captured audio exactly as before, now proven driven by
MSI-delivered interrupts rather than a legacy IRQ. `tapes/test_hda_e2e.py`
and `tapes/test_hda_multi_e2e.py` (which never forced `msi=off` even
before this feature existed) continue to pass unmodified, as does
`tapes/test_audio_multi_e2e.py` (an RTL8139 sharing legacy IRQ 11 with
two ES1370 cards -- the exact historical shared-line scenario, still
legacy-only since ES1370 has no MSI capability), plus `make test`,
`make test-nic-watchdog`, `make test-stack-guard`, and
`make test-page-protection`.

## Deliberately deferred work

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

### MSI-X: still deferred

Plain MSI (one vector, capability ID `0x05`) is now built -- see "MSI/MSI-X
support" above. MSI-X (capability ID `0x11`) is a different, table-based
mechanism (a per-entry MMIO table rather than fixed config-space
registers, and support for many more vectors per device) that no current
consumer needs: `hda.rs`'s single vector is enough for one controller's
interrupt stream, and every other backend either has no MSI capability at
all (ES1370) or hasn't been converted (the NIC family). Worth building if
a future device (a real, physically tested I350, most plausibly) turns
out to need per-queue vector affinity -- not a prerequisite for anything
working correctly today, including I350's still-empty
`CANDIDATE_DEVICE_IDS`.

Multi-core and a genuinely `hlt`-and-wake-redesigned polling loop (as
opposed to the single idle-prompt-loop-scoped "HLT-on-idle" feature
above, which only covers the one loop that's actually idle, not NIC-
receive-dependent loops) remain the two things that would make MSI's
lower, non-shared delivery latency start to matter beyond what it already
does (removing the shared-legacy-line hazard). See "Cooperative/
preemptive scheduling" below for why multi-core specifically remains
deferred.

### Cooperative/preemptive scheduling: investigated, not yet built

Raised as "what would a real scheduler cost, and what would it buy us?"
while designing `poll_background_work` above (which is deliberately the
much cheaper alternative, scoped to kernel-native recurring work only --
see that section). A genuine scheduler, cooperative or preemptive, would
need several pieces this kernel has none of today: per-task stacks and a
small, fixed-size task-control-block table (no heap exists, so this would
be a fixed array of TCBs, each with a statically-sized stack, mirroring
`entry.S`'s single kernel stack/guard page just multiplied per task); a
context-switch primitive (save/restore the full register set plus stack
pointer) and a scheduling policy on top of it; and a cooperative-vs-
preemptive choice (cooperative needs no timer-interrupt integration but
only helps once something actually calls `yield()`; preemptive needs
every switch point audited for what state is safe to interrupt). By far
the most expensive part, though, is auditing every place this kernel
currently assumes single-threaded execution, not building the mechanism
itself: `disk.rs`'s BIOS real-mode trampoline is the sharpest example --
it briefly disables paging and restores the real-mode IVT/PIC mapping to
make one BIOS call, a window during which nothing else can safely run
without corrupting CPU/segment state (see "Protected-mode IRQ window
during BIOS disk calls" above) -- a scheduler would need this, and every
similar assumption throughout `shell.rs`/`nic.rs`, either pinned off or
redesigned entirely.

Genuine concurrency would let a long-running foreground task (e.g. a
`router` tape) share CPU time with an interactive shell instead of
occupying it exclusively, and would let a future blocking primitive avoid
busy-spinning -- but neither is a pressing need today: `poll_background_work`
already gets the low-cost share of that benefit with none of the single-
threading-audit cost, and no tape currently needs to run in the background
while the shell keeps working. **Conclusion: revisit a real scheduler only
if a future tape or workload genuinely needs concurrent execution; until
then this remains a deliberately deferred, larger architectural project.**

### ISA bus support and the 3C509: investigated, not yet built

Every NIC backend today (`rtl8139.rs`, `threecom.rs`, `e1000.rs`) is found
through `pci.rs`'s PCI configuration-space walk (Mechanism #1, ports
`0xcf8`/`0xcfc`); this project has no ISA bus code at all. A 3Com 3C509 was
considered as a next NIC target and investigated, but deliberately not
started: card discovery is the genuinely new, harder half (a 3C509 in its
default ISA Plug-and-Play state needs an LFSR-derived wake-up key sequence
and a serial-identification/CSN-assignment protocol to be assigned an I/O
base and IRQ in software -- a real, standalone `isa.rs` subsystem this
project has never needed; jumperless-configuring a card once via the old
DOS `3c5x9cfg.exe` utility to always answer at one fixed, known I/O port
sidesteps this at the cost of a one-time manual setup step), while the
register-level driver itself would actually be simpler than the PCI
backends already built (plain programmed I/O through a window-based FIFO,
no bus-mastering or descriptor rings at all). No ISA machine was available
to validate discovery either way, so nothing was implemented. QEMU's
`-device help` lists no 3C509 model at all, unlike every PCI backend built
so far, so this driver would have no automated `make test` coverage ever --
every iteration on both discovery and the register-level driver would need
real hardware permanently, a standing cost worth weighing alongside the
discovery-protocol choice when deciding whether/when to build this driver.

Floppy drive support was investigated but not built: this project's disk
stack is built exclusively around INT 13h's *extended* (`AH=42h`) LBA
read/write service, while real floppy BIOS support is legacy CHS
(`AH=02h`/`03h`/`08h`) -- a genuinely different BIOS call convention needing
its own CHS conversion, plus a drive-number parameter the trampoline call
ABI does not have today. QEMU emulates this well (`-fda`/`isa-fdc`), so
unlike the 3C509 this would keep full automated test coverage. A full
accepted design (per-drive geometry caching via `AH=08h`, a new `floppy`
command namespace alongside the untouched existing disk commands, and the
accepted `A`/`B`-vs-file-name-collision edge case) is recorded in
`docs/history/development-notes.md`, along with why a native FDC/8237-DMA
driver and floppy-boot support were both set aside.

A second (non-boot) hard disk (BIOS drive `0x81`, `0x82`, ...) is supported
for config/log storage without touching the boot image, discovered at boot
via `AH=41h`/`AH=48h` against each candidate drive number. Every FOSFS
command (`ls`/`cat`/`write`/`rm`/`format`/`diskinfo`/`readsector`) accepts
an optional leading `diskN` argument (`disk1` is the boot disk, the
existing default; omitting it is unchanged from before this feature).
`copy SRC_DISK NAME DST_DISK [NEW_NAME]` copies a file directly between two
disks through the shared `pmem::TAPE_BASE`/`TAPE_LEN` staging buffer;
`tape load diskN NAME`/`tape save diskN NAME` and `tape run`'s
`@diskN/NAME` startup-argument syntax, and the tape filesystem callback
ABI's `diskN/NAME`-prefixed references (`tapes/ed`, `tapes/router`'s
`dhcp-lease-file`), all reuse the same `strip_disk_selector` parsing rule.
The optional boot-time `start` script also falls back to `disk2` if
`disk1` has none, and an interactive `start` command lets an operator
manually re-run it (e.g. after F12-ing out of a long-running tape) without
a full reboot.

Full design/implementation detail, plus two bugs found and fixed along the
way (a real double fault from `start` traced to inlining collapsing
`execute`'s stack frame plus a too-small kernel stack for one extra level of
nested `execute` calls, fixed by `#[inline(never)]` on large command
handlers and doubling the kernel stack to 64 KiB; and `help`'s own output
wrapping/scrolling itself out of an 80x25 screen), are in
`docs/history/development-notes.md`.

## A tape that visits 64-bit long mode and comes back

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

### A second `tape64_body`, written in Rust: `tapes/longmode-rs/`

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
