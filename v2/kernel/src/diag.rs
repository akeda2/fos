//! Diagnostic shell commands: what the CPU and firmware report, and raw access to PCI
//! configuration space, I/O ports and mapped memory. Real-hardware bugs are found by reading
//! registers, and each diagnostic kernel push costs minutes, so these let the registers be read
//! (and, with care, written) from the shell instead.
//!
//! Nothing here protects the machine from a write that does harm: `pci write`, `io w..` and
//! `mmio w..` do exactly what they are told. Only the reads and writes that would fault are
//! refused (`mmio` needs a mapped address) and the kernel's own image is off limits.

use crate::{cpu, ioapic, paging, pci, tape};

fn number(word: &str) -> Option<u64> {
    match word.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => word.parse().ok(),
    }
}

/// `cpuinfo`: vendor, brand, family/model/stepping, a few feature bits that matter to this
/// kernel, and the invariant-TSC bit that decides how `micros()` is read.
pub fn cpuinfo() {
    let (max_leaf, b, c, d) = cpu::cpuid(0, 0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&b.to_le_bytes());
    vendor[4..8].copy_from_slice(&d.to_le_bytes());
    vendor[8..12].copy_from_slice(&c.to_le_bytes());
    crate::println!("vendor     {} (highest basic CPUID leaf {max_leaf:#x})", core::str::from_utf8(&vendor).unwrap_or("?"));
    let (max_extended, ..) = cpu::cpuid(0x8000_0000, 0);
    if max_extended >= 0x8000_0004 {
        let mut brand = [0u8; 48];
        for (index, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
            let (a, b, c, d) = cpu::cpuid(leaf, 0);
            for (part, register) in [a, b, c, d].into_iter().enumerate() {
                brand[index * 16 + part * 4..][..4].copy_from_slice(&register.to_le_bytes());
            }
        }
        crate::println!("brand      {}", core::str::from_utf8(&brand).unwrap_or("?").trim_matches(|c: char| c == '\0' || c == ' '));
    }
    let (signature, ebx, ecx, edx) = cpu::cpuid(1, 0);
    let (stepping, mut model, mut family) = (signature & 0xf, (signature >> 4) & 0xf, (signature >> 8) & 0xf);
    if family == 0xf {
        family += (signature >> 20) & 0xff;
    }
    if family == 0x6 || family >= 0xf {
        model |= ((signature >> 16) & 0xf) << 4;
    }
    crate::println!("signature  family {family:#x} model {model:#x} stepping {stepping} (CPUID.1 EAX {signature:#010x})");
    crate::println!("logical    {} logical processor(s) per package, APIC id {}", (ebx >> 16) & 0xff, ebx >> 24);
    let (_, _, ext_ecx, ext_edx) = if max_extended >= 0x8000_0001 { cpu::cpuid(0x8000_0001, 0) } else { (0, 0, 0, 0) };
    let (_, leaf7_ebx, ..) = if max_leaf >= 7 { cpu::cpuid(7, 0) } else { (0, 0, 0, 0) };
    let mut features = [
        ("sse2", edx & (1 << 26) != 0),
        ("sse4.2", ecx & (1 << 20) != 0),
        ("aes", ecx & (1 << 25) != 0),
        ("avx", ecx & (1 << 28) != 0),
        ("avx2", leaf7_ebx & (1 << 5) != 0),
        ("rdrand", ecx & (1 << 30) != 0),
        ("x2apic", ecx & (1 << 21) != 0),
        ("apic", edx & (1 << 9) != 0),
        ("pat", edx & (1 << 16) != 0),
        ("nx", ext_edx & (1 << 20) != 0),
        ("1g-pages", ext_edx & (1 << 26) != 0),
        ("long-mode", ext_edx & (1 << 29) != 0),
        ("lahf", ext_ecx & 1 != 0),
        ("hypervisor", ecx & (1 << 31) != 0),
    ]
    .into_iter();
    crate::print(format_args!("features  "));
    for (name, present) in &mut features {
        if present {
            crate::print(format_args!(" {name}"));
        }
    }
    crate::println!();
    let invariant = max_extended >= 0x8000_0007 && cpu::cpuid(0x8000_0007, 0).3 & (1 << 8) != 0;
    crate::println!("tsc        {}", if invariant { "invariant (micros() reads it directly)" } else { "not invariant (micros() uses the PM timer)" });
}

/// `bootinfo`: what the loader handed over.
pub fn bootinfo() {
    let Some(info) = tape::boot_info() else { return crate::println!("No BootInfo recorded.") };
    crate::println!("magic {:#x}, version {}, size {} bytes", info.magic, info.version, info.size);
    crate::println!("kernel   {:#x}, {} KiB, booted from slot {}, crash count {}", info.kernel_base, info.kernel_size / 1024, info.boot_slot, info.crash_count);
    crate::println!("memory   map at {:#x}: {} descriptors of {} bytes", info.memory_map, info.memory_map_count, info.memory_map_descriptor_size);
    crate::println!("acpi     RSDP {:#x}", info.acpi_rsdp);
    crate::println!("runtime  services table {:#x}", info.runtime_services);
    let framebuffer = &info.framebuffer;
    crate::println!("graphics {}x{} stride {} at {:#x}, pixel format {}", framebuffer.width, framebuffer.height, framebuffer.stride, framebuffer.base, framebuffer.format);
    crate::println!("payload  partition start {} sectors {} (the boot disk is found by its GUID)", info.payload_start, info.payload_sectors);
}

fn memory_type(kind: u32) -> &'static str {
    match kind {
        0 => "reserved",
        1 => "loader code",
        2 => "loader data",
        3 => "boot code",
        4 => "boot data",
        5 => "runtime code",
        6 => "runtime data",
        7 => "usable",
        8 => "unusable",
        9 => "ACPI reclaim",
        10 => "ACPI NVS",
        11 => "MMIO",
        12 => "MMIO ports",
        13 => "PAL code",
        14 => "persistent",
        _ => "other",
    }
}

/// `memmap`: the firmware's memory map as the loader passed it (before the kernel merged anything).
pub fn memmap() {
    let Some(info) = tape::boot_info() else { return crate::println!("No BootInfo recorded.") };
    crate::println!("{:>18} {:>10}  type", "physical start", "size");
    let mut usable = 0u64;
    for descriptor in info.memory_descriptors() {
        let bytes = descriptor.pages * 4096;
        if descriptor.kind == 7 {
            usable += bytes;
        }
        let size = if bytes >= 1 << 20 { (bytes >> 20, "MiB") } else { (bytes >> 10, "KiB") };
        crate::println!("{:#018x} {:>6} {}  {}", descriptor.physical_start, size.0, size.1, memory_type(descriptor.kind));
    }
    crate::println!("{} MiB of conventional (usable) memory in the map.", usable >> 20);
}

fn parse_function(word: &str) -> Option<(u8, u8, u8)> {
    // B:D.F, each part hexadecimal (the way `pci` lists them).
    let (bus, rest) = word.split_once(':')?;
    let (device, function) = rest.split_once('.')?;
    Some((u8::from_str_radix(bus, 16).ok()?, u8::from_str_radix(device, 16).ok()?, u8::from_str_radix(function, 16).ok()?))
}

/// `pci read B:D.F [OFFSET [LENGTH]]` dumps configuration space (dwords; default the first 64
/// bytes); `pci write B:D.F OFFSET VALUE` writes one dword. Offsets are multiples of 4 below 256.
pub fn pci_access(mut words: core::str::SplitWhitespace) -> bool {
    let Some(verb) = words.clone().next().filter(|verb| matches!(*verb, "read" | "write")) else { return false };
    words.next();
    let Some((bus, device, function)) = words.next().and_then(parse_function) else {
        crate::println!("Usage: pci read B:D.F [OFFSET [LENGTH]] | pci write B:D.F OFFSET VALUE   (B:D.F in hex, as `pci` lists)");
        return true;
    };
    if pci::read32(bus, device, function, 0) & 0xffff == 0xffff {
        crate::println!("{bus:02x}:{device:02x}.{function}: no device answers there.");
        return true;
    }
    let offset = words.next().and_then(number).unwrap_or(0);
    if offset > 0xfc || offset % 4 != 0 {
        crate::println!("The offset must be a multiple of 4 up to 0xfc.");
        return true;
    }
    if verb == "write" {
        let Some(value) = words.next().and_then(number) else {
            crate::println!("Usage: pci write B:D.F OFFSET VALUE");
            return true;
        };
        pci::write32(bus, device, function, offset as u8, value as u32);
        crate::println!("{bus:02x}:{device:02x}.{function} [{offset:#04x}] <- {:#010x}", value as u32);
        return true;
    }
    let length = words.next().and_then(number).unwrap_or(64).min(256 - offset);
    for at in (offset..offset + length).step_by(16) {
        crate::print(format_args!("{at:#04x}:"));
        for dword in (at..(at + 16).min(offset + length)).step_by(4) {
            crate::print(format_args!(" {:08x}", pci::read32(bus, device, function, dword as u8)));
        }
        crate::println!();
    }
    true
}

/// `io r8|r16|r32 PORT` and `io w8|w16|w32 PORT VALUE`: raw port access.
pub fn io(mut words: core::str::SplitWhitespace) {
    let (Some(verb), Some(port)) = (words.next(), words.next().and_then(number)) else {
        return crate::println!("Usage: io r8|r16|r32 PORT | io w8|w16|w32 PORT VALUE");
    };
    if port > 0xffff {
        return crate::println!("Ports are 16 bits.");
    }
    let port = port as u16;
    match verb {
        "r8" => crate::println!("{port:#06x} -> {:#04x}", cpu::inb(port)),
        "r16" => crate::println!("{port:#06x} -> {:#06x}", cpu::inw(port)),
        "r32" => crate::println!("{port:#06x} -> {:#010x}", cpu::inl(port)),
        "w8" | "w16" | "w32" => {
            let Some(value) = words.next().and_then(number) else { return crate::println!("Usage: io {verb} PORT VALUE") };
            match verb {
                "w8" => cpu::outb(port, value as u8),
                "w16" => cpu::outw(port, value as u16),
                _ => cpu::outl(port, value as u32),
            }
            crate::println!("{port:#06x} <- {value:#x}");
        }
        _ => crate::println!("Usage: io r8|r16|r32 PORT | io w8|w16|w32 PORT VALUE"),
    }
}

/// `mmio r8|r16|r32|r64 ADDRESS` and `mmio w8|w16|w32|w64 ADDRESS VALUE`: access to *mapped*
/// physical addresses (device registers a driver mapped, RAM). An unmapped address is refused
/// instead of faulting; the kernel's own image is refused too.
pub fn mmio(mut words: core::str::SplitWhitespace) {
    let (Some(verb), Some(address)) = (words.next(), words.next().and_then(number)) else {
        return crate::println!("Usage: mmio r8|r16|r32|r64 ADDRESS | mmio w8|w16|w32|w64 ADDRESS VALUE");
    };
    let width = match verb.get(1..) {
        Some("8") => 1u64,
        Some("16") => 2,
        Some("32") => 4,
        Some("64") => 8,
        _ => return crate::println!("Usage: mmio r8|r16|r32|r64 ADDRESS | mmio w8|w16|w32|w64 ADDRESS VALUE"),
    };
    if address % width != 0 {
        return crate::println!("The address must be aligned to {width} bytes.");
    }
    if let Some(info) = tape::boot_info() {
        if address < info.kernel_base + info.kernel_size && address + width > info.kernel_base {
            return crate::println!("{address:#x} is inside the kernel's own image; refused.");
        }
    }
    if !paging::is_mapped(address) || !paging::is_mapped(address + width - 1) {
        return crate::println!("{address:#x}: not mapped (only addresses the kernel has mapped can be read).");
    }
    unsafe {
        match verb {
            "r8" => crate::println!("{address:#x} -> {:#04x}", (address as *const u8).read_volatile()),
            "r16" => crate::println!("{address:#x} -> {:#06x}", (address as *const u16).read_volatile()),
            "r32" => crate::println!("{address:#x} -> {:#010x}", (address as *const u32).read_volatile()),
            "r64" => crate::println!("{address:#x} -> {:#018x}", (address as *const u64).read_volatile()),
            _ => {
                let Some(value) = words.next().and_then(number) else { return crate::println!("Usage: mmio {verb} ADDRESS VALUE") };
                match width {
                    1 => (address as *mut u8).write_volatile(value as u8),
                    2 => (address as *mut u16).write_volatile(value as u16),
                    4 => (address as *mut u32).write_volatile(value as u32),
                    _ => (address as *mut u64).write_volatile(value),
                }
                crate::println!("{address:#x} <- {value:#x}");
            }
        }
    }
}

/// `irqstat`: the I/O APIC's pins (vector, trigger, polarity, mask) for every pin that is
/// routed, the closest v2 has to v1's PIC state dump.
pub fn irqstat() {
    let base = ioapic::first_gsi();
    let mut shown = 0;
    for pin in 0..ioapic::gsi_range().1.saturating_sub(base) + 1 {
        let Some((low, high)) = ioapic::entry(pin) else { break };
        if low & (1 << 16) != 0 && low & 0xff == 0 {
            continue; // masked and never routed
        }
        shown += 1;
        crate::println!(
            "GSI {:>2}: vector {:#04x} {} {}, {}, destination APIC {}",
            base + pin,
            low & 0xff,
            if low & (1 << 15) != 0 { "level" } else { "edge" },
            if low & (1 << 13) != 0 { "active low" } else { "active high" },
            if low & (1 << 16) != 0 { "MASKED" } else { "enabled" },
            high >> 24
        );
    }
    if shown == 0 {
        crate::println!("No I/O APIC pin is routed.");
    }
    crate::println!("Per-NIC interrupt counts are in `nic`; MSI vectors are listed there too.");
}
