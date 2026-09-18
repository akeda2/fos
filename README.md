# fos

`fos` is a deliberately small x86 operating system for learning how a machine
boots. It starts via **legacy BIOS**, enters **32-bit protected mode**, then
runs a Rust kernel without a hosted operating system or standard library.

It is not a general-purpose OS and is intentionally incomplete. The current
milestone demonstrates a custom BIOS loader, memory-map discovery, a
Rust/assembly handoff, VGA and serial consoles, an interrupt-driven PS/2
keyboard and PIT timer, a tiny physical-frame allocator, a small FOSFS
filesystem, and a small command prompt -- all deliberately narrated with
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
   console, builds its physical-frame bitmap from the E820 map, sets up its
   own IDT and remaps the PIC (arming the PIT timer on IRQ0 and the PS/2
   keyboard on IRQ1), then runs the prompt. Each of these steps prints what
   it just configured and why.

The message `BIOS services end now` marks the important transition: code after
that point must not call BIOS interrupts directly from protected mode. The one
narrow exception is `kernel/src/disk.rs`'s trampoline, which briefly restores
the real-mode IVT and BIOS-conventional PIC mapping around each individual
disk sector transfer -- see "Disk access and FOSFS" below.

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
test. It checks boot milestones (including interrupt/allocator setup), line
editing, shell error handling, system inspection, interrupt/memory
introspection, FOSFS formatting, file creation, reading, removal, and halt.
`cargo test` does not work for this crate (a `no_std`, custom-target,
freestanding binary); `make test` is the sole automated validation path.

`make netrom` builds an alternative, optional boot artifact,
`build/netboot.rom`, that boots fos from a NIC's own PCI option ROM instead
of disk/USB; see "Booting from a NIC ROM" below. It does not run as part of
plain `make` (still just `make image`) and does not affect `image`/`run`/
`test`, but is included in `make all` (see below).

`make all` builds everything this repository knows how to produce in one
go: the image, `make netrom`, and every test tape (`make tapes`, i.e.
`tape-primes`/`tape-hello-rs`/`tape-vga-colors` -- see "Tape (`tape`)"
below). Not needed for ordinary kernel/boot work, for which plain `make`
(equivalent to `make image`) is enough. `make help` lists every target
with a one-line description.

## Prompt

The prompt accepts fixed-size, allocation-free command lines. Enter and
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
| `write NAME TEXT` | Creates or replaces a one-sector FOSFS text file. Names use up to 16 uppercase letters, digits, `.`, `_`, or `-`; oversized content, a full directory, or a full data region are each reported distinctly. |
| `rm NAME` | Removes a FOSFS file entry. |
| `serial` | Lists COM1..COM4 as detected by the BIOS at boot (base I/O port, or "not detected"), plus any serial controllers found on PCI/PCIe add-on cards (as `PCI0`, `PCI1`, ...), noting which is the local console. |
| `term [COM1\|COM2\|COM3\|COM4\|PCI0\|PCI1\|...] [BAUD]` | Enters raw terminal mode: forwards PS/2 keystrokes byte-for-byte to a remote machine over the chosen port (default COM1), and displays incoming bytes locally (VGA only, not re-echoed to serial). Press F12 (or F4/F6/F7/F8/F10) to return to the shell. COM1's rate is fixed at 115200 (shared with the local console); COM2/3/4 and PCI ports accept an optional custom `BAUD` that must evenly divide 115200. |
| `nic` | Reports whether an RTL8139 Ethernet controller was found on PCI (and, if so, its MAC address). |
| `nic send` | Transmits one raw broadcast Ethernet test frame. |
| `nic recv` | Polls briefly for one incoming raw Ethernet frame and, if one arrives, hex-dumps it. |
| `nic sniff` | Enables promiscuous mode and prints every frame the card sees (destination/source MAC, ethertype, hex dump) until F12 (or F4/F6/F7/F8/F10) stops the capture, then turns promiscuous mode back off. |
| `nic discover` | Broadcasts a `chat`-protocol discovery probe (re-sent every few seconds) and lists every distinct responder as it replies, running until F12 (or F4/F6/F7/F8/F10) stops it (only fos machines currently running `chat` or `nic discover` reply). |
| `nic peers` | Prints the peer list built so far by `nic discover` (and by any auto-replies `chat` has sent), without sending anything. |
| `chat [PEER_MAC\|PEER_NUMBER]` | Enters a line-based Ethernet chat: broadcast to everyone by default, or unicast (send and receive) to one `aa:bb:cc:dd:ee:ff` peer -- or a numbered peer from `nic peers`/`nic discover`. Type a line and press Enter to send it; incoming lines are shown tagged with the sender's MAC. While running, also auto-replies to `nic discover` probes from other machines. Leave with F12 (or F4/F6/F7/F8/F10) (local keyboard) or by typing `/quit` (works over serial too). |
| `tape status` | Reports what (if anything) is currently loaded: its length and physical address, or that nothing is loaded. |
| `tape load disk NAME` | Loads a FOSFS file straight into a fresh tape buffer (bounded by FOSFS's current 512-byte one-sector file limit). |
| `tape load net` | Waits for a tape sent over Ethernet (see `tape send`/`tools/send_tape.py`): allocates however many frames the sender declares, checks each chunk's sequence number, and reports success or exactly where a transfer broke down. Press F12 (or F4/F6/F7/F8/F10) to give up. |
| `tape send NAME PEER_MAC\|PEER_NUMBER` | Reads a FOSFS file and transmits it as a tape to one peer's `tape load net` (or `tools/send_tape.py`, listening the same way). |
| `tape run` | Executes the loaded tape: a call to its first byte, in ring 0, with no memory protection, passing the kernel's own `console::print_byte` as its one argument so its printed output goes through the same VGA/serial/relay path as any other command's. If it ends with a near `ret`, control returns to the shell; anything else that faults is caught by the CPU exception handlers (see below) but is not recoverable. |
| `term net` | Waits for a remote terminal client over Ethernet (see `tools/term_net.py`) and, once one says hello, hands the live shell over to it entirely -- every other command works identically for the connected peer. Local PS/2/VGA input is disabled meanwhile; F12 (or F4/F6/F7/F8/F10) disconnects it (or the peer can disconnect first). While waiting or connected, also auto-replies to `nic discover` probes, same as `chat`/`tape load net`. This same wait now also runs automatically once at every boot, before the interactive prompt appears. |
| `halt` | Stops the CPU. Reset or power-cycle to restart. |
| `reset` | Reboots the machine immediately via an 8042 keyboard-controller pulse reset -- the same classic mechanism real DOS-era reboot utilities used, still honored by QEMU and virtually every real chipset. Control never returns; the machine re-enters `boot/stage1.S` from scratch. |

Argument-taking commands show a usage message for missing or extra arguments.
Unknown commands and oversized input are reported rather than ignored.

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
own low-memory
region. There is no paging yet -- this only tracks free/used frames,
exposed through `pmem`/`palloc`/`pfree`.

## Disk access and FOSFS

The kernel has no direct access to BIOS interrupts while in protected mode. For
each validated disk operation, it calls the stage-two real-mode trampoline. The
trampoline saves the protected-mode stack, transitions through a 16-bit
protected-mode GDT descriptor into real mode, invokes BIOS EDD (`INT 13h`,
extended read/write) for **exactly one sector**, then restores protected mode
and returns an explicit status. Because the kernel's own IDT and PIC remap
would otherwise break genuine BIOS real-mode interrupt handling (the BDA tick
counter BIOS's disk-timeout logic depends on stops advancing), the kernel
briefly restores the traditional real-mode IVT and BIOS-conventional PIC
mapping around each individual BIOS call, then switches back immediately
after. The kernel rejects raw LBAs beyond the image size. This is
intentionally a teaching bridge, not a general disk driver.

FOSFS is a small custom filesystem in the dedicated data partition. It has a
single superblock, four fixed directory sectors (64 entries), and contiguous
one-sector files. It does not reuse file data after deletion; `format` resets
the directory and allocation frontier. The generated image starts preformatted
with `README.TXT`. Raw `readsector` is read-only. Only FOSFS commands issue
writes, and their LBAs are constrained to the data partition.

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

`nic` exercises a single supported network card: the Realtek RTL8139 Fast
Ethernet controller (`kernel/src/rtl8139.rs`), chosen because it is
well-documented, was extremely common in real PCI-era hardware, and is what
QEMU emulates by default. It is found via the same PCI configuration-space
walk used for add-on serial cards (`kernel/src/pci.rs`, extended with a
vendor/device ID lookup), except this time by exact vendor/device ID (Realtek
`0x10ec:0x8139`) rather than by class code, since NICs -- unlike UARTs --
don't share one common register interface across vendors.

Unlike the UART, an Ethernet controller does not hand over bytes one at a
time via port I/O: it moves whole frames itself, via DMA, into and out of
buffers whose physical address the driver hands it ahead of time. This
kernel has no paging, so physical and virtual addresses are identical and
those buffers can be read and written directly through raw pointers, but
they must still be real, physically contiguous memory the card can reach --
so `nic`'s first use of the card allocates its receive ring (`pmem`'s first
caller to need more than one contiguous frame at a time; see
`pmem::allocate_contiguous`) and four single-frame transmit buffers, and
never frees them.

There is no interrupt handling here: this project's PIC setup leaves every
IRQ but the timer and keyboard masked (see "Current boundaries"), so the
driver's interrupt mask register is simply left at zero and everything is
done by polling status registers, the same approach `term` already takes for
serial I/O. `TCR` (Transmit Configuration Register) is programmed with a
standard IEEE 802.3 interframe gap rather than left at its all-zero reset
value -- the RTL8139's own "transmit OK" status bit only reflects the
chip's own FIFO/abort state, not whether anything downstream actually
accepted the frame, and a too-short gap between frames is a well-known
real-hardware gotcha real switches and NICs can silently enforce by
dropping the frame even while the card itself reports success (QEMU's
virtual NIC never enforces this, which is why it went unnoticed until
tested between two real machines -- see "Real hardware notes" in
`AGENTS.md`). `nic` (no arguments) reports whether a card was found and its
MAC address; `nic send` transmits one broadcast test frame tagged with an
IEEE-reserved "experimental" ethertype (`0x88b5`) so it is unmistakably fos's
own test traffic; `nic recv` polls briefly for one incoming frame and
hex-dumps it. There is deliberately no protocol above raw Ethernet yet -- no
ARP, no IPv4, nothing resembling a socket -- this only proves the card itself
works.

`nic sniff` is a small link-level packet sniffer: it sets the card's receive
filter to promiscuous mode (the RTL8139's `RCR` "accept all packets" bit),
so it is handed every frame it can see on the wire, not only ones addressed
to it, then prints a one-line summary plus a hex dump of each frame as it
arrives. Like `term`, it runs until F12 is pressed -- the same
layout-independent choice, made for the same reason (see the `term`
description above: F12 produces the same scancode regardless of keyboard
layout, unlike a Shift/AltGr combination a Nordic keyboard would put it
behind) -- and always restores the card to its normal, non-promiscuous
filter before returning, so a later `nic send`/`nic recv` is not left seeing
unrelated traffic.

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
its full MAC every time. `nic discover` broadcasts a `CHAT_TAG_DISCOVER_REQUEST`
frame, re-sending it every few seconds (so a machine that starts `chat`/`nic
discover` a little late is still found), and keeps polling the card --
printing each newly-seen peer as it replies -- until F12 stops it, the same
convention `nic sniff`/`term`/`chat` all use; `nic peers` just prints the
list built so far, without sending anything. There is no interrupt-driven
network service in this kernel -- every receive is a polled `nic.receive()`
call somewhere in a foreground loop -- so a probe is only ever answered by a
fos machine whose own `chat` or `nic discover` happens to be running and
polling its card at that exact moment; anyone not actively at a `chat`
prompt or mid-`nic discover` is simply invisible, honestly reflecting that
constraint rather than hiding it behind a fake "background service". Whoever
receives a `CHAT_TAG_DISCOVER_REQUEST` (whether from `nic discover` or a
running `chat` session) replies with one unicast `CHAT_TAG_DISCOVER_REPLY`
frame straight back to the requester's source MAC (never broadcast, so
replies don't also broadcast to every other listener) and remembers the
requester as a peer too, so discovery is naturally symmetric after one
round.

### Tape (`tape`)

`tape` treats a chunk of memory as a paper tape: something you load, then
separately choose to run. A tape is a flat binary always loaded at one
fixed physical address (`0x100000`, 4 MiB reserved -- see `pmem::TAPE_BASE`/
`TAPE_LEN` in `kernel/src/pmem.rs`), the exact same convention the kernel
itself is loaded and entered with (see `linker.ld`/`boot/stage2.S`, fixed
at `0x10000`) -- executed by a bare `call` to its first byte, in ring 0,
on the kernel's own stack, with absolutely no memory protection: this
kernel has no userspace or paging (see "Current boundaries" below), so a
tape is trusted exactly as much as any other kernel code. Ending with a
near `ret` returns control to the shell cleanly; anything else that
faults is at least caught by the CPU exception handlers (see "Interrupts
and the physical-frame allocator" above) rather than silently rebooting,
but is never recoverable -- there is no way back into a hung or corrupted
kernel except a reset. Because every tape always loads at that same fixed
address, its own compiled code can use ordinary absolute addressing --
position-independent code is no longer required (though still harmless,
as `tools/tapes/primes.S` shows), which is what makes writing a tape in a
compiled language like Rust or C practical -- see "Test tape:
`tools/tapes/hello-rs/`" below.

A tape can be loaded two ways. `tape load disk NAME` reads a FOSFS file
straight into the fixed tape buffer -- bounded, for now, by FOSFS's
own 512-byte one-sector file limit, and by `write NAME TEXT` only ever
being able to put printable ASCII bytes on disk in the first place, so a
disk-loaded tape's practical use today is mostly for content that arrived
some other way (e.g. copied there outside this kernel) rather than typed
in live. `tape load net` instead waits for a tape sent over Ethernet: one
`TAPE_TAG_START` frame (sharing `CHAT_ETHERTYPE` with `chat`/`nic
discover`) declares the transfer's total length, then any number of
`TAPE_TAG_DATA` frames each carry an explicit sequence number and up to
1024 bytes, until one `TAPE_TAG_STOP` frame closes it. The receiver
checks the declared length fits the fixed 4 MiB tape region and computes
each chunk's exact byte count from its sequence number and the declared
total length, never from the frame's own (possibly padded) received
length, so a genuine binary tape's own trailing zero bytes are never
mistaken for padding the way `chat`'s text frames get away with
trimming. A skipped or reordered sequence number aborts that attempt
rather than silently loading a corrupt tape, since this deliberately
simple protocol has no retransmission.

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

#### Test tape: `tools/tapes/primes.S`

A worked example of everything above: a hand-written, genuinely
position-independent 32-bit assembly program that computes every prime up
to 1,000,000 with a Sieve of Eratosthenes, then prints a short summary
(count found, largest prime, elapsed CPU cycles) and returns cleanly to
the shell. Since a tape has no fixed load address, it captures its own
runtime base with the classic `call`/`pop` idiom and addresses every
label as an offset from that base -- unlike the tiny single-instruction
tape used while developing `tape` itself, this one has real data (a
1,000,000-byte sieve array) that must be reachable no matter where it
lands in memory. The sieve array is embedded directly in the file as
literal zero-filled space, which is why the assembled binary is about a
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

#### Test tape: `tools/tapes/hello-rs/`

A small Rust reference tape, proving a compiled language (not just
hand-written PIC assembly like `primes.S`) works here now that every
tape always loads at the same fixed physical address (`0x100000`): a
`no_std`/`no_main` binary crate, reusing the kernel's own custom target
spec and `-Z build-std` setup (see its `.cargo/config.toml`), with its
own `linker.ld` fixing the load address the same way `kernel/linker.ld`
fixes the kernel's own. It prints a greeting, sieves primes up to 1,000
(168 primes, largest 997) into a `static mut` array, prints the summary
through the same one callback (`console::print_byte`) `primes.S` uses,
and returns -- ordinary absolute addressing, no PIC trick needed. Build
it with `make tape-hello-rs` (producing `build/tapes/hello-rs.bin`) and
verify it with `tools/tapes/test_hello_rs_e2e.py`. Want to write your own
Rust tape? See `docs/rust-tapes-for-dummies.md` for a from-scratch,
beginner-friendly walkthrough.

#### Test tape: `tools/tapes/vga-colors/`

A second Rust reference tape, this one looping instead of running once.
It writes several rows of text straight to VGA text memory (physical
`0xb8000`) instead of through `console::print_byte`, cycling their
foreground color once per frame, forever, until told to stop -- checked
once per frame through a second callback, `should_abort`
(`shell::combined_tape_should_abort`), that `tape_run` now passes
alongside the usual `putc` callback (see "tape print callback ABI" in
AGENTS.md for the full two-callback ABI). That check recognizes two
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
net` session driving it (see `tools/tapes/test_vga_colors_e2e.py` for a
test that actually exercises both ways, not just documents them). Build
it with `make tape-vga-colors` (producing `build/tapes/vga-colors.bin`).


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
"give up" keys once).

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
cbreak mode (no line buffering, no local echo, but Ctrl-C/Ctrl-D still
work as they always did) so every keystroke -- including a lone Escape,
used to abort a currently running remote command like `tape run` without
ending the session, see "A local keypress isn't enough: remote abort via
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
  handling -- until they return. Only local F12 (or F4/F6/F7/F8/F10) can
  interrupt `tape load net` early. `tape run` cannot be interrupted at
  all for a tape that only ever declares the original one-argument
  `putc` callback (`primes.S`/`hello-rs`) -- same as running it locally,
  since there is nothing in that tape's own code to notice a stop
  request in the first place. A tape that instead declares the newer
  two-callback ABI and checks `should_abort` in its own loop (like
  `tools/tapes/vga-colors/`, see "Test tape: `tools/tapes/vga-colors/`"
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
  summary; see `tools/tapes/primes.S`) are ever relayed, all as one burst
  once the command returns (since nothing drains the relay queue while
  paused inside it). A tape prints by calling back into the kernel's own
  `console::print_byte` (passed to it as `tape_run`'s first argument)
  rather than touching VGA/COM1 hardware directly, so its output goes
  through the exact same combined VGA+serial+relay path as any other
  command's -- it reaches a remote `term net` client too, just delayed
  until the tape returns rather than streamed live.
  `tools/test_term_net_tape_e2e.py` verifies exactly this.

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
The embedded kernel must also fit in 65535 bytes (`tools/write_netrom_layout.py`
enforces this at build time), since it is copied with a single 16-bit-counted
`rep movsb` rather than sector-by-sector.

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

`build/fos.img` is a 4 MiB raw disk image:

- Sector 0: MBR and stage-one loader.
- LBA 2048: active raw payload partition.
- First 32 payload sectors: stage-two loader.
- Remaining payload sectors: flat kernel image at physical `0x10000`.
- LBA 4096: a separate 4096-sector FOSFS data partition.

`tools/write_layout.py` generates the loader constants from the actual kernel
size. The image builder rejects invalid stage-one signatures, oversized stage
two payloads, and kernels exceeding the current BIOS disk-read limit. It
preformats only the image's own FOSFS data partition; it does not touch host
storage.

## Physical media warning

Creating the image is safe: `make image` writes only inside `build/`. Copying
the image to a block device destroys that device's partition table and data.
Only use a dedicated disposable drive, independently verify its device path,
and run a write command yourself after unmounting it. This project never
automatically selects or writes a host block device.

## Current boundaries

This milestone uses one CPU, no paging, heap allocation, userspace, or
scheduler. It has FOSFS (a deliberately tiny file-per-sector filesystem, not a
general-purpose one), interrupt-driven IRQ0/IRQ1 handling for the timer and
keyboard, and one polling-mode network card driver (RTL8139, raw Ethernet
frames only -- no ARP/IPv4/TCP stack, and no NIC interrupt handling; see
"Ethernet (`nic`)"), but no other hardware IRQ drivers (no native disk or USB
interrupt handling). The optional NIC-ROM boot path (see "Booting from a NIC
ROM") does not implement the BIOS-disk trampoline, so disk/FOSFS access is
unavailable when booted that way. The architecture keeps x86/driver code separate from
console and shell policy so later work can add those facilities without
changing the BIOS-to-kernel contract.
