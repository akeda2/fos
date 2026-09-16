use crate::arch::x86::halt_forever;
use crate::boot_info::{BootInfo, MemoryRegion};
use crate::console;
use crate::cpu;
use crate::disk;
use crate::fosfs;
use crate::interrupts;
use crate::keyboard;
use crate::keyboard::Keyboard;
use crate::pmem;
use crate::{print, println};

const LINE_CAPACITY: usize = 80;

pub fn run(info: &BootInfo) -> ! {
    let keyboard_available = Keyboard::initialize().is_some();
    interrupts::initialize();
    if keyboard_available {
        println!("PS/2 keyboard initialized. Type 'help' for commands.");
    } else {
        println!("PS/2 keyboard unavailable; serial input remains available.");
        println!("Type 'help' for commands.");
    }
    let mut line = [0u8; LINE_CAPACITY];
    let mut length = 0;
    let mut saw_carriage_return = false;
    prompt();

    loop {
        let byte = console::read_serial_byte().or_else(keyboard::pop_char);
        let Some(byte) = byte else {
            continue;
        };
        match byte {
            b'\n' if saw_carriage_return => saw_carriage_return = false,
            b'\r' | b'\n' => {
                println!();
                execute(&line[..length], info);
                length = 0;
                saw_carriage_return = byte == b'\r';
                prompt();
            }
            8 | 127 if length > 0 => {
                saw_carriage_return = false;
                length -= 1;
                console::erase_last_character();
            }
            0x20..=0x7e if length < LINE_CAPACITY => {
                saw_carriage_return = false;
                line[length] = byte;
                length += 1;
                print!("{}", byte as char);
            }
            0x20..=0x7e => {
                saw_carriage_return = false;
                println!("\nInput limit is {LINE_CAPACITY} characters.")
            }
            _ => {}
        }
    }
}

fn prompt() {
    print!("fos> ");
}

fn execute(command: &[u8], info: &BootInfo) {
    let (name, arguments) = take_word(command);
    match name {
        b"" => {}
        b"help" => {
            if no_arguments(arguments) {
                println!("System: help clear about bootinfo memmap memory cpuinfo hexdump halt");
                println!("Interrupts: ticks uptime irqstat pmem palloc pfree");
                println!("Disk: diskinfo readsector format ls cat write rm");
            }
        }
        b"clear" if no_arguments(arguments) => console::clear(),
        b"about" if no_arguments(arguments) => println!("fos is a small educational 32-bit x86 operating system."),
        b"bootinfo" if no_arguments(arguments) => print_boot_info(info),
        b"memmap" if no_arguments(arguments) => print_memory_map(info),
        b"memory" if no_arguments(arguments) => print_memory_summary(info),
        b"cpuinfo" if no_arguments(arguments) => print_cpu_info(),
        b"diskinfo" if no_arguments(arguments) => print_disk_info(info),
        b"ticks" if no_arguments(arguments) => print_ticks(),
        b"uptime" if no_arguments(arguments) => print_uptime(),
        b"irqstat" if no_arguments(arguments) => print_irq_stats(),
        b"pmem" if no_arguments(arguments) => print_pmem_stats(),
        b"palloc" if no_arguments(arguments) => palloc_command(),
        b"pfree" => pfree_command(arguments),
        b"readsector" => read_sector_command(info, arguments),
        b"hexdump" => hexdump_command(info, arguments),
        b"format" if no_arguments(arguments) => filesystem_result(fosfs::format(info), "FOSFS formatted."),
        b"ls" if no_arguments(arguments) => filesystem_result(fosfs::list(info), ""),
        b"cat" => cat_command(info, arguments),
        b"write" => write_command(info, arguments),
        b"rm" => remove_command(info, arguments),
        b"halt" => {
            if !no_arguments(arguments) {
                return;
            }
            println!("Halting CPU. Reset or power-cycle to restart.");
            halt_forever();
        }
        b"clear" | b"about" | b"bootinfo" | b"memmap" | b"memory" | b"cpuinfo" | b"diskinfo" | b"format" | b"ls"
        | b"ticks" | b"uptime" | b"irqstat" | b"pmem" | b"palloc" => {
            println!("This command does not accept arguments.");
        }

        _ => println!("Unknown command. Type 'help' for commands."),
    }
}

fn print_ticks() {
    println!("Timer ticks: {} (at {} Hz)", interrupts::ticks(), interrupts::tick_frequency_hz());
}

fn print_uptime() {
    let ticks = interrupts::ticks();
    let frequency = u64::from(interrupts::tick_frequency_hz());
    println!(
        "Uptime: {}.{:02} seconds ({} ticks)",
        ticks / frequency,
        (ticks % frequency) * 100 / frequency,
        ticks
    );
}

fn print_irq_stats() {
    println!(
        "IRQ0 (timer): {} fired; IRQ1 (keyboard): {} fired",
        interrupts::timer_irq_count(),
        interrupts::keyboard_irq_count()
    );
}

fn print_pmem_stats() {
    let (total, free) = pmem::stats();
    let used = total - free;
    println!(
        "Physical frames: {total} total, {free} free, {used} used ({} KiB/frame)",
        pmem::FRAME_SIZE / 1024
    );
}

fn palloc_command() {
    match pmem::allocate() {
        Some(address) => println!("Allocated frame at 0x{address:08x}."),
        None => println!("No free physical frames remain."),
    }
}

fn pfree_command(arguments: &[u8]) {
    let (value, remainder) = take_word(arguments);
    let Some(address) = parse_number(value) else {
        println!("Usage: pfree ADDRESS (as returned by palloc)");
        return;
    };
    if !remainder.is_empty() {
        println!("Usage: pfree ADDRESS");
        return;
    }
    pmem::free(address);
    println!("Frame at 0x{address:08x} released (if it was allocated and in range).");
}

fn print_cpu_info() {
    let vendor = cpu::vendor();
    print!("CPUID vendor: ");
    console::write_bytes(&vendor);
    let (edx, ecx) = cpu::features();
    println!("\nCPUID feature bits: EDX=0x{edx:08x} ECX=0x{ecx:08x}");
}

fn no_arguments(arguments: &[u8]) -> bool {
    if arguments.is_empty() {
        true
    } else {
        println!("This command does not accept arguments.");
        false
    }
}

fn take_word(input: &[u8]) -> (&[u8], &[u8]) {
    let input = trim_start(input);
    let end = input.iter().position(|byte| *byte == b' ').unwrap_or(input.len());
    (&input[..end], trim_start(&input[end..]))
}

fn trim_start(mut input: &[u8]) -> &[u8] {
    while input.first() == Some(&b' ') {
        input = &input[1..];
    }
    input
}

fn filesystem_result(result: Result<(), disk::Error>, success: &str) {
    match result {
        Ok(()) if !success.is_empty() => println!("{success}"),
        Ok(()) => {}
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

fn cat_command(info: &BootInfo, arguments: &[u8]) {
    let (name, remainder) = take_word(arguments);
    if name.is_empty() || !remainder.is_empty() {
        println!("Usage: cat NAME");
        return;
    }
    match fosfs::cat(info, name) {
        Ok(true) => {}
        Ok(false) => println!("File not found."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

fn write_command(info: &BootInfo, arguments: &[u8]) {
    let (name, text) = take_word(arguments);
    if name.is_empty() || text.is_empty() {
        println!("Usage: write NAME TEXT");
        return;
    }
    match fosfs::write(info, name, text) {
        Ok(fosfs::WriteOutcome::Written) => println!("File written."),
        Ok(fosfs::WriteOutcome::InvalidName) => {
            println!("Invalid name: use up to 16 uppercase letters, digits, '.', '_', or '-'.");
        }
        Ok(fosfs::WriteOutcome::ContentTooLarge) => println!("Content too large: FOSFS files are limited to one 512-byte sector."),
        Ok(fosfs::WriteOutcome::DirectoryFull) => println!("Directory full: all 64 FOSFS entries are in use."),
        Ok(fosfs::WriteOutcome::DataFull) => println!("Data region full: no free sector remains in the FOSFS partition."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

fn remove_command(info: &BootInfo, arguments: &[u8]) {
    let (name, remainder) = take_word(arguments);
    if name.is_empty() || !remainder.is_empty() {
        println!("Usage: rm NAME");
        return;
    }
    match fosfs::remove(info, name) {
        Ok(true) => println!("File removed."),
        Ok(false) => println!("File not found."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

fn print_disk_info(info: &BootInfo) {
    println!(
        "Disk: {} sectors; FOSFS partition: LBA {} + {} sectors",
        disk::sector_count(info),
        disk::data_partition_lba(info),
        disk::data_partition_sectors(info)
    );
    println!("Only FOSFS commands may write; raw sector inspection is read-only.");
}

fn read_sector_command(info: &BootInfo, arguments: &[u8]) {
    let (value, remainder) = take_word(arguments);
    let Some(lba) = parse_number(value) else {
        println!("Usage: readsector LBA (decimal or 0x hexadecimal)");
        return;
    };
    if !remainder.is_empty() {
        println!("Usage: readsector LBA");
        return;
    }
    match disk::read_sector(info, lba, |sector| dump_bytes(lba * 512, sector)) {
        Ok(()) => {}
        Err(error) => println!("Disk read failed: {error:?}"),
    }
}

fn hexdump_command(info: &BootInfo, arguments: &[u8]) {
    let (address, remainder) = take_word(arguments);
    let (length, extra) = take_word(remainder);
    let (Some(address), Some(length)) = (parse_number(address), parse_number(length)) else {
        println!("Usage: hexdump ADDRESS LENGTH (kernel memory only; maximum 128 bytes)");
        return;
    };
    let kernel_start = read_u32(core::ptr::addr_of!(info.kernel_start));
    let kernel_end = read_u32(core::ptr::addr_of!(info.kernel_end));
    if !extra.is_empty() || length == 0 || length > 128 || address < kernel_start || address.checked_add(length).is_none_or(|end| end > kernel_end) {
        println!("Address must be within the reserved kernel range; maximum is 128 bytes.");
        return;
    }
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, length as usize) };
    dump_bytes(address, bytes);
}

fn dump_bytes(base: u32, bytes: &[u8]) {
    for (row, chunk) in bytes.chunks(16).enumerate() {
        print!("0x{:08x}: ", base + (row * 16) as u32);
        for byte in chunk {
            print!("{byte:02x} ");
        }
        for _ in chunk.len()..16 {
            print!("   ");
        }
        print!("|");
        for byte in chunk {
            print!("{}", if (0x20..=0x7e).contains(byte) { *byte as char } else { '.' });
        }
        println!("|");
    }
}

fn parse_number(value: &[u8]) -> Option<u32> {
    let (radix, digits) = if value.starts_with(b"0x") {
        (16, &value[2..])
    } else {
        (10, value)
    };
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u32, |number, digit| {
        let value = match *digit {
            b'0'..=b'9' => u32::from(*digit - b'0'),
            b'a'..=b'f' if radix == 16 => u32::from(*digit - b'a' + 10),
            b'A'..=b'F' if radix == 16 => u32::from(*digit - b'A' + 10),
            _ => return None,
        };
        if value >= radix {
            return None;
        }
        number.checked_mul(radix)?.checked_add(value)
    })
}

fn print_boot_info(info: &BootInfo) {
    let boot_drive = info.boot_drive;
    let kernel_start = read_u32(core::ptr::addr_of!(info.kernel_start));
    let kernel_end = read_u32(core::ptr::addr_of!(info.kernel_end));
    let serial_available = info.serial_available != 0;
    println!(
        "Boot drive: 0x{boot_drive:02x}; serial: {}; kernel: 0x{kernel_start:08x}-0x{kernel_end:08x}",
        if serial_available { "available" } else { "unavailable" }
    );
}

fn print_memory_map(info: &BootInfo) {
    let count = usize::from(read_u16(core::ptr::addr_of!(info.memory_region_count)));
    println!("Firmware memory map ({count} entries reported by BIOS INT 15h EAX=E820h):");
    for index in 0..count {
        let region = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(info.memory_regions[index])) };
        print_region(index, region);
    }

}

fn e820_type_name(kind: u32) -> &'static str {
    match kind {
        1 => "usable RAM",
        2 => "reserved (not usable)",
        3 => "ACPI reclaimable",
        4 => "ACPI NVS",
        5 => "bad memory",
        _ => "unknown type",
    }
}

fn print_memory_summary(info: &BootInfo) {
    let count = usize::from(read_u16(core::ptr::addr_of!(info.memory_region_count)));
    let mut usable = 0u64;
    let mut reserved = 0u64;
    for index in 0..count {
        let region = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(info.memory_regions[index])) };
        let length = read_u64(core::ptr::addr_of!(region.length));
        if read_u32(core::ptr::addr_of!(region.kind)) == 1 {
            usable = usable.saturating_add(length);
        } else {
            reserved = reserved.saturating_add(length);
        }
    }
    println!("E820 summary: {usable} usable bytes; {reserved} non-usable bytes (reserved/ACPI/bad).");
}

fn print_region(index: usize, region: MemoryRegion) {
    let base = read_u64(core::ptr::addr_of!(region.base));
    let length = read_u64(core::ptr::addr_of!(region.length));
    let kind = read_u32(core::ptr::addr_of!(region.kind));
    println!("  {index:02}: 0x{base:016x} + 0x{length:016x} type {kind} ({})", e820_type_name(kind));
}

fn read_u16(pointer: *const u16) -> u16 {
    unsafe { core::ptr::read_unaligned(pointer) }
}

fn read_u32(pointer: *const u32) -> u32 {
    unsafe { core::ptr::read_unaligned(pointer) }
}

fn read_u64(pointer: *const u64) -> u64 {
    unsafe { core::ptr::read_unaligned(pointer) }
}
