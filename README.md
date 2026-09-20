# fos

`fos` is a deliberately small x86 operating system for learning how a machine
boots. It starts via **legacy BIOS**, enters **32-bit protected mode**, then
runs a Rust kernel without a hosted operating system or standard library.

It is not a general-purpose OS and is intentionally incomplete. The current
milestone demonstrates a custom BIOS loader, memory-map discovery, a
Rust/assembly handoff, VGA and serial consoles, an interrupt-driven PS/2
keyboard and PIT timer, a tiny physical-frame allocator, a small FOSFS
filesystem, supervisor-only identity paging, and a small command prompt -- all deliberately narrated with
verbose status output at every step, since this project exists to be read
and explained, not just to run.

## What happens at boot

1. The BIOS loads the MBR's 512-byte stage-one loader at physical address
   `0x7c00` and jumps to it. Stage one prints its own narration to the VGA
   text console (see `boot/stage1.S` for why it doesn't also touch serial).
2. Stage one normalizes the real-mode environment and uses BIOS EDD/LBA disk
   services (`INT 13h`, `AH=42h`) to load stage two from the raw payload
   partition at LBA 2048, then jumps to it at `0x8000`.
3. Stage two initializes the COM1 serial port and, from here on, mirrors
   every status message to both VGA text mode and COM1. It records the BIOS
   E820 memory map (`INT 15h`, `EAX=E820h`), enables the A20 address line,
   loads the Rust kernel to physical `0x10000`, masks the legacy 8259 PIC,
   and installs a flat GDT.
4. The loader enters 32-bit protected mode (`CR0.PE=1`) and far-jumps into
   the Rust entry point, passing a versioned boot-information record at
   physical address `0x5000`.
5. The Rust kernel clears BSS, validates that record, initializes its VGA/COM1
   console, enables a supervisor-only identity map for the low physical range,
   makes kernel text/read-only data write-protected, and registers PCI MMIO
   only when a driver claims its BAR, then
   builds its physical-frame bitmap from the E820 map. The shell sets up its
   own IDT and remaps the PIC (arming the PIT timer on IRQ0 and the PS/2
   keyboard on IRQ1), then runs the prompt. Each of these steps prints what
   it just configured and why.

The message `BIOS services end now` marks the important transition: code after
that point must not call BIOS interrupts directly from protected mode. The one
narrow exception is `kernel/src/disk.rs`'s trampoline, which briefly restores
the real-mode IVT and BIOS-conventional PIC mapping, and temporarily disables
paging, around each individual disk sector transfer -- see "Disk access and
FOSFS" below.

Enabling A20 (`enable_a20` in `boot/stage2.S`, duplicated in `boot/netrom.S`
for the same reason described under "Booting from a NIC ROM" below) tries
three methods in order, stopping as soon as a probe write/read pair
confirms A20 is actually on: first the BIOS itself (`INT 15h`, `AX=2401h`),
then the traditional 8042 keyboard-controller gate, and only as a last
resort the "fast A20 gate" system control port (`0x92`). That last method
used to be the *only* one this loader tried, and worked fine in QEMU, but
caused an immediate reset-and-reboot loop on a real AMD Athlon 64 X2
machine -- writing to port `0x92` on some real chipsets has side effects
beyond A20 that a pure emulator won't reproduce. Trying the gentler options
first, and only reaching for `0x92` when they don't work, fixed it there
without weakening A20 support on hardware (or emulators) that only
implements the fast gate.

## Requirements

- Linux or another Unix-like host with GNU Make, GNU binutils (`as` and
  `objcopy`), Python 3, and QEMU's `qemu-system-i386`.
- `rustup`, with the pinned toolchain and `rust-src` component declared in
  `rust-toolchain.toml`. `rustup` installs the pinned toolchain automatically
  on the first build when network access is available.

Build and run:

```sh
make image
make run
```

`make test` builds the image and performs a bounded serial-driven QEMU smoke
test plus a VGA-mode test. It checks boot milestones (including
interrupt/allocator setup), line editing, shell error handling, system
inspection, interrupt/memory introspection, FOSFS formatting, file creation,
reading, removal, halt, and the 80x25/80x50/experimental 132x25 local VGA
mode switches. `make test-vga-modes` runs only the VGA mode test.
`make test-stack-guard` builds a test-only image that deliberately reaches the
kernel stack's absent guard page and verifies that the emergency double-fault
task prints its diagnostic instead of resetting.
`make test-page-protection` similarly builds a temporary image that attempts a
supervisor write to kernel text and verifies the read-only page-fault
diagnostic before restoring the ordinary image.
For a deliberate comparison with the pre-paging teaching model,
`make image-unsafe-memory` writes `build/fos-unsafe-memory.img` with paging
disabled, then restores the normal paged `build/fos.img`; `make
test-unsafe-memory` boots that separate artifact through the smoke test. The
legacy image has no unmapped middle range or stack guard page, so it is a
demonstration artifact rather than a deployment choice.
`cargo test` does not work for this crate (a `no_std`, custom-target,
freestanding binary); `make test` is the primary automated kernel/boot
validation path, while the focused tape tests below cover host-side protocol
helpers and selected multi-guest scenarios.

`make test-tape-net` runs the host-side unit suite for the no-heap shared
networking library in `tapes/tape-net/`. Its synthetic-frame tests cover the
protocol code without requiring a NIC or QEMU. `make test-tape-ip-chat` runs
the host-side tests for the IPv4/UDP chat tape's argument and packet helpers;
`make test-tape-telnet` does the same for the Telnet tape.

`make netrom` builds an alternative, optional boot artifact,
`build/netboot.rom`, that boots fos from a NIC's own PCI option ROM instead
of disk/USB; see "Booting from a NIC ROM" below. It does not run as part of
plain `make` (still just `make image`) and does not affect `image`/`run`/
`test`, but is included in `make all` (see below).

`make all` builds everything this repository knows how to produce in one
go: the image, `make netrom`, and every test tape (`make tapes`, including
`tape-primes`/`tape-hello-rs`/`tape-vga-colors`/`tape-bigsieve`/
`tape-longmode`/`tape-longmode-rs`/`tape-calc`/`tape-ed`/`tape-beep`/
`tape-morse`/`tape-net-chat`/`tape-ip-chat`/`tape-telnet`/`tape-bridge`/
`tape-router` -- see "Tape (`tape`)" below). Not needed for ordinary
kernel/boot work, for which plain `make`
(equivalent to `make image`) is enough. `make help` lists every target
with a one-line description.

The kernel/tape networking boundary and the shared protocol library are
documented separately in [`NETWORKING.md`](NETWORKING.md). That document
covers the raw NIC callbacks, timer/queue semantics, and the staged
Ethernet/ARP/IPv4/ICMP/UDP/TCP implementation in `tapes/tape-net/`.

## Prompt

The prompt accepts fixed-size, allocation-free command lines of up to 256
characters. Enter and
Backspace work on both serial and the US-layout PS/2 keyboard subset. Both
input sources are interrupt-driven (IRQ1 for the keyboard) rather than
polled, alongside a free-running PIT timer on IRQ0. Writing characters into
the 0xb8000 text framebuffer does not move the blinking hardware cursor by
itself -- that is separate CRTC state (ports `0x3d4`/`0x3d5`) driven
explicitly after every VGA write so the caret always tracks the next write
position, matching what real hardware (verified on a Pentium 4 machine
booted from USB) requires. The screen is never cleared at boot, so that
VGA-only viewers keep seeing the full real-mode narration; the kernel
instead reads the cursor position the real-mode loader's own BIOS-teletype
printing already left behind and continues writing from there, rather than
resetting to row 0 and overwriting the top of that narration.

| Command | Description |
| --- | --- |
| `help` | Lists available commands. |
| `clear` | Clears VGA text and sends an ANSI clear request to serial terminals. |
| `vga [d\|80x25\|80x50\|132x25]` | Shows the current mode and available modes, or changes the local VGA text mode. `d` returns to the 80x25 default; 80x50 uses the VGA 8x8-font mode; 132x25 uses experimental extended horizontal timing and may not work on every adapter or monitor. |
| `about` | Describes this educational OS. |
| `bootinfo` | Shows boot drive, serial status, and the reserved kernel memory range. |
| `memmap` | Prints the BIOS E820 memory-map entries, decoded (usable RAM, reserved, ACPI, bad memory). |
| `memory` | Summarizes E820 usable and non-usable memory. |
| `cpuinfo` | Shows the CPUID vendor and raw feature bitfields. |
| `hexdump ADDRESS LENGTH` | Dumps up to 128 bytes inside the reserved kernel range. Addresses accept decimal or `0x` hexadecimal notation. |
| `ticks` | Shows the free-running PIT timer tick count and its configured frequency. |
| `uptime` | Shows elapsed time since boot, derived from the timer tick count. |
| `irqstat` | Shows how many times the timer (IRQ0), keyboard (IRQ1), and spurious IRQ7/IRQ15 interrupts have fired. |
| `pmem` | Summarizes the physical-frame allocator's total/free/used frame counts. |
| `palloc` | Allocates one 4 KiB physical frame and prints its base address. |
| `pfree ADDRESS` | Returns a previously allocated frame to the free pool. |
| `diskinfo` | Shows the total image size and FOSFS partition bounds. |
| `readsector LBA` | Read-only hexadecimal/ASCII dump of one 512-byte image sector. |
| `format` | Initializes the dedicated FOSFS partition, permanently removing its files. |
| `ls` | Lists FOSFS files. |
| `cat NAME` | Displays a FOSFS text file. |
| `write NAME TEXT` | Creates or replaces a flat FOSFS file. Names use up to 16 uppercase letters, digits, `.`, `_`, or `-`; a full directory or data region is reported distinctly. |
| `rm NAME` | Removes a FOSFS file entry. |
| `START` file | Optional boot-time command script. Each non-empty, non-`#` line is dispatched exactly like a shell command after disk/NIC initialization and before the automatic `term net` wait. |
| `serial` | Lists COM1..COM4 as detected by the BIOS at boot (base I/O port, or "not detected"), plus any serial controllers found on PCI/PCIe add-on cards (as `PCI0`, `PCI1`, ...), noting which is the local console. |
| `term [COM1\|COM2\|COM3\|COM4\|PCI0\|PCI1\|...] [BAUD]` | Enters raw terminal mode: forwards PS/2 keystrokes byte-for-byte to a remote machine over the chosen port (default COM1), and displays incoming bytes locally (VGA only, not re-echoed to serial). Press F12 (or F4/F6/F7/F8/F10) to return to the shell. COM1's rate is fixed at 115200 (shared with the local console); COM2/3/4 and PCI ports accept an optional custom `BAUD` that must evenly divide 115200. |
| `nic` | Reports the active supported PCI Ethernet controller and its MAC address; the first initialized interface is selected by default. |
| `nic list` | Lists every initialized Ethernet interface, its driver, MAC address, and active marker. |
| `nic use N` | Selects interface N from `nic list` for the shell and legacy single-interface tape callbacks. |
| `nic send` | Transmits one raw broadcast Ethernet test frame. |
| `nic recv` | Polls briefly for one incoming raw Ethernet frame and, if one arrives, hex-dumps it. |
| `nic sniff` | Enables promiscuous mode and prints every frame the card sees (destination/source MAC, ethertype, hex dump) until F12 (or F4/F6/F7/F8/F10) stops the capture, then turns promiscuous mode back off. |
| `nic discover` | Broadcasts a `chat`-protocol discovery probe through every initialized NIC (re-sent every few seconds) and lists every distinct responder as it replies, running until F12 (or F4/F6/F7/F8/F10) stops it (only fos machines currently running `chat` or `nic discover` reply). |
| `nic peers` | Prints the peer list built so far by `nic discover` (and by any auto-replies `chat` has sent), without sending anything. |
| `chat [PEER_MAC\|PEER_NUMBER]` | Enters a line-based Ethernet chat: broadcast to everyone by default, or unicast (send and receive) to one `aa:bb:cc:dd:ee:ff` peer -- or a numbered peer from `nic peers`/`nic discover`. Type a line and press Enter to send it; incoming lines are shown tagged with the sender's MAC. While running, also auto-replies to `nic discover` probes from other machines. Leave with F12 (or F4/F6/F7/F8/F10) (local keyboard) or by typing `/quit` (works over serial too). |
| `tape status` | Reports what (if anything) is currently loaded: its length and physical address, or that nothing is loaded. |
| `tape load disk NAME` | Loads a complete ELF32 or legacy-flat, multi-sector FOSFS file straight into the fixed tape buffer. |
| `tape load net` | Waits for a tape sent over Ethernet (see `tape send`/`tools/send_tape.py`): checks that the declared image fits the fixed 4 MiB tape region, validates each chunk's sequence number, and reports success or exactly where a transfer broke down. Press F12 (or F4/F6/F7/F8/F10) to give up. |
| `tape send NAME PEER_MAC\|PEER_NUMBER` | Reads a FOSFS file and transmits it as a tape to one peer's `tape load net` (or `tools/send_tape.py`, listening the same way). |
| `tape run` | Executes the loaded tape's validated entry point in ring 0, with no memory protection, passing console, input, allocation, startup-argument, kernel-managed raw-Ethernet, multi-interface, promiscuous-mode, maximum-frame-size, timer, terminal-position, and high-level FOSFS file callbacks. See `docs/rust-tapes-for-dummies.md` and `NETWORKING.md` for the full twenty-two-argument ABI. `tape run 3 4 +` passes `"3 4 +"` as a startup argument; `tape run router @ROUTER.CFG` reads router startup arguments from the uppercase FOSFS file `ROUTER.CFG`; `tape run` alone leaves arguments empty. If the tape ends with a near `ret`, control returns to the shell; anything else that faults is caught by the CPU exception handlers (see below) but is not recoverable. |
| `term net` | Waits for a remote terminal client over Ethernet (see `tools/term_net.py`) and, once one says hello, hands the live shell over to it entirely -- every other command works identically for the connected peer. Local PS/2/VGA input is disabled meanwhile; F12 (or F4/F6/F7/F8/F10) disconnects it (or the peer can disconnect first). While waiting or connected, it also auto-replies to `nic discover` probes; the waiting phase checks every initialized NIC. This same wait runs automatically at boot, and after a local operator gives up to use the shell, one minute without local keyboard or serial activity re-enters the wait. |
| `halt` | Stops the CPU. Reset or power-cycle to restart. |
| `reset` | Reboots the machine immediately via an 8042 keyboard-controller pulse reset -- the same classic mechanism real DOS-era reboot utilities used, still honored by QEMU and virtually every real chipset. Control never returns; the machine re-enters `boot/stage1.S` from scratch. |

Argument-taking commands show a usage message for missing or extra arguments.
Unknown commands and oversized input are reported rather than ignored.

### Local VGA modes

The kernel owns the local VGA mode and clears the text framebuffer whenever
`vga` changes it. `80x25` is the BIOS-compatible default. `80x50` keeps the
normal 80-column timing but selects a copied 8x8 font map, giving twice as
many rows. `132x25` selects an 8-pixel character clock and experimental
extended horizontal timing; QEMU supports the resulting 1056x400 surface, but
plain VGA adapters and attached monitors are not required to accept it.
Switching back to `vga 80x25` restores the register values captured during
kernel initialization. Tapes and the tape library do not need mode-specific
changes because normal tape output goes through the kernel's `putc` callback.

## Interrupts and the physical-frame allocator

The kernel builds a 256-entry IDT with all 32 CPU exception vectors (0-31)
populated plus four hardware gates (the PIT timer on IRQ0, the PS/2
keyboard on IRQ1, and the master/slave PICs' own spurious-interrupt
vectors on IRQ7/IRQ15), remaps the 8259 PIC's vectors clear of that
reserved exception range, and enables interrupts. Every PIC/PIT command
byte is followed by a short `io_wait()` (a throwaway write to port
`0x80`) rather than sent back-to-back -- some real chipsets are slower to
latch a command than QEMU's emulated 8259. Real hardware also
occasionally raises IRQ7 (or IRQ15) on the 8259 for no genuine device
reason at all -- an electrical glitch withdraws the interrupt request
before the PIC can latch it, and the 8259 reports that as a "spurious"
IRQ7/IRQ15 regardless of what's masked; QEMU's idealized PIC essentially
never does this. The handlers for those two vectors read the PIC's
in-service register before deciding whether to send an EOI at all, per
the standard spurious-IRQ protocol, and `irqstat` reports how often it
happens. Without a real gate there, either kind of misdirected IRQ would
have hit an invalid IDT descriptor and produced a CPU exception instead;
rather than silently faulting into a triple-fault reset, each exception
handler prints the vector number, its name, the CPU-pushed error code (if
any), and the faulting `CS:EIP`, then halts -- turning what used to look
exactly like an unexplained reset loop right after boot into a legible
diagnostic (this is exactly how the spurious-IRQ7 case above was
diagnosed on real AMD hardware in the first place). There is still no
fault *recovery*: every exception is treated as fatal. Scancodes and
timer ticks are captured by minimal ISRs and drained by the shell loop
between prompts; see `kernel/src/interrupts.rs`. A small bitmap-based
physical-frame allocator (`kernel/src/pmem.rs`) tracks which 4 KiB frames
the E820 map reports as usable, reserving everything below the kernel's
own low-memory region. The kernel stack has both a low-end canary and an
unmapped 4 KiB guard page. If overflow reaches that absent page, the CPU cannot
place a page-fault frame on the exhausted stack, so IDT vector 8 task-switches
to a dedicated TSS and emergency stack that emits a diagnostic halt instead of
triple-faulting. `kernel/src/paging.rs` uses 4 MiB pages to identity-map the
low 2 GiB physical range as supervisor-only writable pages, except for the
low 4 MiB page table containing that stack guard. It makes page-aligned kernel
text and read-only data non-writable under `CR0.WP`, and maps high PCI MMIO
only after a driver registers the precise BAR range it needs; other high
ranges turn stray pointers into diagnosable page faults. This is paging
infrastructure, not userspace protection: all current code and tapes still
execute in ring 0.

## Disk access and FOSFS

The kernel has no direct access to BIOS interrupts while in protected mode. For
each validated disk operation, it calls the stage-two real-mode trampoline. The
trampoline saves the protected-mode stack, transitions through a 16-bit protected-mode GDT descriptor into real mode,
temporarily clearing `CR0.PG` as well as `CR0.PE`, invokes BIOS EDD (`INT
13h`, extended read/write) for **exactly one sector**, then restores the
caller's original protected-mode/paging state, including the kernel GDT that
contains its TSS descriptors, and returns an explicit status.
Because the kernel's own IDT and PIC remap
would otherwise break genuine BIOS real-mode interrupt handling (the BDA tick
counter BIOS's disk-timeout logic depends on stops advancing), the kernel
briefly restores the traditional real-mode IVT and BIOS-conventional PIC
mapping around each individual BIOS call, then switches back immediately
after. Interrupts remain disabled while the CPU is still in protected mode
with the real-mode IVT loaded; the trampoline enables them only after it has
entered real mode, avoiding an IRQ being decoded through 4-byte IVT entries
as if they were protected-mode IDT gates. The kernel rejects raw LBAs beyond
the image size. This is
intentionally a teaching bridge, not a general disk driver.

FOSFS is a small custom filesystem in the dedicated data partition. Version 3
has a single superblock, eight fixed directory sectors (64 flat entries), and
contiguous multi-sector files. File creation and replacement use a bounded
first-fit extent scan, so deleting a file makes its sectors reusable without a
bitmap or heap. The generated image starts preformatted with `README.TXT` and
all compiled tape binaries; `format` removes them and resets the directory.
Raw `readsector` is read-only. Only FOSFS commands issue writes, and their
LBAs are constrained to the data partition.

`term` repurposes COM1 as a raw link to a remote machine by default, matching
real hardware where most physical machines expose only one serial port.
While `term` is active on a given port, that port stops being available for
anything else and becomes the remote wire exclusively: outgoing PS/2
keystrokes are sent byte-for-byte with no local echo, and incoming bytes are
shown via VGA only (never re-echoed back out over the same port, which would
corrupt the link). Exit is bound to F12 rather than a Ctrl or Shift/AltGr
combination, since F12 occupies the same physical key and produces the same
scancode regardless of keyboard layout (e.g. Nordic vs. US) -- unlike
letter/punctuation keys, whose legends and shifted symbols vary by layout. A
caveat: if serial is your only connection to fos over COM1 (no VGA in use),
entering `term` on COM1 gives up your control channel until the remote end
sends something recognizable, or you regain local VGA access -- an explicit,
documented trade-off rather than a bug.

`term` can also drive COM2, COM3, or COM4 if the BIOS detected one (`serial`
lists what was found, reading the same BIOS Data Area at physical `0x400`
that DOS and other real-mode software have always used, rather than probing
I/O space directly). Those ports are never shared with the interactive
shell, so unlike COM1 -- whose 115200 baud rate must stay fixed to keep
matching whatever terminal program is already attached to the local console
-- their baud rate is a free choice per `term` session, e.g. `term COM2
9600`. A requested rate must evenly divide 115200 (the UART's fixed clock)
so it is reproduced exactly rather than rounded; invalid combinations are
rejected with an explanation instead of silently misconfiguring the link.

Add-on serial cards are also usable, listed as `PCI0`, `PCI1`, ... in
discovery order. Legacy ISA cards jumpered to a standard COM address need no
extra support at all: BIOS POST enumerates ports by address, not by
onboard-vs-add-in, so they land in the same BIOS Data Area lookup as COM1-4.
PCI/PCIe serial cards are found separately, by walking PCI configuration
space (`kernel/src/pci.rs`) for "simple communication controller / serial"
devices (class `0x07`, subclass `0x00`) and reading each one's first I/O-space
Base Address Register -- the same technique that finds any PCI device, using
Configuration Mechanism #1 (`0xcf8`/`0xcfc`), which even PCIe chipsets still
answer for backward compatibility. The underlying UART register interface on
essentially all such cards is 16550-compatible, so no new register-level
code is needed once a card's I/O address is known; a `PCI` port behaves
exactly like a `COM2`/`COM3`/`COM4` one, including a freely chosen baud
rate. Cards that pack more than one port behind a single BAR at internal
byte offsets are not specially decoded (only cards exposing one PCI function
per port are recognized as multiple ports); genuine USB-to-serial adapters
are out of scope entirely, since they need a full USB host-controller and
device stack rather than simple port I/O (see "Current boundaries").

### Ethernet (`nic`)

`nic` initializes every supported PCI network card it can bring up:
the Realtek RTL8139, selected 3Com 3C905/B/C variants, or Intel e1000
(82540EM). The first initialized interface is active by default; `nic list`
shows all interfaces and `nic use N` selects one for shell commands and the
legacy single-interface tape callbacks. New tapes can enumerate and use
several interfaces concurrently through the appended per-interface callbacks.
The RTL8139 is what QEMU emulates by default; QEMU also provides an 82540EM
model through `-device e1000`. The 3C905 backend is included for
physical-machine testing, but the installed QEMU build has no 3C905 model.
Each backend is found by exact vendor/device IDs through the PCI
configuration-space walk in `kernel/src/pci.rs`, rather than by class code,
because NIC register layouts are vendor-specific.

Unlike the UART, an Ethernet controller does not hand over bytes one at a
time via port I/O: it moves whole frames itself, via DMA, into and out of
buffers whose physical address the driver hands it ahead of time. The
kernel's supervisor identity map keeps physical and virtual addresses equal
for those buffers, so they can be read and written directly through raw
pointers, but they must still be real, physically contiguous memory the card can reach --
so the first use of `nic` allocates the physically contiguous DMA state required
by every initialized backend through `pmem::allocate_contiguous` and never
frees it. The RTL8139 uses a receive ring and transmit buffers; the
3C905 uses receive/transmit descriptor lists; and e1000 uses legacy
receive/transmit descriptor rings.

Each backend uses a legacy PCI IRQ when firmware assigns a safe PIC line:
the tiny handler acknowledges the device, counts the notification, and marks
foreground work pending. It never parses a descriptor, copies a frame, or
runs a protocol; the existing foreground `receive` calls drain the rings and
remain the fallback for cards without a usable line. PIC lines are unmasked
only after their IDT gate and device acknowledgement path are ready. `nic`
and `nic list` show per-device IRQ notifications/errors/pending state, while
`irqstat` shows registered PCI-line dispatch counts. The e1000 driver uses
MMIO; it is deliberately the first Intel model because its legacy descriptor
format is also available in QEMU for repeatable testing. `nic` (no arguments)
reports the selected backend and MAC address; it also runs the bounded
foreground health watchdog and reports completed Rx/Tx frames plus
recovery/failure counters. `nic watchdog` performs the same check explicitly.
The watchdog never treats an idle link as failure: it resets and rebuilds
existing DMA/ring state only after a driver observed a documented adapter
fault (RTL8139 overflow or transmit timeout, e1000 receive overrun or
transmit timeout, or 3C905 Host Error/download timeout). It restores the
prior interrupt mask and promiscuous setting without allocating or freeing
DMA memory, while ordinary polling remains the fallback if recovery fails.
`nic send` transmits one broadcast frame tagged with an IEEE-reserved
experimental ethertype (`0x88b5`); and `nic recv` polls briefly for one
incoming frame and hex-dumps it. The shell commands remain deliberately
raw-Ethernet demonstrations. Higher protocols are implemented by networking
tapes through the shared `tapes/tape-net/` library rather than being hidden
inside the kernel.

`nic sniff` is a small link-level packet sniffer: it asks the selected
backend to enable promiscuous reception,
so it is handed every frame it can see on the wire, not only ones addressed
to it, then prints a one-line summary plus a hex dump of each frame as it
arrives. Like `term`, it runs until F12 is pressed -- the same
layout-independent choice, made for the same reason (see the `term`
description above: F12 produces the same scancode regardless of keyboard
layout, unlike a Shift/AltGr combination a Nordic keyboard would put it
behind) -- and always restores the card to its normal, non-promiscuous
filter before returning, so a later `nic send`/`nic recv` is not left seeing
unrelated traffic.

The per-interface tape callback `nic_set_promiscuous_at` exposes the same
receive-all switch to multi-port tapes. It is needed by a layer-2 bridge:
without promiscuous mode, a NIC normally discards frames addressed to hosts
behind another bridge port before the tape can forward them.

`tools/send_frame.py` is `nic sniff`'s host-side counterpart, for testing
against a real fos machine over an actual Ethernet link rather than another
fos instance: it builds one raw frame (destination MAC + this host's own
source MAC + ethertype + payload) and transmits it on a chosen network
interface via a Linux `AF_PACKET` raw socket -- the same "whole frame, no
protocol above Ethernet" level fos's own driver speaks, so no IP address or
ARP entry is needed on either end.

```sh
echo 52:54:00:12:34:56 > tools/mac   # the target fos machine's MAC; see its 'nic' command
sudo python3 tools/send_frame.py "hello fos"
```

### Boot-time `START` script

An optional uppercase FOSFS file named `START` turns a prepared image into a
small appliance: fos reads up to 4096 bytes after disk and NIC initialization,
then dispatches every non-empty line that does not begin with `#` through the
ordinary shell command handler. Commands therefore have the same syntax,
output, and errors as commands entered at `fos> `. The script continues after
an individual command error; a missing `START` file changes nothing.

For a router image, load `ED.BIN` as usual, run `tape run ed START`, and save:

```text
# Start the configured router at boot.
tape load disk ROUTER.BIN
tape run router @ROUTER.CFG
```

The automatic `term net` wait runs only after the script finishes. A
long-running command such as `tape run router @ROUTER.CFG` intentionally owns
the boot session until it returns; put `term net` in `START` only when waiting
for a remote shell is the desired startup action. `make test-start` verifies
comment/blank-line handling and normal shell dispatch with an isolated image.

Sending raw frames needs `CAP_NET_RAW` (in practice, run it with `sudo`).
The destination MAC is read from `tools/mac` (default path; overridable with
`--mac-file`) rather than a required argument, since it is normally the same
physical test machine throughout one session -- and, being specific to
whoever's hardware is at hand, that file is deliberately `.gitignore`d, not
checked in. The payload is either a literal argument, a file's raw bytes via
`--file`, or -- with neither given -- a fixed default test string; the
network interface to send from is auto-detected (first non-loopback
interface, preferring one that is up) unless `--iface` overrides it; and the
ethertype defaults to the same `0x88b5` `nic send` uses, so captured frames
are easy to recognize, but `--ethertype` can send something else.

`chat` is a small two-way text chat built out of the same two primitives
`nic sniff` and `term` already demonstrate separately: `nic sniff`'s
receive-and-print loop and `term`'s line-input-and-echo loop, merged into
one full-duplex loop. Every `CHAT_ETHERTYPE` frame (`0x88b6`, IEEE's other
"local experimental" value, paired with `0x88b5` and kept distinct from it
so `nic sniff`/`nic recv` never mistake chat traffic for test traffic or
vice versa) starts with a one-byte message tag right after the Ethernet
header, so the one ethertype can carry both ordinary text lines and the
small discovery exchange below without inventing a third, unreserved
ethertype. Every typed line becomes one tagged text frame; every matching
incoming text frame is printed tagged with its sender's MAC. With no
argument, `chat` broadcasts to and listens for everyone; `chat
aa:bb:cc:dd:ee:ff` (or, once `nic discover`/`nic peers` has found some
peers, the shorter `chat N` for peer number `N`) instead sends only to that
one peer and only displays frames whose *source* matches it, for a private
two-machine conversation -- every mode calls the exact same
`Rtl8139::send`/`receive`, since addressing here is just a different
destination MAC, not a different protocol or a driver change. Short/long
lines are zero-padded/truncated the same way `nic send` and
`tools/send_frame.py` already handle Ethernet's 60-byte minimum frame size.
Leaving works the same two ways as `nic sniff` and one more: F12 (local
keyboard only), or typing the line `/quit` -- which, unlike `term`, also
works from a serial-only session, since `chat` never takes over the serial
console the way `term` does. A known, accepted simplification: an incoming
line can interleave on screen with a local line that is only half-typed,
since both share one scrolling console.

`nic discover` and `nic peers` add a numbered address book on top of that
same tagged-frame format, so a peer can be named `chat 1` instead of typing
its full MAC every time. `nic discover` broadcasts a `CHAT_TAG_DISCOVER_REQUEST` frame through every
initialized NIC, re-sending it every few seconds (so a machine that starts
`chat`/`nic discover` a little late is still found), and keeps polling every
NIC -- printing each newly-seen peer as it replies -- until F12 stops it, the
same convention `nic sniff`/`term`/`chat` all use; `nic peers` just prints the
list built so far, without sending anything. A reply to an incoming probe is
sent through the same local NIC on which that probe arrived, allowing one
machine to discover peers across multiple attached Ethernet segments. There
is no interrupt-driven network service in this kernel -- every receive is a
polled `nic.receive()` call somewhere in a foreground loop -- so a probe is
only ever answered by a fos machine whose own `chat` or `nic discover` happens
to be running and polling its card at that exact moment; anyone not actively
at a `chat` prompt or mid-`nic discover` is simply invisible, honestly
reflecting that constraint rather than hiding it behind a fake "background
service". Whoever receives a `CHAT_TAG_DISCOVER_REQUEST` (whether from
`nic discover` or a running `chat` session) replies with one unicast
`CHAT_TAG_DISCOVER_REPLY` frame straight back to the requester's source MAC
(never broadcast, so replies don't also broadcast to every other listener)
and remembers the requester as a peer too, so discovery is naturally
symmetric after one round.

### Tape (`tape`)

`tape` treats a chunk of memory as a paper tape: something you load, then
separately choose to run. New Rust tapes are stored as ELF32 `ET_EXEC`
images. The loader validates the i386 ELF header, loads only `PT_LOAD`
segments, clears each segment's BSS tail, validates the entry point, and
then calls it in ring 0. During migration, legacy raw flat binaries remain
accepted and are executed from the historical fixed physical address
(`0x100000`, 4 MiB reserved -- see `pmem::TAPE_BASE`/`TAPE_LEN` in
`kernel/src/pmem.rs`). Both formats run on the kernel's own stack with absolutely no per-tape memory
protection: this kernel has no userspace or private tape address spaces (see
"Current boundaries" below), so a tape is trusted exactly as much as any
other kernel code. Ending with a
near `ret` returns control to the shell cleanly; anything else that
faults is at least caught by the CPU exception handlers (see "Interrupts
and the physical-frame allocator" above) rather than silently rebooting,
but is never recoverable -- there is no way back into a hung or corrupted
kernel except a reset. Because every tape always loads at that same fixed
address, its own compiled code can use ordinary absolute addressing --
position-independent code is no longer required (though still harmless,
as `tapes/primes.S` shows), which is what makes writing a tape in a
compiled language like Rust or C practical -- see "Test tape:
`tapes/hello-rs/`" below.

A tape can be loaded two ways. `tape load disk NAME` reads a complete
multi-sector FOSFS file straight into the tape image buffer. The generated
image contains every compiled tape under a short uppercase `.BIN` name, so
they can be loaded locally without a network sender; `write NAME TEXT` remains
the small interactive text-file path rather than a binary import mechanism.
`tape load net` instead waits for a tape sent over Ethernet: one
`TAPE_TAG_START` frame (sharing `CHAT_ETHERTYPE` with `chat`/`nic
discover`) declares the transfer's total length, then any number of
`TAPE_TAG_DATA` frames each carry an explicit sequence number and up to
1024 bytes, until one `TAPE_TAG_STOP` frame closes it. The receiver
checks the declared length fits the current tape image region and computes
each chunk's exact byte count from its sequence number and the declared
total length, never from the frame's own (possibly padded) received
length, so a genuine binary tape's own trailing zero bytes are never
mistaken for padding the way `chat`'s text frames get away with
trimming. A skipped or reordered sequence number aborts that attempt
rather than silently loading a corrupt tape, since this deliberately
simple protocol has no retransmission.

Each successful load receives a monotonically increasing tape handle shown by
`tape status`. The shell still exposes one active image today, but keeping the
handle, format, entry point, and image length as explicit metadata avoids
another ABI change when resident images and `tape select`/tape-to-tape loading
are added.

The other end of a network load can be another fos machine's `tape send
NAME PEER_MAC|PEER_NUMBER` (reads a FOSFS file and streams it out, reusing
the exact same `chat`-style peer addressing `chat`/`nic peers` already
provide), or an ordinary Linux host running `tools/send_tape.py FILE`,
which speaks the identical wire protocol over a raw `AF_PACKET` socket.
Rather than requiring a MAC address up front, that tool broadcasts the
same `CHAT_TAG_DISCOVER_REQUEST` probe `nic discover` sends and picks the
destination automatically if exactly one fos machine replies; with more
than one, pass `--mac` with either a full MAC address or a peer number
from `tools/peers.py` (see "A numbered peer address book for the Python
tools too" below) instead of typing one out.
Both senders pace frames because `tape load net` uses a polling-only NIC
receiver with a finite receive ring: the kernel sender waits about 10 ms
between frames, while `tools/send_tape.py` defaults to its empirically
reliable 3 ms delay (`--delay 0` explicitly disables that protection).

#### Test tape: `tapes/primes.S`

A worked example of everything above: a hand-written, genuinely
position-independent 32-bit assembly program that computes every prime up
to 1,000,000 with a Sieve of Eratosthenes, then prints a short summary
(count found, largest prime, elapsed CPU cycles) and returns cleanly to
the shell. It still uses the classic `call`/`pop` idiom and base-relative
labels as a compact position-independent assembly example, even though the
loader now places every tape at the fixed address `0x100000`. Unlike the
tiny single-instruction tape used while developing `tape` itself, this one
has real data (a 1,000,000-byte sieve array) embedded directly in the file
as literal zero-filled space, which is why the assembled image is about a
megabyte. It prints by calling back into the kernel's own console code
through a function pointer the kernel passes it as its one argument (see
"tape print callback ABI" in AGENTS.md), rather than touching VGA/COM1
hardware itself -- see the file's own header comment for the full
reasoning.

Build it with `make tape-primes` (produces `build/tapes/primes.bin`,
entirely separate from `make image`/`run`/`test`, though included in
`make all`/`make tapes`), then send it to a running fos machine with
`tools/send_tape.py build/tapes/primes.bin` and run `tape run` there
once `tape load net` confirms it arrived.

#### Test tape: `tapes/hello-rs/`

A small Rust reference tape, proving a compiled language (not just
hand-written PIC assembly like `primes.S`) works with the fixed physical
load address (`0x100000`): a
`no_std`/`no_main` binary crate, reusing the kernel's own custom target
spec and `-Z build-std` setup (see its `.cargo/config.toml`), with its
own `linker.ld` fixing the load address the same way `kernel/linker.ld`
fixes the kernel's own. It prints a greeting, sieves primes up to 1,000
(168 primes, largest 997) into a `static mut` array, prints the summary
through the same one callback (`console::print_byte`) `primes.S` uses,
and returns -- ordinary absolute addressing, no PIC trick needed. Build
it with `make tape-hello-rs` (producing `build/tapes/hello-rs.bin`) and
verify it with `tapes/test_hello_rs_e2e.py`. Want to write your own
Rust tape? See `docs/rust-tapes-for-dummies.md` for a from-scratch,
beginner-friendly walkthrough.

#### Test tape: `tapes/vga-colors/`

A second Rust reference tape, this one looping instead of running once.
It writes several rows of text straight to VGA text memory (physical
`0xb8000`) instead of through `console::print_byte`, cycling their
foreground color once per frame, forever, until told to stop -- checked
once per frame through a second callback, `should_abort`
(`shell::combined_tape_should_abort`), that `tape_run` now passes
alongside the usual `putc` callback (see "tape print callback ABI" in
AGENTS.md for the full tape ABI, since grown further still -- see
"Tape ABI grows alloc/free" and "Tape ABI grows again: `read_key` and
startup arguments" further below). That check recognizes two
independent "give up" signals, either one enough: a local keypress
(F12/F4/F6/F7/F8/F10, via `keyboard::tape_should_abort`) and, if this
tape was started from an active `term net` session, a remote one too
(a raw `0x1b` Escape byte sent inside a `TERM_TAG_DATA` frame, via
`shell::poll_remote_tape_abort` -- see "A local keypress isn't enough:
remote abort via `poll_remote_tape_abort`" in AGENTS.md). Only one line
at the very start and one at the very end print through `putc`; the
animation itself never touches serial or a `term net` relay at all,
since it writes VGA memory directly -- the whole point of the example.
Stopping the tape this way, local or remote, never disconnects a `term
net` session driving it (see `tapes/test_vga_colors_e2e.py` for a
test that actually exercises both ways, not just documents them). Build
it with `make tape-vga-colors` (producing `build/tapes/vga-colors.bin`).

Unlike `hello-rs`/`bigsieve`, this tape doesn't keep its own copy of
`entry.S`, the panic handler, or `print_bytes` -- it depends on
`tapes/tape-rt/`, a small shared crate providing all three, instead (see
`tapes/tape-rt/src/lib.rs`'s header comment for why this crate exists,
and why only this tape uses it -- `hello-rs`/`bigsieve` stay fully
self-contained on purpose, as beginner-facing teaching examples). Any
new future Rust tape (not a from-scratch teaching-ladder example) should
generally depend on `tape-rt` too, rather than copying that boilerplate
a fourth time.

#### Test tape: `tapes/bigsieve/`

A third Rust reference tape, proving a tape's working set isn't actually
capped at its own fixed 4 MiB region -- only its compiled code/data/
`.bss` is. It requests a 64 MiB buffer at run time through two more
callbacks, `alloc`/`free` (`shell::tape_alloc`/`shell::tape_free`, the
tape ABI's third and fourth arguments -- see "Tape ABI grows alloc/free:
using the rest of the machine's RAM" in AGENTS.md for the full four-
callback ABI and the two-pool memory design behind it), zeroes it
manually (memory from `alloc` isn't pre-zeroed), sieves primes up to
67,108,864 into it, prints the summary through the usual `putc`
callback, and frees the buffer before returning. Built with
`make tape-bigsieve` (producing `build/tapes/bigsieve.bin`) and verified
end to end with `tapes/test_bigsieve_e2e.py`, which runs QEMU with
`-m 256` since the buffer comes from a pool that only starts at the
128 MiB mark. Want to write your own Rust tape? See
`docs/rust-tapes-for-dummies.md` for a from-scratch, beginner-friendly
walkthrough, including a section on `alloc`/`free` for tapes that need
more memory than their fixed 4 MiB region.

#### Test tape: `tapes/calc/`

A reverse-Polish-notation (RPN) integer calculator, and the tape that
motivated the ABI's `read_key`/`args_ptr`/`args_len` additions (see
"Tape ABI grows again" in AGENTS.md) -- an earlier attempt at exactly
this tape discovered there was no way for any tape to receive a
parameter or read a keystroke at all. Like `vga-colors`, it depends on
`../tape-rt` rather than duplicating that boilerplate again. Two ways to
use it: `tape run 3 4 +` evaluates `"3 4 +"` once via the new startup-
argument parameters and returns immediately; plain `tape run` instead
starts a small interactive REPL built on the new `read_key` callback,
reading one line at a time -- from local keyboard or an active
`term net` session, indistinguishably -- until a "give up" key (or a
remote Escape byte) ends it. Supports `+ - * / %` with checked overflow/
division-by-zero detection, reporting friendly errors rather than
crashing. Built with `make tape-calc` (producing `build/tapes/calc.bin`)
and verified end to end with `tapes/test_calc_e2e.py`, which exercises
both modes: startup-argument evaluation, an interactive session
including a backspace correction and a division-by-zero error, and a
remote Escape byte cleanly ending the REPL without disconnecting the
`term_net` session driving it.

#### Test tape: `tapes/ed/`

A small line-oriented Rust text editor for complete FOSFS files. Start it
with `tape run ed NAME`; `NAME` must be an uppercase FOSFS name. The editor
loads the whole file through the tape filesystem callbacks, normalizes input
line endings to LF, and uses a 1 MiB buffer in the tape's own fixed image
region for new and small files. Larger existing files use the optional tape
allocator in 1 MiB chunks, up to 4 MiB; they therefore need RAM above the
allocator's 128 MiB starting address. Missing files start empty. It is
intentionally not a full-screen editor:
input is ordinary `read_key` text, so it works from the local keyboard or
through an active `term net` session.

The core commands are:

```text
p              print the entire buffer with line numbers
m              print one screen-sized page at a time
a              append lines at the end
aLINE          append lines after LINE
iLINE          insert lines before LINE
dLINE          delete LINE
sLINE/TEXT     replace LINE with TEXT
sLINE          prompt for replacement text
eLINE          prompt for replacement text and show the old line
w              write the file and continue editing
x              write the file and exit
q              exit without saving
```

`a` and `iLINE` accept lines until a line containing only `.` is entered.
`m` pauses after each 20 lines; press Space or Enter for the next page, or
`q` to stop paging and return to the editor prompt. F12/F4/F6/F7/F8/F10
exits the editor without saving. Build it with
`make tape-ed`; `make image` preloads the resulting `ED.BIN`, and
`make test-tape-ed` verifies loading, editing, saving, and reading the
changed file back from FOSFS.

#### Test tape: `tapes/beep/`

A small 32-bit Rust tape using the current twenty-two-argument ABI and the
`read_key` callback. It waits for the lowercase or shifted `b` key from
either the local keyboard or an active `term net` session, programs PIT
channel 2 to drive the PC speaker at 440 Hz for approximately 100 ms, then
silences it. Each accepted key also prints `Beep.` so the behavior is
observable in serial/remote-terminal tests. Press F12 (or F4/F6/F7/F8/F10)
to return to the shell. Build it with `make tape-beep` (producing
`build/tapes/beep.bin`), send it with `tools/send_tape.py`, run `tape load
net`, then execute `tape run`. The manual QEMU regression test is
`tapes/test_beep_e2e.py`; it exercises both remote and local `b` input and
checks that the remote shell remains usable afterward.

#### Diagnostic tape: `tapes/net-dhcp-test/`

`NET-DHCP.BIN` is a 32-bit Rust hardware diagnostic that visits every
initialized NIC and runs DHCP independently on each one. It keeps retrying
until that interface receives an IP, printing transmit, receive, parse, and
DHCP-error counters along the way; each successful lease triggers one
approximately 100 ms PC-speaker beep before the tape advances to the next
NIC. It uses the kernel's PIT channel 2 for the tone and leaves channel 0's
100 Hz timer untouched. After all leases are acquired, it continues printing
the per-NIC status and counters until manually stopped. Load it with
`tape load disk NET-DHCP.BIN` and run `tape run`; optionally use
`tape run HOST [INTERVAL_SECONDS]` to send ICMP echo requests through every
leased NIC. Without arguments it does not ping. F12/F4/F6/F7/F8/F10 remains
the explicit escape if a segment has no DHCP server. Build it with
`make tape-net-dhcp`; the focused QEMU test is `make test-tape-net-dhcp`.
Each lease report also shows the DNS server addresses from DHCP option 6, or
`none` when the server advertised no DNS resolver. The tape reports this
information but does not configure or query DNS itself.

#### Test tape: `tapes/morse/`

A Rust tape that turns each typed letter or digit into Morse code: the
representation is printed through the normal console callback and played
through the PC speaker using 440 Hz dots and dashes. `tape run ARGUMENTS`
processes its raw trailing argument string once and returns; plain `tape run`
keeps the interactive mode, accepting characters from the local keyboard or
an active `term net` session. One Morse timing unit is approximately 100 ms,
based on the kernel's existing 100 Hz PIT channel 0; PIT channel 2 supplies
the speaker carrier without disturbing the kernel timer. Both modes exit
cleanly, while interactive mode also exits on F12 (or F4/F6/F7/F8/F10). Build
it with `make tape-morse` (producing `build/tapes/morse.bin`) and verify both
modes with `tapes/test_morse_e2e.py`.

#### Test tape: `tapes/net-chat/`

A Rust implementation of the shell's raw-Ethernet chat protocol, proving a
tape can use the kernel-managed NIC callbacks without owning PCI, DMA,
or receive-ring recovery itself. Plain `tape run` broadcasts; `tape run
AA:BB:CC:DD:EE:FF` selects a unicast peer. While running, `/discover`
broadcasts the same discovery probe as `nic discover`, `/peers` lists the
tape's local peer table, `/to MAC` changes the unicast target, `/broadcast`
returns to broadcast, and `/quit` exits. Its wire format is intentionally
identical to the shell's `chat`, so either end can chat with the other. Build
it with `make tape-net-chat` (producing `build/tapes/net-chat.bin`) and run
`tapes/test_net_chat_e2e.py` to verify both directions against a second fos
machine running the ordinary shell command. Use
`python3 tapes/test_net_chat_e2e.py --nic e1000` (or
`make test-e1000-net`) to run the same two-guest integration test through
QEMU's Intel 82540EM backend.

#### Test tape: `tapes/bridge/`

An intentionally small layer-2 learning bridge/switch. It uses the
per-interface tape callbacks rather than the active-interface compatibility
callbacks, learns source MAC addresses in a bounded table, forwards known
unicast destinations to one egress port, and floods unknown, broadcast, and
multicast frames to every other selected port. Frames stay as raw Ethernet
bytes; the tape does not implement ARP, IPv4, routing, TTL changes, or loop
prevention.

Use one-based interface numbers matching `nic list`, for example
`tape run 1 2`, or omit the arguments to bridge every initialized interface.
The tape enables promiscuous reception on its selected ports because a bridge
must receive frames addressed to hosts behind another port. Do not connect
its ports in a physical loop: STP/VLANs and loop prevention are deliberately
out of scope. Build it with `make tape-bridge` and run
`make test-tape-bridge` for the three-guest QEMU test, which uses two
isolated e1000 segments and verifies forwarding in both directions.

#### Test tape: `tapes/router/`

A small fixed-buffer IPv4 router that reuses the bridge tape's
selected-interface polling loop but terminates Ethernet on each port and
forwards valid IPv4 packets between directly connected subnets. Each
configured interface gets an IPv4 address and netmask, answers ARP for that
address, learns neighboring MAC addresses, and uses longest-prefix matching to
select an egress. Forwarded packets have their TTL decremented and IPv4 header
checksum recomputed.

Run it with one-based NIC numbers from `nic list`, for example:

```text
tape run router 1 10.0.1.1 255.255.255.0 2 10.0.2.1 255.255.255.0
```

The leading `router` word is optional and NIC numbers are one-based, matching
`nic list`. Interfaces may use compact CIDR notation such as
`1 10.0.1.1/24`, or the separate `1 10.0.1.1 /24` form; dotted netmasks
remain accepted. The static interfaces may be followed by
`default NIC GATEWAY` to route otherwise-unmatched destinations through an
explicitly selected interface:

```text
tape run router 1 10.0.1.1 255.255.255.0 2 10.0.2.1 255.255.255.0 default 1 10.0.1.254
```

`dhcp` remains the backward-compatible spelling for DHCP on NIC 1 (the
zero-based callback index 0). `dhcp N` adds one-based NIC `N` as the next
priority DHCP uplink, while static triples configure downstream ports:

```text
tape run router dhcp 2 10.0.2.1 255.255.255.0
```

`health A.B.C.D` enables bounded periodic health checks for DHCP uplinks. An
uplink must answer a fresh gateway ARP request and an ICMP echo request to the
target; three missed cycles select the next healthy uplink, and recovery
automatically fails back to the highest-priority healthy lease. NAT and
static forwards use only the active uplink, so existing flows may break during
a change. If DHCP uplinks share an upstream subnet, that subnet is also routed
only through the active uplink; an inactive standby cannot win a
directly-connected route lookup.

By default, the router narrates DHCP lease lifecycle changes and newly created
TCP/UDP/ICMP-echo NAT mappings without printing ordinary packet forwarding. Add `quiet`
to suppress those routine messages on an unattended router; errors and
critical uplink failover warnings still print.

NAT is enabled by default. Add `no-nat` when the upstream network has an
explicit route back to the downstream subnets and should receive the original
source addresses; `nat` may be written explicitly to document or re-enable
masquerading.

For permanent inbound services, configure a static rule before interface
entries: `forward tcp|udp EXTERNAL_PORT INTERNAL_IP [INTERNAL_PORT]`. NIC 1
is the public side; matching inbound TCP/UDP traffic is DNATed to the
configured host, and matching replies are source-translated back to NIC 1's
address and external port. Omitting the internal port keeps the same port.
The fixed no-heap list holds 16 rules, prints each at startup, rejects
duplicate external ports or ambiguous internal endpoints per protocol, and
only permits targets on configured non-primary subnets. Unrelated unsolicited
traffic is still dropped.

If DHCP supplies a router option and no explicit `default` suffix is present,
that router becomes NIC 1's default route. At least two interfaces (the DHCP
interface counts as one) are required; static NICs must be unique and present,
and a default gateway must be on its selected interface's subnet. When NAT is
enabled, TCP/UDP traffic leaving a non-primary NIC through NIC 1 is source-NATed using a
4 MiB physical-memory-backed table, then reverse-translated for replies; this
provides tens of thousands of flow slots and means upstream hosts do not need
an explicit route back to the downstream DHCP subnet. If that allocation is
unavailable, the router falls back to 32 slots. Idle NAT entries expire after
ten minutes. Other traffic is forwarded without NAT, and dynamic routing,
ICMP errors, fragmentation, and IPv6 remain out of scope.
Build it with `make tape-router` and run `make test-tape-router` for the
two-segment QEMU forwarding test.

The optional `dhcp-server` word enables a DHCP server on every configured
interface except NIC 1. Its pool is derived automatically from each
interface's subnet: network, broadcast, and router addresses are excluded,
and the router allocates one in-memory lease slot for every usable address in
the subnet rather than imposing a small fixed lease count. Leases last seven
days, with renewal at 3.5 days and rebinding at 6.125 days. The required
lease-table memory comes from the tape allocator, so an unusually large subnet
can still be refused if the machine cannot provide enough RAM. Offers and
acknowledgements include the subnet mask, router, broadcast address, server
identifier, and renewal timers:

```text
tape run router dhcp-server 1 10.0.1.1/24 2 10.0.2.1/24
```

DNS option 6 is also advertised when available. With `dhcp` on NIC 1, DNS
addresses received from the upstream DHCP server are propagated to downstream
clients. A `dns SERVER [SERVER]` directive in the router configuration
overrides those learned values, which is useful when the upstream interface is
static or when a specific resolver should be given to clients:

```text
dhcp
dhcp-server
dns 192.168.1.53 1.1.1.1
2 10.0.2.1/24
```

The DHCP service remains non-persistent and does not implement DNS itself; it
only tells clients which external resolver addresses to use. It is a small
LAN DHCP service for directly attached segments, not a replacement for a full
network-management daemon.

Lease persistence is optional. Add `dhcp-lease-file NAME` alongside
`dhcp-server` to save bound MAC-to-address assignments in FOSFS and restore
them on the next run:

```text
dhcp-server
dhcp-lease-file DHCP.LEASES
1 10.0.1.1/24
2 10.0.2.1/24
```

If the directive is omitted, or the disk is unavailable, the router remains
diskless and keeps leases only in memory. Persisted leases restore the address
assignment but restart the seven-day lease timer from the new boot; fos does
not currently expose a battery-backed wall clock to tapes.

For a readable, reusable setup, the router can read this configuration from
an uppercase FOSFS file. `#` starts a comment through the end of its line:

```text
# ROUTER.CFG
dhcp-server
forward tcp 8080 10.0.15.2 80
1 10.0.1.1/24
2 10.0.2.1/24
```

After storing that text as `ROUTER.CFG`, run the loaded router tape with:

```text
tape run router @ROUTER.CFG
```

The shell reads the file through the kernel's FOSFS/BIOS disk path and passes
the contents to the router. The router tape's optional lease persistence uses
the same high-level FOSFS callback ABI and never accesses raw sectors or BIOS
directly.

#### Test tape: `tapes/ip-chat/`

An IPv4/UDP line-chat application built on the shared `tapes/tape-net/`
library. It resolves peers with ARP, routes off-subnet traffic through the
configured gateway, and keeps application policy in the tape rather than the
kernel. Start it with `tape run static LOCAL_IP NETMASK GATEWAY [PEER_IP]`
for deterministic configuration, or `tape run dhcp [PEER_IP]` when a DHCP
server is available. While running, `/to A.B.C.D` selects a unicast peer,
`/broadcast` returns to IPv4 broadcast, `/peers` lists configured/heard
peers, and `/quit` returns to the shell. Lines are bounded to 240 bytes and
UDP provides no delivery acknowledgement or retransmission. Build it with
`make tape-ip-chat` (producing `build/tapes/ip-chat.bin`) and run
`tapes/test_ip_chat_e2e.py` to verify static-IP chat in both directions and
shell recovery afterward.

#### Test tape: `tapes/telnet/`

A fixed-buffer TCP terminal client for generic Unix-like Telnet servers. It
uses the shared `tapes/tape-net/` ARP/TCP/DHCP implementation. Choose static
configuration for deterministic setups:

```text
tape run static LOCAL_IP/PREFIX GATEWAY SERVER_IP SERVER_PORT
```

Dotted netmasks remain accepted, as does the separate `LOCAL_IP /PREFIX`
form.

Or use DHCP for the local address, netmask, and gateway while still naming the
Telnet server explicitly:

```text
tape run dhcp SERVER_IP SERVER_PORT
```

The tape negotiates the basic Telnet `WILL/WONT/DO/DONT` options, advertises
an 80x25 `xterm` terminal with NAWS window-size information, escapes literal
`0xff` bytes as required by the protocol, and forwards terminal data
byte-for-byte. UTF-8 is intentionally preserved as raw bytes rather than
decoded inside the tape, so an attached terminal can render the server's
Unicode output without adding a character library. The client does not
implement DNS or authentication policy; a server's login prompt and
credentials are simply terminal bytes. Common ANSI/VT100 control sequences
are preserved for serial and `term net` clients. The local VGA path now
interprets a bounded VT100 subset including cursor movement, erase commands,
SGR colors and hidden text, cursor-shape requests, scroll regions, cursor
visibility, save/restore, and the alternate screen buffer. It answers
terminal status and cursor-position requests (`CSI 5n`/`CSI 6n`) from the
kernel's 1-based 80x25 cursor state. When the tape returns, the kernel
restores local VGA state and emits a terminal reset (SGR, cursor
visibility/shape, alternate-screen, bracketed-paste, and wrapping modes) so a
program cannot leave the following `fos>` prompt invisible, recolored, or
with a changed cursor. Local keyboard arrows, navigation keys, and non-abort
function keys are encoded as standard terminal escape sequences; Tab is
forwarded as the ordinary `0x09` application byte so remote shell
tab-completion works.

Build it with `make tape-telnet` (producing `build/tapes/telnet.bin`) and run
`tapes/test_telnet_e2e.py` for the focused raw-Ethernet interoperability test.
That test uses the same unprivileged multicast QEMU fixture as the other
networking tapes and a small host-side DHCP/ARP/IPv4/TCP/Telnet peer instead
of requiring a TAP bridge or host network configuration.

#### Test tape: `tapes/longmode.S` / `tapes/longmode-rs/`

A tape that jumps the running 32-bit kernel into 64-bit long mode and
back, entirely inside one `tape run` call: it builds its own tiny
identity-mapped page tables and its own private GDT, proves it reached
64-bit mode by writing a 64-bit-only value directly to VGA text memory,
then restores every piece of CPU state it touched (CR0/CR3/CR4/EFER/
GDTR/EFLAGS) before returning, leaving the kernel's own GDT/paging/IDT
completely untouched. Interrupts stay off for the whole excursion (the
kernel's live PIT/keyboard IDT is invalid to service against mid-
transition). See AGENTS.md's "A tape that visits 64-bit long mode and
comes back" for the full design.

The privileged mode-switch mechanics -- CPUID check, page tables, GDT,
state save/restore, the mode transition itself -- live in the reusable
`tapes/longmode64_shim.S`, which calls one external `tape64_body`
function once safely in 64-bit mode. Two interchangeable implementations
of that body ship in this repo: `tapes/longmode.S` (hand-written
`.code64` assembly, built with `make tape-longmode`, producing
`build/tapes/longmode.bin`) and `tapes/longmode-rs/` (ordinary
`#![no_std]` Rust, built with `make tape-longmode-rs`, producing
`build/tapes/longmode-rs.bin`, using a new custom target,
`targets/x86_64-fos-tape.json`). Unlike every other tape here, both
builds need a real `ld` link step (x86-64 ELF relocations can't survive
a linker-free `objcopy`, unlike x86-32's). Verified end to end with
`tapes/test_longmode_e2e.py` and `tapes/test_longmode_rs_e2e.py`, the
only two e2e tests in this repository that launch
`qemu-system-x86_64` instead of `qemu-system-i386`, since
`qemu-system-i386`'s TCG backend cannot emulate the CPUID long-mode
feature bit at all.


### Remote terminal (`term net`)

`term net` is a full remote shell over raw Ethernet, not a byte pipe like
`term`: once a client connects, the live shell itself -- prompt, line
editing, and every existing command (`nic`, `chat`, `tape`, `ls`, ...) --
is handed over to it, so nothing about any other command needs to know or
care that it might be driven remotely. This is possible because of how the
shell's local input loop was already factored: a small `LineEditor` turns
one byte at a time into command lines and calls the same `execute()`
dispatcher regardless of where that byte came from, and every bit of
console output already funnels through one function (`write_serial`) that
a bounded "relay queue" can tap into without touching any individual
`println!` call site.

`term net` waits for a `TERM_TAG_HELLO` frame (sharing `CHAT_ETHERTYPE`
with `chat`/`tape`/discovery) from any peer -- press F12 (or F4/F6/F7/F8/
F10 -- see below) to give up first -- then locks onto that peer's MAC for
the session. Once connected: local PS/2 keyboard and VGA input stop being
read entirely (this is an exclusive takeover, not a mirrored session);
everything the shell prints is captured by the relay queue and forwarded
as `TERM_TAG_DATA` frames; and incoming `TERM_TAG_DATA` payload bytes are
fed byte-by-byte into the same `LineEditor` a local keyboard would drive.
A `TERM_TAG_BYE` frame from the *remote* peer (or the remote end simply
going away) ends only that one session -- `term net` then loops back to
waiting for a fresh `TERM_TAG_HELLO` instead of falling through to the
local console, so the machine stays reachable over the network with no
manual step required to re-arm it. Only a *local* "give up" key press
(F12/F4/F6/F7/F8/F10) ends the wait for good and hands control to the
local console. Like `tape load net`/`chat`, it keeps answering
`CHAT_TAG_DISCOVER_REQUEST` probes the whole time it runs, whether
waiting or connected, so leaving `term net` open is enough to make a
machine discoverable.

fos now starts a `term net` wait automatically at every boot, before the
ordinary interactive prompt ever appears -- so every fos machine is
reachable over the network by default, with no manual step, which
matters most for a machine with no attached screen/keyboard at all. A
local operator watching a real screen presses one of the "give up" keys
below to skip straight to the interactive prompt instead; a remote peer
connecting and then disconnecting does *not* reach that local prompt --
it only resets the wait for the next remote peer, exactly as a manually
typed `term net` session now behaves too. There is no serial equivalent
of "give up" for this wait (same limitation `chat`/`term` already have),
and unlike before, a serial-only operator can no longer reach the local
prompt by having any peer connect and disconnect either -- the intended
path for a serial-only machine is to drive it entirely over `term net`
itself (or a wired local keyboard, even briefly, to press one of the
"give up" keys once). Boot also prints every initialized NIC, its driver,
number, and MAC address before entering the wait. If the local operator
does give up, the shell re-enters the automatic wait after 60 seconds
without local keyboard or serial input.

"Give up" recognizes six function keys, not just F12: F12, F4, F6, F7,
F8, and F10 (see `keyboard::ESCAPE_SCANCODES`). F12 was always chosen for
being a function key -- same scancode on every keyboard layout this
project targets, never reachable through Shift/AltGr -- but some terminal
emulators and KVMs intercept F12 themselves before it reaches a guest OS,
leaving no working "give up" key at all on those setups. Any of the six
now works, with nothing to configure; F1/F2/F3/F5/F9/F11 are deliberately
left alone since some terminals bind those to their own features.

The client side is `tools/term_net.py`, built on the same raw
`AF_PACKET`-socket/MAC-discovery approach as `tools/send_tape.py`. Its
primary mode is non-interactive and script-friendly: give it one or more
command lines as arguments (or via `--stdin`), and it sends each in turn,
waits for a quiet period on the wire, prints whatever came back, then
sends `TERM_TAG_BYE` and exits -- e.g. `tools/term_net.py "about" "nic"
"ls"`. `--interactive` instead opens a raw pass-through session reading
from and writing to this program's own stdin/stdout, closer to a real
remote login for a human at a keyboard; it puts its own stdin into
cbreak mode (no line buffering, no local echo; Ctrl-C still raises
KeyboardInterrupt as usual, and Ctrl-D is recognized explicitly by the
read loop itself to still end the session, since cbreak mode's ICANON-
off means the tty driver no longer treats it as EOF the way canonical
mode would) so every keystroke -- including a lone Escape, used to abort
a currently running remote command like `tape run` without ending the
session, see "A local keypress isn't enough: remote abort via
`poll_remote_tape_abort`" in AGENTS.md -- reaches the fos kernel the
instant it's typed, not batched together with whatever comes next.

Because `tape load net`/`tape run` are ordinary commands dispatched
through the same `execute()`, they work over `term net` too, with no
special-case integration: a remote client can type `tape load net`, have
some other machine (or another fos host) send a tape at the usual
`send_tape.py` targeting the fos machine's own MAC address (that sender
does not need to know or care a `term net` session is even open), then
type `tape run`. Two things are worth knowing before relying on this:

- Neither command's *own* network traffic overlaps with `term_net`'s
  outer loop while it runs: like any command that blocks in its own
  local polling loop, `tape load net` (waiting for tape frames) and
  `tape run` (running the tape) pause `term_net`'s own outer loop --
  including the relay-queue drain and its usual keystroke/`TERM_TAG_BYE`
  handling -- until they return. `tape load net` retains a bounded four-frame
  queue of ordinary `TERM_TAG_DATA` input from its connected peer and replays
  it through the suspended shell editor when it returns, so a following
  command (such as `tape run router @ROUTER.CFG`) sent during the transfer is
  not lost; if that queue fills, whole newest frames are dropped and fos
  reports the overflow. Only local F12 (or F4/F6/F7/F8/F10) can
  interrupt `tape load net` early. `tape run` cannot be interrupted at
  all for a tape that only ever declares the original one-argument
  `putc` callback (`primes.S`/`hello-rs`) -- same as running it locally,
  since there is nothing in that tape's own code to notice a stop
  request in the first place. A tape that instead declares the newer
  two-callback ABI and checks `should_abort` in its own loop (like
  `tapes/vga-colors/`, see "Test tape: `tapes/vga-colors/`"
  above) *is* interruptible remotely, even though `term_net`'s own outer
  loop is paused the whole time: `shell::poll_remote_tape_abort` polls
  the network card directly, on that paused loop's behalf, recognizing
  `REMOTE_ABORT_BYTE` (`0x1b`, plain ASCII Escape) inside an ordinary
  `TERM_TAG_DATA` frame from the connected peer -- exactly what
  `tools/term_net.py --interactive` now forwards for a real Escape
  keypress once its own stdin is in cbreak mode. Stopping a tape this
  way never disconnects the session driving it, the same guarantee a
  local F12 keypress already gets (see "A local keypress isn't enough:
  remote abort via `poll_remote_tape_abort`" in AGENTS.md).
- Only the kernel's own printed messages around a tape -- "Tape loaded",
  "Running N-byte tape...", "Tape returned; back at the fos shell.", and
  so on -- plus the tape's own printed output (like the primes tape's
  summary; see `tapes/primes.S`) are ever relayed. A tape prints by
  calling back into the kernel's own `console::print_byte` (passed to it
  as `tape_run`'s first argument) rather than touching VGA/COM1 hardware
  directly, so its output goes through the exact same combined
  VGA+serial+relay path as any other command's. For a tape like
  `tapes/primes.S` that never calls `should_abort`/`read_key` (it takes
  no input and always runs to completion on its own), this all still
  arrives as one burst once the tape returns, since nothing drains the
  relay queue while `term_net`'s own loop is paused waiting for it --
  `tools/test_term_net_tape_e2e.py` verifies exactly this. But a tape
  that *does* poll `should_abort`/`read_key` (like `tapes/calc`'s
  interactive REPL) gets its output relayed live, as it's printed, not
  just in one final burst: `poll_remote_tape_abort`/
  `poll_remote_tape_input` (`kernel/src/shell.rs`) also drain and forward
  the relay queue on every poll, on `term_net`'s behalf, exactly the way
  they already polled the network card for input/abort bytes -- see
  AGENTS.md's "Live output relaying for `read_key`/`should_abort`-based
  tapes over `term net`" section. `tapes/test_calc_e2e.py` verifies this
  by listening on the wire and confirming the calculator's banner and
  prompt arrive before any input is even sent.

### A numbered peer address book for the Python tools too

fos's own `chat`/`tape send`/`tape load net` accept `PEER_NUMBER` as
shorthand for a full MAC address, backed by the kernel's own `PEERS`
table (`kernel/src/shell.rs`'s `remember_peer`/`peer_by_index`/
`print_peers`, printed with `nic peers`): every MAC any discovery probe
or `chat` reply has ever seen, in first-seen order, for as long as that
one boot stays up. `tools/send_tape.py`/`tools/term_net.py` had no
equivalent -- with more than one fos machine replying to a discovery
probe, both used to just print the full MAC list and ask for `--mac`
spelled out in full, every single run, since neither tool has anywhere
to remember a peer between one short-lived Python process and the next.

They now share a small on-disk address book instead (a JSON file, by
default `~/.cache/fos_peers.json`, overridable with the `FOS_PEERS_FILE`
environment variable -- mainly so tests don't pollute a real user's
cache with throwaway QEMU-session MACs): every discovery either tool
performs merges newly seen peers into it, in first-seen order, the same
way the kernel's own `remember_peer` does; `--mac` on either tool now
accepts a peer number from this list exactly as `chat`/`tape send`
accept one from `nic peers`, in addition to a full MAC address.

`tools/peers.py` manages this address book directly, mirroring `nic
peers`/`nic discover`: with no arguments it just prints the numbered
list, no network traffic at all; `--discover` broadcasts one probe,
merges any replies in, and prints the (possibly grown) list; `--forget`
empties it. Unlike the kernel's own table, this one survives between
runs (and reboots) rather than starting empty every time, so it can also
go stale (a MAC that answered yesterday might be a different machine, or
offline, today) -- `--forget` exists so a person can notice and reset it
deliberately, rather than the tools trying to guess when an entry has
gone stale on their own.


## Booting from a NIC ROM

`make netrom` builds `build/netboot.rom`: an alternative, optional boot
artifact -- a **PCI expansion ("option") ROM** for the RTL8139 that boots fos
directly from a network card's own boot ROM instead of from a disk or USB
stick. This is purely additive: `make image`/`make run`/`make test` above are
completely untouched, and USB/disk booting continues to work exactly as
before. Nothing about this feature changes any existing boot path; it just
adds a second, independent way to reach the same kernel.

Legacy BIOS scans every PCI card at POST for exactly this kind of ROM: a
small image starting with the same `0x55 0xaa` signature the MBR uses, plus a
whole-image checksum and a `PCIR` data structure identifying the card
(vendor/device ID, class code) the ROM belongs to. If found, BIOS "shadows"
(copies) the ROM into memory somewhere in the `0xc0000`-`0xe0000` range and
far-calls into it during POST -- this is the same mechanism real network
(PXE) boot ROMs use, and it runs before BIOS ever attempts a disk boot.

`boot/netrom.S` implements this in two phases, mirroring how real network
boot ROMs behave:

1. **`init_entry`**, far-called by BIOS very early in POST, is a "good
   citizen": it prints a short banner, installs a hook on the real-mode
   `INT 19h` vector ("bootstrap loader", fired later once BIOS is ready to
   boot something), and returns normally so the rest of POST can continue
   uninterrupted.
2. **`boot_entry`**, reached once BIOS actually invokes `INT 19h`, never
   returns. It narrates the same steps as `boot/stage2.S` -- E820 memory map,
   A20 enable, GDT setup, the far jump into 32-bit protected mode -- except
   instead of a BIOS disk read, it copies the Rust kernel from its own
   already-shadowed ROM image straight to physical `0x10000` with a plain
   memory copy, since the kernel is embedded directly in the ROM (via
   `.incbin`) rather than read from a disk sector.

Two details make this meaningfully different from `boot/stage2.S`, both
driven by the fact that an option ROM's *own* memory is effectively read-only
at run time, exactly like the real EPROM/flash chip it represents:

- **The ROM's own load segment is unknown at build time** (it depends on how
  many other cards' option ROMs come before it), so, unlike stage two's fixed
  `0x8000` load address, this code reads its own running segment from `%cs`
  at startup and computes everything else relative to that.
- **Nothing inside the ROM image itself can be written at run time.** Any
  state this code needs to mutate -- its own computed segment/physical base,
  whether serial is present, and the two descriptors used to enter protected
  mode -- lives in a small fixed scratch area in genuine low RAM (`0x600`-
  `0x613`) instead, addressed the same way as the E820/BootInfo buffer
  (`ES=0` with an explicit `%es:` override).

This first version deliberately does not implement the BIOS-disk real-mode
trampoline `boot/stage2.S` provides: `BootInfo.bios_disk_io` is left at zero,
which `kernel/src/disk.rs` already treats as "disk unavailable" and reports
gracefully (`diskinfo` shows zero sectors; FOSFS commands report the disk as
absent) rather than attempting a read. `BootInfo.boot_drive` is likewise a
best-effort `0x80` guess, since BIOS does not hand an `INT 19h`-hooked
handler a meaningful drive number the way it does for a normal MBR boot.
The embedded kernel must still fit in the reserved `0x10000..0x50000`
destination window (`tools/write_netrom_layout.py` enforces this at build
time). The ROM path copies it with a 32-bit-address-size `rep movsb`; the
final ROM image remains subject to the PCI option-ROM size limit enforced by
`tools/build_netrom.py`.

**Physical hardware note:** writing `build/netboot.rom` onto an actual boot
ROM chip is only possible if the card has a writable/socketed option ROM --
most RTL8139-era cards either have no ROM socket at all, or only a small
EEPROM meant for the card's MAC address rather than boot code -- and, where a
socket does exist, programming it typically requires an external EPROM/flash
programmer; this project only builds the image, it does not attempt to flash
any hardware. Validate the image in QEMU first with
`qemu-system-i386 -device rtl8139,romfile=build/netboot.rom` (no `-drive`
needed at all to prove it boots standalone).

## Supported platform

The supported target has all of the following:

- A 32-bit-capable x86 CPU and **legacy BIOS** boot.
- BIOS EDD/LBA disk reads and the E820 memory-map service.
- VGA text mode.
- COM1 for serial output and optional input.
- A PS/2-compatible keyboard/controller for local interactive input.

QEMU is the primary supported environment. A physical USB boot requires a
legacy-BIOS-compatible machine. Native USB keyboard support is not implemented;
USB keyboards work only where firmware provides PS/2-style legacy emulation.
UEFI-only machines and CHS-only BIOS disk services are unsupported.

## Disk image layout

`build/fos.img` is a 16 MiB raw disk image with protective MBR and primary/
backup GPT metadata:

- Sector 0: MBR and stage-one loader.
- LBA 1: primary GPT header; LBAs 2-33: GPT partition entries.
- LBA 2048: GPT payload partition.
- First 32 payload sectors: stage-two loader.
- Remaining payload sectors: flat kernel image at physical `0x10000`.
- LBA 4096: a separate 28639-sector FOSFS data partition containing
  `README.TXT` and every compiled tape.
- Final 33 sectors: backup GPT entries and header.

`tools/write_layout.py` generates the loader constants from the actual kernel
size. The kernel validates the GPT and discovers the FOSFS partition by its
GPT type GUID, falling back to the loader-provided geometry for older images.
The image builder preformats only the image's own FOSFS data partition; it
does not touch host storage.

## Physical media warning

Creating the image is safe: `make image` writes only inside `build/`. Copying
the image to a block device destroys that device's partition table and data.
Only use a dedicated disposable drive, independently verify its device path,
and run a write command yourself after unmounting it. This project never
automatically selects or writes a host block device.

## Current boundaries

This milestone uses one CPU, no heap allocation, userspace, or scheduler. It
has supervisor-only identity paging, but no per-process address spaces or
memory isolation. It has FOSFS (a deliberately tiny flat multi-sector filesystem, not a
general-purpose one), interrupt-driven IRQ0/IRQ1 handling for the timer and
keyboard, and three polling-mode PCI network card drivers (RTL8139,
selected 3C905 variants, and Intel e1000, with raw frame access in the kernel and
ARP/IPv4/ICMP/UDP/TCP/DHCP protocol code in `tapes/tape-net/`; no NIC
interrupt handling), but no other hardware IRQ
drivers (no native disk or USB interrupt handling). The optional NIC-ROM boot
path (see "Booting from a NIC ROM") does not implement the BIOS-disk
trampoline, so disk/FOSFS access is unavailable when booted that way. The
architecture keeps x86/driver code separate from console, shell, and tape
protocol policy so these layers can grow without changing the BIOS-to-kernel
contract.
