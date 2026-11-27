//! System management interrupts (SMIs) on Intel chipsets: which sources the firmware enabled, and stopping the one that
//! is no use to fos.
//!
//! An SMI runs firmware in system management mode with every CPU stopped. The kernel cannot see it happen and cannot
//! make it shorter. On rack (2026-11-27) the firmware's software SMI timer fired 60 times a second. Each SMI added about
//! 170 µs to whatever interrupt it hit, and now and then a 1-2 ms stall lost an audio ring. Clearing that one enable bit
//! stopped them, and the board's BIOS had no option for it (`settings`: `smi-timer off`, docs/config.md).
//!
//! Registers (Intel ICH and PCH datasheets, the same from the ICH9 to the 100-series and later): the ACPI I/O block at
//! PMBASE holds `SMI_EN` at +0x30 and `SMI_STS` at +0x34. PMBASE is where PM1a's event block starts, so it is taken from
//! the FADT (`PM1a_EVT_BLK`) instead of from chipset-specific PCI registers. Those registers are only written after the
//! LPC/eSPI bridge at 00:1f.0 answers as an Intel ISA bridge (class 06/01), so no other vendor's ports are touched.
//!
//! The firmware owns `SMI_EN`. Bit 0 (`GBL_SMI_EN`, every SMI) is never cleared here. The firmware may also lock it.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Offsets in the ACPI I/O block.
pub const SMI_EN: u16 = 0x30;
pub const SMI_STS: u16 = 0x34;
/// `SWSMI_TMR_EN`: a periodic SMI the firmware re-arms by itself (1.5 to 64 ms; about 16 ms on rack). Firmware uses it
/// to poll USB for legacy keyboard emulation; fos takes the USB controllers over at boot, so nothing it needs runs on it.
pub const SWSMI_TMR_BIT: u32 = 6;

/// The FADT's PM1a event block, from `init` (0: none). Bit 16 is set once the chipset was checked, bit 17 if it is Intel's.
static PMBASE: AtomicU32 = AtomicU32::new(0);
const CHECKED: u32 = 1 << 16;
const INTEL: u32 = 1 << 17;
/// `SMI_EN` as the firmware left it, saved before the first change (`restore`); `u64::MAX` until then.
static FIRMWARE: AtomicU64 = AtomicU64::new(u64::MAX);

/// True for an Intel ISA bridge: the vendor/device dword and the class dword of PCI function 00:1f.0.
pub fn is_intel_lpc(id: u32, class: u32) -> bool {
    id & 0xffff == 0x8086 && class >> 16 == 0x0601
}

/// `value` with `bit` set or cleared.
pub fn with_bit(value: u32, bit: u32, on: bool) -> u32 {
    if on { value | 1 << bit } else { value & !(1 << bit) }
}

/// Notes the FADT's PM1a event block, where the chipset's SMI registers are if it is Intel's. Call when ACPI is read.
/// The chipset itself is checked at first use: PCI configuration access may not be set up yet at this point.
pub fn init(pm1a_event: u16) {
    PMBASE.store(u32::from(pm1a_event), Ordering::Relaxed);
}

/// PMBASE, if the FADT gave one and 00:1f.0 is an Intel ISA bridge.
fn base() -> Option<u16> {
    let mut state = PMBASE.load(Ordering::Relaxed);
    if state & CHECKED == 0 {
        let intel = state != 0 && is_intel_lpc(crate::pci::read32(0, 0x1f, 0, 0), crate::pci::read32(0, 0x1f, 0, 8));
        state |= CHECKED | if intel { INTEL } else { 0 };
        PMBASE.store(state, Ordering::Relaxed);
    }
    (state & INTEL != 0).then_some(state as u16)
}

/// `SMI_EN` and `SMI_STS`, or `None` without an Intel chipset. Status bits stay set until the firmware clears them.
pub fn sources() -> Option<(u32, u32)> {
    let base = base()?;
    Some((crate::cpu::inl(base + SMI_EN), crate::cpu::inl(base + SMI_STS)))
}

/// Sets or clears one `SMI_EN` bit (1-31) and returns the register before and after, read back: a bit the firmware
/// locked comes back unchanged. `None` without an Intel chipset or for bit 0.
pub fn set_bit(bit: u32, on: bool) -> Option<(u32, u32)> {
    let base = base()?;
    if bit == 0 || bit > 31 {
        return None;
    }
    let port = base + SMI_EN;
    let before = crate::cpu::inl(port);
    let _ = FIRMWARE.compare_exchange(u64::MAX, u64::from(before), Ordering::Relaxed, Ordering::Relaxed);
    crate::cpu::outl(port, with_bit(before, bit, on));
    Some((before, crate::cpu::inl(port)))
}

/// Writes back the `SMI_EN` saved before the first change. `None` if nothing was changed.
pub fn restore() -> Option<(u32, u32)> {
    let saved = FIRMWARE.load(Ordering::Relaxed);
    let base = base()?;
    if saved == u64::MAX {
        return None;
    }
    let port = base + SMI_EN;
    let before = crate::cpu::inl(port);
    crate::cpu::outl(port, saved as u32);
    Some((before, crate::cpu::inl(port)))
}

/// `smi-timer off` in `settings`: stops the software SMI timer and says what happened, in lines of at most 79 columns.
pub fn apply_timer_off() {
    match set_bit(SWSMI_TMR_BIT, false) {
        Some((before, after)) if after & 1 << SWSMI_TMR_BIT == 0 => {
            let was = if before & 1 << SWSMI_TMR_BIT != 0 { "stopped" } else { "was already off" };
            crate::println!("settings: smi-timer off -- the chipset's software SMI timer {was}");
            crate::println!("  (SMI_EN {before:#010x} -> {after:#010x}): no periodic firmware stalls.");
        }
        Some((before, _)) => {
            crate::println!("settings: smi-timer off -- the firmware keeps the software SMI timer on");
            crate::println!("  (SMI_EN {before:#010x}: the bit did not change).");
        }
        None => crate::println!("settings: smi-timer off -- no Intel chipset SMI registers; nothing changed."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_intel_isa_bridge_counts() {
        // A 100-series LPC and a 6-series one (Sandy Bridge): class 06/01, any device id and revision.
        assert!(is_intel_lpc(0xa145_8086, 0x0601_0031));
        assert!(is_intel_lpc(0x1c44_8086, 0x0601_0005));
        // An AMD FCH's LPC bridge, an Intel host bridge at the wrong class, an empty slot.
        assert!(!is_intel_lpc(0x790e_1022, 0x0601_0051));
        assert!(!is_intel_lpc(0x1918_8086, 0x0600_0007));
        assert!(!is_intel_lpc(0xffff_ffff, 0xffff_ffff));
    }

    #[test]
    fn the_timer_bit_comes_off_and_back() {
        // rack's firmware value: bit 6 set; clearing it gives what `debug smi off 6` read back on the machine.
        assert_eq!(with_bit(0x9000_207b, SWSMI_TMR_BIT, false), 0x9000_203b);
        assert_eq!(with_bit(0x9000_203b, SWSMI_TMR_BIT, true), 0x9000_207b);
    }
}
