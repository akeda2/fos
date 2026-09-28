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
