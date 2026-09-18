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
| `0x100000`-`0x500000` | `kernel/src/pmem.rs` (`TAPE_BASE`/`TAPE_LEN`), any tape's own linker script | The fixed 4 MiB region every tape (`tape load disk`/`tape load net`) is always loaded into, carved out of `pmem`'s general frame pool at boot so `allocate`/`allocate_contiguous`/`palloc` can never hand these frames to anything else. See "Tapes: fixed load address, and writing one in Rust" below. |

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
make image    # Produces build/fos.img only. Plain `make` is equivalent.
make run      # Boots it interactively in QEMU.
make test     # Runs the serial-driven QEMU smoke test.
make all      # Builds everything: image, netrom, and every test tape.
make tapes    # Builds all test tapes only (`build/tapes/*.bin`).
make help     # Lists every target with a one-line description.
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
`cpuinfo`, `hexdump ADDRESS LENGTH`, `halt`, and `reset`.

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
broadcast or unicast text chat; see below), `tape` (load and execute a
raw binary from disk or over Ethernet; see below), and `term net` (a full
remote shell over Ethernet; see below).

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

Because a tape can write VGA memory and move the hardware cursor directly
(see `tools/tapes/primes.S` below) with no way to tell `console.rs` it did
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

`tools/tapes/primes.S` is a worked, non-trivial test tape: a
Sieve-of-Eratosthenes prime counter up to 1,000,000, assembled the same
way as `boot/stage1.S`/`boot/stage2.S` (`as --32` then `objcopy -O
binary`, via the new `make tape-primes` target -- entirely separate from
`make image`/`run`/`test`) rather than compiled by `kernel/`'s own Cargo
build. Unlike the throwaway one-byte `0xc3` tape used to validate `tape`
itself, this one has real data (a 1,000,000-byte sieve array) and so must
be genuine position-independent code: its very first instructions are the
classic `call get_base` / `pop %ebx` idiom to capture its own runtime load
address, since `pmem::allocate_contiguous` gives it no fixed one, and
every later label reference is computed as `%ebx + (label - get_base)`,
an assemble-time constant GNU `as` resolves within this single unlinked
file exactly the way `boot/stage2.S` already resolves its own `label +
STAGE2_BASE` expressions. The sieve array is embedded as literal
zero-filled space (`.fill N, 1, 0`) directly in the file rather than
assumed to be available in whatever memory happens to follow the tape's
own buffer, since only the declared/transferred length is ever actually
allocated for it (see the `tape` protocol notes above) -- this is also
why the assembled binary is roughly a megabyte, sent over the network in
~1000 chunks by `tools/send_tape.py`, since FOSFS's 512-byte file cap
can't hold it. Because a tape can't call back into the kernel for timing
or console output either, it measures elapsed time with a raw `rdtsc`
(reported as uncalibrated CPU cycles, not wall-clock time) and
reimplements VGA text output and CRTC cursor handling by hand, matching
`console.rs`'s own conventions byte-for-byte, plus mirrors every printed
character to COM1 (matching `console.rs`'s own dual VGA+serial habit) --
which is what let this be checked end-to-end by a disposable
serial-driven test harness (`tools/tapes/test_primes_e2e.py`, a QEMU
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
`tools/tapes/test_primes_e2e.py` only reads QEMU's `-serial stdio`
stream, which both bugs left completely untouched. The fix was simply to
have `print_decimal` use `%edi` (free at every call site, and already
proven safe to hold across `print_char` -- `print_char_scroll` itself
saves/restores it) as its digit counter instead of `%ebx`.

Because both of these were VGA-only bugs invisible to the existing
serial-based test, `tools/tapes/test_primes_vga_e2e.py` was added
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

`tools/tapes/primes.S` was rewritten around this: its `_start` prologue
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
That's exactly why `tools/tapes/primes.S` had to be genuinely
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

This also means `tools/tapes/primes.S` no longer *needs* to be
position-independent -- but it still works unchanged (a fixed address is
still a valid address for PIC code, it's just no longer required), so it
was left as-is, as the assembly reference example.

`tools/tapes/hello-rs/` is the new Rust reference example, proving the
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
- `tools/tapes/hello-rs/src/entry.S` is consequently tiny: it exists
  solely to zero `.bss` (the same reason `kernel/src/entry.S` does it --
  a flat binary's `.bss` bytes are only implicitly zero-filled by
  `objcopy -O binary`'s padding, never explicitly, so Rust statics can't
  be trusted to start zeroed without this) before tail-`jmp`-ing into the
  real Rust entry point, `tape_main`, so `tape_main`'s own `ret` returns
  straight to `tape_run`.

`hello-rs`'s `tape_main` mirrors `primes.S` in miniature -- prints a
greeting, sieves primes up to a much smaller 1,000 (168 primes, largest
997) using a `static mut` array rather than a stack buffer (same
reasoning as `primes.S`'s `.bss`-resident sieve: SIEVE_LIMIT bytes of
stack in a tape running on the kernel's own stack, with no guard page,
isn't worth the risk), and returns -- proving real, non-trivial compiled
Rust code runs correctly at the fixed tape address, not just a string
literal. It's built with `make tape-hello-rs` (producing
`build/tapes/hello-rs.bin`) and verified end to end over the network with
`tools/tapes/test_hello_rs_e2e.py`, mirroring `test_primes_e2e.py`.

The same fixed-address convention would work identically for a C tape
(`gcc -m32 -ffreestanding -fno-pic` with a matching linker script) --
Rust was chosen here only because it reuses the kernel's own toolchain
and build-std setup with far less new scaffolding.

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
rerunning `tools/tapes/test_primes_e2e.py`/`test_hello_rs_e2e.py`
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

`tools/tapes/vga-colors/` is the reference tape exercising this: same
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
and verified with `tools/tapes/test_vga_colors_e2e.py`, which goes beyond
the usual serial-stream check (there is deliberately very little to see
there): it reads the real VGA text framebuffer via the QEMU monitor's
`pmemsave` (the same technique `test_primes_vga_e2e.py` uses) to confirm
several distinct colors actually landed there, then uses the monitor's
`sendkey f12` to simulate a *local* physical keypress and confirms both
that the tape's loop stops (and control returns to the fos shell) and
that the exact same `term_net` session driving it remotely is still
alive afterward -- the concrete proof of the guarantee `tape_should_abort`
documents, not just a read of its doc comment.

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

`tools/tapes/test_vga_colors_e2e.py` was extended to prove this actually
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
`tools/tapes/test_primes_e2e.py`'s approach; see
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
test scripts (`tools/tapes/test_primes_e2e.py`,
`tools/tapes/test_hello_rs_e2e.py`, `tools/tapes/test_primes_vga_e2e.py`,
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
project's own e2e tests (`tools/tapes/test_primes_e2e.py`, `tools/
tapes/test_primes_vga_e2e.py`, `tools/tapes/test_hello_rs_e2e.py`, and
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
original rationale for using a function key in the first place. F1/F2/F3/
F5/F9/F11 were deliberately left out since some terminals bind those to
their own emulator-level features (new tab, fullscreen, etc.); F12 stays
first in the list and in every user-facing message since it remains the
most commonly documented, most muscle-memory choice.


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
  usage). Sending `tools/tapes/primes.bin` (~1 MB, 978 chunks) with
  `send_tape.py`'s default (no `--delay`) reliably reproduced the stall;
  the same tape with `--delay 0.003` (3 ms between chunks) reliably
  succeeded -- isolating the cause to the receive ring overflowing under
  an unpaced burst, something a real NIC sending at its own wire speed
  can do far more easily than QEMU's virtual NIC ever does (QEMU's own
  end-to-end test for this same tape, `tools/tapes/test_primes_e2e.py`,
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
