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
  section below for why), loads the kernel at physical `0x10000`, and enters
  flat 32-bit protected mode.
- `kernel/` is a freestanding Rust binary. `linker.ld` reserves the initial
  low-memory kernel region through `0x40000`; do not place loader, boot-info,
  or disk-trampoline data there.
- The packed version-2 `BootInfo` at physical `0x5000` is the loader/kernel ABI.
  Validate its magic, version, size, and count before using it. Use unaligned
  reads for packed multi-byte fields.
- `kernel/src/disk.rs` invokes the stage-two BIOS EDD trampoline. It performs
  exactly one sector transfer and only after validating the LBA and buffer
  constraints. Do not call BIOS interrupts directly from protected mode.
  Because the trampoline drops into real mode, and the kernel's own IDT/PIC
  remap would otherwise break BIOS's real-mode interrupt handling, `transfer`
  briefly restores the real-mode IVT (`IDTR` base 0) and the BIOS-conventional
  PIC vector mapping around each individual BIOS call, then switches back.
- `kernel/src/interrupts.rs` builds a 256-entry IDT (only IRQ0/IRQ1 gates
  populated), remaps the 8259 PIC clear of the CPU exception range, arms the
  PIT at 100 Hz, and services the timer/keyboard IRQs with minimal
  `x86-interrupt` ABI handlers. Input is interrupt-driven, not polled.
- `kernel/src/pmem.rs` is a small bitmap-based physical-frame allocator (4 KiB
  frames) built from the E820 map; it reserves everything below the kernel's
  own low-memory region. There is no paging.
- `kernel/src/fosfs.rs` implements FOSFS, a deliberately small custom
  filesystem: one superblock, four directory sectors, 64 fixed entries, and
  contiguous one-sector files.
- `boot/netrom.S` is an optional, parallel boot path: a PCI expansion ROM that
  boots fos from a NIC's own boot ROM instead of disk/USB (`make netrom`,
  producing `build/netboot.rom`; never part of `make image`/`run`/`test`).
  It reuses `stage2.S`'s narration/E820/A20/GDT logic but must treat its own
  ROM image as read-only at run time (a shadowed option ROM cannot be
  written back to) -- see "Low-memory map" below for the scratch RAM this
  forces it to use instead.

## Low-memory map

Real-mode code before the kernel's own memory management exists shares a
handful of fixed low-memory addresses across `stage1.S`, `stage2.S`, and
`netrom.S`. There is no single enforcing mechanism for this -- it is
convention only -- so treat this table as authoritative before adding any new
fixed address below `0x40000`, and update it in the same change that adds one:

| Address | Used by | Purpose |
| --- | --- | --- |
| `0x500`-`0x510` | `stage2.S` (`verify_a20`) | Two probe bytes used to detect whether the A20 line is enabled. |
| `0x600`-`0x613` | `netrom.S` | Scratch RAM for state a shadowed (read-only) option ROM cannot mutate in place: computed ROM segment/physical base, serial-availability flag, and the GDT/protected-entry descriptors used to enter protected mode. |
| `0x5000` | `stage2.S`, `netrom.S`, `kernel/` | The packed `BootInfo` struct: the loader/kernel ABI. |
| `0x7a00` | `stage2.S`, `netrom.S` | Real-mode stack pointer during boot. |
| `0x7c00` | `stage1.S` | MBR load address (fixed by the BIOS). |
| `0x8000` | `stage2.S` | Stage-two load address (fixed by `stage1.S`). |
| `0x9f000` | `stage2.S`, `netrom.S` | Protected-mode stack pointer, just below the conventional End-of-Low-Memory / EBDA boundary. |
| `0x10000`-`0x40000` | `stage2.S`, `netrom.S`, `kernel/linker.ld` | The Rust kernel's load address and reserved low-memory region. |

## Educational verbosity

This project exists to be read and explained, not just to run. Boot stages
and kernel subsystems should narrate what they are doing and why in their
status messages (e.g. which BIOS interrupt is being called, what a PIC remap
or IDT load accomplishes), not just announce bare completion. When adding a
boot step or kernel subsystem, prefer an explanatory message over a terse one,
and keep `README.md`'s command table and "What happens at boot" walkthrough in
sync with the actual command surface and boot sequence.

## Disk image and safety

`tools/build_image.py` creates `build/fos.img`, a 4 MiB raw image:

| Location | Contents |
| --- | --- |
| LBA 0 | MBR / stage-one loader |
| LBA 2048 | Active raw boot payload partition |
| First 32 payload sectors | Stage-two loader |
| Following payload sectors | Flat 32-bit Rust kernel |
| LBA 4096 | Dedicated 4096-sector FOSFS data partition |

The image builder is the single source of truth for partition placement.
`tools/write_layout.py` emits matching stage-loader constants from the actual
kernel size. Never add host block-device writes, automatic device selection,
or commands that let FOSFS write outside its own partition. Copying an image
to a device is destructive and must remain an explicit user action.

## Build and validation

The pinned toolchain is `nightly-2026-09-13`, with `rust-src`. Required host
tools are GNU Make, GNU `as`/`objcopy`, Python 3, and `qemu-system-i386`.

```sh
make image    # Produces build/fos.img only.
make run      # Boots it interactively in QEMU.
make test     # Runs the serial-driven QEMU smoke test.
```

Always run `make test` for boot, loader, kernel, shell, disk, or FOSFS changes.
The smoke harness must use bounded waits, preserve serial output on failure,
and wait for the prompt before sending input.

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

System commands: `help`, `clear`, `about`, `bootinfo`, `memmap`, `memory`,
`cpuinfo`, `hexdump ADDRESS LENGTH`, and `halt`.

Interrupt/memory commands: `ticks`, `uptime`, `irqstat`, `pmem`, `palloc`, and
`pfree ADDRESS`.

Storage commands: `diskinfo`, `readsector LBA`, `format`, `ls`, `cat NAME`,
`write NAME TEXT`, and `rm NAME`.

Serial commands: `serial` (lists COM1..COM4 via the BIOS Data Area, plus any
PCI-discovered serial controllers as `PCI0`, `PCI1`, ...) and
`term [COM1|COM2|COM3|COM4|PCI0|PCI1|...] [BAUD]` (raw terminal pass-through;
F12 exits).

`readsector` is read-only. `hexdump` is limited to the reserved kernel range
and at most 128 bytes. FOSFS names are limited to 16 uppercase alphanumeric or
`.`, `_`, and `-` characters. The fixed command buffer and single-sector
filesystem are intentional teaching constraints.

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

Ethernet commands: `nic` (status; probes and initializes the RTL8139 driver
on first use), `nic send` (one raw broadcast test frame), `nic recv`
(polls briefly for one incoming frame and hex-dumps it), `nic sniff`
(promiscuous-mode capture of every frame seen, until F12 stops it),
`nic discover`/`nic peers` (broadcast/list a numbered address book of other
listening fos machines), `chat [PEER_MAC|PEER_NUMBER]` (line-based
broadcast or unicast text chat; see below), and `tape` (load and execute a
raw binary from disk or over Ethernet; see below).

`nic` is backed by `kernel/src/rtl8139.rs`, a polling-mode driver for the
Realtek RTL8139 -- chosen as the one supported NIC because it is
well-documented, common in real PCI-era hardware, and QEMU's default
emulated card. Unlike serial cards (found by class code, since UARTs share
one register model across vendors), the RTL8139 is found by exact
vendor/device ID (`pci::find_device`, Realtek `0x10ec:0x8139`), since NIC
register layouts are vendor-specific. `pci.rs` gained a second capability
for this: `PciAddress::enable_io_and_bus_master`, setting the PCI command
register's I/O-space and bus-master bits (the latter required for any
device that performs DMA, unlike the UARTs, which only ever use plain port
I/O).

The driver polls status registers rather than using the card's interrupt
line -- consistent with `interrupts.rs` leaving every IRQ but the timer and
keyboard masked at the PIC -- and asks `pmem` for real physical frames for
its DMA buffers rather than using `static` kernel memory, since a receive
ring large enough to be useful would not fit in the kernel's small reserved
low-memory region (`linker.ld`). Its receive ring must be one physically
contiguous block, which single-frame-at-a-time `pmem::allocate` cannot
guarantee, hence `pmem::allocate_contiguous` (this project's first caller to
need more than one contiguous frame). There is no protocol above raw
Ethernet frames (no ARP, no IPv4, no sockets) and no support for any NIC
besides the RTL8139; both are explicit scope boundaries, not oversights.

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
`CHAT_TAG_DISCOVER_REQUEST`, re-sending it every `DISCOVER_REBROADCAST_TICKS`
(a plain `interrupts::ticks()` comparison, not a timer interrupt) so a peer
that only starts `chat`/`nic discover` partway through is still found, and
busy-polls `receive` -- printing each newly-seen peer as it arrives -- until
`keyboard::poll_term_event()` reports F12, the same escape convention
`nic sniff`/`term`/`chat` all use; `nic peers` just prints the table.
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

`tape` (`shell.rs`) is a "load, then run" pair built the same way as
everything else here: no new abstraction layer, just a fixed-size physical
buffer (`pmem::allocate_contiguous`, freed and replaced as a whole by each
new load, tracked in one `static mut TAPE: Option<(address, length)>`
accessed only through raw-pointer `tape_get`/`tape_set`/`tape_take` helpers,
the same convention `PEERS`/`PEER_COUNT` already use) and a `call` straight
to its first byte. There is no size ceiling baked in beyond whatever
contiguous RAM `pmem` still has free -- a declared length that doesn't fit
just fails the load cleanly (`Not enough contiguous free physical memory
for a N-byte tape ...`) rather than being silently truncated or rejected by
an arbitrary constant.

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
that attempt and frees its frames immediately, since this protocol
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

`tape load disk NAME` reuses `fosfs::read` the same way, bounded by FOSFS's
existing 512-byte one-sector file limit. Because `write NAME TEXT` (the
only way to put a file on FOSFS from the interactive shell today) only
ever accepts printable ASCII text, a disk-loaded tape's contents are, for
now, whatever printable-ASCII-constrained bytes ended up in that file --
genuinely useful machine code on disk would need to arrive some other way
(most practically, `tape load net` followed by a future `tape save disk`,
which does not exist yet). This is an honest, currently-real limitation,
not a bug; see the FOSFS multi-sector-file extension noted as deferred
future work below, which this limitation is also downstream of.

`tape run` is a bare `unsafe extern "C" fn()` `call` (via
`core::mem::transmute`) to the loaded buffer's physical address, which
this kernel's flat, unpaged memory model lets it also treat directly as a
usable pointer -- the same convention `rtl8139.rs`'s own DMA buffers rely
on. A tape that ends with a near `ret` returns control to the shell
cleanly (verified end-to-end with a real one-byte `0xc3` tape sent over a
live QEMU multicast netdev from a host-side Python script and executed);
anything else is trusted exactly as much as any other kernel code -- a bad
opcode is at least caught by the CPU exception handlers added earlier this
project (see "Real hardware notes" below) rather than corrupting silently,
but there is still no way back into a hung or corrupted kernel except a
reset, consistent with "Current boundaries" in README.md.

The reserved low-memory kernel region (`linker.ld`/`pmem::RESERVED_BELOW`/
`tools/write_layout.py`'s `KERNEL_RESERVED_END`) grew from `0x20000` to
`0x40000` alongside `tape`, since the feature's own code pushed the kernel
binary past the previous 64 KiB ceiling with essentially no headroom left
beforehand (the prior build was already only dozens of bytes under it).
All three constants must always match; see "Low-memory map" above for why
growing this region is safe (nothing else lives between `0x20000` and the
real-mode stack pointer's `0x9f000`).

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

If real hardware ever produces a boot loop, a screen that fails to update,
or output that only appears over one of VGA/serial, suspect this class of
gap first -- QEMU's emulated devices are close to real hardware but not a
perfect model of the more idiosyncratic chipset-level behavior above.

## Deliberately deferred work

- Paging, heap allocation, filesystems beyond FOSFS, userspace, scheduling,
  native USB, further hardware IRQ drivers beyond the timer/keyboard, robust
  fault recovery, any protocol above raw Ethernet frames (no ARP, no IPv4,
  no sockets), and support for any NIC besides the one RTL8139 driver.
- A native ATA/AHCI disk driver. The current protected-mode-to-real-mode BIOS
  trampoline is educational and intentionally narrow, not a scalable disk
  architecture.
- Broad physical-machine support. The documented target requires legacy BIOS,
  BIOS EDD/LBA, E820, VGA text, COM1, and PS/2-compatible input. QEMU is the
  primary validation environment.

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
