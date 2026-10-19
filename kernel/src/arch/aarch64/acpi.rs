//! Just enough ACPI to find the hardware a board does not let us hard-code: the console UART (SPCR), the interrupt
//! controller (MADT) and the PCI config window (MCFG). edk2 provides these tables on QEMU `virt` and on the Pi 4, and they
//! are plain memory the firmware's identity map still covers. Everything is read through the XSDT; the tables sit in
//! memory the kernel maps as RAM (ACPI reclaim/NVS), and none is modified.

fn read<T: Copy>(address: u64, offset: u64) -> T {
    unsafe { core::ptr::read_unaligned((address + offset) as *const T) }
}

/// The physical address of the first table with `signature`, found through the RSDP's XSDT.
pub fn find_table(rsdp: u64, signature: &[u8; 4]) -> Option<u64> {
    if rsdp == 0 || read::<[u8; 8]>(rsdp, 0) != *b"RSD PTR " || read::<u8>(rsdp, 15) < 2 {
        return None;
    }
    let xsdt: u64 = read(rsdp, 24);
    let length: u32 = read(xsdt, 4);
    (0..u64::from(length).saturating_sub(36) / 8).map(|entry| read::<u64>(xsdt, 36 + entry * 8)).find(|&table| read::<[u8; 4]>(table, 0) == *signature)
}

fn table_length(table: u64) -> u64 {
    u64::from(read::<u32>(table, 4))
}

/// The ECAM base of the first MCFG allocation.
pub fn mcfg_base(rsdp: u64) -> Option<u64> {
    let table = find_table(rsdp, b"MCFG")?;
    (table_length(table) >= 60).then(|| read(table, 44))
}

/// The serial console from SPCR: its register base, when it is a memory-mapped PL011 or SBSA generic UART (interface
/// types 3, 0x0d and 0x0e; the SBSA UART is a PL011 subset that transmit and receive treat the same).
pub fn spcr_uart(rsdp: u64) -> Option<u64> {
    let table = find_table(rsdp, b"SPCR")?;
    if table_length(table) < 52 {
        return None;
    }
    let (interface, space): (u8, u8) = (read(table, 36), read(table, 40));
    let address: u64 = read(table, 44);
    (space == 0 && address != 0 && matches!(interface, 0x03 | 0x0d | 0x0e)).then_some(address)
}

/// The GICv2 bases from MADT: (distributor, CPU interface). Entry type 0xC is the distributor (base at +8, GIC version
/// at +20), type 0xB a CPU interface (physical base at +32).
pub fn madt_gic(rsdp: u64) -> Option<(u64, u64)> {
    let table = find_table(rsdp, b"APIC")?;
    let end = table_length(table);
    let (mut distributor, mut cpu_interface) = (None, None);
    let mut offset = 44;
    while offset + 2 <= end {
        let (kind, length): (u8, u8) = (read(table, offset), read(table, offset + 1));
        if length == 0 {
            break;
        }
        match kind {
            0x0c if read::<u8>(table, offset + 20) <= 2 => distributor = distributor.or(Some(read::<u64>(table, offset + 8))),
            0x0b if cpu_interface.is_none() && read::<u64>(table, offset + 32) != 0 => cpu_interface = Some(read::<u64>(table, offset + 32)),
            _ => {}
        }
        offset += u64::from(length);
    }
    Some((distributor?, cpu_interface?))
}

static mut RSDP: u64 = 0;

/// Remembers the RSDP for the shell's `acpi` command.
pub fn remember(rsdp: u64) {
    unsafe { RSDP = rsdp };
}

/// Calls `each(signature, address, length)` for every table in the XSDT.
pub fn list(mut each: impl FnMut([u8; 4], u64, u64)) {
    let rsdp = unsafe { RSDP };
    if rsdp == 0 || read::<[u8; 8]>(rsdp, 0) != *b"RSD PTR " || read::<u8>(rsdp, 15) < 2 {
        return;
    }
    let xsdt: u64 = read(rsdp, 24);
    let length: u32 = read(xsdt, 4);
    for entry in 0..u64::from(length).saturating_sub(36) / 8 {
        let table: u64 = read(xsdt, 36 + entry * 8);
        each(read(table, 0), table, table_length(table));
    }
}
