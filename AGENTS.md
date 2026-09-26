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
  `0x10000..0x9f000` region, not the old single-transfer 127-sector
  convention, is the current kernel-size limit.
- `kernel/` is a freestanding Rust binary. `linker.ld` reserves the initial
  low-memory kernel region through `0x9f000`; do not place loader, boot-info,
  or disk-trampoline data there.
- `kernel/src/entry.S` reserves a 64 KiB kernel stack with a low-end canary
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
  `0x9f000` reserved-region ceiling, not any BIOS/hardware limit. FOSFS
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
  four dynamic page tables -- covering e1000/i350/HDA's own BAR mappings
  plus the local APIC's fixed MMIO window (see "MSI/MSI-X support" below);
  exhausting this pool just means MSI is unavailable that boot (every
  driver falls back to its legacy PIC IRQ), not a fault. This retains
  physical-pointer compatibility for DMA and the BIOS trampoline while
  leaving unrelated high ranges unmapped for useful page-fault
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
fixed address below `0x9f000`, and update it in the same change that adds one:

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
| `0x10000`-`0x9f000` | `stage2.S`, `netrom.S`, `kernel/linker.ld` | The Rust kernel's load address and reserved low-memory region. |
| `0x100000`-`0x500000` | `kernel/src/pmem.rs` (`TAPE_BASE`/`TAPE_LEN`), any tape's own linker script | The fixed 4 MiB region every tape (`tape load disk`/`tape load net`) is always loaded into, carved out of `pmem`'s general frame pool at boot so `allocate`/`allocate_contiguous`/`palloc` can never hand these frames to anything else. See "Tapes: fixed load address, and writing one in Rust" below. |
| `0x500000`-`0x501000` | `stage2.S`, `netrom.S` | Protected-mode stack pointer during boot (one page, stack grows down from the top). Relocated here from a fixed `0x9f000` -- see below -- once the flat GDT is loaded and every segment register already points at it, this stack has no remaining reason to stay below 1 MiB; `pmem::initialize` reserves this page defensively (via `reserve_range`, alongside the tape region immediately below it) even though nothing runs concurrently with stage two's own brief setup-then-far-jump-to-`kmain` window, including `reboot::soft_reboot`'s identical re-entry into this same code. |

This region's own hard outer limit is `0x9f000` -- and, more fundamentally
than any EBDA convention, `0xA0000` is the true, non-negotiable wall
immediately above it: VGA's own memory-mapped I/O window begins there on
every x86 PC, so nothing reserved for the kernel may ever cross it
regardless of how the EBDA itself is configured. Measure real headroom
with the linked `__kernel_end` symbol's own address, **not** `size
build/kernel.elf`'s text/data/bss column sums: those sums silently ignore
the `ALIGN(4096)` padding gaps `linker.ld` inserts between
`.text`/`.rodata`/`.data`, so a small, isolated content-size change can
cost a full extra 4 KiB page (or two) of real position with no visible
warning in `size`'s own output -- exactly the mistake that first produced
this section's now-corrected "roughly 31 KiB of slack" claim (see the
AC'97 section below for the full story). Get the trustworthy number with
`nm build/kernel.elf | grep __kernel_end` (needs a temporary `strip =
"none"` in `kernel/Cargo.toml`, since the shipped profile strips all
symbols) before assuming a small `.bss` addition is free, not the
linker's own `ASSERT` failing after the fact. If this genuinely runs out
again, the sustainable fix is moving the reserved region's own contents
to make more room (e.g. shrinking `.bss`, or reducing `kernel_stack`'s
size), not nudging `0x9f000` itself toward the hard `0xA0000` ceiling.

## Educational verbosity

This project exists to be read and explained, not just to run. Boot stages
and kernel subsystems should narrate what they are doing and why in their
status messages (e.g. which BIOS interrupt is being called, what a PIC remap
or IDT load accomplishes), not just announce bare completion. When adding a
boot step or kernel subsystem, prefer an explanatory message over a terse one,
and keep `README.md`'s command table, its "Tape command-line reference"
table (every tape's own `tape run [ARGS]` syntax, kept in one place rather
than scattered across each tape's own source), and "What happens at boot"
walkthrough in sync with the actual command surface and boot sequence. A
new tape's own doc comment remains the source of truth for its syntax;
add or update its row in that table in the same change. Inline documentation
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

## Keeping this file itself small: moving detail to `docs/history/`

This file is loaded into every session's context, so its size has a real,
ongoing cost distinct from any other doc in this repo -- unlike
`README.md` or `NETWORKING.md`, which are read on demand. Treat it as a
*current-state reference*, not a project journal: architecture facts, ABI
contracts, command syntax, the low-memory map, and build/test commands
belong here because a future change has to keep them accurate. A
chronological account of how something was investigated, debugged, or
iterated on does not need to stay accurate going forward -- it only needs
to stay findable -- so it belongs in `docs/history/` instead.

Concretely, move a section (or trim it while adding new content) once it
is mostly one of these: a bug postmortem ("X was found and fixed, root
cause was Y"); a "first we tried A, then B" design-iteration narrative;
detailed end-to-end verification/test narration beyond naming the test
file; or a fully investigated-but-deliberately-deferred feature writeup
whose reasoning won't change unless someone actually starts building it.
Keep in this file: the resulting current behavior, the file/function names
involved, and any constraint a future change must respect.

The mechanics, matching what every existing `docs/history/*.md` file
already does (see `development-notes.md`'s own intro paragraph): pick an
existing thematically-matching file (`development-notes.md` as the general
catch-all; `audio-hda.md` for sound hardware; `real-hardware-notes.md` for
physical-machine-only findings; `networking-and-discovery.md` for the
chat/discovery/NETFS/idle-loop cluster) or start a new one if none fits,
and append the section's *original, unedited* text at the end of it under
a `## <original title> (full detail)` heading followed by the original
heading(s) and body verbatim -- never paraphrase what you're moving. Leave
a short paragraph in its place here: what the thing is, its current
behavior/facts, and a closing "Full detail ... is in
`docs/history/<file>.md`." sentence. `docs/history/` files are
append-only dumping grounds, not edited for style after the fact, and are
never loaded automatically -- link to the right one by name rather than
assuming a reader has seen it.

## Disk image and safety

`tools/build_image.py` creates `build/fos.img`, a 16 MiB raw image with a
protective MBR and primary/backup GPT metadata:

| Location | Contents |
| --- | --- |
| LBA 0 | MBR / stage-one loader |
| LBA 2048 | GPT boot payload partition |
| First 32 payload sectors (LBA 2048-2079) | Stage-two loader |
| LBA 2080 | One-sector A/B kernel-slot metadata block (see "Debug mode" below) |
| LBA 2081 onward (`SLOT_SECTORS` each, slot 0 then slot 1) | The two fixed-size kernel slots -- currently 1144 sectors each, sized to the whole reserved low-memory region (`KERNEL_RESERVED_END`); see below |
| Immediately after both slots (`DATA_START_LBA`) | Dedicated GPT-discovered FOSFS data partition containing README and all compiled tapes |
| Final 33 sectors | Backup GPT entry array and header |

The image builder is the single source of truth for partition placement and
writes both GPT copies. The kernel validates the primary GPT and discovers the
FOSFS partition by its type GUID, with loader-provided geometry retained as a
compatibility fallback for older images. `tools/write_layout.py` emits
matching stage-loader constants from the actual kernel size; `SLOT_SECTORS`,
`SLOT1_LBA`, and `DATA_START_LBA` are all computed there, not hardcoded
elsewhere -- `DATA_START_LBA` in particular is deliberately derived from
where slot 1 actually ends (`SLOT1_LBA + SLOT_SECTORS`), not an independent
literal: an earlier, independent hardcoded value (`4096`) silently ended up
*inside* slot 1's range once `KERNEL_RESERVED_END` grew enough to push
`SLOT_SECTORS` past it, a real (if dormant, since no kernel had yet grown
large enough to actually write into the overlap) 32-sector overlap between
the kernel slot and this partition's own superblock/directory sectors. Never
add host block-device writes, automatic device selection, or commands that
let FOSFS write outside its own partition. Copying an image to a device is
destructive and must remain an explicit user action.

Only one of the two fixed-size kernel slots is ever the boot loader's actual
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

Two fixed-size kernel slots (see the disk layout table above) let a new
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
| `KUP_TAG_PUT_ERROR` (`0x3e`) | Server -> client. One-byte code: `1` `TOO_LARGE` (bigger than one kernel slot, or the shared staging buffer), `2` `CHECKSUM_MISMATCH`, `3` `DISK_ERROR` (writing the slot, or reading metadata, failed), `4` `BUSY` (another update is already in flight). |

`ambient_discovery_poll` -- the same idle-prompt-loop hook that also serves
NETFS and ordinary discovery -- dispatches all three request-carrying tags
via a shared `dispatch_kup_frame` helper, gated behind
`debug::kernel_update_enabled()`. Unlike NETFS (still idle-prompt-only --
see that section above), KUP is **also** dispatched from both of
`term_net`'s own receive loops (waiting for a `HELLO`, and an active
session's own loop) through that same helper, so a kernel update can be
pushed while `term_net` is running instead of requiring a local escape to
the idle prompt first -- added specifically because real-hardware
debugging sessions kept needing to interrupt an active `term_net`
connection just to push one fix. KUP's own transfer state
(`serve_kup_put_*`) is independent of whatever `term_net` itself is doing
with the NIC, so this is safe; the shared-staging-buffer caveat below
still applies regardless of which loop happens to be polling for it. KUP
remains unavailable inside `chat`, `nic discover`, or a running tape's own
polling loop, none of which call `dispatch_kup_frame`.

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

Every `cargo build` invocation in the Makefile (the kernel plus every Rust
tape -- none of these are a Cargo workspace) shares one `CARGO_TARGET_DIR`
(`build/cargo-target`), so `-Z build-std`'s `core`/`compiler_builtins` and
any shared local path dependency (e.g. `tape-rt`) compile once and are
reused across crates instead of once per crate; Cargo's own target-dir
locking keeps this safe under `make -j`. The four tapes that visit 64-bit
long mode (`tape-longmode`, `tape-longmode-rs`, `tape-cpu64-lab`,
`tape-cpu64-show`) link against one shared, file-tracked
`build/tapes/longmode64_shim.o` prerequisite rather than each
independently re-assembling it -- also required for `-j` safety, since
four phony recipes each writing that same path with an inline `as`
command would race. `make all -j$(nproc)` builds the image and every tape
in well under a minute on a clean checkout on typical multi-core hardware;
prefer `-j$(nproc)` over a bare `-j` so job count doesn't run away as more
tapes get added.

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
`bootinfo`, `memmap`, `memory`, `cpuinfo`, `dmesg`, `hexdump ADDRESS LENGTH`,
`halt`, `reset`, and `reboot [soft|hard]`.

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
runtime. It kept growing after that (`0x70000` for ES1370 ADC capture,
`0x80000` for multi-instance ES1370, `0x90000` later still -- see
`docs/history/audio-hda.md` for the exact measurements behind each step),
each time for the same reason: some feature's own `.bss` growth left no
headroom under the previous ceiling.

It was then grown again to `0x98000` -- this time deliberately, not
reactively, alongside a fix to a real bug this exact history had already
caused twice: `kernel/src/debug.rs` used to keep its
own hand-copied `SLOT1_LBA`/`SLOT_SECTORS` (see "A/B kernel slots and
automatic fallback" below), and *both* of the `0x70000`/`0x80000` steps
above required a manual, easy-to-forget update to that copy -- the first
time this was missed, it caused a real, fully deterministic
soft-reboot-after-KUP crash; the second time (the `0x80000` -> `0x90000`
step, undocumented until this investigation found it) went unnoticed only
because no kernel had yet grown large enough to actually write into the
resulting mismatch. Both `kernel/src/debug.rs`'s slot constants and
`tools/build_image.py`'s FOSFS partition start (`DATA_START_LBA`, which
had *also* silently drifted to sit partly inside kernel slot 1 once
`SLOT_SECTORS` grew enough) are now generated/derived from
`tools/write_layout.py` instead of hand-copied, making both classes of
drift structurally impossible rather than merely documented against a
third occurrence -- see `tools/write_layout.py`'s own doc comment and the
`kernel` `Makefile` target.

All three constants (`linker.ld`'s `ASSERT`, `pmem::RESERVED_BELOW`,
`tools/write_layout.py`'s `KERNEL_RESERVED_END`) must still always match
by hand -- see "Low-memory map" above for why those three specifically
can't be generated the same way.

Most recently, grown again to the current `0x9f000` -- the sustainable
fix "Low-memory map" above had named but not yet applied, rather than a
reaction to a specific feature's `.bss` growth this time: the boot-time
protected-mode stack pointer (`stage2.S`/`netrom.S`'s `protected_entry`)
was relocated from a fixed `0x9f000` to `0x500000` (see "Low-memory map"
above), reclaiming the entire `0x7000` (28 KiB) gap that used to exist
purely to leave that stack room below the EBDA. This raised the
kernel's own reserved-region ceiling all the way up to that boundary
with no fresh safety margin needed -- `0x9f000` was already the
conservative value the stack's own placement had chosen -- turning
roughly 3.5 KiB of remaining headroom into roughly 31 KiB, verified with
`size build/kernel.elf` and confirmed safe by `make test-debug-mode`,
which exercises `reboot::soft_reboot`'s repeated re-entry into this
exact relocated-stack code path, including a real KUP-triggered reset.

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

### `dmesg`: a kernel log ring buffer

`console.rs` keeps a small, always-on ring buffer (`KLOG`, currently 3072
bytes -- see "Low-memory map" above; this kernel's `.bss` headroom below
`0x9f000` is only a few KiB, all of which every kernel feature competes
for) of every byte any `println!`/`print!` in this kernel has sent, tapped
at `write_serial` -- the same single choke point `RELAY_QUEUE`/
`PRINT_RELAY_QUEUE` already use, so no call site anywhere needs to know it
exists. Unlike those two, which are opt-in and drop new bytes once full
(losing a live session's most recent bytes would corrupt it), `KLOG` is
always capturing and, once full, overwrites its own *oldest* bytes instead
-- ordinary dmesg-ring semantics, appropriate for a log read after the
fact. `KLOG_DROPPED` counts overwritten bytes so `dmesg` (`shell.rs`) can
report when its own history is incomplete rather than silently claiming
completeness.

`console::seed_klog_from_vga`, called once from `console::initialize`
right after it reads back the CRTC cursor position, seeds this ring with
whatever text is already sitting in the VGA framebuffer from row 0 through
that cursor -- `stage1.S`/`stage2.S`'s own real-mode narration (and, under
QEMU, even SeaBIOS's own pre-boot banner), still exactly where BIOS
teletype output left it, since the screen is never cleared at boot (see
"Prompt" in README.md). This is the only way `dmesg` can "replay the full
bootup process" at all, given that code is 16-bit real mode with no notion
of this ring buffer. Because the ring is small, ordinary shell use can
evict this narration within a couple thousand bytes of subsequent output
-- `tools/qemu_smoke.py` runs `dmesg` immediately after boot specifically
to catch it before that happens, and asserts stage one's own VGA-only text
(never sent to COM1 directly, so its appearance in that serial capture at
all proves the VGA seed, not just live COM1 mirroring) comes back out
through it. `dmesg`'s own dump (`console::print_dmesg`) suppresses the
ring's own capture for its duration (`KLOG_SUPPRESSED`) so replaying the
log does not also feed the replay back into itself -- everything printed
*before* that suppression starts, including the command's own typed-input
echo and `dmesg`'s own status header, is fair game and legitimately
reappears in the replayed output, which is correct: the ring is exactly
what was written to the console, in order, nothing more, nothing less.

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
than the `82540EM` QEMU emulates as `e1000`. `i350::CANDIDATE_DEVICE_IDS`
now carries exactly one confirmed real device ID, `0x1521` (copper),
read via `lspci -vv` against a physical Intel Ethernet Server Adapter
I350-T2 -- `0x1520` (the I350 Virtual Function/SR-IOV ID) remains
deliberately excluded, since it has a completely different, mailbox-based
register interface with no direct PHY/link access; enabling every
plausible I350 sibling ID at once was also rejected, since fiber/backplane
variants skip copper-PHY autonegotiation entirely. The intended path
broadens deliberately to sibling copper IDs only once each has actually
been tested, never guessed from the datasheet alone; the register-level
implementation itself remains unverified against real hardware regardless
of the ID now being enabled -- QEMU has no I350 model at all, so, like the
3C509 (see "Deliberately deferred work" below), every remaining bit of
verification here is a standing, permanent real-hardware-only cost. Full
detail (the three register-layout differences from `e1000.rs`,
scaffolding this module's effect on the reserved low-memory region) is in
`docs/history/development-notes.md`.

### UHCI: a USB host controller driver, and isochronous USB Audio playback

`kernel/src/uhci.rs` is a minimal UHCI (USB 1.1, I/O-port-based) host
controller driver, discovered via `pci::find_uhci_controllers`'s class-code
walk (class 0x0C/subclass 0x03/prog-if 0x00), supporting up to
`uhci::MAX_INSTANCES` (4) controllers. Bring-up (`Uhci::reset_and_start`)
builds a page-sized Frame List pointing at one fixed control Queue Head
plus a pool of up to `TD_POOL_CAPACITY` (34) Transfer Descriptors; there is
no IRQ handler (UHCI has no MSI capability, and enumeration's control
transfers have no real-time deadline), so everything is foreground-polled.
`Uhci::control_transfer` builds a SETUP+DATA+STATUS TD chain and polls each
TD's status with a 500 ms deadline. The `usb` shell command lists every
discovered controller/port and drives full standard enumeration (device +
configuration descriptor dump) on demand, always resetting and
re-enumerating from scratch rather than caching a previous result.

**Isochronous playback (`usb play [diskN] NAME`)**: `IsocOutStream` keeps a
rolling window of `ISOC_TD_COUNT` (250, i.e. ~250 ms of look-ahead --
enough margin against real host-scheduling jitter when serviced from
`poll_background_work` rather than a tight dedicated loop) Frame List
entries primed ahead of hardware's own `USBFRNUM` position, and tracks
`error_count` for any packet whose TD shows an error bit on reuse. Minimal
USB Audio Class 1.0 descriptor parsing (`find_uac_playback_endpoint`/
`find_uac_device`) requires PCM/16-bit, a fixed per-frame packet size
(`(rate/1000) * channels * bytes_per_sample`, computed as a nominal value
even when the endpoint's own `wMaxPacketSize` is larger under Adaptive
sync), and Synchronous/Adaptive (not Asynchronous) sync. Confirmed working
end to end against a real, physically attached class-compliant USB Audio
device (a TerraTec Aureon Dual USB, `0ccd:0077`, via QEMU `usb-host`
passthrough), not just QEMU's synthetic `usb-audio` device.

**`audio.rs` integration**: `uhci::UsbAudioDevice` is a third `Audio`
variant (`Audio::Uhci`), discovered once at `audio::initialize()` time
alongside ES1370/HDA and kept serviced by `poll_background_work`'s
`uhci::service_active_audio` call -- there is no isochronous IRQ path, so a
foreground loop must keep priming it or it starves. `IsocOutStream` can
source from either a fixed buffer (`usb play`) or a `PcmRing` (network-fed
playback, mirroring `Es1370::fill_period`).

**Isochronous capture (`usb capture [diskN] NAME`)** is real-hardware-only
-- QEMU's synthetic `usb-audio` device has no capture support at all.
`find_uac_device` discovers both playback and capture endpoints from one
single port-reset/enumeration pass (a device's two directions can have
different formats -- the real TerraTec card is stereo playback/mono
capture), and `IsocInStream` uses its own dedicated capture TD pool
(separate from playback's, since both directions can run concurrently
once `audio.rs` keeps a playback stream continuously running).
`Audio::capture_format()` lets a tape read the capture format
independently of the playback format; the tape ABI's
`audio_capture_sample_rate`/`audio_capture_channels_and_bits` (and
`_at`-suffixed) callbacks expose it. Confirmed on the real TerraTec card
with both directions running simultaneously.

**USB Mass Storage (Bulk-Only Transport)**: `Uhci::bulk_transfer` is a
data-stage-only transfer primitive (a caller-owned DATA0/DATA1 toggle;
internally split into successive bounded `TD_POOL_CAPACITY`-sized passes
for a transfer larger than one TD chain can hold) underlying
`kernel/src/usb_storage.rs`, which implements BOT's CBW/CSW framing and a
SCSI CDB subset (`MscDevice::inquiry`/`wait_until_ready`/`request_sense`/
`read_capacity_10`/`read_10`/`write_10`, thin wrappers around
`MscDevice::command`) against a device whose native block size is exactly
512 bytes. Any transport error drives BOT's own reset-recovery procedure
(`bulk_only_mass_storage_reset` + `clear_endpoint_halt`). Real UHCI
hardware needs a settle delay between bulk-transfer stages, and another
before reusing TD-pool memory after a timeout (`wait_ticks`, in both
`bulk_transfer` and `control_transfer`) -- found and fixed against real
hardware, not QEMU. The standalone `usb storage` command stays read-only
by design (discovers the first device, prints `INQUIRY`/`READ CAPACITY`,
hex-dumps sector 0); `write_10` is exercised only through the `disk.rs`
integration below.

`disk::SecondaryDisk` gained a `backend` field
(`SecondaryBackend::Bios(u8)`/`SecondaryBackend::Usb(MscDevice)`, the
latter carrying its own device handle directly so more than one secondary
disk can be natively USB-backed at once -- see `usb native` below), so a
USB Mass Storage device becomes an ordinary `diskN` with no other
`disk.rs`/`shell.rs` changes needed. `usb storage attach` (never automatic --
discovering a device means calling `uhci::initialize_all()`, which resets
every UHCI controller and is confirmed to break that same device's own
pre-existing BIOS-backed `diskN` access via legacy USB support, not only a
USB-booted `disk1`) registers the discovered device as the next `diskN`.
`usb storage promote` goes further and switches `disk::Device::Boot`
itself over to the native driver, cross-validating the discovered USB
device against `disk1`'s own BIOS-read MBR bytes plus a capacity floor
(not an exact match -- a real Pentium 4's legacy-USB BIOS reports a wrong,
image-baked capacity for its own boot stick) before switching, and
refusing (`BootPromoteError::NotSameDisk`) otherwise; `bootinfo` reports
whether `disk1` is currently promoted. Confirmed end to end on the real
Pentium 4 machine this whole driver was built for: `usb storage promote`
correctly identified its genuine USB boot stick and `ls`/`cat`/`write`/`rm`
kept working through the native driver afterward. `reboot soft` remains a
deliberately unaddressed gap even once promoted, since stage two always
depends on BIOS to reload the kernel from scratch.

`usb native` (`disk::promote_all_and_attach`) generalizes `usb storage
promote` from "always exactly `disk1`" to every disk a boot can match:
`usb storage promote`/`usb storage attach` only ever protected the boot
disk or attached one brand-new device, but any *secondary* disk that also
happens to be BIOS-USB-legacy-backed (a real, non-hypothetical case --
`attach_usb_disk`'s own doc comment already records finding exactly this
under QEMU, before `usb native` existed to fix it) carried the identical
risk with no promote path of its own. `usb native` reads every
not-yet-USB-backed disk's own BIOS baseline (`disk1` plus every secondary
`SecondaryBackend::Bios` disk) before touching UHCI at all, discovers
every attached USB Mass Storage device in one `usb_storage::open_all`
pass (each given its own address via a new shared `uhci::
allocate_device_address` allocator -- also adopted by `uhci::
find_uac_device`'s USB Audio discovery, so a simultaneous Mass Storage +
Audio device no longer collide on a single hardcoded address), and
cross-validates each baseline against an unused discovered candidate with
the exact same content/capacity-floor rule `usb storage promote` already
used, trying every remaining candidate rather than giving up after one
(`disk.rs`'s `try_promote_with_candidate` puts a rejected candidate
through `MscDevice::recover`'s BOT reset-recovery first, so its toggle
state stays valid for a retry against a different disk). Any discovered
device matching no existing disk is attached fresh as a new secondary
disk, same as `usb storage attach`, so one command now covers both jobs
for every disk that needs it.

Confirmed on real hardware with two genuinely simultaneous USB Mass
Storage sticks (not just two QEMU `usb-storage` devices): a fresh boot's
first `usb native` call promotes every matching disk correctly, but
running `usb`/`usb native`/`usb storage attach`/`usb storage promote`
more than once in the *same* boot can degrade BIOS's own USB Legacy
Support mapping for **any** BIOS-USB-legacy secondary disk, not only the
boot disk as previously documented -- a later call in that boot can then
fail to cross-validate a still-genuine secondary disk against its own
device and instead attach it fresh as an apparently new, unrelated disk.
Reset the machine before each fresh USB-discovery attempt when more than
one BIOS-USB-legacy disk is at stake, rather than iterating these
commands live against an already-touched boot. Full detail (including an
unrelated, intermittent, never-root-caused `SET_ADDRESS` enumeration
timeout hit during the same session) is in
`docs/history/development-notes.md`.

Verified end to end by `tapes/test_uhci_e2e.py`, `test_uhci_play_e2e.py`,
`test_uhci_audio_facade_e2e.py`, `test_usb_storage_e2e.py`,
`test_usb_storage_disk_e2e.py`, and `test_usb_boot_promote_e2e.py` (the
first function boots `build/fos.img` from a QEMU `usb-storage` device to
reproduce the real Pentium 4 scenario end to end; a second function,
`test_promotes_multiple_disks`, extends this with a *second* attached
`usb-storage` device that becomes an ordinary BIOS `disk2`, and confirms
`usb native` promotes both disks in one pass). Full detail (every
bug found along the way -- UHCI's port-enable/`PORTSC`-write-back
requirements, the isochronous TD endpoint-field and missing-error-checking
bugs, a real device's `bMaxPacketSize0`=8 off-by-one, the bulk-transfer
settle-time and TD-pool-size bugs -- and the exact verification narration
for each increment) is in `docs/history/development-notes.md`.

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
at runtime today, and the real enforced limit is `linker.ld`'s `ASSERT
(__kernel_end < 0x98000, ...)` (the exact ceiling has grown several times
since; see "Low-memory map" above for the current value and its own
remaining headroom), not the flat file's byte count. Whatever caused the
~28 KB growth described here was evidently something other than pure
`.bss`-truncation behavior at that point in this project's history; take
the mechanism explained below as the current, verified one. Stage two now
avoids the former single-transfer 127-sector ceiling by splitting kernel
loading into bounded INT 13h extended-read calls that respect each BIOS
64 KiB boundary. The remaining limit is the reserved `0x10000..0x9f000`
low-memory region, currently `SLOT_SECTORS` (1144) sectors, and
`tools/write_layout.py` emits that limit.

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

`tapes/calc/` (depending on `../tape-rt`) is the tape that motivated the
`read_key`/`args_ptr`/`args_len` ABI additions above. It evaluates reverse
Polish notation, chosen because it needs only a stack and one left-to-right
token pass -- no precedence table or parentheses. `tape run 3 4 +` evaluates
via startup arguments and returns immediately; plain `tape run` starts an
interactive `read_key`-based REPL (never calling `should_abort` alongside
`read_key`, per that section's warning above). Errors (stack underflow,
division/remainder by zero, `i32` overflow, an unrecognized token, a full
stack, leftover operands) report a friendly message rather than panicking.
Verified end to end with `tapes/test_calc_e2e.py`. Built with `make
tape-calc`. Full design detail (the exact `EvalError` cases and test
scenarios) is in `docs/history/development-notes.md`.

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

`poll_background_work(nic: Option<&nic::Nic>)` (`shell.rs`) is one shared,
cheap-to-call-unconditionally hook that drains the print-relay queue, relays
queued `term_net` output when nested inside an active session, and calls
`nic.watchdog()`. It replaces four independent, slightly-diverged copies of
this same polling pattern that had grown across `term_net`'s loops, `nic
sniff`, `nic discover`, `chat_command`, and the tape-callback polling
functions -- and, in the process, fixed a real gap where NIC watchdog
recovery only ever ran from `nic`/`nic list`/`nic watchdog`'s own command
handlers, never from inside any long-running loop. It is deliberately
scoped to kernel-native recurring work only, not to backgrounding a *tape*
itself -- see "Cooperative/preemptive scheduling" below for why a real
scheduler remains out of scope. Full detail (the four duplicated call sites
audited, and confirmation that the tape ABI/e2e suite are unaffected) is in
`docs/history/networking-and-discovery.md`.

### `term net` stays resident after a remote disconnect

`term_net`'s "wait for `HELLO`, then run a session" body runs inside an
outer `loop`. Only a *local* escape (F12 etc.) sends `TERM_TAG_BYE` and
returns to the local shell; every other way a session ends (a remote
`TERM_TAG_BYE`, or a fresh `HELLO` superseding a stale one) prints a
disconnect message and re-enters the "waiting for a `HELLO`" state with no
local action required. Both call sites (`run`'s automatic boot-time call
and `term_command`'s manual `"term net"`) share this one function. A
serial-only operator therefore has no way back to the local prompt except
driving everything over `term net` itself or briefly attaching a local
keyboard -- the old "connect then disconnect" trick no longer falls
through. Full detail (why this changed, and the affected e2e tests) is in
`docs/history/networking-and-discovery.md`.

### `term net` at boot, and more "give up" function keys

`shell::run` calls `term_net` once, automatically, before entering the
ordinary interactive prompt loop, so every fos machine waits for a remote
terminal client by default at every boot. A local operator watching a real
screen presses one of the "give up" keys to reach the local prompt
immediately instead of waiting; there is deliberately no serial equivalent
(reading one byte to mean "give up" would eat the first character of real
serial input). `keyboard::ESCAPE_SCANCODES` recognizes six function keys --
F12, F4, F6, F7, F8, F10 -- since some terminal emulators/KVMs intercept
F12 itself before it reaches the guest; F12 stays first and remains the
documented default. F2 is reserved separately for `term_printer`'s
"abandon this line" behavior (see the parallel-port driver below), not
added to this list. Full detail (why several function keys instead of one)
is in `docs/history/networking-and-discovery.md`.

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

### USB audio and USB-boot media do not reliably coexist on real hardware

Confirmed on two different real machines, for two different reasons,
neither fixable in software with the approaches tried so far. On a
Pentium 4 (USB-booting via UHCI), any command that calls
`uhci::initialize_all()` (`usb`/`usb play`/`usb capture`/`audio`) breaks
BIOS's own "USB Legacy Support" `INT13h` emulation for the boot disk
(`FOSFS disk error: FirmwareFailure`); only a genuine hardware reset
(`reset`/`reboot hard`, or a power cycle) recovers it, since `reboot soft`
never touches the CPU's RESET line. Host Controller Reset alone (not just
Global Reset) is enough to trigger this -- there is no gentler
UHCI-touching operation to fall back to. On an AMD Athlon 64 X2
(USB-booting), BIOS POST itself fails to find any boot device at all
whenever a USB audio device is physically connected -- a firmware/hardware
limitation that happens before fos ever runs, not a fos bug.

Practical conclusion: treat "don't use USB audio and USB-boot media
together" as the safe assumption on real hardware in general, not a
per-machine quirk to work around case by case. The native USB Mass
Storage driver (see "UHCI: a USB host controller driver..." above) closes
the Pentium 4 case specifically: `usb storage attach`/`usb storage
promote` let fos access its own boot disk without depending on BIOS's
`INT13h` emulation staying intact at all, confirmed on the actual Pentium
4 machine that exhibited this problem. It does not help the AMD machine's
POST-time failure (which happens before fos ever loads), and `reboot
soft` remains a gap even after promotion, since it always depends on BIOS
to reload the kernel from scratch regardless of `disk1`'s promoted state.
Full detail (the Global-Reset-removal experiment that falsified the
original "Global Reset specifically" hypothesis, and the exact
`FirmwareFailure`/no-boot-device symptoms observed on each machine) is in
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

`send_tape.py` keeps a small on-disk JSON address book (`~/.cache/
fos_peers.json` by default, overridable via `FOS_PEERS_FILE`) mirroring the
kernel's own `PEERS` table, reused by `term_net.py` and `tools/peers.py`.
Every discovery either tool performs merges newly seen peers into it in
first-seen order; `--mac` on either tool accepts a full MAC address or a
1-based index into this list, exactly like `chat`/`tape send`. `tools/
peers.py` is the standalone equivalent of `nic peers`/`nic discover`: no
arguments prints the cached list, `--discover` broadcasts a probe and
merges replies, `--forget` empties it (needed since, unlike the kernel's
own table, this one survives indefinitely across invocations and can go
stale). Full detail is in `docs/history/networking-and-discovery.md`.

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

`kernel/src/shell.rs` loads an optional `hostname` file the same way
`locate_start_file` looks for `start`, checking `disk1` then every
secondary disk in order; contents are trimmed and capped at
`HOSTNAME_CAPACITY` (16 bytes). `interface_hostname(index, out)` appends a
1-based interface number to the configured base name (`myhost1`,
`myhost2`, ...), since one machine can have several NICs; this formatted
name is what's sent/displayed everywhere. `nic list` prints a
`hostname: NAME` line per configured interface once one exists.

`send_chat_control` appends the formatted hostname as a payload after the
existing tag byte on every `CHAT_TAG_DISCOVER_REQUEST`/`REPLY` frame;
neither side validates an exact/maximum frame length, so old and new
senders/receivers interoperate. The peer table (`Peer`, replacing the old
bare-MAC array) holds an optional hostname per entry; `remember_peer` only
fills in a hostname for an entry that doesn't have one yet (first-seen name
wins for that boot). `print_peers`/`nic peers` show `MAC (name)` once
known.

`ambient_discovery_poll()`, called once per iteration only from the idle
local-prompt loop (the one loop already proven safe to extend with silent
background polling), does two things every fos machine needed but no
existing loop provided: it drains and answers incoming discovery probes/
replies on every interface with no console output at all, and it
self-announces on a schedule -- one immediate broadcast when the idle loop
starts, then every `AMBIENT_BOOT_BROADCAST_TICKS` (~3s) for
`AMBIENT_BOOT_BURST_TICKS` (~30s) to catch two machines booting close
together, settling to a much slower `AMBIENT_STEADY_BROADCAST_TICKS`
(~60s) afterward. `nic discover`'s own command keeps its separate, faster,
unmodified rebroadcast loop. Verified end to end by
`tools/test_hostname_discovery_e2e.py` (not part of `make test`). Full
detail (the exact design iterations this went through, and why ambient
discovery only replies-and-remembers rather than also broadcasting from
`poll_background_work`) is in `docs/history/networking-and-discovery.md`.

### NETFS: a simple network filesystem over discovered peers

NETFS lets a file be fetched from, or pushed to, a discovered peer's own
disk directly, without bouncing through a disk both machines share. It
reuses `CHAT_ETHERTYPE` and this project's usual no-retransmission,
no-authentication design. Ten tags continue the numbering after
`TERM_TAG_BYE`: `NETFS_TAG_GET_REQUEST`/`GET_START`/`GET_DATA`/`GET_STOP`/
`GET_ERROR` and `PUT_REQUEST`/`PUT_DATA`/`PUT_STOP`/`PUT_ACK`/`PUT_ERROR`.
Both directions verify the whole file's RFC 1071 checksum once fully
reassembled before installing/writing anything; nine error codes cover
not-found, disk-unavailable, too-large, invalid-name, directory-full,
data-full, checksum-mismatch, disk-error, and a peer already mid-transfer
(`BUSY`). Peer references accept only an `nic peers` index or a known
hostname, never a raw MAC (its colons would collide with `HOST:diskN`'s
own).

Command syntax, layered onto commands that already move a whole file
through the shared `pmem::TAPE_BASE`/`TAPE_LEN` staging buffer:

```
copy HOST[:diskN]/NAME DST_DISK [NEW_NAME]     remote source (NETFS GET)
copy SRC_DISK NAME HOST[:diskN] [NEW_NAME]     remote destination (NETFS PUT)
tape load HOST[:diskN] NAME                    remote source, straight into the tape buffer
tape save HOST[:diskN] NAME                    remote destination, from the loaded tape
tape run @HOST[:diskN]/NAME                    remote source, as a startup-argument file
```

Copying directly between two remote endpoints is rejected -- go through a
local disk in two steps instead. **Serving is scoped to the idle prompt
loop only** (`ambient_discovery_poll`, alongside KUP and ordinary
discovery): a fos machine must be sitting idle at its own local prompt to
answer NETFS traffic at all, never inside `chat`, `nic discover`, `term
net`, or a running tape. Unlike ordinary discovery traffic, NETFS activity
is printed. A PUT is stateful across many idle-loop iterations
(`NetfsPutTransfer`); a second concurrent `PUT_REQUEST` is rejected with
`BUSY`. NETFS serving reuses the same shared staging buffer a locally
typed `copy`/`tape load` command uses, so a local operator running one of
those on the *serving* machine during an in-flight PUT can corrupt both --
an accepted, documented limitation of fos's single-threaded, no-locking
design. Verified end to end by `tools/test_netfs_e2e.py` (not part of
`make test`). Full detail (the checksum placement rationale, and every
verified scenario) is in `docs/history/networking-and-discovery.md`.

### HLT-on-idle: letting the CPU actually rest at the idle prompt

`arch::x86::halt_until_interrupt` executes one `hlt` and returns once any
interrupt wakes it, called only from `shell::run`'s idle prompt loop when a
whole iteration found nothing to do -- the plain `sti;hlt` atomic idiom
isn't needed here since interrupts are already unconditionally enabled and
the 100 Hz PIT is never masked, bounding any missed wakeup to one tick
(~10ms). This is deliberately scoped to *only* that one loop, not
`term_net`'s session loop, `nic_sniff`, `nic_discover`, `chat_command`, or
any tape network-transfer loop -- all of those actively poll NIC receive
with no interrupt backing, so halting there would just add latency.
Because this same loop also serves NETFS/KUP traffic arriving in paced
chunks a few milliseconds apart, `ambient_discovery_poll` reports whether
it received a frame this call, and the loop only halts once
`AMBIENT_IDLE_GRACE_TICKS` (2 ticks, ~20ms) have passed with no NIC or
keyboard/serial activity -- so an active transfer never gets halted
mid-stream. One accepted caveat: COM1 has no interrupt behind it, so a
serial-typed byte can incur up to one PIT tick of added latency once the
machine is truly idle and halting. Full detail (the throughput-regression
verification this required) is in `docs/history/networking-and-discovery.md`.

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

HDA's own sample rate and bit depth are negotiated, not fixed: `hda.rs`
queries a discovered DAC widget's `AC_PAR_PCM` capability and prefers
48 kHz over 44.1 kHz and 24-bit over 16-bit whenever the widget actually
advertises support, falling back to the historical 44.1 kHz/16-bit
baseline otherwise (`negotiate_format`) -- unlike channel count, which
was already negotiated this way via `AC_WCAP_CHAN_CNT_EXT`, this was
previously just a hardcoded `0x4010` format value, chosen for parity with
ES1370, not a hardware limit HDA silicon actually has. ES1370 itself is
unaffected: it is genuinely silicon-limited to 44100 Hz/16-bit (only 4
hardwired rates, format field only encodes 8/16-bit PCM). **24-bit
specifically cannot be exercised under QEMU at all**: its synthetic HDA
codec only ever implements 16-bit PCM (`QEMU_HDA_PCM_FORMATS` in
`hda-codec-common.h`), so that path is real-hardware-only, exactly like
this section's own already-documented "unverified against real
hardware" status below -- and the 24-bit sample container size (a 4-byte
container per sample, matching Linux ALSA's `S24_LE` convention, chosen
over RFC 3190's tightly-packed 3-byte wire convention) is a documented
assumption pending that same real-hardware confirmation, not a verified
fact.

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

## AC'97: a third audio backend, and reduced ES1370 headroom to make room for it

`kernel/src/ac97.rs` is a third PCI sound driver, alongside ES1370 and HDA,
for Intel's ICH-family AC'97 ("Native Audio Mixer" / "Native Audio Bus
Master") controller -- motivated by a real, physically owned machine (the
Pentium 4/ICH4 test box used throughout this file's real-hardware
sections) whose onboard audio is otherwise permanently inaccessible: fos
has no driver for any other codec family that chipset exposes. It is
QEMU's own emulated sound device too (`-device AC97`, PCI ID `8086:2415`,
Intel's original `82801AA`), so, unlike the still-fully-real-hardware-only
I350/3C509 drivers, this one gets real `make test`-style QEMU coverage.
`ac97::CANDIDATE`-equivalent discovery matches exactly two IDs: `0x2415`
(QEMU's own model) and `0x24d5` (the confirmed real ICH4 device) -- a
deliberately narrow pair, not a speculative broadening across every
ICH-generation ID, even though Intel's own AC'97 register layout is
documented backward-compatible across generations.

**Scope: playback-only, no network-fed streaming yet.** This driver
programs only the PCM-out bus-master channel (no PCM-in/mic-in), and
always plays its own built-in test tone -- there is no `submit`-backed
network-fed ring yet, unlike ES1370/HDA. That gap is deliberate, not an
oversight: a per-instance `audio::PcmRing` costs a fixed 16 KiB of
`.bss`, and this kernel's reserved low-memory region had almost no real
slack left when this driver was first added (see the next paragraph).
`ac97::MAX_INSTANCES` is 1 (a real motherboard south-bridge function,
never something a machine has more than one of, unlike an add-in ES1370
card).

**A genuine measurement bug in this project's own "check `size
build/kernel.elf`" convention was found while fitting this driver in.**
Summing `size`'s text/data/bss columns (the documented way to check
reserved-region headroom) ignores the `ALIGN(4096)` padding gaps
`linker.ld` inserts between `.text`/`.rodata`/`.data` -- a small, isolated
content-size change can silently cost a full extra 4 KiB page (or two) of
real position if it happens to push a section boundary across its next
alignment point, with no way to see this from `size`'s own output. The
real, trustworthy number is the linked `__kernel_end` symbol's own address
(`nm build/kernel.elf | grep __kernel_end`, which needs a temporary
`strip = "none"` in `kernel/Cargo.toml` since the shipped profile strips
all symbols) -- not a sum of section sizes. Measured this way, this
kernel had only **64 bytes** of real slack below `0x9f000` before this
driver, not the "roughly 31 KiB" this file's own "Low-memory map" section
previously claimed (stale since some earlier growth step, never
re-verified with the correct method). `AGENTS.md`'s own low-memory
guidance has been corrected to describe measuring `__kernel_end` directly
instead of trusting `size`'s column sums.

Given that real 64-byte margin, the actual fix was not growing
`0x9f000` (a real hardware wall sits close enough above it -- 0xA0000 is
where VGA's own memory-mapped I/O begins on every x86 PC, non-negotiable,
unlike the softer EBDA-convention framing this file used to give
`0x9f000` itself) but freeing existing headroom instead:
`es1370::MAX_INSTANCES` dropped from 4 to 2, reclaiming ~65 KiB of
`.bss` (two of its four 16 KiB ring-pairs) that no realistic machine
running this project was ever going to use -- confirmed unaffected by
`tapes/test_audio_multi_e2e.py`, which only ever exercises two
simultaneous ES1370 instances. This left real slack at roughly 56 KiB
measured the correct way, comfortable room for this driver and further
near-term growth.

A real bug was found and fixed while first bringing this driver up under
QEMU: acknowledging only `SR_BCIS` (the period-completion status bit) and
never enabling/clearing `SR_FIFOE` (FIFO error) left that bit permanently
set the instant the ring's fixed-size buffer underran (immediate, under
QEMU's own unthrottled mixer callback with no host audio backend
attached) -- an unacknowledged status bit re-fires the shared PCI IRQ
line (also used by the e1000 NIC in the same test configuration) the
instant `iret` restores `EFLAGS.IF`, hanging the whole machine. This is
the exact ES1370 interrupt-storm lesson this project already learned
once (see `es1370.rs`'s own `acknowledge_dac1` doc comment), now hit a
second time in a second driver; the fix is acknowledging every
write-1-to-clear bit in `SR` on every interrupt, not only whichever one
triggered period-refill logic. Confirmed fixed, and confirmed the
sliding-`LVI` playback ring genuinely loops indefinitely rather than
halting after its first two periods, by capturing real QEMU `-audiodev
wav` output during a several-second `audio` session and verifying the
built-in test tone's oscillation continues unbroken all the way through
the capture, not just its first fraction of a second.

**Confirmed on the real P4/ICH4 machine this driver targets -- but that
machine's own onboard AC'97 has no physical codec behind it.**
`PciAddress::assign_io_bar` (`pci.rs`) was added as a general fallback for
a BAR firmware never assigns (`pci` on this machine shows `bar0=0x00000000`
for the AC'97 function), following the standard PCI BAR-sizing idiom
(write all-1s, read back the size mask, restore). It found nothing to
assign: both BAR0 and BAR1 read back as a hard `0` even after that probe,
in full D0 power (ruling out a low-power/disabled-device explanation via
`PciAddress::debug_power_state`, added alongside it). Per the PCI spec, an
I/O BAR's own type bit is hardwired and always readable regardless of
size -- a probe reading back literal `0` on both BARs means there is no
register storage behind either one in silicon at all, not a firmware
oversight any software fix can repair. The most likely explanation: this
specific board's ICH4 south bridge exposes the AC'97 *function* in PCI
config space (for driver-detection purposes) even though its OEM never
populated an actual codec chip -- audio was frequently a discrete
add-in-card option on boards of this era. `ac97::init_card` reports the
raw BAR/power-state readback whenever a BAR can't be resolved, so this
diagnosis is directly visible on a real machine hitting the same
condition, not something that requires re-deriving by hand. The driver
itself is otherwise confirmed correct end to end under QEMU (real
register layout, real interrupt-storm bug found and fixed, sustained
playback confirmed via `-audiodev wav` capture) -- this is a genuine
hardware-population gap on one specific test machine, not an unverified
code path.

Full detail (the exact register-level layout cross-checked against
QEMU's `hw/audio/ac97.c`/`ac97.h` source, the sliding-`LVI` ring
technique's own derivation, the alignment-padding investigation
narrative, and the full real-hardware BAR-assignment investigation) is in
`docs/history/audio-hda.md`.

## Real-time audio streaming over RTP/UDP: the `audio-rtp` tape

`tapes/audio-rtp/` streams the same raw PCM audio `audio-net` does, but
over genuine RTP (RFC 3550) carried in UDP/IPv4 instead of a bespoke
raw-Ethernet tag protocol, so a real tool (`ffmpeg`, `gstreamer`, any RTP
client) can send to or receive from `fos` directly, not just another fos
instance. `tapes/tape-net/src/lib.rs` gained a matching `RtpPacket` type
(parse/encode over borrowed slices/caller-owned buffers) and a
`sequence16_at_or_after` wraparound helper; see `NETWORKING.md`'s "RTP
scope"/"`tapes/audio-rtp`" sections for the full wire-format design.
Format is adaptive, not fixed: the tape maps the local device's actual
format to one of a small set of RTP payload types it understands (RFC
3551's `L16`/44100/stereo = PT 10 for ES1370; two documented dynamic-range
conventions for HDA's negotiable 48 kHz/16-bit and 48 kHz/24-bit) and
rejects anything else outright rather than generalizing. Scope is device 0
only per invocation: `listen`/`send`/`capture` each drive exactly one
direction; `duplex PEER_IP [PEER_PORT]` is the one mode that drives both
directions at once (still device 0 only), over one shared local UDP port
(always `DEFAULT_PORT`/5004).

Opus support (`tapes/opus-codec/`, this project's first vendored C
dependency, built from libopus 1.5.2's fixed-point, decode-and-encode
source tree by the root Makefile's `opus-codec-lib` recipe) adds a fourth
fixed dynamic payload type, `PAYLOAD_TYPE_OPUS` (98): `listen_mode`
decodes it via a persistent `opus_codec::Decoder`, and `send`/`capture`/
`duplex` accept `[opus] ... [BITRATE]` (default
`opus_codec::DEFAULT_BITRATE_BPS`, 96 kbps) to encode via
`opus_codec::Encoder`. Both directions are gated to HDA's negotiated
16-bit/48 kHz-family format specifically (ES1370's fixed 44100 Hz doesn't
match libopus's five accepted rates, and this vendored build has no float
API so it only ever produces/consumes 16-bit PCM). The decoder/encoder
state buffer is allocated through the tape ABI's own `alloc`/`free`
callbacks (`custom_support.h` redirects libopus's internal
`opus_alloc`/`opus_free`), and `config.h` uses libopus's
`NONTHREADSAFE_PSEUDOSTACK` scratch-allocation mode rather than
`VAR_ARRAYS`, since the encoder's SILK pipeline needs more scratch than
fos's kernel stack guard page tolerates when tapes run nested inside the
shell's own call chain.

Verified end to end by `tapes/test_audio_rtp_e2e.py` (raw PCM),
`test_audio_rtp_opus_e2e.py`/`test_audio_rtp_opus_send_e2e.py` (Opus
decode/encode against a real Intel HDA QEMU guest), and
`test_audio_rtp_duplex_e2e.py` (concurrent duplex). Confirmed against the
real TerraTec Aureon Dual USB card (via `usb-host` passthrough, the same
device the UHCI section above uses): this surfaced a real bug -- capture
code was reusing the *playback* format for a device whose two directions
differ (stereo playback, mono capture) -- fixed by `capture_local_format()`
sourcing from the tape ABI's dedicated `audio_capture_sample_rate`/
`audio_capture_channels_and_bits` callbacks instead of `local_format()`;
`duplex opus` against the real card now plays and captures correctly
labeled audio simultaneously. Real-hardware interop with a genuine,
unmodified `ffmpeg` session, and the 24-bit path this tape's format table
already accounts for, remain future work. Full detail (the Opus
stack-overflow investigation, every e2e test's verification methodology,
and the real-hardware duplex bug narrative) is in
`docs/history/audio-hda.md`.

## MSI/MSI-X support: a local APIC, and Intel HDA as the first consumer

`kernel/src/lapic.rs` is a minimal local APIC driver, added to receive
Message Signaled Interrupts (MSI). The legacy 8259 PIC (`interrupts.rs`)
keeps running exactly as before for every existing backend; the local APIC
is purely additive -- used only to receive MSI, never to relay legacy
INTx -- and there is still no I/O-APIC anywhere in this kernel, since MSI
bypasses I/O-APIC redirection entirely. `lapic::initialize()` runs once
from `interrupts::initialize()`: it checks CPUID for an APIC, reads
`IA32_APIC_BASE` via new `arch::x86::rdmsr`/`wrmsr` helpers, maps the
LAPIC's 4 KiB MMIO window with the existing `paging::map_mmio`, and
programs a dedicated spurious vector (`0x60`). If it ever fails (no APIC,
or `map_mmio` has no free dynamic page table slot), every MSI-capable
driver transparently falls back to its legacy PIC IRQ path -- not a fault.

`pci.rs` gained a capability-list walk (`PciAddress::find_capability`) and
`PciAddress::enable_msi(vector, destination_lapic_id)`, handling only the
simplest case (one vector, 32-bit message address -- this single-core
kernel only ever has one destination). `interrupts.rs` reserves vectors
`MSI_VECTOR_BASE` (`0x50`)-`0x5f` with `allocate_msi_vector`/
`register_msi_irq`/`dispatch_msi_irq`; unlike legacy `PCI_IRQ_HANDLERS`,
`MSI_HANDLERS` holds exactly one handler per vector, since MSI's whole
point is exclusive ownership. `hda.rs` is the first (and only) consumer:
`init_controller` tries LAPIC + MSI before falling back to its previous
legacy-IRQ path; `hda status` reports which one is active. ES1370 predates
MSI and always takes the legacy path. `pci`'s device dump also gained a
read-only `msi=yes/no` capability-presence flag per function.

The local APIC's own `map_mmio` call is a fourth consumer of `paging.rs`'s
dynamic-page-table pool (alongside e1000/i350/HDA), which is why that pool
is now 4 slots (see "Low-memory map" above) -- confirmed necessary by
trying it at the prior, smaller ceiling. Verified end to end by dropping
the `msi=off` QEMU workaround from the HDA e2e tests and asserting real
MSI-driven audio capture/playback. Full detail (the exact register-level
MSI address/data encoding, and the full verification list) is in
`docs/history/development-notes.md`.

## Deliberately deferred work

- Per-process paging/isolation, heap allocation, filesystems beyond FOSFS,
  userspace, scheduling,
  further hardware IRQ drivers beyond the timer/keyboard, robust
  fault recovery, any protocol above raw Ethernet frames (no ARP, no IPv4,
  no sockets), and support for NIC families beyond the current RTL8139,
  selected 3C905, Intel e1000, and NE2000-compatible backends. USB itself
  is no longer fully deferred -- see "UHCI: a USB host controller driver,
  and isochronous USB Audio playback" above for what's built (enumeration,
  isochronous OUT/playback, isochronous IN/capture via `usb capture`, and
  `audio.rs`/tape-ABI integration for both directions, confirmed running
  genuinely concurrently on real hardware); EHCI/OHCI/xHCI remain
  deferred, and no tape yet exercises simultaneous playback+capture
  through the tape ABI itself even though the underlying engine now
  supports it. USB Mass Storage (Bulk-Only Transport) is also built,
  wired into `disk.rs` as an ordinary secondary disk, and can also
  transparently take over `disk::Device::Boot` itself -- see "USB Mass
  Storage (Bulk-Only Transport)", "Wiring USB Mass Storage into
  `disk.rs`", and "Closing the Pentium 4 gap for real" above -- via the
  explicit `usb storage attach`/`usb storage promote` shell commands
  (never automatic at boot). `reboot soft`'s own from-scratch,
  BIOS-dependent kernel reload remains a separate, deliberately
  unaddressed gap even once `disk1` is promoted -- see that section's
  own closing paragraph.
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

A genuine scheduler would need per-task stacks and a fixed-size TCB table
(no heap exists), a context-switch primitive, and a cooperative-vs-
preemptive choice -- but by far the most expensive part would be auditing
every place this kernel assumes single-threaded execution, most critically
`disk.rs`'s BIOS real-mode trampoline, which cannot tolerate any
concurrent execution during its brief real-mode window. `poll_background_work`
(above) already gets the low-cost share of the benefit (kernel-native
recurring work shared across existing polling loops) with none of that
audit cost, and no tape currently needs to run in the background while the
shell keeps working. **Revisit a real scheduler only if a future tape or
workload genuinely needs concurrent execution.** Full detail (the
investigation that led to this conclusion) is in
`docs/history/development-notes.md`.

### ISA bus support and the 3C509: investigated, not yet built

Every NIC backend today is found through `pci.rs`'s PCI configuration-space
walk; this project has no ISA bus code at all. A 3Com 3C509 was considered
as a next NIC target and investigated, but deliberately not started: ISA
Plug-and-Play discovery (an LFSR wake-up key sequence and a serial-
identification/CSN-assignment protocol) is a real, standalone `isa.rs`
subsystem this project has never needed, while the register-level driver
itself would actually be simpler than the PCI backends already built.
QEMU has no 3C509 model at all, so unlike every PCI backend this driver
would have no automated `make test` coverage ever -- a standing,
real-hardware-only cost. Full detail (the discovery-protocol tradeoffs
considered) is in `docs/history/development-notes.md`.

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
tape that jumps the running 32-bit kernel into 64-bit long mode, proves it
got there, and jumps back -- entirely inside one `tape run` call, leaving
the kernel's own GDT, page tables, and IDT untouched once it returns. This
is a small, one-shot, fully reversible demonstration of the mode switch
itself, not the same thing as "Deferred optional x86-64 path" below (a
permanent, parallel 64-bit boot target built from the ground up).

The design splits into a reusable, privileged shim
(`tapes/longmode64_shim.S`: the `tape_main` entry point, the CPUID
long-mode check with its own early-return path, identity-mapped page
tables, a private GDT, and the full CR0/CR3/CR4/EFER/GDTR/EFLAGS save-and-
restore around the transition) and one implementation of a narrow
`tape64_body` contract it calls once safely landed in 64-bit mode with
interrupts off: an ordinary SysV64 `extern "C" fn()`, no arguments, no
return value, free to do whatever it wants and expected to return via a
plain `ret`. `tapes/longmode.S` implements it in hand-assembled `.code64`
(hex-dumping a 64-bit-only immediate straight to VGA text memory, since
calling back through the 32-bit `putc` callback would need a second nested
mode transition); `tapes/longmode-rs/` implements the identical contract
in ordinary `#![no_std]` Rust instead, as a `staticlib` crate with no
`_start` of its own, using a new, genuinely 64-bit
`targets/x86_64-fos-tape.json` (SSE/MMX/AVX disabled, `"rustc-abi":
"softfloat"`, since the kernel never sets CR4.OSFXSR/OSXMMEXCPT;
`"code-model": "small"`, not `"kernel"`, since this tape loads low at
`pmem::TAPE_BASE`) -- proof the shim/body split is a real, reusable
boundary, not just a refactor. Interrupts stay off for the whole excursion
since the kernel's IDT is a 32-bit-format IDT, invalid to service while
CR0/CR3/CR4/EFER/GDTR are mid-transition.

Because x86-64 ELF relocations are RELA-style (unlike `primes.S`'s
REL-style x86-32 relocations, which let a linker-free `as`+`objcopy`
pipeline work), both variants need a real `ld` link step:
`ld -Ttext=0x100000 --oformat=elf64-x86-64 -e tape_main` against the
assembled/compiled objects, before `objcopy -O binary`. Neither variant's
real 64-bit excursion can be exercised under `qemu-system-i386` (its TCG
backend cannot emulate the long-mode CPUID bit at all); `tapes/
test_longmode_e2e.py` and `tapes/test_longmode_rs_e2e.py` are the only two
e2e tests in this repository that launch `qemu-system-x86_64` instead.
Confirmed working on real hardware too: a Pentium 4 correctly took the
graceful no-long-mode-support path, and an AMD Athlon 64 X2 completed the
full round trip. Built with `make tape-longmode`/`make tape-longmode-rs`.
Full detail (the exact register save/restore sequence and page-table
layout) is in `docs/history/development-notes.md`.

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
