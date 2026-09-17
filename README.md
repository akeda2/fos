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
`make`/`make all`, and does not affect `image`/`run`/`test`.

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
| `irqstat` | Shows how many times the timer (IRQ0) and keyboard (IRQ1) interrupts have fired. |
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
| `term [COM1\|COM2\|COM3\|COM4\|PCI0\|PCI1\|...] [BAUD]` | Enters raw terminal mode: forwards PS/2 keystrokes byte-for-byte to a remote machine over the chosen port (default COM1), and displays incoming bytes locally (VGA only, not re-echoed to serial). Press F12 to return to the shell. COM1's rate is fixed at 115200 (shared with the local console); COM2/3/4 and PCI ports accept an optional custom `BAUD` that must evenly divide 115200. |
| `nic` | Reports whether an RTL8139 Ethernet controller was found on PCI (and, if so, its MAC address). |
| `nic send` | Transmits one raw broadcast Ethernet test frame. |
| `nic recv` | Polls briefly for one incoming raw Ethernet frame and, if one arrives, hex-dumps it. |
| `nic sniff` | Enables promiscuous mode and prints every frame the card sees (destination/source MAC, ethertype, hex dump) until F12 stops the capture, then turns promiscuous mode back off. |
| `nic discover` | Broadcasts a `chat`-protocol discovery probe (re-sent every few seconds) and lists every distinct responder as it replies, running until F12 stops it (only fos machines currently running `chat` or `nic discover` reply). |
| `nic peers` | Prints the peer list built so far by `nic discover` (and by any auto-replies `chat` has sent), without sending anything. |
| `chat [PEER_MAC\|PEER_NUMBER]` | Enters a line-based Ethernet chat: broadcast to everyone by default, or unicast (send and receive) to one `aa:bb:cc:dd:ee:ff` peer -- or a numbered peer from `nic peers`/`nic discover`. Type a line and press Enter to send it; incoming lines are shown tagged with the sender's MAC. While running, also auto-replies to `nic discover` probes from other machines. Leave with F12 (local keyboard) or by typing `/quit` (works over serial too). |
| `halt` | Stops the CPU. Reset or power-cycle to restart. |

Argument-taking commands show a usage message for missing or extra arguments.
Unknown commands and oversized input are reported rather than ignored.

## Interrupts and the physical-frame allocator

The kernel builds a 256-entry IDT with only two populated gates (the PIT
timer on IRQ0 and the PS/2 keyboard on IRQ1), remaps the 8259 PIC's vectors
clear of the CPU's reserved exception range, and enables interrupts. Scancodes
and timer ticks are captured by minimal ISRs and drained by the shell loop
between prompts; see `kernel/src/interrupts.rs`. A small bitmap-based
physical-frame allocator (`kernel/src/pmem.rs`) tracks which 4 KiB frames the
E820 map reports as usable, reserving everything below the kernel's own
low-memory region. There is no paging yet -- this only tracks free/used
frames, exposed through `pmem`/`palloc`/`pfree`.

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
serial I/O. `nic` (no arguments) reports whether a card was found and its
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
