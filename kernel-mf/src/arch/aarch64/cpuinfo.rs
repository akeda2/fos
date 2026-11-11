//! `cpuinfo` and `io` on aarch64: the CPU describes itself through ID registers (no CPUID), and there is no port space.

macro_rules! sysreg {
    ($name:literal) => {{
        let value: u64;
        unsafe { core::arch::asm!(concat!("mrs {}, ", $name), out(reg) value, options(nomem, nostack)) };
        value
    }};
}

fn part_name(part: u64) -> &'static str {
    match part {
        0xd03 => "Cortex-A53",
        0xd07 => "Cortex-A57",
        0xd08 => "Cortex-A72",
        0xd09 => "Cortex-A73",
        0xd0b => "Cortex-A76",
        0xd0c => "Neoverse N1",
        _ => "unknown part",
    }
}

pub fn cpuinfo() {
    let midr = sysreg!("midr_el1");
    let (implementer, variant, part, revision) = ((midr >> 24) & 0xff, (midr >> 20) & 0xf, (midr >> 4) & 0xfff, midr & 0xf);
    let vendor = match implementer {
        0x41 => "Arm",
        0x51 => "Qualcomm",
        0x61 => "Apple",
        0x00 => "software (QEMU TCG)",
        _ => "unknown",
    };
    println!("implementer {vendor} ({implementer:#x})  part {} ({part:#x})  r{variant}p{revision}", part_name(part));
    println!("mpidr       {:#x}  running at EL{}", sysreg!("mpidr_el1") & 0xff_00ff_ffff, crate::exceptions::current_el());
    let isar0 = sysreg!("id_aa64isar0_el1");
    let pfr0 = sysreg!("id_aa64pfr0_el1");
    let mmfr0 = sysreg!("id_aa64mmfr0_el1");
    let field = |register: u64, shift: u32| (register >> shift) & 0xf;
    let features = [
        ("fp", field(pfr0, 16) != 0xf),
        ("asimd", field(pfr0, 20) != 0xf),
        ("aes", field(isar0, 4) != 0),
        ("pmull", field(isar0, 4) >= 2),
        ("sha1", field(isar0, 8) != 0),
        ("sha2", field(isar0, 12) != 0),
        ("crc32", field(isar0, 16) != 0),
        ("atomics", field(isar0, 20) != 0),
        ("rdm", field(isar0, 28) != 0),
        ("gicv3-sysreg", field(pfr0, 24) != 0),
    ];
    print!("features   ");
    for (name, present) in features {
        if present {
            print!(" {name}");
        }
    }
    println!();
    println!("address     {} physical bits (PARange {}), counter {} Hz", [32, 36, 40, 42, 44, 48, 52][field(mmfr0, 0).min(6) as usize], field(mmfr0, 0), crate::timer::frequency());
}

pub fn io(_rest: &str) {
    println!("There is no I/O port space on aarch64; device registers are memory (`pci BB:DD.F probe` reads a BAR).");
}
