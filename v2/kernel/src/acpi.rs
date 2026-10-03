//! The ACPI tables the kernel reads: only the static ones, no AML (design note,
//! section 6). Found through the RSDP the loader passes: RSDP -> XSDT (or the old
//! RSDT) -> tables by signature.
//!
//! M2 uses two of them: the FADT, for the ACPI power-management timer (a 3.579545
//! MHz counter that ticks the same on every chipset, which is what calibrates the
//! local APIC timer) and the MADT, for the local APIC's address and, later, the
//! I/O APICs. The tables live in ACPI-reclaimable or NVS memory, which the page
//! tables map.

fn read_u8(address: u64) -> u8 {
    unsafe { (address as *const u8).read_unaligned() }
}
fn read_u16(address: u64) -> u16 {
    unsafe { (address as *const u16).read_unaligned() }
}
fn read_u32(address: u64) -> u32 {
    unsafe { (address as *const u32).read_unaligned() }
}
fn read_u64(address: u64) -> u64 {
    unsafe { (address as *const u64).read_unaligned() }
}

/// Whether the byte sum of `[address, address + length)` is zero, which is how
/// every ACPI table checksums itself.
fn checksum_ok(address: u64, length: usize) -> bool {
    (0..length).fold(0u8, |sum, offset| sum.wrapping_add(read_u8(address + offset as u64))) == 0
}

pub struct Tables {
    root: u64,
    /// Entry size in the root table: 8 for the XSDT, 4 for the RSDT.
    entry_size: u64,
    pub revision: u8,
}

/// Validates the RSDP at `rsdp` and locates the root table.
pub fn open(rsdp: u64) -> Option<Tables> {
    if rsdp == 0 || unsafe { core::slice::from_raw_parts(rsdp as *const u8, 8) } != b"RSD PTR " || !checksum_ok(rsdp, 20) {
        return None;
    }
    let revision = read_u8(rsdp + 15);
    if revision >= 2 && read_u64(rsdp + 24) != 0 {
        Some(Tables { root: read_u64(rsdp + 24), entry_size: 8, revision })
    } else {
        Some(Tables { root: u64::from(read_u32(rsdp + 16)), entry_size: 4, revision })
    }
}

impl Tables {
    /// Address of the first table with this 4-character signature.
    pub fn find(&self, signature: &[u8; 4]) -> Option<u64> {
        let length = u64::from(read_u32(self.root + 4));
        let entries = (length - 36) / self.entry_size;
        for index in 0..entries {
            let at = self.root + 36 + index * self.entry_size;
            let table = if self.entry_size == 8 { read_u64(at) } else { u64::from(read_u32(at)) };
            if table != 0 && unsafe { core::slice::from_raw_parts(table as *const u8, 4) } == signature && checksum_ok(table, read_u32(table + 4) as usize) {
                return Some(table);
            }
        }
        None
    }
}

/// What the kernel takes from the FADT.
pub struct Fadt {
    /// I/O port of the 24- or 32-bit power-management timer, 0 if the chipset has none.
    pub pm_timer_port: u16,
    pub pm_timer_32_bit: bool,
    pub pm1a_control: u16,
    pub sci_interrupt: u16,
}

pub fn fadt(tables: &Tables) -> Option<Fadt> {
    let table = tables.find(b"FACP")?;
    let flags = read_u32(table + 112);
    Some(Fadt {
        pm_timer_port: read_u32(table + 76) as u16,
        pm_timer_32_bit: flags & (1 << 8) != 0,
        pm1a_control: read_u32(table + 64) as u16,
        sci_interrupt: read_u16(table + 46),
    })
}

/// What the kernel takes from the MADT.
pub struct Madt {
    pub local_apic_address: u64,
    /// Enabled processors seen (not used to start them: single core for now).
    pub processors: u32,
    pub io_apics: u32,
    /// The first I/O APIC: its register window and the first interrupt (GSI) it serves.
    pub io_apic_address: u64,
    pub io_apic_gsi_base: u32,
    /// ISA interrupt source overrides: (ISA irq, GSI, MPS INTI flags).
    pub overrides: [(u8, u32, u16); 16],
    pub override_count: usize,
}

pub fn madt(tables: &Tables) -> Option<Madt> {
    let table = tables.find(b"APIC")?;
    let length = u64::from(read_u32(table + 4));
    let mut result = Madt { local_apic_address: u64::from(read_u32(table + 36)), processors: 0, io_apics: 0, io_apic_address: 0, io_apic_gsi_base: 0, overrides: [(0, 0, 0); 16], override_count: 0 };
    let mut at = table + 44;
    while at + 2 <= table + length {
        let (kind, size) = (read_u8(at), u64::from(read_u8(at + 1)));
        if size < 2 {
            break;
        }
        match kind {
            0 if read_u32(at + 4) & 1 != 0 => result.processors += 1, // local APIC, enabled
            1 => {
                if result.io_apics == 0 {
                    result.io_apic_address = u64::from(read_u32(at + 4));
                    result.io_apic_gsi_base = read_u32(at + 8);
                }
                result.io_apics += 1;
            }
            2 if result.override_count < 16 => {
                result.overrides[result.override_count] = (read_u8(at + 3), read_u32(at + 4), read_u16(at + 8));
                result.override_count += 1;
            }
            5 => result.local_apic_address = read_u64(at + 4), // address override
            _ => {}
        }
        at += size;
    }
    Some(result)
}
