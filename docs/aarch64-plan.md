# Plan: a Raspberry Pi (aarch64) edition

Status: **plan only, no code.** Written 2026-10-19 after a colleague asked "can we run this on a Raspberry Pi?".
First target: **Pi 4 (BCM2711)**; Pi 400 and CM4 are the same silicon. The Pi 5 (RP1 south bridge) comes later, see the end.

## What carries over, what does not

| Piece | Verdict |
| --- | --- |
| `abi` (`TapeApi`, FOSFS/FAT helpers, keymaps), `crates/*`, tape *sources* | Reusable. Rebuild for an aarch64 tape target. Audit for x86 asm first (`rdtsc`, port I/O). |
| FOSFS, FAT/exFAT, shell (`term.rs`), line editor, tape loader logic | Reusable once the arch-specific calls sit behind a small `arch` layer. |
| `xhci.rs`, `usb_*`, `fosfs.rs`, `fatvol.rs`, `fat` | Mostly MMIO; need memory barriers and non-cached DMA memory (x86 hides both). |
| `cpu.rs`, `gdt.rs`, `idt.rs`, `isr_stubs.S`, `apic.rs`, `ioapic.rs`, `msi.rs`, `acpi.rs`, `paging.rs`, `serial.rs` (port I/O), PCI via 0xCF8 | x86 only. Aarch64 gets its own `arch/aarch64/`. |
| `hda.rs`, `e1000.rs`, `rtl8*.rs`, `threecom.rs`, `ahci.rs`, `nvme.rs`, `ehci.rs`, `ohci.rs` | Not used on a Pi (no such hardware). They stay x86 only. |
| Tapes `cpu64-*`, `longmode-rs`, `memtest`, `fpcheck` | x86 specific; skipped. All the rest (clock, gonio, ltas, play, record, ptp, router, ...) are portable in principle. |
| BIOS boot chain | Irrelevant. |

Size of the job: the new core (boot, MMU, exceptions, GIC, timer) is about 2-3k lines; drivers (Genet, PCIe bridge,
mailbox, SD) about 3-4k more. The large x86 drivers are simply not needed.

## Decisions to make

1. **Boot path.** Two options:
   - *edk2 for Pi 4 (UEFI) + our loader built for `aarch64-unknown-uefi`.* Reuses the loader, the A/B slot logic, GOP
     framebuffer and the UEFI memory map; `BootInfo` carries a device-tree pointer where x86 has the RSDP. Cost: one more
     firmware layer, and edk2 owns the first bring-up of PCIe/USB.
   - *Raw `kernel8.img` loaded by the Pi firmware (config.txt `arm_64bit=1`).* Simpler and smaller, but no A/B slots
     and no loader; the framebuffer comes from the mailbox, memory map from the device tree.
   Recommendation: **UEFI first** (it keeps "only POST/boot differs" true, as on x86), raw `kernel8.img` as a fallback if edk2
   proves troublesome.
2. **Hardware floating point.** aarch64 has NEON always on; tapes can use hard-float with no `fp_level` dance. The
   `x86_64-fos-tape-fp` split becomes a non-issue there.
3. **Where to start: QEMU `virt` before real hardware.** `qemu-system-aarch64 -M virt` has PL011, GICv3, generic timer, PCIe
   (ECAM), `qemu-xhci`, virtio/e1000e NICs, edk2. That gives a testable core (M1-M2) with the existing Python harness, long before
   the Pi-specific drivers. QEMU `raspi4b` exists but is incomplete (no PCIe/Genet), so it is not the test target.

## Milestones

- **M0 - arch split (largely done 2026-10-19).** Done: x86-only modules moved to `kernel/src/arch/x86_64/` and re-exported at the crate root; `crate::timer` is the neutral system timer; PCI config access is `crate::pci_config` (x86: ports 0xCF8/0xCFC). Left, best done with `cargo check --target aarch64-unknown-none-softfloat` as the guide (221 errors at the start of M1): neutral names for interrupt enable/sleep/halt, `paging::map_device`/`is_mapped`, `msi::*` and the CPU id; `cfg`-gating the PC-only drivers (PS/2, VGA text, RTL8139/8168, 3C905, e1000, HDA, AHCI, EHCI/OHCI, RTC/PIT in `clock.rs`, `power`, `diag`, COM2-4 in `uart.rs`), which sit inside enum dispatch in `nic.rs`, `block.rs`, `storage.rs` and `term.rs`.
  `serial`, `barrier`, `pci config`). No behaviour change; `make test` must stay green. This is the risky refactor, do it first.
- **M1 - core on QEMU virt (first step done 2026-10-19).** Done: the loader builds for `aarch64-unknown-uefi` and boots under AAVMF (it syncs the
  instruction cache and allocates the kernel as LoaderCode); `abi::elf` accepts the host CPU's ELF machine and RELATIVE relocation;
  `kernel/src/main.rs` dispatches to `main_x86_64.rs` or `main_aarch64.rs`; a minimal aarch64 kernel (`arch/aarch64/`: `_start`, PL011) prints its BootInfo;
  `make test-aarch64-boot` (image: `make image-aarch64`). Also done: exception vectors, generic timer (`make test-aarch64-faults`), the physical allocator and our own page tables
  (`arch/aarch64/paging.rs`: identity map, RAM NX in 2 MiB blocks, kernel W^X, guard pages, `map_device`; EL1 or EL2). Also done: GICv2 (`arch/aarch64/gic.rs`, QEMU virt default and the Pi's GIC-400) with a 100 Hz physical-timer IRQ through a register-saving IRQ vector. Also done: the GOP framebuffer (mapped non-cacheable) with the shared 8x16 text console; the test takes a QEMU `screendump` (ramfb) and checks pixels. Also done: PCI config through ECAM (base from the ACPI MCFG; `arch/aarch64/pci_config.rs`), a polling virtio-blk driver (`virtio_blk.rs`, test scaffolding), and the portable block/GPT/FOSFS/storage stack mounting the boot volume. Also done: a serial shell (`shell.rs`, shared `lineedit`; help, ls, cat, disks, mem, uptime; tested by typing into QEMU's serial). Also done: UART and GIC bases from ACPI SPCR/MADT. Also done: xHCI (shared `xhci.rs`, polling) and a USB boot keyboard feeding the shell (`usb_kbd.rs`; QEMU `qemu-xhci` + `usb-kbd`). Left: growing the module list (log, fosfs, shell) until `cargo check` shows no `cfg` gaps. The UART base is the QEMU one
  (`pl011::QEMU_VIRT_BASE`); the Pi's comes from the SPCR/DT tables.
- **M2 - tape ABI on aarch64 (first step done 2026-10-19).** Done: `make tapes-aarch64` builds `hello` for the built-in hard-float `aarch64-unknown-none` target (no custom JSON needed) with the shared `tapes/tape.ld`; `tape-rt` has an aarch64 `_start`; `kernel/src/tape_aarch64.rs` loads a tape from FOSFS into an executable 2 MiB-aligned region (`paging::set_executable`, `cpu::sync_code`), enables FP, and calls it on its own stack with the full `TapeApi` (unsupported calls are stubs: no NIC, audio, serial, gfx); shell `run NAME [ARGS]`; `make test-aarch64-boot` runs `hello`. Done too: the `.fos_build` stamp carries a CPU byte (0 x86-64, 1 aarch64, from the ELF machine; `stamp_tape.py` uses the Rust toolchain's objcopy for aarch64) and `tapesync` skips tapes for another CPU (`Action::OtherCpu`); the manifest carries it in bits 16-23 of the ABI word. Done too (2026-10-19): `gfx_*` for screen 0 on the framebuffer (`gfxcheck` runs in `make test-aarch64-boot`, read back from a QEMU screendump), UTF-8 `putc`, `FP_NEON`. Also: `keymap us|sv` (shell command and `settings` file) for the USB keyboard; `calc` and `ed` run (`ed` writes through `fs_write`, tested over the serial console). Left: the other tapes (the ones that need only screen/keys/files first), `tape load/save/list` as on x86, screen-indexed gfx calls for a second screen. Original plan: shared `tapes/tape.ld`, tape ELF loader checks `e_machine`,
  `tools/stamp_tape.py` and `build_image.py` learn the architecture. `make test-aarch64-*` mirrors the key tests.
- **M3 - PCIe + xHCI + USB keyboard/storage on QEMU.** PCI through ECAM, MSI via GIC ITS or legacy SPI, reuse `xhci.rs` with barriers and
  uncached DMA buffers.
- **M4 - Pi 4 bring-up.** Device-tree parsing (memory, GIC-400, timer, UART), BCM2711 PCIe root complex (inbound window limits DMA; the
  allocator's "below 4 GiB" option helps), the VL805 xHCI behind it, GIC-400 (GICv2), mailbox property interface (framebuffer, clocks),
  Genet v5 Ethernet (MDIO to the BCM54213 PHY), SD/eMMC (Arasan, EMMC2) or boot from USB only to start.
- **M5 - tapes on the Pi.** clock and gonio first (screen + network + USB audio through xHCI); then ptp/audio-net. No HDA on the Pi;
  audio is USB only (plus HDMI audio, later, if wanted).
- **M6 - image tooling.** `tools/build_image.py --arch aarch64`: GPT with an ESP holding the edk2 files, loader and kernel slot, plus FOSFS.
  `tools/write_stick.py` stays user-run only.

## Known hardware traps (to check, none verified yet)

- **Update the Pi 4's bootloader EEPROM first** (an old one may not boot the current edk2 build or USB/SD the way its README expects): write
  "Misc utility images > Bootloader" from Raspberry Pi Imager to the SD card, boot it once, wait for the green screen. Needs the SD card.
- Expected: edk2 on the Pi 4 gives a GOP framebuffer (so no mailbox code is needed), ACPI with SPCR (UART), MADT (GIC-400) and an MCFG
  for the PCIe/VL805 (with a config-window quirk). The kernel reads the UART and GIC from those tables (`arch/aarch64/acpi.rs`), tested on QEMU only.
- Pi 4 PCIe/xHCI needs the firmware (or us) to reset the VL805 through the mailbox; edk2 does it already.
- Genet has no PTP hardware stamping, so PTP on a Pi is software-stamped (accuracy like our `software` mode).
- DMA coherency: treat all DMA memory as non-cacheable at first, optimise later.
- The Pi 4 firmware only gives an HDMI framebuffer after its own display setup; resolution follows the monitor EDID.
- x86 code leans on strong ordering: every driver ring/doorbell sequence needs an audit for `dmb`/`dsb`.

## Two screens

Experiment on skylake3 (2026-10-19): the UEFI GOP lists one handle, and a read-only dump of the Intel display engine
(`gpu` shell command) shows only pipe A on (DDI C); the second screen is detected on DDI D but never lit. On a PC a second screen
therefore means native Intel modesetting (AUX/EDID, link training, second pipe), a large Intel-only job.
On the Pi each HDMI output is addressed through the firmware mailbox, which should make a second framebuffer cheap (verify the
display-selection property tag on real hardware). The tape side is the same for both: an append-only ABI call set
(`gfx_screens`, `gfx_select` or a screen index in `gfx_*`), and `audio_screen::Screen` taking an index. This is a good reason to do
the Pi port before dual-screen on PC.

**Done on the Pi 4 (2026-10-20).** `kernel/src/mailbox.rs` talks to the VideoCore mailbox (property channel 8). The firmware needs
`max_framebuffers=2` in `config.txt` (`tools/fetch_rpi4_firmware.py` adds it to the pftf config, so `make image-pi4` has it); without
it the display select is ignored and a new framebuffer replaces display 0's. With it, `get number of displays` says 2 (ids 2 = HDMI0,
7 = HDMI1) and one message (select display, physical/virtual size, depth 32, pixel order BGR, allocate, pitch) gives display 1 its own
buffer (outside kernel RAM, mapped non-cacheable by `paging::map_framebuffer`). Boot registers it as tape screen 1
(`framebuffer::add_screen`; screen 0 stays the console). The shell has `display` (what the firmware reports) and `display test N`
(colour bars). **Tape ABI 20** (append-only): `gfx_screens` and `gfx_info_on`/`open_on`/`close_on`/`fill_on`/`blit_on` with a screen
number; the old `gfx_*` calls are screen 0. Tape side: `tape_rt::gfx_screens`, `gfx_*_on`, and `audio_screen::Screen::on(index)`;
`gfxcheck screen=N` exercises it. The kernel blanks screen 1 when a tape ends. Verified on the Pi: `gfxcheck screen=1` draws correctly
(colours right) on the second screen, console on screen 0 untouched. x86 reports 1 screen. Not done: screen sizes that differ (a tape
sizes its buffers from `gfx_info_on`).

**Two tapes at once (2026-10-20).** `run A args + B args` runs two tapes side by side on core 0 (`tape_aarch64.rs`, `run_two`).
Not a second core: the kernel is single-core and its state is unlocked. Each tape gets a region (`load_slot`), its own stack and a
`TapeApi`; `fos_task_switch` (asm, x19-x30 + d8-d15) switches tasks when a tape calls `wait_for_event` (`tape_rt::idle`), and the CPU
sleeps only once every live tape has idled. Only the focused tape gets keys; Tab moves the focus, Esc ends the focused tape. A tape
that never idles starves the other. `+` is a second tape only if the next word loads as a tape (`calc 3 4 + 6 *` still works).
`clock`, `gonio` and `ltas` take `screen=N`. Verified in QEMU (`test-aarch64-boot`: two clocks, Tab, Esc) and on the Pi 4:
`run clock screen=0 + gonio screen=1` with USB audio capture, no stutter or lag. The Tab focus switch itself is only checked by hand.

## Pi 4 SD card (M6, first cut)

`make image-pi4` builds `build/fosfor-aarch64/fosfor-pi4.img`: the aarch64 image with the pftf/RPi4 edk2 release (pinned by version and
SHA-256, `tools/fetch_rpi4_firmware.py`, kept in `build/rpi4-firmware`) copied into the ESP root (`build_image.py --esp-dir`; the ESP
writer now handles subdirectories and VFAT long names, `fsck.vfat` is clean, and the x86 ESP is byte-identical to before). The Pi's own
firmware reads `config.txt`, `start4.elf` and `RPI_EFI.fd` from the first FAT volume; edk2 then runs our `\EFI\BOOT\BOOTAA64.EFI`.
A person flashes it: `sudo python3 tools/write_stick.py /dev/sdX --image build/fosfor-aarch64/fosfor-pi4.img`. **Unverified on the
Pi:** that the Pi 4 EEPROM boots from a GPT disk's ESP (if not, a hybrid MBR entry for the ESP is the fix) and that edk2 offers
the loader without a boot-menu step. `config.txt` has `enable_uart=1` and the PL011 on GPIO 14/15, so the serial cable shows everything.

## Pi 5 (later)

Ethernet, USB and GPIO sit on the RP1 chip behind a PCIe link: a different NIC (Cadence GEM), a different xHCI, a different
interrupt tree. Most of M0-M3 carries over; M4 does not. Treat it as its own milestone after the Pi 4 works. One Pi 5 is on
hand (Pi 4s are plentiful). Its CPU is Cortex-A76 (ARMv8.2-A): it adds dot-product, FP16 and LSE atomics over the A72's
ARMv8.0, still NEON-width (no SVE). Tapes stay on the ARMv8.0 baseline; anything newer is chosen at run time from
`ID_AA64ISAR0/1_EL1` (a future capability call, not a new `fp_level` value) and never assumed at build time.

**Dual-screen gonio+clock (future)**: the screen-index ABI above must work on both Pi generations; design `gfx_*` for it from the
start (the Pi 4 has two micro-HDMI outputs).

## Decisions taken (2026-10-19)

- Target: Pi 4; UEFI-first (edk2 + our loader), raw `kernel8.img` only as a fallback.
- Order: QEMU `virt` first, then the Pi.
- A 3.3 V USB serial adapter is available for early bring-up; the SD card is still to be found.

## Pi 4 bring-up findings (2026-10-19, verified on hardware)

The Pi 4 boots pftf edk2 -> our loader -> kernel, shows the boot log, runs the serial shell and a USB keyboard.

- **EL2 has one execute-never bit.** The kernel runs at EL2, where descriptor bit 54 (UXN) is the only XN bit; PXN is ignored. Code pages
  must clear both. QEMU boots at EL1 (UXN only blocks EL0) and never shows it; the symptom was a hang the moment the MMU came back on.
- **Serial:** the firmware gives no usable SPCR, so when the GIC distributor is 0xff841000 the kernel uses the PL011 at 0xfe201000
  (GPIO 14/15, 115200). An in-memory early log (`dmesg`, serial only) is replayed onto the screen once the console exists.
- **Bring-up tools:** pixel breadcrumbs (`early.rs`: `mark`, `bits`) before the console works; shell `acpi`, `peek`, `poke`, `dmesg`, `usb`.
- **PCIe:** no MCFG. The BCM2711 root complex at 0xFD500000 (config via EXT_CFG_INDEX/DATA, `pci_config.rs`) is already linked up by
  the firmware; the VL805 xHCI is 01:00.0. Outbound window CPU 0x6_0000_0000 -> PCI 0xf8000000 (`cpu_address()` translates BARs);
  inbound window 4 GB at 0.
- **DMA:** a 4 MiB non-cacheable pool (`memory::dma_pool`, serves allocations with a `below` limit) since the VL805 is assumed non-coherent.
  Works for xHCI on hardware. Never use exclusive atomics on that memory.
- **USB:** the Pi's ports sit behind a VIA hub (2109:3431) on the VL805; `usb_hub.rs` now builds for aarch64 against `usb_kbd.rs`
  and the keyboard is found. xHCI events arrive by MSI (see "Interrupts" below).
- **SD card (`sdhci.rs`, written 2026-10-19, tested on QEMU `sdhci-pci` only):** standard SDHCI by polling and PIO, 32-bit register accesses only (the Arasan EMMC2 at 0xFE340000, like Linux's `sdhci-iproc`, dislikes 8/16-bit ones), card reinitialised from scratch (CMD0/8/ACMD41/2/3/9/7, 4-bit, 25 MHz), SDSC and SDHC. Shell `cp SRC DST` and `cat diskN/NAME` exercise it (`make test-aarch64-boot` boots virtio and reads/writes a second SD copy). **Unverified on the Pi:** the 100 MHz base clock, 3.3 V power and the reset of a card the firmware left running.
- **Still missing on the Pi:** Pi 5; interrupts for the SD card and the UART.

### Pi 4 Ethernet (Genet), written 2026-10-19, unverified on hardware

`kernel/src/genet.rs`: GENETv5 at 0xFD580000, polled, one TX buffer and 128 RX buffers from the non-cacheable DMA pool.
MAC from the firmware's UMAC registers (fallback `02:f0:5b:00:00:01`); BCM54213PE PHY at MDIO 1 gets the RGMII-RXID skew
and is auto-negotiated only when it has no link. Tape ABI NIC 0 is the Genet; multicast is promiscuous mode. QEMU has no
Genet, so only `make tapes-aarch64` and the boot test cover it; the `netcheck` tape (DHCP, ARP, four pings) and the shell
`nic` command are the first things to run on the Pi. Unknown until then: bring-up order, speed/duplex bits, PHY state left by edk2.

## Genet RX ring and serial KUP

- Genet RX: the MAC's write pointer starts at descriptor 0 after reset, while the producer index counts from where the
  firmware left it. The driver indexes from 0 and resets PROD/CONS to 0; an unwritten descriptor reads `0x08008000`.
- Serial KUP (`kup serial` in the shell, `tools/send_kernel_serial.py PORT KERNEL`, both architectures): framed
  stop-and-wait over the console UART, whole-image CRC-32, written to the inactive slot only after it matches, then
  reset; the loader gives it one try. x86 needs the 4 KiB COM1 ring for 2 KiB frames. Tests: `make test-aarch64-kup`,
  `make test-kup-serial`. Only the Pi can verify: the BCM2711 watchdog reset, SD slot writes through EMMC2, Genet RX/DHCP.

## Interrupts (2026-10-20, verified on the Pi, USB keyboard included, and in QEMU)

- `gic::register(intid, level, handler)` routes an SPI to CPU 0 and runs `handler(intid)` in IRQ context.
- `arch/aarch64/msi.rs` has the x86 `msi` interface. Pi 4: the PCIe controller's MSI block, SPI 148, 32 messages latched
  in MSI_INTR2. **The target address must be 0xF_FFFF_FFFC** (above 4 GiB): the firmware's inbound window covers the first
  4 GiB, and with 0xFFFF_FFFC the VL805's message was taken for a memory write and never arrived. QEMU: GICv2m frame.
- `xhci.rs` `enable_interrupts_all` (shared with x86, self-test and storm guard included) runs before IRQs are enabled in
  `kmain`; `no-msi` in `settings` keeps polling.
- Genet: INTRL2_0 receive-done bits 13-15 (all three fire), SPI 157, a storm guard. The foreground still drains the ring;
  the interrupt wakes the shell's and tapes' `wfi`, so a ping now takes 0.3 ms instead of 10 ms.
- Tests: `make test-aarch64-boot` checks MSI delivery through the GICv2m frame; the Pi checks are the `nic` and boot log lines.

## Faster serial KUP (2026-10-20)

`kup serial BAUD` (aarch64 shell) and `tools/send_kernel_serial.py PORT KERNEL --fast BAUD` raise the PL011 (and the host
port) for the transfer; the kernel restores the firmware's speed afterwards, also when no sender starts within 30 s. The UART
clock is derived from the firmware's 115200 divisors. On the Pi: **921600 works** (375 KB in 5.5 s instead of 33 s); 1.5 and
3 Mbaud got no answer (host adapter or line, not investigated). x86 COM1 still runs at 115200 only.

## UART and SD interrupts

- **UART (`pl011.rs`)**: the receive and receive-timeout interrupts (Pi 4 SPI 121, QEMU SPI 1) empty the 16-byte FIFO into a
  4 KiB ring, so a busy foreground (flash write, redraw) no longer overruns it. A byte below the trigger level raises no
  interrupt on QEMU, so `read_byte` also polls the FIFO (interrupts off, to keep order) when the ring is empty.
- **SD (`sdhci.rs`)**: Pi 4 only (EMMC2 SPI 126). Status bits are still polled; a wait longer than 50 us sleeps in `wfi` until the
  controller raises its line (the handler masks the signal enable again). QEMU keeps spinning.
- `irqs` shell command: counts for timer, UART, SD, Genet and MSI vectors.
- Verified on the Pi (serial KUP at 921600 into slot 0): `irqs` shows UART, SD, Genet and xHCI MSI counts rising; `ls` works;
  QEMU aarch64 boot, faults and serial-KUP tests pass.

- SD under load (Pi 4): `cp` now stages up to 256 KiB and `cmp A B` compares two files; 92 KB tapes copied chain-wise
  (ed -> e1 -> e2 -> e1 -> e3, netcheck -> n1) all compared identical while the SD interrupt count rose (327).
- The paging bring-up probes (on-screen register rows, MMU-switch pixel markers) are gone; the Pi still boots.

## Tape file calls

`fs_read/write/remove` and `fs_stream_*` now live in `kernel/src/tape_fs.rs` for both ports (the x86 `tape.rs` puts its FAT32/exFAT
branch in front): `NAME`, `diskN/NAME` and `@LABEL/NAME`, `FS_BAD_NAME` for invalid names, streaming for recorders. Before, aarch64
knew only the boot volume and stubbed the streams. Tested: `run ed disk2/tapenote` on the QEMU SD card, read back by the shell, absent
from the boot volume (`make test-aarch64-boot`); x86 `test-ed`, `test-record`, `test-recorder`, `test-recorder-fat`, `test-config-fat`.
Not on aarch64 yet: FAT32/exFAT volumes (no `fatvol`/USB storage there), so a PC-formatted card is not readable.

## Crash safety and file listing

- **No journal, copy-on-write instead.** FOSFS keeps no allocation map: the directory (8 sectors, one 64-byte entry per file) is the only record of what is used,
  so a power cut cannot leave leaked or double-booked space. `Volume::write` now puts the new contents in a free extent of its own and commits with the single
  directory-sector write; the old file or the new one survives, never a mix. Only on a disk with no free run does it overwrite in place. Streams (`reserve`)
  already wrote data before the length. `fsck [diskN]` lists extents and reports damaged, overlapping or duplicate entries without changing anything.
  Not covered: a torn 512-byte directory-sector write (the SD card's own atomicity is assumed) and FAT volumes, whose writer already commits the entry last.
- **ABI 19: `fs_list`** (and `tape_rt::list_file`): the nth file of `""`, `diskN` or `@LABEL`; `disk_info` now works on aarch64 (free space, label).

## FAT32/exFAT on aarch64

`fatvol.rs` (the glue to the `fat` crate) now builds on both ports; the FAT branches of the tape file calls moved from
`tape.rs` into `tape_fs.rs`, so `fs_read/write/remove/list/stream_*` and `disk_info` behave the same everywhere. A disk with
no FOSFS volume but a FAT32/exFAT one (GPT basic data, MBR, or a superfloppy) is reached by `diskN/NAME`; the shell's `ls
diskN` and `cat diskN/NAME` use the same calls. aarch64 has no wall clock yet, so FAT files get the fixed default date.
Tested in QEMU on a `mkfs.vfat` virtio disk (`make test-aarch64-boot`); the Pi's SD card and USB sticks are not tried
(USB mass storage is not ported).

## USB mass storage on aarch64

`usb_storage.rs` (Bulk-Only Transport, one SCSI command per exchange, 64 KiB per command) now builds on both ports; the EHCI
host is x86-only. `usb_kbd::probe_device` hands a non-keyboard device with a Mass Storage interface to `attach_xhci`, a hub's
children go the same way, and an unplugged or replugged root port marks its disk stale so a returning stick keeps its disk
number (`usb` rescans). Sticks are therefore ordinary `diskN` entries: FOSFS or FAT32/exFAT, `ls`, `cat`, `cp`, `fsck`, and the
tape file calls all work on them. QEMU `usb-storage` on the xHCI controller is tested in `make test-aarch64-boot`; the Pi's
VL805 controller with a real stick, and unplug/replug on aarch64, have not been tried. Booting from a USB stick is not
supported (the Pi boots from its SD card).

## USB audio on aarch64

`usb_audio.rs` and `audio.rs` build on both ports. The EHCI and HDA parts are satisfied by stub modules
(`ehci_none.rs`, `hda_none.rs`, included with `#[path]`), so only xHCI audio exists here. `usb_kbd::probe_device`
attaches audio-class devices, `xhci::drain` hands isochronous events to `usb_audio::irq_event`, and the tape table's
`audio_*` calls are the same as on x86. Shell: `audio`, `audio modes`, `audio tone [HZ] [SECONDS]`.
Verified on a Pi 4 with a 2-out/6-in class 2.0 interface: attach, modes, 4 s playback with feedback locked
(6.001 vs 6.000 frames/packet), no errors. Whether the tone is audible and clean, capture, and DMA coherency of the
isochronous buffers are still to confirm by ear/loopback.

## Clock tape, software wall clock, `recv` (2026-10-20)

- `clock` builds for `aarch64-unknown-none` (in `A64_TAPES`). The `dsp` crate became portable: `sqrt64` is `fsqrt` on
  aarch64 and the AVX2 FFT build is x86-64 only (NEON vectorises the generic loop).
- `clock.rs` is shared. The Pi 4 has no RTC, so the software clock starts **unset** and advances on `timer::micros()`;
  `date set YYYY-MM-DD HH:MM[:SS]` (aarch64 shell) or the clock tape's NTP sets it, and FAT files then get its date.
  The `wall_clock` and `wall_clock_set` tape calls are real. Ideas not done: NTP at boot, a battery-backed RTC on a HAT.
- `recv NAME [BAUD]` (aarch64 shell) and `tools/send_file_serial.py PORT FILE NAME [--fast BAUD]` copy one file over the
  console UART with the serial-KUP framing (`kup_serial::serve_file`). Deploy a tape without taking out the SD:
  `python3 tools/send_file_serial.py /dev/ttyUSB3 build/fosfor-aarch64/tapes/clock clock --fast 921600` (154 KB in 2.3 s).
  The shell must be idle at its prompt. `NAME` may be `diskN/NAME`. Tested in `make test-aarch64-kup` (QEMU).
- Verified on the Pi: DHCP on the Genet, NTP from the LAN server, the machine's clock set (round trip 432 us).

## Gonio on aarch64 (2026-10-20)

`gonio` builds unchanged for `aarch64-unknown-none` (`A64_TAPES`) and runs on the Pi 4 with a USB class 2.0 interface (microphones on
inputs 1-2): scope, level meters, correlation and the loudness panel look as on x86. **No QEMU test**: QEMU's `usb-audio` is
output-only and the port has no HDA, so there is no capture device to run it against; it is verified on the Pi only. Deploy:
`python3 tools/send_file_serial.py /dev/ttyUSB3 build/fosfor-aarch64/tapes/gonio gonio --fast 921600`.

## Time at boot: `start` script and `ptp` (2026-10-20)

The shell runs the boot volume's `start` file before the prompt (one command per line, `#` comments and blank
lines skipped; `start` reruns it; long-running commands go last). The Pi's `start` is `run ptp l2 settime tz=+2`:
the `ptp` tape (in `A64_TAPES`) uses software timestamps, since Genet has none, and `wall_clock_set` sets the clock.
Verified on the Pi against clock1 (`ptp master both`): `reset` and the clock is right at the prompt (offset ~90 us).

Genet finding: `UMAC_CMD` promiscuous alone does not pass multicast on the Pi 4. The firmware leaves
`UMAC_MDF_CTRL = 0x18000` (two address filters), and they still apply; Linux (`bcmgenet_set_rx_mode`) writes 0 there
with promiscuous. `genet::set_promiscuous` now does the same and restores the value on `off`. Shell: `nic promisc
on|off`, `nic rx multicast`. Later: `nic_multicast_join` could use the 17 MDF entries instead of promiscuous.

Open: IP-mode ptp (UDP 224.0.1.129) gets Announce/Sync but clock1 never answers the Delay_Req (the Pi does send it,
86 bytes, `nic_send` true); L2 mode works. Not yet found whether the switch (IGMP snooping), clock1 or our frame is
at fault.

## Services on the Pi (2026-10-20)

`run NAME ARGS &N`, `ps` and `kill N|NAME|all` work as on x86 (`docs/design.md` "Services"; `kernel/src/tasks.rs` is the same file,
`service_host.rs` and the `ctx` block in it are the per-kernel parts). `&1` is the Genet; `&0` owns no NIC. Services use tape regions
2 and 3 (0 and 1 are for `run A + B`). A foreground tape's end no longer blanks screen 1 while a service runs. Verified on the Pi:
`run clock ntp=off screen=1 &0` ticks on the second screen next to the shell and `run calc`; it stalls while a shell command or a
tape load blocks (SD reads), because switching is cooperative. `run A ARGS + B ARGS` (two tapes, Tab moves the keys) gives A `screen=0` and B `screen=1` unless their arguments name a `screen=`. `run A | B [split=h|v]` shares one screen instead: the console is cut in two (side by side, or stacked with `split=v`), each tape sees its half as a whole screen (`framebuffer::split_set`, tape-rt always uses the numbered gfx calls), the first one has the keys. Up to four tapes: `run A | B + C | D` splits each physical screen (screen numbers: 0 and 1 are whole screens, 2+2P and 3+2P the halves of screen P; a pair may name `screen=P`). `run A | B screen=1 &0` starts a background pair on screen 1 while the shell stays on screen 0 (`rotate=` turns the screen its group draws on; settings `rotate` and `rotate1`; not for background pairs). Ethernet KUP is x86 only; the Pi uses serial KUP.

## Ethernet KUP and discovery on the Pi (2026-10-20)

`kernel/src/netservice.rs` speaks the x86 wire format (ethertype 0x88b6) from the shell's idle loop: it answers
`tools/peers.py --discover` (name = `hostname` file + `1`, else `fos-` + last MAC bytes) and takes a kernel update
(`tools/send_kernel_update.py`, `devloop.py push`) with the same checks as x86: length, RFC 1071 checksum, one sender at a time,
no retransmission. The image is staged in RAM and written to the inactive slot by `kup_serial::install`, then the board resets.
It needs a `kernel-update` line in the boot volume's `debug` file (deliver it once with `send_file_serial.py`), is served
only at the prompt (not while a tape runs) and not while a service owns the NIC. Verified on the Pi: discovered as
`fos-b12e7b`, 542824-byte image pushed with `--delay 0.002`, ACK received, board restarted. The first kernel with this
still has to arrive by serial KUP.

## Audio tapes on the Pi (2026-10-20)

`play`, `record`, `recorder` and `ltas` build unchanged for aarch64 (`A64_TAPES`) and run on the Pi with the USB audio interface
(2 out, 6 in, 24-bit, 48 kHz). Verified on the Pi 4: `record` took 10 s of 6 channels into a FOSFS file on the SD card (8.4 MiB/s
while writing, slowest block 69 ms, 0 transfer errors, 0 overruns); `play` played it back (the 2400 underruns it prints are the
300 ms of silence it waits out after the ring is empty, the same on x86); `ltas` runs its analysis at 814 us per 8192-point FFT
with 0 overruns; `recorder` shows its panel. Not checked by ear or eye (the user was away from the Pi), and no QEMU test (QEMU's
`usb-audio` is output-only and there is no HDA). The Pi shell has no `help NAME` / `NAME help` yet (x86 prints the tape's
`.fos_help` section); `run record help` there starts a recording called help.wav.

## Shell parity, `help NAME`, `grow`, and the ESP as `esp/` (2026-10-20)

The Pi shell gained `write`, `rm`, `sleep`, `clear`, `grow diskN`, `debug`, `help NAME` (and `run NAME help`: the tape's `.fos_help`
section, read without running it) and the x86 spellings `memory`, `irqstat`, `copy`, `reboot`. **`esp`** names the boot disk's
FAT32 EFI System Partition on both kernels (`fatvol::mount_esp`, the same lookup `bootcode` uses): `ls esp`, `cat esp/config.txt`,
`write esp/NAME TEXT`, `rm esp/NAME`, `cp`/`copy` to and from FOSFS, and a tape's `fs_read/fs_write/fs_list` with `esp/NAME` (so
`run ed esp/config.txt` edits the Pi's `config.txt`). Root directory only (the FAT driver does not enter `EFI/` or `overlays/`).
Verified: `make test-aarch64-boot`, `make test-fat-shell`; on the Pi `ls esp` lists the firmware files and `cat esp/config.txt`
shows `config.txt` (CRLF line ends print as dots). A bad edit of `config.txt` or the firmware files can stop the Pi booting; keep
the SD card's image to hand.

## I2S audio on the Pi: the Maltex HAT (2026-10-22, verified on the Pi)

`kernel/src/i2s_pi.rs` drives the BCM2711 PCM block as I2S *slave* (the HAT is master: 48 kHz, 2 ch, 24-bit left-justified in
32-bit slots) with legacy DMA channel 5 (RX) and 4 (TX) over 64 KiB rings in the non-cacheable pool; header comment has the
register and polarity facts (this card needs the frame clock inverted). It is the audio facade's third backend
(`Backend::I2s`, `AUDIO_KIND_I2S`), so `audio`, `audio capture|tone|duplex`, `play`, `record`, `gonio` and `audio-net` see it
like USB audio. Nothing probes the HAT: switch it on with `i2s audio on` or `i2s-audio` in `settings`; USB audio still wins the
automatic choice. `audio use capture 1|2` drives GPIO16 (AES3 / analog; the card's jumper 4-5). `i2s probe` and `i2s capture`
are diagnostics. Verified on the Pi: capture 48000 frames/s with no drops, analog noise floor about -84 dBFS, playback and
duplex run at 48000 frames/s with no underruns. Only hardware with a signal shows: channel order and level of real audio, AES3
(GPIO13 is the lock indicator, not read yet), and that playback is *heard* (jumper 8-9 hardwires the card's output to its
input; remove it or use 7-8 so GPIO26 chooses). The position is polled (no interrupts), so a reader or writer must call in at
least every 170 ms. Open: IRQ-driven position, GPIO13 lock status in `audio`, `audio-net` on the Pi with the HAT.

## USB serial, `fterm` and the network tapes (2026-10-23, verified on the Pi)

The shared `usb_serial` layer (PL2303, TI) runs on the Pi: xHCI enumeration in `usb_kbd.rs` offers each device to it, `ehci_none.rs`
has the stubs it needs, and the tape ABI `serial_*` calls number the adapters from 1 (no 16550 UARTs; the PL011 is the console).
`peer_info`, `hostname`, `nic_active` (one NIC, so a service's pinned NIC or 0) and `disk_format_exfat` are real now. `fterm`
(serial list, term net to rack1), `net-chat` (both ways, discovery), `ip-chat` (DHCP, UDP both ways) and `telnet` (DHCP, TCP to a host
listener) are in `A64_TAPES` and ran on the Pi. Not verified: data through the PL2303 cable. Still stubs: the NIC extras genet lacks
(PTP stamps, multicast filter, VLAN filter, TX offload, counters). `nettest`, `bridge` and `router` build and start (in `A64_TAPES`): bridge and router refuse to run with one NIC (the Pi has only genet; no USB Ethernet driver), so their forwarding is untested on the Pi. `nettest serve=1` on the Pi answered a master on rack1 (round trip 133 us) and a soak showed 0.9-2.7% loss on frames the Pi sent: a bit-error rate on the wire (loss grew with frame size; a broadcast generator seen by a host sniffer lost single frames), because the MAC's RGMII TX clock delay (`ID_MODE_DIS`) was off while the PHY's is off too. With the MAC delay on, 20000 frames at 100 Mbit/s all arrived. Still to port: `audio-net`.

### `audio-net` (2026-10-23)

`audio-net` runs on the Pi. libopus is cross-built by `make opus-lib-aarch64` (clang, `--target=aarch64-unknown-none-elf`) against
the tiny freestanding headers in `crates/opus-codec/libc-a64/` (math maps to `__builtin_*`; `acos` and friends are declared only for
code the linker drops). `duplex` has an aarch64 `duplex_switch` (x19-x30, d8-d15). Verified: `send proto=rtp codec=opus src=tone=440`
encodes at about 335 us per 20 ms frame (686 us worst) and announces via SAP. Not verified: receiving it elsewhere, I2S input and
playback with a real signal, `duplex`, hardware timestamps (genet has none).

`opus-bench` runs on the Pi too (same libopus archive): 100 stereo 20 ms frames at complexity 10, 96 kbit/s: encode 565 us average
(2.8 % of a core), decode 137 us. That is every general-purpose tape; `memtest`, `fpcheck`, `cpu64-*` and `longmode-rs` are x86-only.

## Debug file, crash record and watchdog (2026-10-23, verified on the Pi)

`kernel/src/crash.rs` is the Pi's `debug.rs`/`config.rs` half: the `debug` file (`reboot-on-crash`, `reboot-delay N`,
`crash-on-boot`, `watchdog [N]`, `kernel-update`), `debug` status, the crash-loop guard (the loader's crash count) and the
watchdog. Differences from x86:

- **The crash record is a file**, `crash` on the boot volume, printed and deleted by the next boot. A RAM page was tried first and
  the Pi's firmware clears RAM across a reset. The handler skips the write if the fault hit inside `block::read/write`
  (`crash::disk_busy`), so it cannot damage the file system, and puts the SD driver in poll-only mode (`sdhci::force_polling`),
  because inside the timer interrupt nothing can wake a `wfi`.
- **The watchdog has two halves**: the 100 Hz tick checks the foreground's `crash::pet()` (shell loop, `block::*`, `netfs`, tape
  `should_abort`; ten minutes while a tape runs) and records the interrupted ELR; behind it the BCM2711 PM watchdog (15 s, fed by
  the tick once a second) resets a hang with interrupts masked, without a record.
- The unclean-boot streak (three in a row keeps the watchdog off) is cleared after five minutes of uptime, as on x86.

Verified: `debug crash` and `debug hang` with `reboot-on-crash` + `watchdog 10`: the record appears at the next boot, the Pi
resets, and the streak guard and its clearing work. Not verified: the hardware-only half (a hang with interrupts masked) and
`crash-on-boot` as a crash loop.

## bootcode on the Pi (2026-10-23, verified on the Pi)

`bootcode` (`kernel/src/bootcode.rs`) is shared with x86; on aarch64 only the UEFI-loader half exists. It replaces
`\EFI\BOOT\BOOTAA64.EFI` from `loader.efi` on the boot volume (`make loader-aarch64`, then
`foscopy.py FILE :disk1/loader.efi`), checking PE machine 0xAA64 and the loader banner, writing `BOOTAA64.NEW` beside it and
switching with one directory-sector write. Verified: show, update + confirm, readback identical, reboot on the new loader.

## I2C on the Pi (2026-10-23, verified on the Pi without a device)

`kernel/src/i2c.rs`: BSC1 (GPIO2 SDA / GPIO3 SCL, alt0) as a master, shell `i2c [status|speed KHZ|scan|read|write|get]`. The
divider comes from the firmware's core clock (mailbox id 4, 500 MHz). Waits spin 30 us, then `wfi` with the BSC interrupt (GIC SPI 117)
armed, as the SD driver does. Tape ABI 23 (`i2c_speed`, `i2c_transfer`, `tape_rt::i2c_*`; x86 answers -4) and the tape `i2cscan`
(verified on the Pi: scan at 100 and 400 kHz, the speed returns to 100 kHz when the tape ends). No repeated START yet (`get` is write, STOP, read). Verified with an empty bus: a scan takes 26/13/3/1 ms
at 50/100/400/1000 kHz (nine clocks per probe, so the clock and divider are right) and one interrupt per probe. A NACK can raise
DONE before ERR, and the next transfer needs TA to fall first; both bit at 400 kHz. Not verified: a real device, clock stretching,
transfers over 16 bytes (FIFO refill), a tape against a real device.

Target: the DFRobot ~4.5" DSI panel (chips seen: ICN6211 DSI-to-RGB bridge, GD32V MCU, AEP7510, AJ2004). The ICN6211 is set up over
I2C (usually 0x2c; Linux `chipone-icn6211.c`), but the Pi's DSI1 controller also needs a driver.

## install on the Pi (2026-10-23, built, not yet run on a second disk)

`install diskN [LABEL] [bare] [force]` + `install diskN confirm` (`kernel/src/install.rs`, shared with x86) clones the boot disk onto
another disk: new GPT, the ESP sector for sector (on the Pi it holds the firmware files, `config.txt`, `start4.elf`, `RPI_EFI.fd` and
`BOOTAA64.EFI`), the running kernel as slot 0 with fresh metadata, and a FOSFS volume with every file. No BIOS code on aarch64.
Verified: the command is registered, and it refuses the boot disk. Not verified: an actual copy (needs a USB disk or second SD in the
Pi; USB boot is not enabled in the Pi's EEPROM yet, so a copy on USB can only be checked by reading it back or booting it from SD later).

## USB-MIDI on the Pi

`usb_midi.rs` is shared with x86 (xHCI only, also behind a hub). The Pi's `usb_kbd.rs` now attaches MIDI like
`usb_hid.rs` does (audio first, then MIDI on the same configuration), releases it with the port and routes its
events. The `midi` command (`midi`, `midi monitor`, `midi send [#N] HEX..`) moved from `term.rs` to the shared
`midishell.rs`. Verified: both kernels build, the Pi shows "no USB MIDI device" and refuses a send. Not verified: a
real MIDI device (none attached yet).

## Touch screens, USB and DSI (2026-10-23)

Shell `touch` lists the panels, `touch monitor` prints finger events (down/moved, up) until Escape; tapes read them with ABI 24
`touch_info` / `touch_read` (`tape_rt::touch_event`, `touch_extent`); `kaffe` presses its buttons with them. One event queue
(`kernel/src/usb_pointer.rs`), two sources:

- **USB touch screens** (both kernels, xHCI): a HID digitizer's report descriptor is parsed by `crates/hid` (`parse_pointer`,
  `Pointer::decode`), its interrupt-IN endpoint kept queued like a keyboard's. Checked with an ELAN 04f3:249f (10 contacts,
  3312 x 1872) on the Pi's hub; that panel declares X and Y twice per finger, so a contact starts at the finger collection, not at
  a repeated field. Coordinates are the panel's own; `touch_read` reports the tape screen a panel sits on, -1 for a USB panel
  (any monitor), so a tape takes it as covering its screen.
- **The DSI display's chip** (Pi only, `kernel/src/dsitouch.rs`): an FT5406-style controller at 0x38 on the display connector's
  I2C bus, BSC0 on GPIO44/45 alt1 (`i2c bus dsi` selects it in the shell; `i2c scan` there shows 0x38, and 0x45 for the
  panel's own controller, as on the official 7" screen). No interrupt line is used: the chip is read (400 kHz, at most every 8 ms)
  whenever something asks for an event. 800 x 480 on the DFRobot 4.5"; it is tape screen 0. The tape i2c calls stay on the header bus.

Verified on the Pi: both panels' events in `touch monitor`, `kaffe` buttons on the USB panel. Not verified: two fingers, `kaffe` on
the DSI screen, an x86 USB touch screen.

Rotation (2026-10-23): a single tape takes `rotate=r|l|flip` (`run kaffe screen=0 rotate=l`; `f` and `180` also mean flip),
and touch follows the rotation of the screen it belongs to (`usb_pointer::upright`; the shell `touch monitor` stays raw).
Checked on the Pi's DSI screen with `kaffe`; an x86 run and a USB panel on a rotated HDMI screen are untested.
