# fos development guide

> **SIZE LIMIT -- read before adding anything.** This file (`CLAUDE.md` is a
> symlink to it) is loaded into every session, and Claude Code **refuses to
> load it above 150,000 bytes**. Keep it under **100,000 bytes**; `make test`
> fails above 125,000. Check with `wc -c AGENTS.md`. New material goes in
> `docs/`, `NETWORKING.md`, `AUDIO.md`, `DEFERRED.md` or `README.md`, with at
> most a short pointer here. See "Keeping this file itself small" below.

## Project purpose

`fos` is a minimal educational operating system. The current supported path is
legacy BIOS -> 16-bit real mode -> 32-bit protected mode -> a `no_std` Rust
kernel. Boot stages emit explanatory messages to VGA text memory and COM1.
Keep changes small and explainable; avoid general frameworks or abstractions
that obscure hardware behavior.

**v2** (UEFI loader + 64-bit kernel, in `v2/`) is being built alongside this v1
tree; design and milestones are in `docs/v2-design.md`, targets are `make v2-*`.
Everything below describes v1 unless it says otherwise.

See NETWORKING.md for more networking stuff
See AUDIO.md for audio interface stuff
See DEFERRED.md for things put aside for now.

Try not to fill up this file, put new stuff in the above files or under docs/

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
  `keyboard::restore_after_bios_disk_call` then repairs the 8042 (BIOS USB legacy
  emulation can change its translation bits): it holds the keyboard interface off
  (`0xAD`) for the command-byte exchange and re-enables it (`0xAE`), because a
  real keystroke arriving between "read command byte" and its reply was once
  read as the command byte and written back, corrupting the configuration.
- `kernel/src/interrupts.rs` builds a 256-entry IDT (timer, keyboard,
  spurious-PIC, safe legacy-PCI IRQ gates, and vector 8's double-fault task
  gate), remaps the 8259 PIC clear of the CPU exception range, and arms the
  PIT at 100 Hz. NIC handlers only acknowledge/count/mark pending work;
  descriptor and protocol parsing remains foreground polling.
- `nic` status and ordinary foreground NIC traffic run a bounded watchdog,
  never an ISR or timer task. It only resets a backend after that driver
  latched a documented DMA/Tx fault, reuses its DMA allocations, restores
  interrupt/promiscuous state, and exposes progress/recovery counters.
  The e1000, e1000e and I350 receive rings are 128 descriptors deep, the
  RTL8168's and 3C905's 64 (`RX_COUNT`/`RX_DESC_COUNT`, buffers as one contiguous
  run of pages), and an RX overrun is only counted (`Health::rx_overruns`, shown
  by `nic list`), not answered with a reset: with 8-16 descriptors a tape
  forwarding a gigabit stream (`nettest`, `bridge`, `router`) overran constantly
  and each overrun reset the adapter. The RTL8168 treats RDU/FOVW as soft: its
  handler masks them until `receive` has drained the ring, so dropped frames
  cannot become an interrupt flood; only a PCI system error still resets it.
  Measured on the AMD/nForce machine with `python3-netraw` raw-frame blasts
  at the 1 Gb/s TP-Link RTL8168H: before, 3983 of 5000 frames arrived and the
  card reset itself; after, 40000 of 40000 arrived with no recovery (the
  3C905 takes a paced 10000 frames/s with no loss). The RTL8139 (one fixed 32 KB
  ring buffer) and NE2000 (16 KB on the card, whose DP8390 overflow procedure
  needs a reset) keep their reset-on-overflow behavior: `tapes/test_nettest_e2e.py`
  only requires them to keep forwarding after a burst. `shell::report_nic_watchdog`
  prints the first recovery at once and then at most one summary per 5 s.
- `kernel/src/pmem.rs` is a small bitmap-based physical-frame allocator (4 KiB
  frames) built from the E820 map; it reserves everything below the kernel's
  own low-memory region.
- `kernel/src/paging.rs` enables supervisor-only identity mappings for the
  first 2 GiB, plus RAM from 2 to 4 GiB only where a whole 4 MiB page lies inside
  usable E820 regions (`range_is_usable_ram`; never the PCI hole, and
  `identity_mapped()` tells `pmem` which big-pool chunks are safe). RAM above
  4 GiB is out of a 32-bit kernel's reach (no PAE); only `memtest`'s long-mode
  excursions touch it. The low 4 MiB uses 4 KiB PTEs to leave the kernel-stack guard
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
  `MAX_DYNAMIC_PAGE_TABLES` is 8.
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
  A non-builtin command word `NAME` runs `NAME.sh` or loads/runs `NAME.bin`
  (`run_external_command`; disk1 then diskN, builtins win, script nesting
  capped by `MAX_SCRIPT_DEPTH`). An optional `settings` file (`load_settings`)
  holds `no-msi` (NICs stay on legacy IRQs; see "NIC MSI" below) and
  `deny-term-net`, which closes `term net`, KUP
  (`dispatch_kup_frame`) and NETFS serving per NIC; scripts expand `$1`..`$9`/`$*`; see README.md.
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
- `tapes/nettest/` is a two-port link impairment tape (per-direction loss with
  bursts, delay/jitter, rate limit, queue limit, named presets such as
  `badwifi`, optional ANSI statistics panel). The engine is `tapes/nettest/src/
  lib.rs` (host unit tests); its e2e test links the guest NICs to the host over
  TCP because QEMU's multicast netdev echoes a guest's own frames back to it.
  QEMU's e1000 model also blackholes receive for ~1 s after promiscuous mode is
  enabled (e1000e does not); see README.md "Test tape: `tapes/nettest/`".
- `tapes/router/` is a fixed-buffer layer-3 tape (no dynamic routing, ICMP
  errors, fragmentation or IPv6). Syntax lives in README.md's "Tape
  command-line reference"; `tape run router @router.cfg` reads a lowercase FOSFS
  file (<= 4096 bytes, `#` comments) through the startup-argument ABI. NIC
  numbers are one-based; `dhcp [N]` adds ordered DHCP uplinks, `health ADDR`
  requires fresh gateway ARP *and* ICMP echo, fails over after three missed polls
  and fails back on recovery. A DHCP uplink that gets no offer in its boot budget
  is left unconfigured/unhealthy and keeps retrying in the background; startup is
  only refused for an operator abort or a NIC/callback error, never for one
  silent uplink. NAT (default on; `no-nat`/`nat`) and `forward tcp|udp` apply
  only to the active uplink, using a 4 MiB physical-memory table (32-slot
  fallback). `dhcp-server` serves every non-uplink interface (seven-day leases,
  one in-memory slot per address; `dhcp-lease-file NAME` optionally persists
  through the tape filesystem callbacks; `dns` overrides propagated DNS). A
  forward target must be on a non-primary, non-uplink subnet. Echo requests
  addressed to a router interface are answered before the inbound NAT check. A
  packet that misses the ARP cache is held (`HELD_FRAME`, 3 s) and sent when the
  reply arrives. Full detail is in NETWORKING.md ("Router tape detail").

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
| `0x5600`-`0x560b` | `kernel/src/debug.rs` (`CRUMB_ADDR`) | `debug::crumb`: the last progress marker a driver left (`debug crumb` shows it after a hang reset), for bring-up on hardware with no other way to see where a freeze happened. |
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
The audio PCM jitter rings (`audio::PcmRing`, 160 KiB of `.bss` once) now take
their bytes from `pmem` on first use, which is what made room for xHCI: the
linked `__kernel_end` was `0x9efc8`-ish before and is about `0x7b000` after.

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

**Hard limit: Claude Code will not load this file above 150,000 bytes.
Budget: stay under 100,000. `tools/check_agents_size.py` (run by `make test`)
fails above 125,000.** This file is loaded into every session, so its size has a
real, ongoing cost unlike `README.md` or `NETWORKING.md`, which are read on demand.
Whoever adds text here is responsible for keeping the budget: when a change adds
more than a few hundred bytes, trim or move something in the same change, and
re-check `wc -c AGENTS.md`.

Treat this file as a *current-state reference*, not a project journal. Keep:
architecture facts, ABI contracts, command syntax, the low-memory map,
constraints a future change must respect, and build/test commands. Do **not**
add: bug postmortems ("X was found and fixed, root cause was Y"), "first we tried
A, then B" narratives, test-run narration beyond naming the test file, or
fully investigated-but-deferred feature writeups. Those belong in `docs/history/`,
and per-tape or per-protocol reference belongs in `README.md` (tape table),
`NETWORKING.md` or `AUDIO.md`.

Mechanics, as every existing `docs/history/*.md` file does (see
`development-notes.md`'s intro): pick a thematically matching file
(`development-notes.md` as catch-all; `audio-hda.md` for sound hardware;
`real-hardware-notes.md` for physical-machine-only findings;
`networking-and-discovery.md` for chat/discovery/NETFS/idle-loop) or start a new
one, append the section's *original, unedited* text at the end under a
`## <original title> (full detail)` heading followed by the original heading(s)
and body verbatim -- never paraphrase what you move -- and leave a short paragraph
here: what the thing is, its current behavior, the file/function names involved,
and a closing "Full detail ... is in `docs/history/<file>.md`." sentence.
`docs/history/` files are append-only, never loaded automatically, and are linked
to by name.

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
frames. `tools/default-debug.txt` is written to every image's FOSFS data
partition by default under the name `debug` (wired into `TAPE_IMAGE_ARGS`
in the Makefile, alongside `router.cfg`), enabling `reboot-on-crash` and
`kernel-update` out of the box -- this project's own default deployment
target is a real, possibly remote/inconvenient-to-reflash test machine, so
debug mode's whole point (unattended recovery, network kernel pushes
instead of a fresh physical flash) should already be on rather than
something every new image has to opt into by hand. Must stay under
`debug.rs`'s `DEBUG_FILE_CAPACITY` (1024 bytes) or the whole file is
ignored with a warning. One line per option, `#` comments and blank lines
ignored:

| Option | Effect |
| --- | --- |
| `reboot-on-crash` | A fatal CPU exception resets the machine instead of halting forever. |
| `reboot-delay N` | Seconds to print the crash details before actually resetting (default 3); gives a slow serial link time to flush. |
| `kernel-update` | Enables KUP, the network kernel-update service (see below). Off by default -- this is a powerful, write-to-disk-and-reboot capability, not something to leave enabled on every machine. |
| `watchdog [N]` | Software watchdog (default 60 s; `tools/default-debug.txt` enables it). If the foreground makes no progress for N seconds -- a spin with interrupts on, an interrupt storm starving the foreground, a wait that cannot end -- the timer interrupt records a crash record (vector 240, with the `CS:EIP` the CPU was stuck at) and resets the machine, so a freeze leaves evidence and recovers unattended. Armed only after the boot milestone. Hang-loop guard: with the watchdog on, slot-metadata bytes 14/15 (`running`, `unclean_boots`) record whether the previous boot ended cleanly (`halt`, `reset`, `reboot`, a KUP restart all clear it) -- kept on disk because RAM does not survive a reset on every machine (the AMD/nForce test machine's POST wipes it, so its crash record and report never survive either); after 3 unclean boots in a row the watchdog stays off, and a boot that runs 5 minutes clears the streak. Needs no `reboot-on-crash`. Pets: `keyboard::poll_term_event`, `poll_background_work`, the idle loop, each BIOS disk call, the polling tape callbacks; a running tape gets a 10-minute limit instead. `debug hang` is the test command. |
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
`debug::kernel_update_enabled()`. Like NETFS (`dispatch_netfs_serve_frame`),
KUP is **also** dispatched from both of
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
complementary debug-mode test proving the *client tooling* itself works
(driving the real `term_net.py`/`send_kernel_update.py` CLIs as
subprocesses over a genuine host tap interface bridged to the QEMU
guest's primary NIC), not just the kernel/protocol side. Full detail
(the exact NIC-driver-exercise workflow this enables, and a non-obvious
`term_net`/KUP escape-timing gotcha) is in
`docs/history/networking-and-discovery.md`.

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
make test-most # make test plus the boot-critical safety tests (stack guard, page protection, debug mode, start, scripts, release loops).
make test-unit # Host-side cargo tests of the tape crates.
make test-all  # Everything, with a summary table; TEST_JOBS=N runs independent tests in parallel.
make all      # Builds everything: image and every test tape.
make tapes    # Builds all test tapes only (`build/tapes/*.bin`).
make help     # Lists every target with a one-line description.
```

Always run `make test` for boot, loader, kernel, shell, disk, or FOSFS changes.
Run `make test-most` before committing a kernel change; `make test-all` is the
full sweep. Groups, the known-failing/flaky lists and the parallel rules
(`TEST_JOBS`) live in `tools/run_tests.py`; a new e2e script with no make
target must be added to its `E2E_SCRIPTS`, and one that boots `build/fos.img`
and changes it must be marked in `EXCLUSIVE`.
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

`reset` (and `reboot hard`) is an unconditional 8042 pulse reset.
`reboot`/`reboot soft` (`kernel/src/reboot.rs`, `shell.rs`'s `reboot_command`)
instead jumps back into `boot/stage2.S`'s `_start` through the
`BootInfo::soft_reboot` callback (`soft_reboot_entry`): BIOS POST is skipped but
E820 collection, A20 verification, `select_kernel_slot`'s A/B state machine and
a fresh kernel load all run, so a pending KUP kernel is tried the same way
after either kind of reboot. `soft_reboot` follows `disk::call_trampoline`'s
real-mode-drop discipline (`cli`, restore the real-mode IVT and BIOS PIC
mapping, then drop) and falls back to a hardware reset when `BootInfo` has no
callback. **`reboot-on-crash` deliberately never uses it** (`debug::handle_crash`):
a soft reboot assumes stage two's code at `0x8000` and `BootInfo` survived,
which a crash handler cannot assume. Two soft-reboot bugs (a stale `SLOT1_LBA`
copy in `debug.rs`; `collect_memory_map` clearing only `%bp` of `%ebp`) are
written up in `docs/history/development-notes.md`; both were invisible to
hardware resets. Full detail is in `docs/history/development-notes.md`.

### NE2000 clones, `nic sniff`, `chat`, discovery and the `tape` loader

`kernel/src/ne2000.rs` (RTL8029AS, `10ec:8029`; QEMU `ne2k_pci` only -- no real
card was ever verified, see DEFERRED.md) is the only driver with no host-RAM DMA: packets live in the card's
16 KiB, reached through remote-DMA port I/O (`REG_RSAR`/`REG_RBCR`/`REG_DATA`),
so it never calls `pmem::allocate_contiguous`. Its receive ring is 8 pages of
256 bytes, each entry prefixed by a 4-byte header (status, next page,
little-endian byte count -- getting that order backwards was a silent-failure
bug, see `docs/history/development-notes.md`).

`Rtl8139::set_promiscuous` toggles `RCR.AAP` on top of `RCR_CONFIG`; `nic sniff`
enables it, prints frames until the F12 escape and always disables it again.
`tools/send_frame.py` (stdlib only; destination MAC from the `.gitignore`d
`tools/mac`) sends one raw frame; keep its default ethertype equal to
`shell.rs`'s `TEST_ETHERTYPE` (`0x88b5`).

`chat` is a full-duplex loop over `CHAT_ETHERTYPE` (`0x88b6`, distinct from
`TEST_ETHERTYPE`). Every such frame has a one-byte tag after the Ethernet
header (`CHAT_TAG_TEXT`, `CHAT_TAG_DISCOVER_REQUEST`/`_REPLY`); text is
trimmed of trailing zero padding, and serial input also works. `chat MAC|N`
targets one peer. `nic discover`/`nic peers` keep a numbered table (`PEERS`,
`MAX_PEERS` = 16, no heap): discover broadcasts a request on every NIC and
re-sends every `DISCOVER_REBROADCAST_TICKS`; a request is answered by one
unicast reply through the NIC it arrived on, and the requester is remembered,
so discovery is symmetric. With no background listener a probe is only
answered while the peer is inside `chat`, `nic discover` or the idle prompt
(see "Hostname-based NIC discovery"). Hostnames ride after the tag byte.

`tape` is "load, then run": one fixed 4 MiB buffer at `pmem::TAPE_BASE`, plus
metadata (length, format, entry, handle); the loader accepts validated ELF32
`ET_EXEC` images (only `PT_LOAD` segments, BSS cleared) and legacy flat images
and rejects anything over `TAPE_LEN`. `tape load net`/`tape send` add
`TAPE_TAG_START` (declares the exact length), `_DATA` (sequence number; its
size comes from the sequence and declared length, never the padded frame) and
`_STOP`. A gap aborts the attempt (no retransmission) but the loop keeps
listening; the first accepted sender is locked in. `tape send NAME PEER`
reuses chat's peer addressing and `fosfs::read`; `tools/send_tape.py` builds
the same frames by hand and can discover peers itself. Senders pace frames
(kernel: one 100 Hz tick; `send_tape.py`: 3 ms default, verified on real
hardware) because `tape load net` polls a finite ring. `tape load disk` reads
the whole file; the image preloads every compiled tape. `tape run` does a
cdecl `call` to the entry with the append-only callback ABI (below); a tape
that ends in a near `ret` returns to the shell, anything else is trusted like
kernel code. Low-memory growth history: the reserved-region constants
(`linker.ld` `ASSERT`, `pmem::RESERVED_BELOW`, `write_layout.py`'s
`KERNEL_RESERVED_END`) must match by hand; slot metadata and the FOSFS
partition start are generated from `tools/write_layout.py`. Full detail
(including the removed `console::resync_cursor`) is in
`docs/history/development-notes.md`.

### `dmesg`: a kernel log ring buffer

`console.rs` keeps an always-on ring (`KLOG`, 3072 bytes -- `.bss` headroom
below `0x9f000` is small) tapped at `write_serial`, the single choke point the
relay queues also use. Unlike those opt-in queues it overwrites its oldest
bytes when full and counts them in `KLOG_DROPPED`. `console::seed_klog_from_vga`
(from `console::initialize`) seeds it from the VGA text already on screen, which
is how `dmesg` replays the real-mode stage-one/two narration (the screen is
never cleared at boot). `console::print_dmesg` suppresses capture
(`KLOG_SUPPRESSED`) while dumping. `tools/qemu_smoke.py` runs `dmesg` right
after boot, before ordinary output evicts that narration. Full detail is in
`docs/history/development-notes.md`.

### `pci` and the I350 backend

`pci` lists every function (bus:dev.func, IDs, class, header type, BAR0,
revision, subsystem, interrupt line/pin, command/status, `msi=`) via
`pci::scan_devices()` (multifunction-aware, `MAX_PCI_DEVICES` = 64) and may
scroll. It exists to read an unknown card's exact IDs before writing a driver.
`kernel/src/i350.rs` mirrors `e1000.rs` for Intel's I350 (PCIe, moved and
multi-queue register map). `i350::CANDIDATE_DEVICE_IDS` holds only `0x1521`
(copper, confirmed with `lspci -vv` on an I350-T2); the SR-IOV VF `0x1520` is
excluded on purpose (mailbox interface, no PHY access) and sibling IDs are added
only after each is tested on hardware. QEMU has no I350 model, so verification
is real-hardware only. Full detail is in `docs/history/development-notes.md`.

### e1000e: a sixth NIC backend (82574L/82579/I219)

`kernel/src/e1000e.rs` keeps `e1000.rs`'s legacy single-queue descriptor layout.
Candidate IDs: `0x1503` (82579V, a real ITX board), `0x10d3` (82574L, QEMU's
`-device e1000e`, so it has `make test` coverage) and `0x15b7` (I219-LM, enabled
but never booted: real-hardware-only, and Linux's K1/ULP and NVM-arbitration
workarounds for it are not implemented). `0x1502` stays excluded until
confirmed. All IDs take `EXTCNF_CTRL.MDIO_SW_OWNERSHIP` around reset.
Confirmed on the real 82579V and under QEMU. Rule learned from Linux's
`e1000_reset_hw_ich8lan`: after `CTRL.RST`, **any register access, even a read,
can hang e1000e silicon** (QEMU never shows it); see `disable_pcie_master` and
`force_power_state_d0`. Out of scope: the Management Engine handshake, K1/EEE/ULP
and flash writes (the MAC is read from `RAL0`/`RAH0`). Backend order in
`nic::initialize()`: RTL8139, 3C905, e1000, e1000e, I350, NE2000, RTL8168.
Full detail is in `docs/history/development-notes.md`.

### RTL8168/RTL8111: a seventh NIC backend (two real chips confirmed)

`kernel/src/rtl8168.rs` claims `10ec:8168` and `10ec:8161`, uses the I/O-port
BAR0, a 64-descriptor receive ring and 32 transmit descriptors (2 KiB buffers),
asynchronous transmit and the RTL8139-style reset-and-reprogram recovery. The only
chip-specific branch is `CR` enable order from the `TxConfig` version bits (XID
`0x4c0`+ enables the MACs after `TxConfig`/`RxConfig`); Linux's PHY/ERI/OCP/ASPM
tuning is left out. `init` logs revision, version bits, MAC and PHY status (see
`dmesg`). Confirmed on the AMD/nForce machine (TP-Link TG-3468 at 1000 Mb/s,
onboard LAN at 100 Mb/s): `term net`, 5 KB and 284 KB tape loads. QEMU has no
RTL8168, so nothing automated covers it. Test caveat: keep the `term net`
session on a different NIC than the one under test. 

**Interrupt-storm guard** (`interrupts::guard_against_storm`): legacy PCI IRQs
are level-triggered, so a cause a handler cannot clear freezes the machine. More
than 500 dispatches of one line in one tick masks that PIC line (`irqstat`
reports it). The RTL8168 handler also disables its own interrupts on an overflow
or PCI error until `recover` has reset the card. Full detail is in
`docs/history/real-hardware-notes.md`.

### 3C905 interrupts and receive ring

Found on real hardware (QEMU has no 3C905). On the 3c90x an event must be enabled
in **both** `SET_STATUS_ENABLE` and `SET_INTERRUPT_ENABLE` to raise INT#;
`INTERRUPT_MASK` is UpComplete | HostError (TxComplete excluded: it stays asserted
until the Tx status is popped, which the driver never does). Every command-register
write goes through `issue_command`, which waits out `CmdInProgress`, because three
places write it and the card ignores a command sent while busy. The receive ring is
64 descriptors; a small ring overflowed during `tape load net`'s slow console
output. Diagnostics: `irqstat` (PIC pending/in-service/mask) and `pci` (INTx status
bit 3, command bit 10). `send` sets no TxIndicate (the 31-deep TxStatus stack
overflowed after ~30 frames and stopped the transmitter) and `service_tx_status` pops error entries; both ported from v2,
where they were confirmed on the real card; not yet run on real hardware in v1. Full detail is in `docs/history/real-hardware-notes.md`.

### UHCI: a USB host controller driver, USB Audio and USB Mass Storage

`kernel/src/uhci.rs` is a minimal UHCI (USB 1.1, I/O-port) driver found by class
walk (0x0C/0x03/0x00), up to `uhci::MAX_INSTANCES` (4). `reset_and_start` builds
a page-sized frame list pointing at one control QH plus a pool of
`TD_POOL_CAPACITY` (34) TDs; there is no IRQ (no MSI on UHCI), everything is
foreground-polled, `control_transfer` has a 500 ms deadline. `usb` lists
controllers/ports and fully re-enumerates every time. Device addresses come from
`uhci::allocate_device_address`.

- **Isochronous playback** (`usb play [diskN] NAME`): `IsocOutStream` keeps
  `ISOC_TD_COUNT` (250, ~250 ms) frame-list entries primed ahead of `USBFRNUM`
  and counts failed packets in `error_count`. Minimal UAC 1.0 parsing
  (`find_uac_device`) needs PCM/16-bit, a fixed per-frame packet size and
  Synchronous/Adaptive sync. `uhci::UsbAudioDevice` is the third `Audio` variant
  (`Audio::Uhci`), serviced from `poll_background_work` via
  `uhci::service_active_audio` (no isochronous IRQ, so a loop must keep priming
  it). Sources: a fixed buffer or a `PcmRing`.
- **Capture** (`usb capture`, real-hardware-only; QEMU's `usb-audio` cannot
  capture): `IsocInStream` has its own TD pool; `Audio::capture_format()` and the
  `audio_capture_*` tape callbacks expose a capture format that may differ from
  playback (the TerraTec Aureon Dual is stereo out, mono in). `usb play`/`usb capture`
  and `audio.rs` must address the device by `UacDevice.address`, the value
  `find_uac_device` just took from `allocate_device_address` -- it changes on
  every discovery, so never hardcode 1.
- **Mass Storage (Bulk-Only)**: `Uhci::bulk_transfer` (caller-owned DATA0/1
  toggle, split into `TD_POOL_CAPACITY` passes) under `kernel/src/usb_storage.rs`
  (CBW/CSW plus a SCSI subset: `inquiry`, `wait_until_ready`, `request_sense`,
  `read_capacity_10`, `read_10`, `write_10`; 512-byte blocks only; any transport
  error runs BOT reset-recovery). Real hardware needs a settle delay between bulk
  stages and before reusing TD memory after a timeout (`wait_ticks`). `usb storage`
  stays read-only. A device is identified by controller + port
  (`MscDevice::same_physical_device`): rediscovery resets the port and readdresses
  it, so `disk::refresh_known_usb_device` replaces the existing disk's handle
  instead of attaching a duplicate (`UsbAttachError::AlreadyAttached`).
- **`diskN` integration**: `SecondaryBackend::{Bios(u8), Usb(MscDevice)}`.
  `usb storage attach` (never automatic: `uhci::initialize_all()` resets every
  controller and breaks BIOS USB-legacy `INT13h` for any disk still served that
  way) adds the next `diskN`. `usb storage promote` switches `Device::Boot` to the
  native driver after cross-validating the device against `disk1`'s BIOS-read MBR
  and a capacity floor (`BootPromoteError::NotSameDisk` otherwise). `usb native`
  (`disk::promote_all_and_attach`) does that for every BIOS-backed disk, then
  attaches unmatched devices fresh. On real hardware, repeating USB discovery in
  one boot can degrade BIOS legacy mapping for any BIOS-USB disk: reset before
  each fresh attempt when more than one is at stake. `reboot soft` stays a gap
  even when promoted (stage two needs BIOS to reload the kernel).

- **xHCI and EHCI** (`kernel/src/xhci.rs`, `ehci.rs`; `usb xhci`, `usb ehci`,
  `usb ehci storage`, `usb hosts`): polling-only drivers built for USB Mass
  Storage. xHCI: BIOS handoff, controller reset, command/event/transfer rings,
  Address Device, control and bulk transfers (64 KiB TRB-boundary splitting),
  Configure Endpoint, root ports only. EHCI: one reprogrammed async-schedule QH
  with the schedule switched off between transfers, qTD chains with explicit data
  toggles, high-speed only (full/low speed goes to the companion), and **one level
  of hub** -- required on Sandy Bridge, whose EHCI ports sit behind Intel's
  rate-matching hub. `usb_storage.rs` drives all three hosts through its `Host`
  enum (`open_all`: UHCI, xHCI, then EHCI ports), so `usb storage [attach|promote]`
  and `usb native` work over each. Like UHCI they must not run at boot: taking a
  controller ends the BIOS's INT13h emulation of the stick behind it, so
  `promote` reads its BIOS baselines first; after a takeover reset the machine
  before pushing a kernel (KUP needs the BIOS disk). USB waits pet the watchdog.
  `usb native` takes tens of seconds and a remote term session usually loses its
  output: read it with `dmesg`. `usb xhci`/`usb ehci` leave ports a disk uses alone
  (reopening a port replaces its slot/address and kills that disk's handle).
  Real machines (all confirmed): Skylake = xHCI only (boot stick promoted);
  Sandy Bridge = 2 EHCI behind the rate-matching hub (boot stick promoted) + NEC
  xHCI; AMD/nForce = OHCI + EHCI (boot stick promoted; OHCI has no driver).
  Tests: `make test-xhci`, `test-ehci`, `test-usb-hosts` (QEMU `qemu-xhci`/
  `usb-ehci` + `usb-storage`; QEMU's hub is full speed, so hub code is
  hardware-only). `debug::crumb`/`debug crumb` (see the low-memory map) helped
  find where bring-up stalled.

- **AHCI** (`kernel/src/ahci.rs`; `sata`, `sata drives`, `sata attach`): a polling
  SATA driver (one command slot, 512-byte sectors, LBA48 with 28-bit fallback,
  bounce buffer, cache flush after each write). `disk.rs` gains
  `SecondaryBackend::Ahci` (secondary `diskN` only; boot-disk promotion is not
  implemented). Explicit only, like USB: it must not run at boot, because the BIOS
  reads those disks through the same HBA. Only AHCI-mode controllers qualify
  (`sata` shows which machines have them); IDE-compatibility mode, NVMe and RAID
  are not driven. Test: `make test-ahci` (QEMU `ich9-ahci`; 28-bit commands have no
  automated coverage).

Tests: `tapes/test_uhci_e2e.py`, `test_uhci_play_e2e.py`,
`test_uhci_audio_facade_e2e.py`, `test_usb_storage_e2e.py`,
`test_usb_storage_disk_e2e.py`, `test_usb_boot_promote_e2e.py`. Confirmed on a real
TerraTec Aureon Dual (playback, capture, duplex) and on the Pentium 4 USB-boot
machine. Full detail (every bug found, with verification narration) is in
`docs/history/development-notes.md`.

### Tape print callback ABI

`tape_run` calls the entry as cdecl `extern "C" fn(putc: extern "C" fn(u32))`,
passing `console::print_byte`, a thin wrapper over `console::write_bytes` -- the
one path that handles VGA placement, scrolling, the cursor, the COM1 mirror and
the `term net`/printer relay queues. Tapes therefore never touch the CRTC, the
kernel's cached `ROW`/`COLUMN` is never stale, and `tape_run` needs no
`resync_cursor`. The argument is `u32` so the stack slot is unambiguous.
`tapes/primes.S` captures the callback from `4(%esp)`; `%ebx` stays its PIC base
register, so `print_decimal` must not reuse it. `tools/test_term_net_tape_e2e.py`
asserts the primes digits reach a remote `term net` client. Full detail is in
`docs/history/development-notes.md`.

### Tapes: fixed load address, and writing one in Rust

Every tape loads at the fixed physical `pmem::TAPE_BASE` (`0x100000`, `TAPE_LEN` 4
MiB), reserved from the frame pool at boot by `pmem::reserve_range`;
`allocate_tape_buffer` is only a bounds check and loading overwrites in place, so
tapes need not be position-independent (`primes.S` still is, as the assembly
example). A Rust tape (`tapes/hello-rs/` is the reference) is a `no_std`/`no_main`
crate using `targets/i686-fos.json` and the kernel's `-Z build-std` invocation,
with its own `linker.ld` at `0x100000`. `tape_run` is a plain cdecl call into an
already-running protected-mode CPU, so `tapes/hello-rs/src/entry.S` only zeroes
`.bss` and tail-jumps to `tape_main` (the ELF loader also clears BSS tails). Large
scratch arrays are `static mut`, not stack, because tapes run on the 64 KiB kernel
stack. A C tape would work the same way. Build with `make tape-hello-rs`; test:
`tapes/test_hello_rs_e2e.py`. `docs/rust-tapes-for-dummies.md` is the step-by-step
guide. Full detail is in `docs/history/development-notes.md`.

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

`TapeEntry` is now `extern "C" fn(putc, should_abort, alloc: extern "C" fn(u32) ->
u32, free: extern "C" fn(u32, u32))`, with `shell::tape_alloc`/`tape_free`. Every
ABI extension is append-only cdecl: `tape_run` always pushes all arguments and
cleans up, older tapes ignore the extra slots, and a tape may omit only a
*trailing* run of parameters (to use `free` it must still declare `should_abort`).
`alloc`/`free` use a separate "big pool" in `pmem.rs` (`BIG_BITMAP`,
`BIG_POOL_BASE` = `TRACKED_BYTES` (128 MiB) to `BIG_POOL_END` = 4 GiB (`u64`), 1 MiB
chunks, ~500-byte bitmap), filled by `mark_big_region_free` per E820 region (only
chunks `paging::identity_mapped`). If the big pool has no run that long, `alloc`
falls back to the 4 KiB frame pool (4 MiB reserve kept); `free` tells them apart
by address, so a 128 MiB guest can still `alloc`. Requests round up to whole chunks, memory is **not
zeroed**, and there is no auto-free on return (a tape that never frees leaks until
reboot). `tapes/bigsieve/` (64 MiB sieve, exact prime-count check, avoids `u32`
overflow of `candidate * candidate`) is the reference; `make tape-bigsieve`,
`tapes/test_bigsieve_e2e.py` (needs QEMU `-m 256`). Full detail is in
`docs/history/development-notes.md`.

### Tape ABI grows again: `read_key` and startup arguments

- **Startup arguments**: `tape run TEXT...` passes the raw trailing bytes as
  `args_ptr: u32`/`args_len: u32`; no quoting convention is imposed and the slice
  points into the already-parsed command line (valid during the synchronous call).
- **`read_key: extern "C" fn() -> u32`**: non-blocking, call once per loop pass;
  `0` = nothing, `1` (`TAPE_EVENT_ABORT`) = stop, `0x100 + byte` = input. It reads
  whichever source started the tape (local keyboard or `term net`). **A tape using
  `read_key` must not also call `should_abort`**: both drain the same sources and
  would race for keystrokes. A `TERM_TAG_DATA` frame can carry many bytes, so
  `poll_remote_tape_input` queues everything past the first into
  `TAPE_INPUT_QUEUE` (256 bytes), drained before the network is polled again.
  Backspace (ASCII 8) makes `read_key` call `console::erase_last_character()`
  itself unless raw mode is set (`set_input_mode`, see "Cursor keys in tapes"), so
  a tape's line editor just decrements its length. `tape-rt` is the natural home
  for line-reading helpers. `tapes/calc/` is the reference user (RPN calculator;
  `tape run 3 4 +` or an interactive REPL; `tapes/test_calc_e2e.py`). Full detail is
  in `docs/history/development-notes.md`.

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

Trailing callbacks after `args_ptr`/`args_len` (append-only cdecl, a networked tape
must declare every earlier parameter): `nic_mac(u32)->u32`, `nic_send(u32,u32)->u32`,
`nic_receive(u32,u32)->u32` (active interface; `nic_mac` must be called first and a
zero return is failure, not an idle network), then `nic_max_frame_len()->u32`,
`ticks()->u64` (the PIT tick count, for deadlines; tapes must not reprogram the
timer), then the multi-interface set `nic_count`, `nic_mac_at`, `nic_send_at`,
`nic_receive_at`, `nic_set_promiscuous_at` (zero-based indices, unlike the shell's
one-based `nic use N`; promiscuous mode is for `tapes/bridge/` and must be restored
before returning). Later slots: 36 `wait_for_event`, 37 `set_input_mode`, 38
`micros`, 39/40 `alloc_frames`/`free_frames` (the 4 KiB frame pool below the big
pool's 128 MiB start; the kernel keeps a 4 MiB reserve; `tapes/memtest/`), 41
`kernel_stat(id, arg)` (free frames/big chunks, per-NIC recoveries/overruns, RAM reach), 42
`e820_entry(index, out)`. `alloc`/`free` use the big pool, falling back to frames.

The kernel owns every NIC (PCI, DMA, rings, overflow recovery) and never parses
ARP/IPv4/UDP/TCP: those live in the fixed-buffer `tapes/tape-net/` library
(NETWORKING.md). Because raw reception and `term net` input share one ring, the
kernel keeps non-terminal frames for the tape (a four-frame FIFO, and a frame
polled with a too-small buffer is retained for a retry sized by
`nic_max_frame_len()`) and queues terminal frames for `read_key`. DHCPv4 is
protocol-library code too: a consuming tape exposes `tape run dhcp` and a fallback
such as `static IP/PREFIX GATEWAY`. References: `tapes/net-chat/` (shell `chat`
ethertype/tags; `tapes/test_net_chat_e2e.py`), `tapes/ip-chat/` (ARP/IPv4/UDP;
`test_ip_chat_e2e.py`, two static-IP guests), `tapes/telnet/` (single-connection
client with a bounded VT100 subset in the kernel's local VGA callback; auth, DNS and
multi-connection are out of scope; `test_telnet_e2e.py`). Full detail is in
`docs/history/networking-and-discovery.md`.

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

`shell::run` calls `term_net` once before the prompt loop, so every machine waits
for a remote client at boot; a local operator presses a "give up" key
(`keyboard::ESCAPE_SCANCODES`: F12 first, then F4, F6, F7, F8, F10, since some KVMs
eat F12) to reach the prompt. There is deliberately no serial equivalent. F2 is
reserved for `term_printer`'s "abandon this line". Parallel-port printing:
`kernel/src/parallel.rs` is an SPP-only LPT driver found via the BIOS Data Area
(`0x408`-`0x40E`); `Printer::write_byte` strobes and polls Busy up to
`BUSY_POLL_ATTEMPTS` (200,000), reporting a `PrinterFault`. Status polarities
follow the common convention and are only checked under QEMU `-parallel`.
`term printer`/`term LPT1..4` and `print [diskN] NAME` / `print run CMD...` use a
second relay queue (`PRINT_RELAY_QUEUE`) fed from `relay_push`; typed input is
echoed teletype-style and F2 abandons the local buffer; `help` refuses while a
printer session is active; tape output reaches the printer through
`relay_pending_print_output()`. Real-hardware follow-ups (dead CMOS battery faking
"out of paper", dot-matrix line buffering) are in
`docs/history/development-notes.md`; the rest in
`docs/history/networking-and-discovery.md`.

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

`tools/send_tape.py`, `term_net.py`, `send_frame.py` need `AF_PACKET` raw sockets.
Do not run an agent as root; a human grants the capability once to a dedicated
copy of Python (an agent must never attempt this silently):

```sh
cp "$(command -v python3)" ~/.local/bin/python3-netraw
sudo setcap cap_net_raw+eip ~/.local/bin/python3-netraw   # verify: getcap
~/.local/bin/python3-netraw tools/term_net.py --iface wlp2s0 --mac MAC "tape status"
```

Always pass `--iface` explicitly (the default is the alphabetically first up
interface, which may be a VPN), and keep `send_tape.py`'s `--delay 0.003` pacing.
Prefer `tools/devloop.py` (below). Full detail is in
`docs/history/real-hardware-notes.md`.

### Ending blocking commands from a term net session

`tape load net`, `nic sniff` and `nic discover` used to end only on a local
F12: they consumed the session's own frames, and a loop polling a *different*
NIC than the session's could never see its disconnect at all (the real machine
had to be walked to twice). `remote_stop_frame` now treats the client's
disconnect (`TERM_TAG_BYE`, which also sets `PENDING_REMOTE_BYE` so the session
ends afterwards) or its escape byte (`REMOTE_ABORT_BYTE`, the remote F12) in a
frame the loop receives as "stop"; `remote_stop_via_session_nic` polls the
session NIC when it is not the one the loop polls. After an escape the session
keeps working. `make test-release-loops` covers each loop on both the session
NIC and a different one.

### Register access from the shell (`pci read|write`, `io`, `mmio`, `nic regs`)

Real-hardware-only bugs are found by reading registers, and each diagnostic
kernel push costs minutes, so the shell can read them directly: `pci read B:D.F
[OFFSET [LENGTH]]` and `pci write B:D.F OFFSET VALUE` for config space (MSI
capabilities, interrupt routing), `io r8|r16|r32 PORT` / `io w... PORT VALUE`,
`mmio r8|r16|r32 ADDR` / `mmio w... ADDR VALUE` (only mapped addresses --
`paging::is_readable`/`is_writable` refuse the rest instead of faulting -- and
never kernel text), and `nic regs [N]` (RTL8168 and 3C905 dumps; add one per
driver as needed). `make test-hwaccess` checks them against known QEMU values.

### `tools/devloop.py`: the safe way to drive a real machine

One wrapper (run it with `python3-netraw`) over `term_net.py`, `send_tape.py`
and `send_kernel_update.py`: `wait`, `run CMD...`, `push KERNEL` (KUP, then waits
for the new boot), `tape-into N FILE` and `put FILE NAME` (installs a file on the
machine's FOSFS volume without reflashing: loads it as a tape, then `tape save`;
it validates the name, refuses a `debug` file without `kernel-update` unless
`--force`, backs up the old file to `build/devloop-backup/`, and reads the result
back; pass a real MAC as `--mac` so it can pick a different NIC to load through).
e.g. `devloop.py --iface IF --mac MAC put tools/default-debug.txt debug` enables
the watchdog on a running machine. Prefer it to ad-hoc scripts: it encodes
the traps that gave false results bringing up the RTL8168 and 3C905 -- `tape
load net` listens only on the *selected* NIC (it runs `nic use N` and restores
the old selection), the term session is held on a *different* NIC and never
killed mid-load (a disconnect on another NIC cannot end the load), success
requires the tape handle to advance (not just a matching size), a gapped
transfer is retried slower, and a one-byte release tape always frees the wait.
`term_net.py --until TEXT` makes its *last* command wait for a marker instead of a
quiet period: a blocking command that prints as it goes (`tape load net` now
relays its output live while polling a different NIC) is no longer "finished"
the moment output pauses, which used to make the client send a disconnect and
end the command itself.

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
local disk in two steps instead. **Serving happens at the idle prompt
(`ambient_discovery_poll`) and in both of `term_net`'s receive loops, all
through `dispatch_netfs_serve_frame`** (a GET pauses an active session while
it is sent): never inside `chat`, `nic discover` or a running tape. The host
side is `tools/foscopy.py` (`HOST[:diskN]/NAME` syntax, `--mcast` test seam;
`tools/test_foscopy_e2e.py`, in `run_tests.py`'s `E2E_SCRIPTS`). Unlike ordinary discovery traffic, NETFS activity
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



**Network-polling loops also halt (`shell::idle_wait`).** `term_net`'s wait and
session loops, `nic sniff`, `nic discover`, `tape load net` and the NETFS wait
loops call `idle_wait()` (`sti; hlt`, `arch::x86::wait_for_interrupt`) when an
iteration found nothing, so they no longer pin the CPU. This is only done when
every NIC has an enabled receive IRQ (`nic::all_rx_interrupt_driven`), so a
frame wakes the CPU immediately and no NIC looks silent (8+ frames
completed with 0 interrupts); otherwise the loops spin as before. The 3C905s
on the AMD/nForce test machine used to trip that check -- see "3C905 interrupts"
below -- and now deliver one interrupt per frame. Loops that
read COM1 (`term`, `chat`) must not halt: serial has no IRQ and a 16-byte FIFO
can overflow. Tapes get the same via the last ABI callback, `wait_for_event`
(slot 36, `tape_wait_for_event` -> `idle_wait`). `tapes/tape-rt` stores it
(`set_wait_hook`) so any loop can call `tape_rt::idle()` without a new
parameter; `tape-net` itself is a pure protocol library with no waits. Router,
bridge, net-chat, ip-chat, telnet, net-dhcp-test, calc and ed use it; a loop
must sleep only on a pass with no frame *and* no key (`read_key` yields one
event per call). The audio tapes and beep/morse timing loops still spin. `make test-idle-cpu`
measures QEMU's host CPU use (about 7% idle vs 100% for a spinning tape).
Halting only costs a frame no latency because NIC IRQs really fire; see the
PIC-mask note in `docs/history/development-notes.md` before touching
`interrupts::use_kernel_pic_mapping`.
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

### NIC MSI (RTL8168 and e1000e)

`nic::try_enable_msi` runs per NIC in `nic::initialize` for the two backends
that implement `trigger_test_interrupt` (RTL8168: FSWInt via `TPPOLL`; e1000e:
Link Status Change via `ICS` -- QEMU's 82574L ignores Linux's RXSEQ choice). It
enables the PCI MSI capability on a vector from `interrupts`, forces one
interrupt, and waits `MSI_SELFTEST_TICKS` (3) for the message; on failure it
disables MSI again and the NIC keeps its legacy IRQ (boards such as nForce may
lack an HT MSI mapping). The `no-msi` settings option skips all of it.
`nic list`/`irqstat` report the vector. NICs on MSI are exempt from the
storm/suspect rules. `make test-msi` (part of `make test`) covers it on QEMU's
e1000e. Confirmed on the AMD/nForce machine: both RTL8168s take MSI (the
RTL8168G/H ignores `FSWInt`, so the test also enables Tx-OK and sends one
broadcast frame). The I350 (`ICS` 0x1504, Link Status Change) uses the same
path and is confirmed on the second machine (e1000e + two I350 ports, all on
MSI, 284 KB tape loads on each with no errors); QEMU has no I350 model.
The e1000e (82579, `8086:1503`) also takes MSI on a Sandy Bridge board (single
NIC, so no tape-load test there; counters clean after normal term net traffic).

### `nic map`, the microsecond clock, asynchronous transmit

`nic map` (`shell::nic_map`) prints each NIC's link and which other NICs of the
machine hear its broadcast probes (ethertype `0x88b9`, magic `FOSMAP`); it
retries up to three rounds while nothing is heard (links coming up). Link state
comes from `Nic::link_status` (e1000e, I350 and RTL8168 read `STATUS`; the I350
read was removed once after a scare and works again; 3C905 has `link_up`).
`interrupts::micros()` (ABI slot 38, `tape_rt::micros`) reads the TSC when the CPU
reports an invariant one (CPUID 0x80000007 EDX bit 8; calibrated once against
the PIT clock for 50 ms on first use) and otherwise combines the tick counter
with the PIT's current count; the PIT runs in mode 2 (rate generator, one
sweep per tick) instead of mode 3 for that reason. The PIT path costs three
legacy port accesses, which capped nettest at ~40k frames/s; the TSC path lifted
it to gigabit line rate (Skylake, 1500 B frames). `send` on the e1000, e1000e
and I350 is asynchronous: 64 TX descriptors with contiguous buffers, and it only
waits when the descriptor it needs is still in flight (a stuck one still sets
`recovery_pending`). The RTL8139 (4 slots), RTL8168 (32 descriptors) and 3C905
(one frame in flight, collected at the next `send`) are asynchronous the same way; the RTL8139
is tested in QEMU, the RTL8168 and 3C905 changes are real-hardware-only and not yet
verified (the AMD machine currently has only 3C905s and was off). `tapes/nettest/` also has a generator and a sink (`gen=N`,
`sink=N`; engine in `tapes/nettest/src/lib.rs`; ready-made `selftest`/`routertest`
set-ups; the user manual is `docs/nettest.md`), tested with a four-NIC QEMU
loop (`make test-tape-nettest-selftest`); `tools/test_nic_map_e2e.py` covers
`nic map`.

### Cursor keys in tapes

Arrow/Home/End/Delete keys reach a tape through `read_key` as ANSI sequences
(`keyboard.rs` generates them locally; `term net` forwards them), and
`tape-rt`'s `KeyDecoder` decodes them. Over `term net`, only a frame holding
a lone `0x1b` is the remote abort (`shell::is_remote_abort`); a longer payload
starting with Escape is input. ABI slot 37, `set_input_mode` (bit 0 = raw
keys), stops `read_key` erasing on Backspace for a tape that redraws itself
(`tape_rt::set_raw_keys`). `ed`'s `eN` is the reference user;
`tapes/test_ed_e2e.py` (term net) and `tapes/test_ed_keys_e2e.py` (local PS/2
via the QEMU monitor) cover it.

