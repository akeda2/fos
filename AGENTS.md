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
| `0x7a00` | `stage2.S`, `netrom.S` | Real-mode stack pointer during boot. |
| `0x7c00` | `stage1.S` | MBR load address (fixed by the BIOS). |
| `0x8000` | `stage2.S` | Stage-two load address (fixed by `stage1.S`). |
| `0x9f000` | `stage2.S`, `netrom.S` | Protected-mode stack pointer, just below the conventional End-of-Low-Memory / EBDA boundary. |
| `0x10000`-`0x60000` | `stage2.S`, `netrom.S`, `kernel/linker.ld` | The Rust kernel's load address and reserved low-memory region. |
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
| First 32 payload sectors | Stage-two loader |
| Following payload sectors | Flat 32-bit Rust kernel |
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
make all      # Builds everything: image, netrom, and every test tape.
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
and `reset`.

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
instead: `objcopy -O binary` bakes a `.bss` array's *entire* size into
the shipped flat binary as zero bytes (there's no "reserved but unbacked"
concept once a `no_std` freestanding binary is flattened), so a 1 GiB
bitmap (32 KiB, up from the original 4 KiB) directly inflated
`kernel.bin` by ~28 KB. Stage two now avoids the former single-transfer
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
