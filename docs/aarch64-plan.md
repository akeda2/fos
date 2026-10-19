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

- **M0 - arch split (first step done 2026-10-19: the x86-only modules moved to `kernel/src/arch/x86_64/` and re-exported; boot tests pass; PCI port I/O, `uart.rs` and the interface traits are still to do).** Move x86 code under `kernel/src/arch/x86_64/` behind a thin interface (`irq`, `timer`, `mmu`,
  `serial`, `barrier`, `pci config`). No behaviour change; `make test` must stay green. This is the risky refactor, do it first.
- **M1 - core on QEMU virt.** `aarch64-fos-kernel` target, entry from the UEFI loader, EL1, MMU with identity map (kernel text RO/X, rest
  NX, device memory attribute for MMIO), exception vectors, GICv3, generic timer (`micros()` from `CNTVCT`), PL011 console, framebuffer
  from GOP. Shell runs; `hello` tape runs.
- **M2 - tape ABI on aarch64.** `targets/aarch64-fos-tape.json` (hard-float), shared `tapes/tape.ld`, tape ELF loader checks `e_machine`,
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

## Pi 5 (later)

Ethernet, USB and GPIO sit on the RP1 chip behind a PCIe link: a different NIC (Cadence GEM), a different xHCI, a different
interrupt tree. Most of M0-M3 carries over; M4 does not. Treat it as its own milestone after the Pi 4 works.

## Decisions taken (2026-10-19)

- Target: Pi 4; UEFI-first (edk2 + our loader), raw `kernel8.img` only as a fallback.
- Order: QEMU `virt` first, then the Pi.
- A 3.3 V USB serial adapter is available for early bring-up; the SD card is still to be found.
