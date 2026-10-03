# fos v2: design note

Status: draft, decisions marked **Decided**. Milestone M0 is under way. It records what we
decided in conversation, what the v1 work taught us, and the open questions that
should be settled before the first line of v2 code.

## 1. Why a v2

fos v1 is BIOS -> real mode -> 32-bit protected mode -> a `no_std` Rust kernel.
It works, it is used for network testing in production, and it teaches the
x86 boot path well. Its limits are the same few things over and over:

- **32-bit address space.** The kernel cannot address RAM above 4 GiB. The
  Skylake test machine has 16 GiB. `memtest` needs a hand-written long-mode
  excursion to reach most of it, and the pools (4 KiB frame pool, 1 MiB big
  pool, tape `alloc` vs `alloc_frames`) exist only to work around that and the
  low-memory budget.
- **The low-memory region.** The whole kernel, its `.bss`, its stack and all
  static buffers must fit below `0x9f000`. We hit that wall three times this
  year (xHCI, audio rings, page tables) and fixed it by moving 160 KiB of audio
  buffers out of `.bss`. v2 has no such region.
- **BIOS-only boot and disk access.** Every disk is reached through INT13h in
  real mode (one round trip per call) or through a native driver we had to write
  *because* the BIOS emulation breaks (USB). Under UEFI there is no INT13h at all.
- **Legacy hardware assumptions.** 8259 PIC, PIT, VGA text mode, PS/2. Newer
  boards drop some of them.

A 64-bit UEFI kernel removes the first three outright and makes the fourth a
decision instead of an accident.

## 2. Goals and non-goals

Goals:

1. Boot on the machines we own: the Skylake (UEFI-capable, 16 GiB), and in
   principle any x86-64 UEFI machine, with QEMU + OVMF as the test bed.
2. One flat 64-bit address space with all RAM identity-mapped; no pools split by
   address range, no long-mode excursions, no `u32` physical addresses.
3. Keep what makes fos *fos*: small, readable, narrated boot, ring-0 trusted
   tapes, remote-first development (`term net`, KUP, `devloop`), a flat FOSFS.
4. Stay usable day to day as a network lab OS: the NIC drivers, `nettest`,
   `router`, `bridge`, `memtest` and friends all have to come across.

Non-goals (for v2.0):

- SMP, preemptive scheduling, user mode / process isolation. Tapes stay ring 0.
- Secure Boot, a bootloader menu, network boot, graphics beyond a text console.
- Pentium 4 / 32-bit-only CPUs. They stay on v1 (see section 12).
- Wi-Fi, NVMe-class performance work, USB hubs beyond what we already have.

## 3. Repository layout and relationship to v1

**Decided: v2 is a `v2/` tree in this repository.** v1 stays exactly where it is
at the top level, untouched, and keeps working for the Pentium 4 and as the
"real mode to protected mode" teaching edition. Nothing moves or is renamed.

v2 is a new top-level tree in the same repository:

```
v2/
  loader/     UEFI application (x86_64-unknown-uefi): the "stage two" of v2
  kernel/     x86_64 kernel (custom no-std target), identity-mapped
  tapes/      64-bit tapes (the shared tape-rt/tape-net crates, widened)
  tools/      image builder (GPT + FAT32 ESP + payload + FOSFS), test harness
```

Drivers are **copied and widened**, not shared, at first: the v1 drivers are
33 K lines of kernel code that assume `u32` physical addresses in a hundred
places, and a shared crate would force v1 and v2 to move in lock step. Once v2
has settled, the pure-protocol and register-level parts can be factored into
crates both trees depend on. FOSFS, the KUP wire protocol, NETFS, `term net` and
the peer address book are format/wire contracts and stay byte-compatible so the
existing host tools (`devloop.py`, `send_kernel_update.py`, `foscopy.py`,
`term_net.py`) work unchanged against both.

## 4. Boot path

```
firmware (UEFI) -> loader (PE/COFF app, \EFI\BOOT\BOOTX64.EFI on the ESP)
                -> kernel (ELF64, read from an A/B slot by LBA)
```

The loader is the narrated "stage two": it prints what it is doing to the
firmware console, GOP and serial. It is written in Rust for the
`x86_64-unknown-uefi` target (the compiler already has it; no `std`), with a
hand-rolled minimal binding to the handful of boot services it needs rather than
a dependency: `OutputString`, `AllocatePages`, `GetMemoryMap`, `LocateProtocol`
(GOP, Block I/O, Loaded Image), `OpenProtocol`, `ExitBootServices`, and the
configuration table lookup for the ACPI RSDP. That is roughly 600 lines.

Loader steps:

1. Find its own disk through Loaded Image -> Device Handle -> Block I/O.
2. Read the A/B slot metadata block and run `select_kernel_slot`'s state machine
   (a port of the logic now in `boot/stage2.S`), exactly as v1: one try for a
   pending slot, automatic fallback, same on-disk metadata bytes.
3. Read the chosen slot's kernel (raw, by LBA, through Block I/O) and load its
   ELF64 `PT_LOAD` segments at their linked physical addresses.
4. Collect the memory map, GOP framebuffer description, RSDP pointer, the boot
   disk's GPT partition GUID, and the UEFI runtime-services pointer into a
   `BootInfo` v4.
5. `ExitBootServices` (with the standard retry loop when the map key goes stale),
   then jump to the kernel entry in the mode UEFI left us in: long mode,
   identity-mapped, interrupts off.

The kernel immediately replaces the firmware's page tables and GDT with its own.

`BootInfo` v4 is the loader/kernel ABI, passed by pointer in a register (not at a
magic physical address): magic, version, size; memory map as 64-bit
`(base, length, type)` triples (UEFI types folded into the E820 numbering the
kernel already understands); framebuffer base/size/geometry/pixel format; RSDP;
runtime services pointer and runtime-region list; boot slot number and slot
metadata; the boot disk GUID; and the loader's own reserved ranges. Validate
magic, version and size first, as v1 does.

## 5. Image layout

A GPT disk with three partitions:

| Partition | Contents |
| --- | --- |
| ESP (FAT32, ~32 MiB) | `\EFI\BOOT\BOOTX64.EFI` (the loader) and nothing else the kernel depends on |
| Payload (raw) | slot metadata block, then kernel slot 0 and slot 1 (A/B), same layout idea as v1 |
| FOSFS | the data partition (tapes, `debug`, `hostname`, `start`, `router.cfg`...) |

The kernel is deliberately **not** a file on the ESP: KUP writes the inactive
slot over the network, and writing raw slots needs no FAT writer in the kernel.
The loader reads slots by LBA through Block I/O (it has the firmware's drivers
there). `tools/build_image.py` gains a small pure-Python FAT32 writer for the ESP
(stdlib only, in keeping with the project's tool list) instead of depending on
`mtools`/`mkfs.fat`.

## 6. The kernel: mode, memory, interrupts

- **Target**: a custom `x86_64-fos` target (no red zone, no SSE in the kernel,
  `panic = abort`), built with `-Z build-std` like v1. Kernel code never uses
  SSE; tapes may enable it for themselves.
- **Memory map**: the kernel is linked at a fixed physical address above 1 MiB
  and is no longer size-constrained. One physical allocator over the whole
  memory map replaces `pmem`'s two pools; its bitmap is allocated at boot from
  the first usable region large enough, not baked into `.bss`. Allocation takes a
  "below 4 GiB" flag for DMA devices that need it (RTL8139, EHCI/UHCI, and anything
  that cannot do 64-bit descriptors).
- **Paging**: identity-map all RAM with 2 MiB (and 1 GiB where possible) pages,
  kernel text read-only + executable, everything else NX. Unmapped guard pages
  for stacks. MMIO is mapped on demand (`map_mmio` keeps its role, but with a real
  allocator behind it instead of a fixed pool of eight page tables). This keeps
  the property v1 argues for: wild pointers fault legibly.
- **Interrupts**: 64-bit IDT; the 100 Hz tick comes from the local APIC timer
  calibrated against the TSC and the HPET/ACPI PM timer (the PIT is not
  guaranteed on UEFI machines). Legacy INTx drivers need the I/O APIC; MSI/MSI-X
  stays the preferred path and is extended to more drivers. A double-fault IST
  stack replaces v1's task gate.
- **ACPI**: only what is needed: RSDP -> XSDT -> MADT (APIC IDs, I/O APIC),
  FADT (reset register, PM timer), MCFG (PCIe config space), HPET. No AML
  interpreter.
- **Reboot and power**: `reset`/`reboot` use the UEFI runtime `ResetSystem`
  (warm or cold); `reboot soft` stops being a special case.
- **Crash evidence**: v1 stores the crash record in RAM, which some machines wipe.
  v2 can also write a small record into a UEFI NVRAM variable through runtime
  services, which survives every reset (rate-limited to spare the flash).

## 7. Console and input

- **Output**: the GOP linear framebuffer, drawn by a small text console with an
  embedded 8x16 bitmap font and the ANSI subset v1 already honours. Scrolling
  uses a shadow buffer and one blit; the framebuffer is mapped write-combining
  (PAT) so it is not painfully slow. COM1 mirroring and the `term net`/printer
  relay queues stay exactly as they are (the single `console::write_bytes`
  choke point survives the port). The "every command fits 80x25" rule becomes
  "fits an 80-column text grid", which is what every remote session assumes
  anyway.
- **Input**: **Decided: a local keyboard is required**, not optional. PS/2 where
  the board has it (a legacy 8042 path, easy), and USB HID keyboards through the
  xHCI/EHCI drivers we already have: boot-protocol HID needs interrupt-IN
  endpoints, which neither driver implements yet, and USB enumeration has to be up
  early enough to type at the first prompt. Because the Skylake's only USB
  controller is xHCI and its firmware may no longer emulate PS/2 for a USB
  keyboard once we take the controller over, USB HID is the critical path for
  local input on that machine and gets its own milestone early (M3b, section 11)
  instead of being left until the end. The firmware's own `SimpleTextInput`
  covers the loader and the first moments of the kernel until then.

## 8. Storage and the boot disk

v1's lesson: native drivers beat BIOS emulation, and a UEFI machine has no
BIOS emulation. v2 therefore needs native drivers for the boot media from the
start: **xHCI** (the Skylake's only USB controller), **AHCI** and **EHCI** (the
older boards), all of which exist in v1 and port directly; NVMe later.

The new problem is **finding the boot disk without the firmware**. After
`ExitBootServices` there is no Block I/O. The loader passes the boot disk's GPT
partition GUID (and the ESP's); the kernel scans its native controllers, reads
each disk's GPT and picks the one carrying that GUID. For a USB stick that means
the kernel brings up the xHCI/EHCI controller *during boot* rather than on an
explicit `usb native` command, and does so before the first FOSFS access (the
`start` file, `hostname`, `debug`). The controller handoff, which v1 learned to do
carefully, becomes a normal boot step.

KUP keeps its design: write the inactive slot through the native driver and
update the metadata block, A/B fallback in the loader. `reboot` goes through
firmware, so a pending slot is tried on the next boot.

## 9. Tape ABI v2

v1's tape ABI is a cdecl call with 43 appended arguments. v2 replaces it with
**one pointer to a versioned table**:

```rust
#[repr(C)] pub struct TapeApi {
    pub version: u32, pub size: u32,       // size lets old tapes ignore new fields
    pub putc: extern "C" fn(u32),
    pub should_abort: extern "C" fn() -> u32,
    pub alloc: extern "C" fn(usize) -> usize,     // one allocator, no pools
    pub free: extern "C" fn(usize, usize),
    pub args: *const u8, pub args_len: usize,
    /* ... keys, NICs, clocks, filesystem, audio ... */
}
extern "C" fn tape_main(api: &TapeApi);     // SysV64 ABI
```

- Tapes are ELF64 `ET_EXEC` images at a fixed load address inside the kernel's
  identity map (the same "fixed address, no PIC needed" idea as v1).
- Append-only still holds: fields are only ever added at the end and `size` tells
  a tape what exists. No more declaring 38 unused parameters to reach one.
- Large static buffers still go in `.bss`/`alloc`, never the stack.
- `memtest` loses its 64-bit excursion, its two pools, `alloc_frames` and
  `e820_entry` games: a 64-bit tape just allocates RAM and tests it, and the
  memory map is a field of the table.
- The shared crates (`tape-rt`, `tape-net`) are widened to `usize` and built for
  the 64-bit target. The protocol code in `tape-net` is already pure Rust and
  moves over almost unchanged.

## 10. What ports how

Rough size of the v1 kernel and how much changes (lines of kernel Rust):

| Area | Lines (v1) | v2 work |
| --- | --- | --- |
| shell, fosfs, console logic, term net, KUP, NETFS | ~12 K | mostly widen types; console gets a framebuffer backend |
| NIC drivers (e1000, e1000e, I350, RTL8139/8168, 3C905, NE2000) | ~6 K | widen DMA addresses; `allocate_below_4g` for 32-bit-DMA parts; MSI everywhere |
| USB (UHCI, EHCI, xHCI, usb_storage), AHCI | ~5 K | widen; boot-time bring-up |
| audio (HDA, ES1370, AC'97, USB audio) | ~4 K | widen; late, lower priority |
| pmem, paging, gdt, interrupts, lapic, arch, entry.S | ~3.5 K | **rewritten** for 64-bit; no real-mode trampoline |
| disk.rs, boot_info, debug, reboot | ~2.5 K | disk loses the BIOS backend; boot_info v4; reboot via firmware |

Things that **disappear**: the real-mode disk trampoline and its PIC/IVT/paging
dance, A20 handling, `stage1/stage2` assembly, the low-memory map, the 16/32-bit
GDT juggling, the 4-slot `map_mmio` pool, the `longmode` shim, and every "keep it
under `0x9f000`" rule.

Things that **stay**: FOSFS v3 on disk, the A/B slot format, KUP/NETFS/`term net`
wire formats, the shell command surface, the narrated-boot philosophy, the
debug/watchdog/crash-record machinery, `devloop`.

## 11. Milestones (each ends with something runnable and a test)

| # | Milestone | Done when |
| --- | --- | --- |
| M0 | Loader skeleton | OVMF boots `BOOTX64.EFI`; it narrates to serial + firmware console, prints the memory map, GOP mode and RSDP. QEMU smoke test. |
| M1 | Kernel hello | Loader loads an ELF64 kernel from a raw slot and exits boot services; kernel switches to its own GDT/page tables and prints through serial + framebuffer. |
| M2 | Memory and interrupts | Physical allocator, identity map with NX/guard pages, IDT, LAPIC timer tick, faults reported legibly. |
| M3 | Shell and network | PCI scan, one NIC (e1000 under QEMU), `term net` session working from boot. This is the first point `devloop` can drive v2. |
| M3b | Local keyboard | PS/2 (QEMU) and USB HID boot-protocol keyboards (xHCI/EHCI interrupt-IN) deliver keys to the shell prompt; tested in QEMU with `usb-kbd` on `qemu-xhci` and `usb-ehci`; verified on the Skylake once it is reachable. |
| M4 | Storage | xHCI + AHCI (+ EHCI) native, boot-disk discovery by GUID, FOSFS read/write, `start`/`hostname`/`debug` honoured. |
| M5 | Tapes | Tape ABI v2, ELF64 loader, `hello-rs`, `calc`, `memtest`, `nettest` running; `make test-tapes-v2` in QEMU. |
| M6 | Update and recovery | KUP over the network with A/B fallback, watchdog, crash record (RAM and NVRAM), `reboot`. |
| M7 | Real hardware | Boot the Skylake from a stick, `term net` over its e1000e/I350s, run `nettest` on the lab cabling. Fix what only hardware shows. |
| M8 | The rest | Remaining NIC drivers, audio, `router`/`bridge`, USB HID keyboard, polish. |

Testing uses QEMU with OVMF (`OVMF_CODE_4M.fd` + a per-run copy of
`OVMF_VARS_4M.fd`, both present in this environment) and the serial console the
whole project already tests through. The v1 harness's habits carry over: bounded
waits, preserve serial output on failure, wait for the prompt before typing.

## 12. Compatibility and branching

- v1 (the current top-level tree) is maintained for the Pentium 4 and as the
  protected-mode teaching edition. Bug fixes land there; features may or may not.
- v2 develops under `v2/` on `main`. The two editions share the repository, the
  host tools and the docs; v1 remains the default build (`make` builds v1,
  `make v2-...` targets build v2) until v2 is good enough to promote.
- Wire and disk formats are shared, so one set of host tools drives both.
- The README and `AGENTS.md` grow a "which edition" paragraph; v2 gets its own
  `AGENTS.md` section kept small, with detail in `docs/`.

## 13. Risks and open questions

1. **UEFI quirks on real firmware.** OVMF is forgiving. The Skylake's firmware
   will have its own idea of the memory map, ExitBootServices retries, GOP
   formats and runtime-region handling. Mitigation: M7 early, narrated loader,
   a serial-less fallback of "print on the framebuffer".
2. **Local input.** No PS/2 on some boards means a USB keyboard stack, and a local
   keyboard is a requirement, so USB HID (interrupt-IN transfers on xHCI and EHCI,
   early enumeration) is on the critical path and scheduled as M3b.
3. **Boot-disk discovery.** Needs the native storage drivers working at boot, with
   the xHCI/EHCI handoff done correctly the first time. This is the part of v1's
   USB story (BIOS legacy emulation, takeover order, slow enumeration) that gets
   *more* important, not less.
4. **Framebuffer speed.** An uncached framebuffer makes scrolling crawl; needs PAT
   write-combining or a dirty-rectangle shadow buffer. Measure on the Skylake.
5. **IRQ routing without the PIC.** Legacy-INTx devices need the I/O APIC and MADT
   interrupt-source overrides. A fallback is to keep using the PIC where the
   firmware still provides it, but that is a hardware-by-hardware gamble; the NICs
   we care about already use MSI.
6. **ACPI scope creep.** Hold the line at "tables only, no AML". Anything that
   needs AML (some power/thermal/hotplug) is out of scope.
7. **Effort.** Roughly a third of the v1 kernel is rewritten and the rest is a
   mechanical-but-large widening. The milestones keep it shippable at every step;
   nothing needs the whole port finished before it is useful.
8. **Open decision**: do we want the loader to also provide a tiny UEFI shell-like
   fallback (boot the other slot by key press) for a bad kernel? Probably yes, but
   after M6.
9. ~~`v2/` directory or a sibling repository?~~ **Decided**: a `v2/` tree in this
   repository.

## 14. First steps (what to do next)

1. ~~Settle layout/branching~~ (decided: `v2/` in this repo).
2. Build M0 on this machine: a `x86_64-unknown-uefi` loader that prints the
   memory map under OVMF, plus the Python ESP/GPT image writer it needs, plus a
   `make test-v2-loader` smoke test.
3. Boot that same image on the Skylake as an early reality check: if firmware
   quirks bite, we want to know before the kernel exists.
