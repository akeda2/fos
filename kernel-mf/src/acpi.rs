//! The ACPI tables the server kernel reads: only the static ones, no AML. RSDP -> XSDT (or RSDT) -> tables by signature.
//!
//! Used: the FADT (the power-management timer, which calibrates the APIC timer), the MADT (local APICs: which CPUs exist,
//! and where the local and I/O APIC registers are) and the SRAT (which CPUs and memory ranges belong to which NUMA node).
//! Table pages the memory map does not describe (a BIOS often calls them plain reserved memory, and the RSDP sits in the
//! BIOS ROM area) are mapped on demand, so the reader does not depend on how the firmware labelled them.

use crate::vm;

fn read<T: Copy>(address: u64) -> T {
    vm::map_device(address, core::mem::size_of::<T>() as u64);
    unsafe { (vm::direct(address) as *const T).read_unaligned() }
}

fn checksum_ok(address: u64, length: usize) -> bool {
    (0..length).fold(0u8, |sum, offset| sum.wrapping_add(read::<u8>(address + offset as u64))) == 0
}

pub struct Tables {
    root: u64,
    /// Entry size in the root table: 8 for the XSDT, 4 for the RSDT.
    entry_size: u64,
    pub revision: u8,
}

pub fn open(rsdp: u64) -> Option<Tables> {
    if rsdp == 0 || read::<u64>(rsdp) != u64::from_le_bytes(*b"RSD PTR ") || !checksum_ok(rsdp, 20) {
        return None;
    }
    let revision = read::<u8>(rsdp + 15);
    if revision >= 2 && read::<u64>(rsdp + 24) != 0 {
        Some(Tables { root: read::<u64>(rsdp + 24), entry_size: 8, revision })
    } else {
        Some(Tables { root: u64::from(read::<u32>(rsdp + 16)), entry_size: 4, revision })
    }
}

impl Tables {
    /// Address of the first table with this signature.
    pub fn find(&self, signature: &[u8; 4]) -> Option<u64> {
        let length = u64::from(read::<u32>(self.root + 4));
        for index in 0..(length.saturating_sub(36)) / self.entry_size {
            let at = self.root + 36 + index * self.entry_size;
            let table = if self.entry_size == 8 { read::<u64>(at) } else { u64::from(read::<u32>(at)) };
            if table != 0 && read::<[u8; 4]>(table) == *signature && checksum_ok(table, read::<u32>(table + 4) as usize) {
                return Some(table);
            }
        }
        None
    }
}

pub struct Fadt {
    /// I/O port of the PM timer (3.579545 MHz), 0 if the chipset has none.
    pub pm_timer_port: u16,
    pub pm_timer_32_bit: bool,
}

pub fn fadt(tables: &Tables) -> Option<Fadt> {
    let table = tables.find(b"FACP")?;
    Some(Fadt { pm_timer_port: read::<u32>(table + 76) as u16, pm_timer_32_bit: read::<u32>(table + 112) & (1 << 8) != 0 })
}

pub const MAX_CPUS: usize = 256;

pub struct Madt {
    pub local_apic_address: u64,
    /// APIC ids of the usable processors (enabled, or able to be enabled), in table order.
    pub cpus: [u32; MAX_CPUS],
    pub cpu_count: usize,
    /// The first I/O APIC: its register window and the first interrupt (GSI) it serves.
    pub io_apics: u32,
    pub io_apic_address: u64,
    pub io_apic_gsi_base: u32,
}

pub fn madt(tables: &Tables) -> Option<Madt> {
    let table = tables.find(b"APIC")?;
    let length = u64::from(read::<u32>(table + 4));
    let mut result = Madt { local_apic_address: u64::from(read::<u32>(table + 36)), cpus: [0; MAX_CPUS], cpu_count: 0, io_apics: 0, io_apic_address: 0, io_apic_gsi_base: 0 };
    let mut at = table + 44;
    while at + 2 <= table + length {
        let (kind, size) = (read::<u8>(at), u64::from(read::<u8>(at + 1)));
        if size < 2 {
            break;
        }
        // Local APIC (0): flags bit 0 enabled, bit 1 online-capable. x2APIC (9): the same flags, a 32-bit id.
        let cpu = match kind {
            0 if read::<u32>(at + 4) & 3 != 0 => Some(u32::from(read::<u8>(at + 3))),
            9 if read::<u32>(at + 8) & 3 != 0 => Some(read::<u32>(at + 4)),
            _ => None,
        };
        if let Some(id) = cpu.filter(|_| result.cpu_count < MAX_CPUS) {
            result.cpus[result.cpu_count] = id;
            result.cpu_count += 1;
        }
        match kind {
            1 => {
                if result.io_apics == 0 {
                    result.io_apic_address = u64::from(read::<u32>(at + 4));
                    result.io_apic_gsi_base = read::<u32>(at + 8);
                }
                result.io_apics += 1;
            }
            5 => result.local_apic_address = read::<u64>(at + 4),
            _ => {}
        }
        at += size;
    }
    Some(result)
}

pub const MAX_RANGES: usize = 64;

/// The NUMA topology the SRAT describes.
pub struct Srat {
    /// Physical memory ranges: (start, length, node).
    pub memory: [(u64, u64, u32); MAX_RANGES],
    pub memory_count: usize,
    /// Distinct proximity domains seen (CPUs and memory together).
    pub nodes: usize,
    pub cpu_affinities: usize,
}

pub fn srat(tables: &Tables) -> Option<Srat> {
    let table = tables.find(b"SRAT")?;
    let length = u64::from(read::<u32>(table + 4));
    let mut result = Srat { memory: [(0, 0, 0); MAX_RANGES], memory_count: 0, nodes: 0, cpu_affinities: 0 };
    let mut seen = [u32::MAX; 64];
    let mut note = |node: u32, result: &mut Srat| {
        if !seen[..result.nodes].contains(&node) && result.nodes < seen.len() {
            seen[result.nodes] = node;
            result.nodes += 1;
        }
    };
    let mut at = table + 48;
    while at + 2 <= table + length {
        let (kind, size) = (read::<u8>(at), u64::from(read::<u8>(at + 1)));
        if size < 2 {
            break;
        }
        match kind {
            // Processor local APIC affinity: domain low byte at +2, flags at +4, domain bytes 1-3 at +9.
            0 if read::<u32>(at + 4) & 1 != 0 => {
                let node = u32::from(read::<u8>(at + 2)) | (read::<u8>(at + 9) as u32) << 8 | (read::<u8>(at + 10) as u32) << 16 | (read::<u8>(at + 11) as u32) << 24;
                note(node, &mut result);
                result.cpu_affinities += 1;
            }
            // Memory affinity: domain at +2, base at +8, length at +16, flags at +28 (bit 0 enabled).
            1 if read::<u32>(at + 28) & 1 != 0 => {
                let node = read::<u32>(at + 2);
                note(node, &mut result);
                if result.memory_count < MAX_RANGES {
                    result.memory[result.memory_count] = (read::<u64>(at + 8), read::<u64>(at + 16), node);
                    result.memory_count += 1;
                }
            }
            // Processor x2APIC affinity: domain at +4, flags at +12.
            2 if read::<u32>(at + 12) & 1 != 0 => {
                note(read::<u32>(at + 4), &mut result);
                result.cpu_affinities += 1;
            }
            _ => {}
        }
        at += size;
    }
    Some(result)
}
