# History: assorted development investigations and bug postmortems

This file collects several detailed, chronological development notes moved
out of `AGENTS.md` to keep that file within its size budget. Each section
below is otherwise unedited from its original place in `AGENTS.md`.

## Two soft-reboot bugs found and fixed during development

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



## Intel I350 support: scaffolded, not yet enabled (full detail)

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



## A physical printer port (full detail)

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




## Floppy drive support: investigated, not yet built (full detail)

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



## A second (non-boot) disk for config/log storage (full detail)

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



## Tape ABI grows a second callback (`should_abort`), `tapes/tape-rt/`, and remote abort (full detail)

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



## Live output relaying for `read_key`/`should_abort`-based tapes over `term net` (full detail)

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



## `tapes/calc/`: the reverse-Polish-notation calculator this all was for (full detail)

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



## MSI/MSI-X support: a local APIC, and Intel HDA as the first consumer (full detail)

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

**Low-memory constraint, now resolved**: the local APIC's own `map_mmio`
call is a fourth potential consumer of `paging.rs`'s dynamic-page-table
pool (alongside e1000/i350/HDA). At the time this feature was built, that
pool was deliberately *not* widened from 3 to 4 slots to cover it --
confirmed by trying it, not assumed: even one more static 4 KiB page
table pushed `__kernel_end` past `linker.ld`'s ceiling (then `0x90000`),
which had essentially no spare room at all. The pool is now 4 slots:
once that ceiling grew to `0x98000` for an unrelated reason (see
"Low-memory map" above), there was real headroom again, and the gap was
closed deliberately rather than left as a standing, if narrow,
MSI-availability risk. If this pool is ever exhausted anyway (4 *other*
high regions already mapped when the LAPIC tries to map its own), the
failure remains graceful: no local APIC, no MSI that boot, every driver
on its legacy path -- not a fault.

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



## Cooperative/preemptive scheduling: investigated, not yet built (full detail)

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



## ISA bus support and the 3C509: investigated, not yet built (full detail)

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


## A tape that visits 64-bit long mode and comes back (full detail)

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
code/stack, 0x10000..0x98000; VGA text memory, 0xb8000; and the tape's
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


## UHCI: a USB host controller driver, and isochronous USB Audio playback (full detail)

### UHCI: a USB host controller driver, and isochronous USB Audio playback

`kernel/src/uhci.rs` is a minimal UHCI (USB 1.1, I/O-port-based) host
controller driver, added because many class-compliant USB audio
interfaces are full-speed (12 Mbps) even when marketed as "USB 2.0" --
UHCI alone is enough bandwidth, and sidesteps EHCI's companion-controller
port-handoff dance entirely. `pci::find_uhci_controllers` (`pci.rs`) is
the second class-code PCI walk in this file (matching class 0x0C/subclass
0x03/prog-if 0x00, alongside `find_serial_ports`'s UART walk), since
`find_devices` only matches by vendor:device ID and every existing NIC/
audio backend needs the full `PciAddress` (for `enable_io_and_bus_master`/
`interrupt_line`), unlike `find_serial_ports`'s bare I/O addresses.

Bring-up (`Uhci::reset_and_start`): Global Reset, then Host Controller
Reset (self-clearing), then a single page-sized Frame List
(`pmem::allocate`, 1024 32-bit link pointers -- one per 1 ms frame) whose
every entry points at one fixed control Queue Head, then Run/Stop set.
One small fixed pool (also `pmem::allocate`, sub-page) holds that QH plus
up to `TD_POOL_CAPACITY` (34) Transfer Descriptors -- enough for one
control-transfer pipeline; there is no general per-device
bandwidth-reservation scheduler, since this driver only ever runs
enumeration's control transfers, never periodic (isochronous/interrupt)
traffic. No IRQ handler is registered: UHCI has no MSI capability (same
bucket as ES1370), and enumeration's control transfers have no real-time
deadline, so foreground polling of each TD's status is enough.

A real, non-obvious UHCI-specific bug, found only by booting against a
real, verified QEMU `piix3-usb-uhci` + `usb-audio` device rather than
trusting the spec/header from memory: **UHCI never auto-enables a port
after reset** (`Uhci::reset_port` must explicitly write the `EN` bit --
the spec's own wording is "software sets this bit to 1 to enable the
port"), and, separately, **`PORTSC`'s `EN`/`SUSPEND` bits are ordinary
read/write storage, not sticky-on-zero**: a later write meant only to
clear the write-1-to-clear `CSC`/`ENC` change bits must read-modify-write
the whole register (or simply write back the exact value just read) --
writing only the change-clear bits silently re-disables the port that was
just enabled. Both were confirmed by reading QEMU's own
`hw/usb/hcd-uhci.c`/`include/hw/usb/uhci-regs.h` directly after the first
symptom (every control transfer failing with a CRC/timeout TD status)
turned out to trace back to `uhci_find_device` skipping any port whose
`UHCI_PORT_EN` bit isn't set -- the same "verify against QEMU source"
discipline the NE2000 header-byte-order bug (above) already established
for this project. `reset_port` also observes the USB 2.0 7.1.7.3 reset
recovery time (`TRSTRCY`, >=10 ms) before the first request, which is
correct per spec even though it turned out not to be this bug's root
cause.

`Uhci::control_transfer` builds one SETUP-TD -> zero-or-more data-stage
TDs (toggling `DATA0`/`DATA1` per packet, at most `max_packet` bytes each)
-> one zero-length STATUS-TD chain, patches each TD's own link field to
the next TD with the Depth-First bit set (so the whole chain runs within
one frame rather than one TD per frame), points the QH's element pointer
at the first TD, and polls each TD's control word for `ACTIVE` clearing
(done) or an error bit (`STALL`/`BABBLE`/CRC-`TIMEOUT`) with a 500 ms
deadline. The STATUS stage's data toggle is always `DATA1` regardless of
how many data packets preceded it (a USB spec correction rule, not a
simple continuation of the alternating pattern). `get_descriptor`/
`set_address`/`set_configuration` are thin wrappers building the matching
standard SETUP packet.

The `usb` shell command (`shell.rs`) lists every discovered controller
(`io_base`, `irq`, whether `USBSTS.HCHALTED` is clear) and, per root port,
drives the full standard enumeration sequence far enough to print a real
device descriptor (vendor/product ID, class, `bMaxPacketSize0`) and
configuration descriptor (hex-dumped, `dump_bytes`) before issuing
`SET_CONFIGURATION` -- proving the whole chain end to end with no
audio-specific code involved. It always resets the port and re-enumerates
from scratch on every call rather than caching a previous result, since a
real device can be unplugged/replugged between commands. Low-speed
devices (realistically only HID keyboards/mice) are detected and reported
but not enumerated further. Verified end to end by `tapes/test_uhci_e2e.py`
against QEMU's real `piix3-usb-uhci` + `usb-audio` device (vendor:product
`0x46f4:0x0002`, its configuration descriptor's real USB Audio Class 1.0
`CS_INTERFACE` `HEADER` descriptor bytes, and idempotent re-enumeration
across two independent `usb` calls in the same session).

#### Isochronous OUT transfers, and `usb play`

Enumeration alone cannot play audio: USB Audio Class streaming uses an
*isochronous* endpoint, structurally unlike a control transfer -- no
Queue Head is involved at all. `IsocOutStream` (`uhci.rs`) is a second,
separate Transfer-Descriptor pool (`isoc_td_pool_addr`, one more
`pmem::allocate` page, `ISOC_TD_COUNT` = 16 TDs) driven by a fundamentally
different mechanic: because the Frame List has exactly 1024 entries and
hardware walks through all of them once per ~1.024 real seconds,
*continuous* streaming requires refreshing roughly one Frame List entry
per real millisecond, sustained for the whole playback -- unlike a
control/bulk queue, where the same Queue Head is simply revisited every
frame. `IsocOutStream::service`, polled from a foreground loop (no IRQ or
MSI involved -- `USBINTR` stays masked exactly as `reset_and_start`
already leaves it), keeps a small window of entries primed ahead of
hardware's own position, read via `USBFRNUM` (confirmed against QEMU's
own `frame_addr = fl_base + ((frnum & 0x3ff) << 2)`: the register is
11 bits, but only its low 10 select a Frame List entry). Each isochronous
TD sets `TD_CTRL_IOS` ("Isochronous Select"), confirmed against QEMU's
`uhci_complete_td` to mean exactly one try with no retry/error-counter
semantics -- unlike every control-transfer TD here, which uses UHCI's
ordinary 3-retry budget. Because the shared `pmem::TAPE_BASE`/`TAPE_LEN`
staging buffer (the same one `copy`/`tape load`/NETFS already document
reusing) is one contiguous physical block, each TD's buffer pointer
references it directly at the right offset -- no separate packet-buffer
pool or copy step needed.

Two real bugs were found only by actually streaming audio into QEMU's
`usb-audio` device and inspecting its recorded WAV output, not by code
review: the TD token's endpoint field was first coded at bits 14:11
(colliding with the *device address* field, bits 14:8) instead of the
correct bits 18:15 -- confirmed against QEMU's own extraction,
`uint8_t ep_id = (td->token >> 15) & 0xf;` (`hw/usb/hcd-uhci.c`) -- so
every packet silently reached a nonexistent device address instead of
the real one, and `IsocOutStream` reported "Playback complete" with
correct timing throughout, because an isochronous TD's `Active` bit
clears identically whether a packet succeeded or failed (see `IOS`
above) and nothing was checking the error bits at all. The second fix is
exactly that gap: `IsocOutStream::error_count` (incremented in
`install_next`, when a soon-to-be-reused pool slot's previous occupant
shows `TD_CTRL_ERROR_MASK`, and in a final sweep of the un-recycled tail
batch once streaming ends) makes a genuinely failed packet visible
instead of silently discarded -- the `usb play` command prints a warning
whenever it's nonzero.

Minimal USB Audio Class 1.0 descriptor parsing lives in `shell.rs`
(`find_uac_playback_endpoint`), walking a configuration descriptor's raw
bytes generically by each sub-descriptor's own `bLength`, looking for an
AudioStreaming alternate setting (`bInterfaceClass` AUDIO/`bInterfaceSubClass`
AUDIOSTREAMING, `bNumEndpoints == 1` -- alternate setting 0 always
declares zero endpoints per UAC1's own "idle" convention, so it's skipped
naturally) whose Format-Type descriptor is PCM/16-bit and whose
isochronous OUT endpoint is Synchronous or Adaptive sync (not
Asynchronous, which needs an unimplemented feedback endpoint). It
requires the sample rate to be evenly divisible by 1000 and the
endpoint's `wMaxPacketSize` to exactly match `(rate/1000) * channels *
bytes_per_sample` -- the fixed-packet-size assumption `IsocOutStream`
relies on -- rejecting (not attempting) any device that would need a
per-frame varying packet size, such as a 44.1 kHz device. `Uhci::
set_interface` (the missing standard `SET_INTERFACE` request UAC1's own
alternate-setting convention needs) is a thin wrapper reusing
`control_transfer`, exactly like `set_configuration`.

`usb play [diskN] NAME` (`shell.rs`) mirrors `enumerate_usb_port`'s own
"reset and redo everything from scratch" sequence -- so it never depends
on a separate, prior `usb` call having already addressed the device --
scanning every controller/port for the first device offering a usable
endpoint, completing `SET_ADDRESS`/`SET_CONFIGURATION`/`set_interface`,
then streaming the named file (raw PCM, already in the device's exact
negotiated format -- no resampling, the same expectation every other
audio backend here already has) through `IsocOutStream`, polling
`keyboard::poll_term_event`/`poll_background_work` each iteration exactly
like `nic sniff`'s exclusive loop so F12 stops playback early. On
completion or abort, `IsocOutStream::stop` restores every Frame List
entry the stream touched back to the control Queue Head, and
`set_interface` quiesces the streaming interface back to alternate
setting 0. Verified end to end by `tapes/test_uhci_play_e2e.py`: the
command discovers the real 48 kHz/16-bit/stereo/192-byte-packet format,
blocks for close to a seeded 1-second tone's real duration (proving
genuine ~1 kHz-paced streaming, not an instant no-op), reports no failed
packets, and the QEMU-captured WAV file's dominant frequency (a
single-bin Goertzel DFT, the same dependency-free technique
`tapes/test_audio_rtp_opus_e2e.py` already uses) matches the seeded tone
-- real, audible, correct playback, not just a clean-looking status
line. Isochronous *capture* (IN) remains explicit follow-on work; see
"Deliberately deferred work" below. `audio.rs`/tape-ABI integration is
covered next.

#### Confirmed on real, non-emulated USB Audio Class hardware

Unlike ES1370/HDA (still "unverified against real hardware" as of this
writing), this driver has been confirmed against a genuine, physically
attached class-compliant sound card -- a TerraTec Aureon Dual USB
(C-Media chipset, `0ccd:0077`) -- via QEMU's `usb-host` PCI passthrough
(`-device usb-host,vendorid=...,productid=...` in place of the synthetic
`usb-audio` device), not just emulation. Real enumeration (device and
full 253-byte configuration descriptor), `find_uac_playback_endpoint`,
and `usb play` all worked unchanged against real silicon, and surfaced
two things QEMU's own model never could:

- **A real off-by-one in `control_transfer`'s data-stage bounds check**,
  found only because this real device's `bMaxPacketSize0` is 8 -- the
  smallest legal full-speed value, and far smaller than QEMU's
  synthetic device's 64. Reading the full 253-byte configuration
  descriptor at 8 bytes/packet needs exactly 32 data TDs (SETUP + 32
  DATA + STATUS = all 34 of `TD_POOL_CAPACITY`'s slots, with zero to
  spare), but the old check (`slot >= TD_POOL_CAPACITY - 1`) rejected
  the transfer one slot too early -- it fired the instant `slot` reached
  the last *valid* index (33, correctly reserved for STATUS), not only
  once a write would actually go out of bounds. Fixed to
  `slot >= TD_POOL_CAPACITY` (the first genuinely invalid index, since
  slot 34 is where the Queue Head itself lives, immediately after the
  TD pool). QEMU's own device, with a much larger `max_packet0`, never
  came anywhere near this boundary, so no amount of emulated testing
  could have found it.
- **The Adaptive-sync `wMaxPacketSize` fix above was directly confirmed
  necessary**, not just theoretically justified from reading the
  descriptor: this real device's isochronous OUT endpoint declares
  `wMaxPacketSize=200` (Adaptive sync, some slack above the 192-byte
  nominal for 48 kHz/16-bit/stereo), which the old exact-equality check
  would have rejected outright. With the `>=`-nominal check,
  `find_uac_playback_endpoint` correctly computes and uses the 192-byte
  nominal size regardless of the endpoint's own larger ceiling, and a
  real 2-second tone streamed to physical hardware in almost exactly
  2000 ms with zero reported packet errors.

This required a one-time, explicit `sudo setfacl -m u:USER:rw
/dev/bus/usb/BUS/DEV` grant on the specific device node (mirroring the
project's existing `CAP_NET_RAW` precedent for raw sockets -- an agent
never runs this silently); real hardware access is otherwise blocked by
the host's own `snd-usb-audio`/`usbhid` kernel drivers already claiming
every interface, which QEMU's `usb-host` backend detaches automatically
once it has write access to the device node. No automated test exists
for this (there's no way to attach real hardware to `make test`'s QEMU
fixture) -- this was a manual, one-time verification session, the same
category of confirmation `tapes/longmode.S`'s own real-hardware section
above documents.

#### `audio.rs` integration: a USB device tapes can actually target

`usb play` proved the isochronous engine but only as a one-shot file
player, invisible to the tape ABI -- a tape like `audio-net`/`audio-rtp`
can only ever reach a device through `audio.rs`'s `Audio` enum and its
`audio_submit_at`/... callbacks, the same way it already reaches
ES1370/HDA. `uhci::UsbAudioDevice` (a thin wrapper: the owning
controller, the discovered `UacEndpoint`, and an `UnsafeCell`-
guarded `IsocOutStream` -- the same single-threaded interior-mutability
pattern `PcmRing` itself already relies on) is now a third `Audio`
variant, `Audio::Uhci`. `audio::initialize()` gains a third discovery
phase after ES1370/HDA's: `uhci::discover_playback_device()` calls the
same `find_uac_device()` `usb play`/`usb capture` use (moved into
`uhci.rs` so all three callers share it, since `audio.rs` must not
depend back on `shell.rs`), then immediately starts a dedicated
`PcmRing`-fed
`IsocOutStream` -- discovered once, like every other backend here;
plugging a device in after that first call isn't noticed until reboot.

The isochronous engine itself gained a second data source alongside
`usb play`'s existing zero-copy fixed buffer: `IsocOutSource::Ring`,
draining a `PcmRing` via `pop_into` (falling back to silence on
underrun, mirroring `Es1370::fill_period` exactly) into a small
dedicated packet-copy pool, and never reporting `Finished` -- unlike a
file, a live network-fed stream has no natural end. Because there is no
interrupt for this backend at all, `poll_background_work` (`shell.rs`)
gained one more unconditional call, `uhci::service_active_playback`,
keeping the active stream's look-ahead window filled whenever *any*
foreground loop happens to be running (a tape's own receive loop, `nic
sniff`, `term_net`, ...) -- sitting idle at the local prompt starves it,
an honest tradeoff of having no isochronous IRQ path, unlike ES1370/
HDA's unconditional ISR-driven playback.

Two more real bugs surfaced building this, again only by actually
streaming real audio rather than trusting the design on paper:

- **A permanent-stall bug in `service`'s own window-tracking math.**
  `next_index.wrapping_sub(current) & FRAME_INDEX_MASK` is *unsigned*
  modulo 1024, so "3 frames behind schedule" (a value of 1021) is
  indistinguishable from "1021 frames ahead" without a sign correction --
  a caller that goes a while between calls (exactly `audio-net`'s own
  receive loop, paced by how often network frames actually arrive, not a
  fixed timer) looked *already caught up* by this miscalculation and
  silently primed nothing at all from that point on, permanently
  starving the ring. Fixed by computing a genuinely signed gap (treating
  a raw value past half the Frame List's range as negative) and, when
  behind, resyncing a few frames ahead of *now* rather than mechanically
  replaying an unrecoverable backlog into slots that would otherwise
  only play a full ~1 s lap later.
- **`ISOC_TD_COUNT`'s look-ahead window needed real margin against host-
  scheduling jitter, not just uniform per-call overhead.** The original
  16 ms (sized for `usb play`'s own tight, gap-free polling loop) was
  provably too thin for `poll_background_work`-driven servicing: an
  otherwise-correct stream still intermittently dropped a small fraction
  of a percent of submitted bytes under this project's own QEMU dev
  environment, tightening to zero only once raised to 250 ms -- verified
  by five consecutive clean (zero-drop) runs at that size, where 16 ms
  and 64 ms each still occasionally dropped a few hundred bytes. 250 ms
  of playback latency is imperceptible for this use case; the real fix
  for a tighter bound would be an actual isochronous IRQ path, which
  remains out of scope.

Verified end to end by `tapes/test_uhci_audio_facade_e2e.py`: mirrors
`tapes/test_audio_net_e2e.py`'s exact shape (the unmodified `audio-net`
tape, loaded and run over `term net`, this script acting as the sending
peer) but with only `piix3-usb-uhci`/`usb-audio` attached -- no ES1370/
HDA -- so the USB device becomes `audio::get()`'s device 0 automatically
and the tape's existing, non-indexed `audio_submit` path reaches it with
zero tape-code changes. Confirms the discovered format, zero dropped
bytes, and the QEMU-captured WAV file's Goertzel-DFT dominant frequency
matching the sent tone. Also confirmed against the same real TerraTec
card described above, via `usb-host` passthrough: the facade path
(`audio` listing the device, `audio-net` streaming through it) works
identically on genuine hardware, with the same "no automated coverage
for real hardware" caveat as the rest of this section.

#### Isochronous capture (IN): `usb capture`, and unifying discovery

Capture is the mirror-image direction, needed before a tape can ever
record from a USB microphone the way `audio-net`/`tapes/audio-rtp`
already can from ES1370/HDA's ADC. It is **real-hardware-only from the
start**: QEMU's synthetic `usb-audio` device has zero capture support at
all (confirmed against its own source, `hw/usb/dev-audio.c` -- no
`in.*` state, no capture altset), so unlike playback there is no QEMU
stage to even attempt; every check here ran directly against the real
TerraTec card, which does support it (interface 2, a genuinely different
format from its own playback interface -- mono, not stereo).

Two real-device facts forced a design change before capture could even
be added. First, `UacPlaybackEndpoint`/`find_playback_device`'s single-
direction assumption doesn't hold: a device's playback and capture
interfaces can (and, on the TerraTec card, do) have entirely different
formats, so the type is now direction-agnostic (`UacEndpoint`, used for
either), returned in a `UacDevice { playback: Option<UacEndpoint>,
capture: Option<UacEndpoint> }`. Second, and more importantly:
`find_uac_device` discovers *both* directions from one single port
reset/`SET_ADDRESS`/`GET_DESCRIPTOR` pass, walking the same fetched
configuration descriptor twice (`find_uac_endpoint`, parameterized by
direction) and calling `set_interface` for whichever were actually
found -- deliberately not two independent calls. This project had
*just* learned, empirically, that two full resets of the same real
device back to back with no settle time between them can intermittently
fail (the `audio.rs` facade's own first real-hardware attempt, above);
unifying discovery avoids ever risking that same class of flakiness
within a single boot, rather than rediscovering it a second time by
building capture's own independent reset path.

`IsocInStream` (`uhci.rs`) is `IsocOutStream`'s mirror image, reusing
the identical Frame-List-window-priming machinery, but with the data
flow reversed in the one way that actually matters: for OUT, the driver
already knows each packet's exact content before installing its TD; for
IN, hardware decides how many bytes it actually sends (`ActLen`, read
back via the already-existing `decode_actlen` once `Active` clears), so
a completed TD's data must be *harvested* -- read out and folded into a
running total -- before its pool slot is reused for a fresh receive TD.
Zero-copy, exactly like `IsocOutStream`'s own `Buffer` source: each
receive TD's buffer pointer is a pre-computed offset directly into the
caller's destination (the shared tape staging buffer), so hardware
writes samples straight into their final position with no copy pool at
all. `usb capture [diskN] NAME` (`shell.rs`) mirrors `usb play`'s own
shape closely: discovers via `find_uac_device`, requires `.capture`,
captures into the staging buffer until either its 4 MiB capacity is
reached or F12 is pressed, then writes exactly however many bytes were
actually received out to the named FOSFS file via `fosfs::write`.

Verified manually against the real TerraTec card's microphone (no
automated test is possible -- see above): `usb capture` correctly
reports the real capture format (`48000 Hz/16-bit/1ch, 96 bytes/packet`,
matching the card's own descriptor exactly), and the captured file, read
back with `cat`, contains genuinely non-silent audio (a real, if quiet,
room-noise peak/RMS reading, the same threshold-based technique
`tapes/test_audio_net_real_mic_e2e.py` already uses for ES1370's own
real-microphone verification) with zero reported packet errors, reliably
across repeated runs. `usb play` was re-confirmed working unchanged on
the same real device after the discovery unification, both independently
and immediately alongside a capture session on the other interface.

#### `audio.rs` integration for capture: `capture_format()`, and a genuinely concurrent second TD pool

Wiring the discovered capture endpoint into `audio.rs`'s
`Audio::Uhci::capture()` (previously a stub always reporting zero) reused
playback's own wiring shape almost exactly: `discover_playback_device`
became `discover_usb_audio_device`, now starting an `IsocInStream` via a
new `Uhci::start_isoc_ring_in` (mirroring `start_isoc_ring_out`'s own
`Ring` source -- a small dedicated packet-copy pool, since a received
packet has no single fixed ring address to land in) whenever the
discovered device also offers a capture endpoint, and
`service_active_playback` became `service_active_audio`, servicing both
streams from the one `poll_background_work` call. `IsocInStream` itself
grew a matching `IsocInSink` enum (`Buffer`/`Ring`, exactly mirroring
`IsocOutSource`), with a new `overrun_count` (the capture-side
counterpart to `underrun_count`, incremented on a short `PcmRing::push`
exactly like `Es1370::drain_capture_period`'s identical accounting).

This surfaced a real, previously undocumented hazard before it could
ever bite on real hardware: `usb capture`'s original `IsocInStream`
shared its per-controller isoc TD pool with `IsocOutStream`, safe only
because that earlier increment never ran playback and capture at the
same time. `audio.rs`'s facade keeps a playback stream continuously
running from the moment *any* USB Audio device is discovered, so
starting a capture stream alongside it on a device offering both
directions -- exactly the real TerraTec card -- means both streams are
now genuinely active at once. Sharing one pool's `entries_written %
ISOC_TD_COUNT` slot indexing between two independently-counting streams
would let one direction's TD writes silently clobber the other's. The
fix: `Uhci` gained a second, dedicated `isoc_capture_td_pool_addr` page
(a fourth `pmem::allocate` call in `init_controller`, alongside the
existing frame list/control pool/OUT isoc pool) with its own
`isoc_capture_td_addr`/`write_isoc_capture_td`/
`read_isoc_capture_td_control` methods, used exclusively by
`IsocInStream`.

`Audio::capture_format()` is the new facade method this integration
needed: for ES1370/HDA it is a pure delegation back to `format()` (their
real hardware genuinely shares one format between directions, so there
is nothing separate to compute), but `UsbAudioDevice::capture_format()`
answers from its own, independently discovered capture endpoint instead,
confirmed necessary by the real device: the TerraTec Aureon Dual's own
capture interface is mono against its stereo playback format. Four new,
purely additive tape-ABI callbacks --
`tape_audio_capture_sample_rate`/`tape_audio_capture_channels_and_bits`
(device 0) and their `_at`-suffixed counterparts, appended at the very
end of `TapeEntry`'s parameter list following this project's established
"cdecl caller-cleans, existing tapes ignore the extra trailing stack
slots" convention -- let a tape correctly interpret `audio_capture`'s
bytes without assuming they share `audio_sample_rate`'s own numbers.

Verified against the real TerraTec card (no QEMU capture support exists,
as established above, so this remains manual/real-hardware-only): the
`audio` command's own status output now prints the capture format on its
own line, confirmed genuinely independent of the playback line --
`[0] USB Audio ...: 48000 Hz / 2ch / 16-bit.` immediately followed by
`ADC: 48000 Hz / 1ch / 16-bit.` -- and, with both directions running
simultaneously (the playback stream continuously priming silence with no
tape ever submitting to it, and the capture stream draining the real
microphone), both reported exactly 250 periods serviced with zero
underruns beyond the expected "nothing submitted yet" ones and zero
capture overruns or packet errors on either side, confirming the
dedicated second TD pool actually prevents the slot-collision hazard
described above rather than merely avoiding it by accident.

#### USB Mass Storage (Bulk-Only Transport): a bulk-transfer primitive, and the standalone `usb storage` command

Motivated by a real, physically confirmed problem: `uhci::initialize_all()`
resetting any UHCI controller (triggered by `usb`/`usb play`/`usb
capture`/`audio`) breaks a real Pentium 4's BIOS "USB Legacy Support"
`INT13h` emulation for its USB-booted disk (see "USB audio and USB-boot
media do not reliably coexist on real hardware" below). A native USB Mass
Storage driver is the only real fix -- it lets fos stop depending on BIOS
continuing to own the boot disk once running -- and this increment builds
the first, self-contained layer of it: enumeration and read/write against
a discovered device, verified standalone, with no `disk.rs`/FOSFS
integration yet (a separate, later step, since making a USB Mass Storage
device transparently *become* `disk::Device::Boot` needs its own
heuristic and is out of scope here).

`Uhci::bulk_transfer` (`uhci.rs`) is the foundational new primitive: a
sequence of data-stage packets only, with no SETUP or STATUS stage,
reusing `control_transfer`'s exact TD-chain-building/link-patching/
completion-polling machinery for the data stage alone. The one genuinely
new mechanic bulk transfers need that control transfers don't: a bulk
endpoint's DATA0/DATA1 toggle is real, persistent *endpoint* state that
must survive across separate calls (unlike a control transfer, always a
fresh self-contained SETUP+DATA+STATUS unit that always starts its own
data stage at toggle 1). The caller owns this state (`toggle: &mut bool`,
read on entry, written back only on success) rather than `Uhci` itself
tracking per-endpoint state it has no lifetime-bounded way to associate
with a specific device -- the same division of ownership `IsocOutStream`/
`IsocInStream` already established for their own per-stream state. On any
transport error, `*toggle` is left completely unchanged: USB Mass
Storage's own Bulk-Only Transport (BOT) error-recovery procedure resets
the device's toggle back to DATA0 anyway, so this function does not need
to guess how many packets actually completed before a mid-chain failure.
`Uhci::bulk_only_mass_storage_reset` (the class-specific "Bulk-Only Mass
Storage Reset" request) and `Uhci::clear_endpoint_halt`
(`CLEAR_FEATURE(ENDPOINT_HALT)`, targeting one bulk endpoint) are BOT's
own mandated pair of recovery requests, both thin `control_transfer`
wrappers mirroring `set_address`/`set_configuration`/`set_interface`'s
existing SETUP-byte-array construction convention exactly.

`kernel/src/usb_storage.rs` is a new file (not folded into `uhci.rs`
alongside `UsbAudioDevice`, unlike everything else UHCI-related so far):
Bulk-Only Transport's own CBW/CSW framing and the SCSI command subset
below are a real, separate protocol layer above raw bulk transfers, the
same reasoning that already keeps `es1370.rs`/`hda.rs` separate from
`pci.rs` despite depending on it. `find_msc_endpoints` walks a
configuration descriptor generically by `bLength` (the identical shape
`uhci::find_uac_endpoint` already uses), recognizing interface class
`0x08` (Mass Storage) / subclass `0x06` (SCSI transparent -- the
universal choice for real USB flash drives) / protocol `0x50`
(Bulk-Only Transport) and its two bulk endpoints; unlike USB Audio
Class's alternate-setting-0-is-idle convention, Mass Storage has no such
special case, so the first matching interface's endpoints are used
directly with no `set_interface` call needed at all. `find_msc_device`
mirrors `find_uac_device`'s exact "reset and redo everything from
scratch, one port reset per attempt" discovery shape.

`MscDevice` owns one discovered, already-`SET_CONFIGURATION`'d device:
its two bulk endpoints, their independently tracked DATA0/DATA1 toggles,
and a monotonically increasing CBW tag. `MscDevice::command` drives one
full CBW-out/data-stage/CSW-in exchange and is the only place that talks
to `bulk_transfer` directly; every other method (`inquiry`,
`wait_until_ready` -- a bounded-retry `TEST UNIT READY` loop, since real
removable media can report "not ready" for a moment right after
enumeration --, `request_sense`, `read_capacity_10`, `read_10`,
`write_10`) is a thin SCSI CDB wrapper around it. Any transport-level
failure or CSW phase error drives BOT's own two-request reset-recovery
procedure (`recover`, calling `bulk_only_mass_storage_reset` then
`clear_endpoint_halt` on both endpoints and resetting both tracked
toggles to match) before the error is returned, so the *next* command
starts from a clean, known device state rather than compounding a stuck
one. `read_capacity_10` rejects (`Error::Unsupported`) any device whose
native block size isn't exactly 512 bytes, matching this project's
established "reject rather than emulate" convention (the same one
`audio_rtp_tape::payload_type_for` already applies to unsupported audio
formats) rather than attempting 4Kn-to-512-byte emulation in a first cut.

The standalone `usb storage` shell command (`shell.rs`'s
`usb_storage_command`) mirrors `usb play`/`usb capture`'s own "prove the
driver against real/QEMU hardware with a dedicated command before any
deeper integration" precedent: it discovers the first Bulk-Only Transport
device found, prints its `INQUIRY` vendor/product strings and `READ
CAPACITY` result, and hex-dumps its first sector -- deliberately
read-only, since issuing a real `WRITE(10)` against whichever USB device
happens to enumerate first, with no disk-selection safeguard, is not a
safe default action for an always-available diagnostic command (`write_10`
itself is real, tested code -- see below -- just not wired into this
command). Verified end to end by `tapes/test_usb_storage_e2e.py` against
QEMU's synthetic `usb-storage` device: discovery, `INQUIRY` reporting
QEMU's own vendor/product strings, `READ CAPACITY` matching a backing
image's real size, and `READ(10)` of the first sector matching a known
seeded byte pattern exactly. `write_10` was manually verified end to end
during development (write a pattern to block 1, read it back, confirm an
exact match) via a temporary shell-command hook removed before committing
the read-only command; `write_10` itself is now wired into `disk.rs` --
see the next section -- so this manual verification is superseded by that
integration's own real, permanent test.

#### A real bug found only on genuine UHCI hardware: bulk transfers need settle time between stages

QEMU's synthetic `usb-storage` device never exposed this, but a real
Kingston USB flash drive against real UHCI silicon reliably failed its
very first `INQUIRY` with `TransportFailed(Timeout)` -- confirmed, via a
temporary raw register/TD/QH dump added directly to `bulk_transfer`'s own
timeout path, that the controller itself never cleared a TD's `Active`
bit at all (no error bit ever set), and that the *specific* failing
transfer varied between attempts (sometimes the data stage, sometimes the
CSW). Root cause: unlike `control_transfer`, which keeps one Queue Head
continuously armed across its entire SETUP+DATA+STATUS chain without
ever stopping it mid-exchange, each stage of a bulk transfer
(CBW/data/CSW) is a *separate* `bulk_transfer` call that stops the queue
on its own completion and the next call restarts it completely fresh --
with zero settle time between the stop and the next restart. Real UHCI
hardware needs a brief turnaround gap there that this driver's design
never provided. A 10ms (`wait_ticks(1)`) delay was added after *every*
stage transition -- CBW-to-data, data-to-CSW, CBW-to-CSW (when there is
no data stage), and after a command's own final CSW read completes
(needed separately: FOSFS's own multi-sector directory reads issue
several independent `read_10` calls back to back with no pacing of their
own, unlike the standalone `usb storage` command's single open()-to-
return sequence) -- fully resolving the issue. `MscDevice::command` also
gained a bounded retry wrapper (`command_once` renamed from the original
`command`, retried up to 3 times on a transport-level or phase error,
never on an ordinary `CommandFailed` SCSI response) as a defense-in-depth
measure against any remaining marginal timing on other real devices,
though the settle delays alone were what actually fixed this specific
drive.

**A real crash was also found and fixed during this investigation**:
before the settle delays existed, aggressively raising the retry count
alone (without the delays) made a genuine CPU fault *more* likely, not
less -- more attempts meant more exposure to a real hardware race: when
a bulk transfer times out, the controller may still be mid-transaction
on the wire at the exact moment software gives up, and immediately
reusing that same Transfer-Descriptor-pool memory for BOT's own
reset-recovery request (`recover`) can hit real hardware while it is
still touching that memory. A short settle delay (`wait_ticks(2)`) was
added to both `bulk_transfer`'s and `control_transfer`'s own timeout
paths, after the queue is stopped but before that memory can be reused by
any subsequent call -- eliminating the crash entirely in further testing.
The lesson generalized here: retrying more aggressively is not a
substitute for fixing a real timing bug, and can actively make a marginal
hardware race more likely to manifest as a hard failure instead of a
soft one.

All of this was found and fixed through live iteration against the exact
real Pentium 4 machine this whole feature was built for, using KUP (see
"Debug mode" above) to push each fix over the network without ever
needing to physically re-flash the boot USB stick -- confirming the
debug-mode design's own stated purpose (fast, unattended-feeling
iteration against real hardware) in practice, not just in the abstract.

**A second, independent real bug surfaced once the timing issue above was
fixed**: promoting `disk1` and then `tape load disk1 ed.bin` (a real,
23-sector file) failed with `TransportFailed(InitFailed)` -- `ls`/`cat`/
small `write`s had all worked because they never requested more than a
few sectors in one `read_10`/`write_10` call. Root cause: a single
`bulk_transfer` TD chain is bounded by `TD_POOL_CAPACITY` (34) TDs of at
most `max_packet` bytes each -- 2176 bytes at this device's full-speed
64-byte packet size -- comfortably less than one `MAX_TRANSFER_SECTORS`-
sized (16 sectors, 8192 bytes) FOSFS chunk. `bulk_transfer` itself now
splits an oversized transfer into successive bounded TD-chain passes
(with the same real-hardware-confirmed settle delay between them),
transparently to every caller -- `MscDevice::command`/`command_once`
needed no changes at all. Confirmed fixed on the real P4 with both a
23-sector tape (`ed.bin`) and a 678-sector one (`audio-rtp.bin`, loading
correctly with its exact real byte count both times). `tapes/
test_usb_storage_disk_e2e.py` gained a permanent regression test
(copying a real 24584-byte preloaded file onto and back off the attached
USB disk via `copy`, well over the 2176-byte single-pass boundary) --
confirmed to actually fail with the same `FirmwareFailure` symptom when
`bulk_transfer`'s internal chunking is reverted, not just incidentally
passing.

#### Wiring USB Mass Storage into `disk.rs` as an ordinary secondary disk

`disk::Device` used to be a closed two-variant enum (`Boot`/
`Secondary(usize)`) that *was* a BIOS drive number end to end -- every
secondary disk was addressed by a `drive_number` the trampoline could
call directly. Rather than adding a third `Device` variant (which would
have meant threading a USB/BIOS distinction through every function that
already matches on `Device`), the distinction was pushed one level down:
`SecondaryDisk` (the existing per-slot struct
`discover_secondary_disks` already populates) gained a `backend` field
(`SecondaryBackend::Bios(u8)` or `SecondaryBackend::Usb`), so `Device`
itself, `device_sector_count`, `device_data_partition_lba`/`sectors`,
and the `diskN` selector parsing in `shell.rs` needed *no changes at
all* -- a USB disk is simply one more entry in the same
`SECONDARY_DISKS` table a BIOS secondary disk already occupies, assigned
the next available `diskN` number exactly the same way. Only
`transfer`/`transfer_chunk` needed a real branch: `is_usb_backed`
checks a device's backend, and a `Usb`-backed transfer goes through a
new `usb_transfer` helper (`usb_storage::MscDevice::read_10`/`write_10`
against the single `USB_DISK` static) instead of `call_trampoline`,
after the same `device_sector_count` bounds check every other transfer
already gets.

`disk::attach_usb_disk` (called only by the new `usb storage attach`
shell command, never automatically) does the actual discovery + capacity
read + `SECONDARY_DISKS` registration in one call, rejecting a second
attach while one is already attached (`UsbAttachError::AlreadyAttached`
-- at most one USB disk is supported, since `usb_storage::open()` itself
only ever finds the first Bulk-Only Transport device). This is
deliberately **not** part of `discover_secondary_disks`'s automatic
boot-time BIOS probing, for the same reason the standalone `usb storage`
command stayed read-only: discovering a USB Mass Storage device means
calling `uhci::initialize_all()`, which resets every UHCI controller
found -- confirmed to break BIOS's own legacy `INT13h` emulation. An
operator must explicitly opt into `usb storage attach` each boot, and the
command itself prints an explicit warning before doing so.

**This risk turned out to be more general, and more immediate, than the
original Pentium 4 finding alone suggested -- confirmed concretely while
testing this exact feature, not just inferred.** Booting a plain QEMU
guest with a `usb-storage` device attached and no special configuration
showed `disk2: found (BIOS drive 0x81, 2048 sectors)` in the ordinary
boot log: SeaBIOS's own USB legacy support had *already* exposed that
same physical device as an ordinary BIOS secondary disk before any native
USB code ever ran. Running `usb storage attach` afterward (which
discovers a *different* `diskN`, e.g. `disk3`, for the identical device)
immediately broke `disk2`'s own BIOS-backed access
(`FOSFS disk error: FirmwareFailure` on the very next command against
it) -- the newly attached native `diskN` remained fully readable and
writable throughout, only the now-redundant BIOS path to the same
hardware broke. This means the danger is not limited to "don't combine
USB audio with a USB-booted machine" -- **any machine with a USB Mass
Storage device already plugged in (very likely already reachable as an
ordinary BIOS secondary `diskN`, courtesy of the same legacy-USB support
that makes it USB-bootable at all) should be assumed to break that
device's own BIOS-backed `diskN` the moment any UHCI-touching command
runs**, not only `disk1`/the boot disk specifically. `attach_usb_disk`'s
own doc comment records this finding in full.

Verified end to end by `tapes/test_usb_storage_disk_e2e.py`: `usb storage
attach` reports a new `diskN`, `format`/`write`/`cat` against it round-trip
byte-exact content, and a second `usb storage attach` call is correctly
rejected. One command in this flow was observed to occasionally fail with
a transient `FirmwareFailure` in a busy host-scheduling environment during
development (5 of 6 manual repeats were completely clean) -- consistent
with this project's other UHCI-adjacent QEMU tests' documented
host-timing jitter (see "Isochronous OUT transfers" above), not a
reproduced logic bug; the test itself does not retry, so a failure there
is worth one manual re-run before assuming a regression, per that test's
own doc comment.

#### Closing the Pentium 4 gap for real: `usb storage promote`

The original scoping plan flagged transparently switching
`disk::Device::Boot` itself over to this driver as a separate, riskier
follow-on, needing "its own heuristic" beyond simple attachment -- fos
has no BIOS call that answers "is drive `0x80` a USB device", and no way
to correlate a discovered USB Mass Storage device with a specific BIOS
drive number. The plan's own first-considered heuristic ("assume the one
discovered device is the boot device") was deliberately **not** used:
it's cheap to state but wrong exactly when it matters most (a machine
with more than one USB storage device attached).

`disk::promote_boot_to_usb` instead cross-validates using the disk's
actual content. Before touching UHCI at all, it reads `disk1`'s own
LBA 0 (its MBR -- present and BIOS-readable on every fos image
regardless of GPT/partition layout) via the *ordinary* BIOS path, and
also records `disk1`'s own BIOS-reported sector count. This baseline
read is a one-shot opportunity: discovering the USB device immediately
afterward may itself break BIOS's access to this exact disk (the
confirmed mechanism the previous section documents), so it must happen
strictly first. Only if the discovered USB device's own LBA 0 bytes
match that baseline exactly, *and* its own reported capacity is at least
as large as BIOS's own figure for `disk1`, does `Device::Boot` switch
over (a new `BOOT_USB_DEVICE` static, deliberately independent of
`USB_DISK` above -- promoting the boot disk and attaching an ordinary
secondary USB disk are mutually exclusive uses of whatever single device
is discovered); otherwise `promote_boot_to_usb` returns
`BootPromoteError::NotSameDisk` and `Device::Boot` is left completely
untouched. Capacity is deliberately a floor, not an exact match --
**confirmed necessary on real hardware, not just a defensive choice**: a
real, physically tested Pentium 4 machine's own USB-boot BIOS legacy
emulation reported `disk1`'s capacity as exactly 16 MiB (32768 sectors),
matching the *original boot image's own baked-in GPT size*, not the
physical USB stick's real ~14.4 GB capacity -- an exact-match requirement
would have wrongly refused to promote the machine's own genuine boot
media every time. The MBR content match is the real identity check (two
independent physical disks sharing a byte-identical MBR is not
realistically possible by accident), documented as "confirmed, not just
assumed," matching this whole follow-on's own honest framing.
`is_usb_backed`/`usb_transfer` (the same dispatch
`transfer`/`transfer_chunk` already use for a `SecondaryBackend::Usb`
disk) were extended to also recognize a promoted `Device::Boot` and
route it to `BOOT_USB_DEVICE` instead of `USB_DISK` -- no other function
needed to change, since `device_data_partition_lba`/`sectors` for
`Device::Boot` already just return the GPT-discovered (or
loader-provided) geometry established once at `disk::initialize` time,
which remains correct: promotion only changes *how* sectors are
transferred, never which physical bytes they refer to.

The explicit `usb storage promote` shell command (never automatic,
printing the same kind of "this resets every UHCI controller" warning
`usb storage attach` already does) drives this. `bootinfo` gained one
conditional line reporting whether `disk1` is currently promoted.
Verified end to end by `tapes/test_usb_boot_promote_e2e.py` -- the one
test in this whole feature that actually boots `build/fos.img` itself
from a QEMU `usb-storage` device (`-boot order=c`, SeaBIOS's own USB
Legacy Support) rather than an ordinary IDE `-drive`, faithfully
reproducing the real Pentium 4 scenario entirely inside QEMU: `disk1` is
ordinarily BIOS-readable before promotion, `usb storage promote`
discovers and confirms the very disk it just booted from, `bootinfo`
reflects the promoted state, and `disk1` remains fully readable *and
writable* afterward -- proving ordinary FOSFS access to the boot disk no
longer depends on BIOS's own legacy USB emulation staying intact, even
though discovering the device during `promote` already reset the UHCI
controller that emulation depends on. A second scenario in the same test
file (`test_refuses_mismatched_device`) confirms the cross-validation
actually rejects a clearly different USB device (a distinctly-sized,
all-zero backing image) rather than ever guessing, leaving `disk1`
completely untouched and still BIOS-backed.

**Confirmed on the real, physical Pentium 4 machine this whole feature
was built for** -- not just the QEMU repro above. Live-iterated against
it over the network via KUP (see "Debug mode" above and the bulk-transfer
timing-bug section above, both found and fixed during this same session):
`usb storage promote` correctly identified the machine's own genuine USB
boot stick (a Kingston DataTraveler 3.0) despite BIOS reporting a wildly
different, smaller capacity for it (see above), switched `disk1` over,
and `ls`/`cat`/`write`/`rm` all continued working correctly through the
native driver afterward -- the exact real-hardware scenario
(`FOSFS disk error: FirmwareFailure` after any UHCI-touching command)
that originally motivated this entire driver is now closed on the actual
machine that exhibited it.

**Still deliberately out of scope**: `reboot soft` re-enters
`boot/stage2.S` from scratch and always depends on BIOS to reload the
kernel, completely independent of this feature's own, purely Rust-level
`BOOT_USB_DEVICE` state (which a fresh boot always reinitializes to
`None` regardless of whether the previous boot had promoted it). If
`disk1`'s BIOS access is already broken by the time a soft reboot is
attempted, it fails exactly as already documented in the previous
section -- promoted or not. A real fix would need stage two itself to
speak USB, an undertaking on the scale of a native 16-bit real-mode USB
driver, not attempted here.
