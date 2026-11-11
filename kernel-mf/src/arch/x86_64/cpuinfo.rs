//! The shell commands that read this CPU's identity and its port space: `cpuinfo` (CPUID) and `io` (raw port access).

use crate::cpu::cpuid;
use crate::sysshell::number;

/// `io r8|r16|r32 PORT` and `io w8|w16|w32 PORT VALUE`: raw port access.
pub fn io(rest: &str) {
    use crate::cpu;
    let mut words = rest.split_whitespace();
    let usage = "Usage: io r8|r16|r32 PORT | io w8|w16|w32 PORT VALUE";
    let (Some(verb), Some(port)) = (words.next(), words.next().and_then(number)) else { return println!("{usage}") };
    if port > 0xffff {
        return println!("Ports are 16 bits.");
    }
    let port = port as u16;
    match verb {
        "r8" => println!("{port:#06x} -> {:#04x}", cpu::inb(port)),
        "r16" => println!("{port:#06x} -> {:#06x}", cpu::inw(port)),
        "r32" => println!("{port:#06x} -> {:#010x}", cpu::inl(port)),
        "w8" | "w16" | "w32" => {
            let Some(value) = words.next().and_then(number) else { return println!("Usage: io {verb} PORT VALUE") };
            match verb {
                "w8" => cpu::outb(port, value as u8),
                "w16" => cpu::outw(port, value as u16),
                _ => cpu::outl(port, value as u32),
            }
            println!("{port:#06x} <- {value:#x}");
        }
        _ => println!("{usage}"),
    }
}

/// `cpuinfo`: CPUID of the CPU running the shell.
pub fn cpuinfo() {
    let (max_leaf, b, c, d) = cpuid(0, 0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&b.to_le_bytes());
    vendor[4..8].copy_from_slice(&d.to_le_bytes());
    vendor[8..12].copy_from_slice(&c.to_le_bytes());
    println!("vendor     {} (highest basic CPUID leaf {max_leaf:#x})", core::str::from_utf8(&vendor).unwrap_or("?"));
    let (max_extended, ..) = cpuid(0x8000_0000, 0);
    if max_extended >= 0x8000_0004 {
        let mut brand = [0u8; 48];
        for (index, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
            let (a, b, c, d) = cpuid(leaf, 0);
            for (part, register) in [a, b, c, d].into_iter().enumerate() {
                brand[index * 16 + part * 4..][..4].copy_from_slice(&register.to_le_bytes());
            }
        }
        println!("brand      {}", core::str::from_utf8(&brand).unwrap_or("?").trim_matches(|c: char| c == '\0' || c == ' '));
    }
    let (signature, ebx, ecx, edx) = cpuid(1, 0);
    let (stepping, mut model, mut family) = (signature & 0xf, (signature >> 4) & 0xf, (signature >> 8) & 0xf);
    if family == 0xf {
        family += (signature >> 20) & 0xff;
    }
    if family == 0x6 || family >= 0xf {
        model |= ((signature >> 16) & 0xf) << 4;
    }
    println!("signature  family {family:#x} model {model:#x} stepping {stepping} (CPUID.1 EAX {signature:#010x})");
    println!("logical    {} logical processor(s) per package, APIC id {}", (ebx >> 16) & 0xff, ebx >> 24);
    let (_, _, ext_ecx, ext_edx) = if max_extended >= 0x8000_0001 { cpuid(0x8000_0001, 0) } else { (0, 0, 0, 0) };
    let (_, leaf7_ebx, ..) = if max_leaf >= 7 { cpuid(7, 0) } else { (0, 0, 0, 0) };
    let features = [
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
    ];
    print!("features  ");
    for (name, present) in features {
        if present {
            print!(" {name}");
        }
    }
    println!();
    let invariant = max_extended >= 0x8000_0007 && cpuid(0x8000_0007, 0).3 & (1 << 8) != 0;
    println!("tsc        {}", if invariant { "invariant" } else { "not invariant" });
}
