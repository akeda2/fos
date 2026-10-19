//! fos v2 loader, milestone M1: read the kernel from the payload partition, leave
//! the firmware behind and jump into the 64-bit kernel.
//!
//! This is the v2 counterpart of `boot/stage2.S`, and like it, it narrates. The
//! sequence (docs/design.md, section 4):
//!
//! 1. say what the firmware is and what memory, graphics and ACPI it provides;
//! 2. find the payload partition on the boot disk (a GPT partition of the fos
//!    payload type) and read its slot-metadata sector to learn the active kernel;
//! 3. read that slot into memory and place its ELF64 segments at their linked
//!    physical addresses;
//! 4. collect everything the kernel will need into a [`BootInfo`];
//! 5. `ExitBootServices` (after which the firmware console, Block I/O and every
//!    other boot service are gone) and jump to the kernel's entry point.
//!
//! The A/B fallback state machine arrives in a later milestone (M6); M1 simply boots
//! the slot the metadata calls active.
#![no_std]
#![no_main]

mod console;
mod disk;
mod elf;
mod uefi;

use core::panic::PanicInfo;
use fosfor_abi::{BootInfo, Framebuffer, SlotChoice, SlotMetadata, BOOT_INFO_MAGIC, BOOT_INFO_VERSION, RUNNING_BOOTING, RUNNING_UP, SECTOR, select_slot, slot_start};
use uefi::{Handle, MemoryDescriptor, Status, SystemTable};

/// Room for the memory map the kernel receives (it stays allocated for the kernel):
/// real machines report 50-250 entries of 48 bytes.
const MEMORY_MAP_BYTES: usize = 64 * 1024;
/// Sectors read from the slot per Block I/O call.
const READ_CHUNK_SECTORS: usize = 128;

/// The firmware's entry into the loader.
#[unsafe(no_mangle)]
pub extern "efiapi" fn efi_main(image: Handle, system_table: *mut SystemTable) -> Status {
    let table = unsafe { &*system_table };
    console::init_serial();
    console::set_text_out(table.con_out);
    let boot = unsafe { &*table.boot_services };

    println!();
    println!("Fosfor OS loader (milestone M1): UEFI application running.");
    println!();
    describe_firmware(table);

    // UEFI arms a 5-minute watchdog before starting an application and reboots
    // the machine when it expires; a loader (or a kernel not yet running its own
    // timers) must switch it off.
    let status = (boot.set_watchdog_timer)(0, 0, 0, core::ptr::null());
    println!("Firmware watchdog disabled (status {status:#x}).");
    println!();

    summarize_memory(boot);
    let framebuffer = describe_graphics(boot);
    let rsdp = describe_acpi(table);
    println!();

    // ---- find and read the kernel ----
    let Some(payload) = disk::find_payload(boot) else {
        fatal("no payload partition (fos GPT type) found on any disk")
    };
    println!("Payload partition: LBA {} + {} sectors.", payload.start, payload.sectors);
    let mut sector = [0u8; SECTOR];
    if payload.read(0, sector.as_mut_ptr(), 1).is_err() {
        fatal("cannot read the slot-metadata sector");
    }
    let Some(mut metadata) = SlotMetadata::parse(&sector) else { fatal("the slot-metadata sector is not valid") };

    // Did the previous boot end badly? `running` is 1 from the moment this loader hands
    // over until the kernel reaches its milestone, 2 after that, 0 after a clean stop.
    match metadata.running {
        RUNNING_BOOTING => {
            metadata.crash_count = metadata.crash_count.saturating_add(1);
            println!("Previous boot never reached the kernel's milestone (crash, hang or power loss): {} in a row.", metadata.crash_count);
        }
        RUNNING_UP => {
            metadata.unclean_boots = metadata.unclean_boots.saturating_add(1);
            println!("Previous boot ended without a clean stop (after reaching its milestone): {} unclean boot(s).", metadata.unclean_boots);
        }
        _ => {}
    }
    let SlotChoice::Boot(slot) = select_slot(&mut metadata);
    let slot = u64::from(slot);
    println!(
        "Slot metadata: booting slot {slot} (active {}, pending {:?}, attempted {}, confirmed {}).",
        metadata.active_slot, metadata.pending_slot, metadata.attempted, metadata.confirmed
    );
    let sectors = metadata.slot_sectors[slot as usize] as usize;
    println!("Kernel slot {slot}: {sectors} sectors ({} KiB).", sectors / 2);
    if sectors == 0 {
        fatal("the selected slot is empty");
    }
    metadata.running = RUNNING_BOOTING;
    let boot_crash_count = metadata.crash_count;
    let mut written = [0u8; SECTOR];
    metadata.write_into(&mut written);
    if payload.write(0, written.as_ptr(), 1).is_err() {
        println!("Warning: the slot metadata could not be written back (read-only media?); fallback and crash tracking are off this boot.");
    }
    let kernel_image = read_slot(boot, &payload, slot, sectors);
    println!("Kernel image read ({} bytes). Loading its ELF64 segments...", sectors * SECTOR);
    let kernel = match elf::load(boot, kernel_image) {
        Ok(kernel) => kernel,
        Err(error) => {
            println!("ELF load failed: {error:?}");
            fatal("cannot load the kernel")
        }
    };
    println!("Kernel at {:#x}, {} KiB, entry {:#x}.", kernel.base, kernel.size / 1024, kernel.entry);

    // ---- build BootInfo, then leave the firmware ----
    let info = allocate_pages(boot, 1) as *mut BootInfo;
    let map_buffer = allocate_pages(boot, MEMORY_MAP_BYTES / 4096) as *mut u8;
    unsafe {
        info.write(BootInfo {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            size: core::mem::size_of::<BootInfo>() as u32,
            memory_map: map_buffer as u64,
            memory_map_count: 0,
            memory_map_descriptor_size: 0,
            framebuffer,
            acpi_rsdp: rsdp,
            runtime_services: table.runtime_services as u64,
            kernel_base: kernel.base,
            kernel_size: kernel.size,
            boot_slot: slot as u32,
            crash_count: u32::from(boot_crash_count),
            payload_guid: payload.guid,
            payload_start: payload.start,
            payload_sectors: payload.sectors,
        });
    }
    println!("BootInfo at {:#x}. Exiting boot services: the firmware console, Block I/O and", info as usize);
    println!("every other boot service end here; the kernel narrates from now on.");
    exit_and_jump(image, boot, map_buffer, unsafe { &mut *info }, kernel.entry)
}

/// `AllocatePages` for `pages` 4 KiB pages anywhere, as loader data.
fn allocate_pages(boot: &uefi::BootServices, pages: usize) -> u64 {
    let mut address = 0u64;
    if (boot.allocate_pages)(uefi::ALLOCATE_ANY_PAGES, uefi::LOADER_DATA, pages, &mut address) != uefi::SUCCESS {
        fatal("out of memory");
    }
    address
}

/// Reads kernel slot `slot` (`sectors` long) into freshly allocated pages.
fn read_slot(boot: &uefi::BootServices, payload: &disk::Partition, slot: u64, sectors: usize) -> &'static [u8] {
    let bytes = sectors * SECTOR;
    let buffer = allocate_pages(boot, bytes.div_ceil(4096)) as *mut u8;
    let mut done = 0;
    while done < sectors {
        let count = READ_CHUNK_SECTORS.min(sectors - done);
        if payload.read(slot_start(slot) + done as u64, unsafe { buffer.add(done * SECTOR) }, count).is_err() {
            fatal("a read of the kernel slot failed");
        }
        done += count;
    }
    unsafe { core::slice::from_raw_parts(buffer, bytes) }
}

/// Gets the final memory map and exits boot services, retrying when the map key
/// goes stale (any allocation or console output between the two calls changes the
/// map), then jumps to the kernel. Nothing may print between the calls.
fn exit_and_jump(image: Handle, boot: &uefi::BootServices, map_buffer: *mut u8, info: &mut BootInfo, entry: u64) -> ! {
    for _ in 0..8 {
        let mut size = MEMORY_MAP_BYTES;
        let (mut key, mut descriptor_size, mut version) = (0usize, 0usize, 0u32);
        let status = (boot.get_memory_map)(&mut size, map_buffer as *mut MemoryDescriptor, &mut key, &mut descriptor_size, &mut version);
        if status != uefi::SUCCESS {
            fatal("GetMemoryMap failed at exit");
        }
        info.memory_map_count = (size / descriptor_size) as u64;
        info.memory_map_descriptor_size = descriptor_size as u64;
        if (boot.exit_boot_services)(image, key) == uefi::SUCCESS {
            // The firmware is gone. The kernel is entered like a C function with the
            // BootInfo pointer in rdi (System V ABI), in the long mode and identity
            // mapping the firmware left; it replaces both immediately.
            #[cfg(target_arch = "x86_64")]
            let kernel: extern "sysv64" fn(*const BootInfo) -> ! = unsafe { core::mem::transmute(entry as usize) };
            // aarch64: AAPCS64, the BootInfo pointer in x0, MMU on with the firmware's identity map, interrupts masked below.
            #[cfg(target_arch = "aarch64")]
            let kernel: extern "C" fn(*const BootInfo) -> ! = unsafe { core::mem::transmute(entry as usize) };
            kernel(info)
        }
    }
    fatal("ExitBootServices kept failing")
}

/// Who built this firmware and which UEFI specification it implements.
fn describe_firmware(table: &SystemTable) {
    print!("Firmware: ");
    let mut vendor = table.firmware_vendor;
    while !vendor.is_null() && unsafe { *vendor } != 0 {
        let unit = unsafe { *vendor };
        print!("{}", if unit < 0x80 { unit as u8 as char } else { '?' });
        vendor = unsafe { vendor.add(1) };
    }
    println!(" revision {:#x}", table.firmware_revision);
    let revision = table.header.revision;
    println!("UEFI specification {}.{}", revision >> 16, revision & 0xffff);
}

/// Summarizes the memory map: the kernel turns the real thing (taken again at the
/// moment boot services end) into its allocator.
fn summarize_memory(boot: &uefi::BootServices) {
    static mut BUFFER: [u8; MEMORY_MAP_BYTES] = [0; MEMORY_MAP_BYTES];
    let mut size = MEMORY_MAP_BYTES;
    let (mut key, mut descriptor_size, mut version) = (0usize, 0usize, 0u32);
    let status = (boot.get_memory_map)(&mut size, core::ptr::addr_of_mut!(BUFFER) as *mut MemoryDescriptor, &mut key, &mut descriptor_size, &mut version);
    if status != uefi::SUCCESS || descriptor_size < core::mem::size_of::<MemoryDescriptor>() {
        println!("Memory map: GetMemoryMap failed (status {status:#x}, needs {size} bytes).");
        return;
    }
    let count = size / descriptor_size;
    println!("Memory map: {count} entries of {descriptor_size} bytes (descriptor version {version}, key {key:#x}).");
    let mut pages_by_kind = [0u64; 16];
    let base = core::ptr::addr_of!(BUFFER) as *const u8;
    for index in 0..count {
        let entry = unsafe { core::ptr::read_unaligned(base.add(index * descriptor_size) as *const MemoryDescriptor) };
        if let Some(total) = pages_by_kind.get_mut(entry.kind as usize) {
            *total += entry.pages;
        }
    }
    println!("Totals by type (MiB):");
    for (kind, pages) in pages_by_kind.iter().enumerate() {
        if *pages != 0 {
            println!("  {:>6} MiB  {}", pages / 256, uefi::memory_type_name(kind as u32));
        }
    }
    let reusable: u64 = [1usize, 2, 3, 4, 7].iter().map(|&kind| pages_by_kind[kind]).sum();
    println!("Reusable after ExitBootServices: {} MiB.", reusable / 256);
}

/// The graphics output protocol: a linear framebuffer the kernel draws its text
/// console into, if the firmware set one up. Returns what to tell the kernel.
fn describe_graphics(boot: &uefi::BootServices) -> Framebuffer {
    let none = Framebuffer { base: 0, size: 0, width: 0, height: 0, stride: 0, format: fosfor_abi::PIXEL_NONE };
    let mut interface: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = (boot.locate_protocol)(&uefi::GRAPHICS_OUTPUT_PROTOCOL, core::ptr::null_mut(), &mut interface);
    if status != uefi::SUCCESS || interface.is_null() {
        println!("Graphics: no graphics output protocol (status {status:#x}); the kernel will use serial only.");
        return none;
    }
    let graphics = unsafe { &*(interface as *const uefi::GraphicsOutput) };
    let mode = unsafe { &*graphics.mode };
    let info = unsafe { &*mode.info };
    println!(
        "Graphics: {}x{} {} pixels, {} per line, framebuffer at {:#x}, {} KiB.",
        info.horizontal_resolution,
        info.vertical_resolution,
        match info.pixel_format {
            0 => "RGB",
            1 => "BGR",
            2 => "bit mask",
            _ => "no linear framebuffer",
        },
        info.pixels_per_scan_line,
        mode.frame_buffer_base,
        mode.frame_buffer_size / 1024
    );
    if info.pixel_format == fosfor_abi::PIXEL_NONE {
        return none;
    }
    Framebuffer {
        base: mode.frame_buffer_base,
        size: mode.frame_buffer_size as u64,
        width: info.horizontal_resolution,
        height: info.vertical_resolution,
        stride: info.pixels_per_scan_line,
        format: info.pixel_format,
    }
}

/// The ACPI root pointer, found in the firmware's configuration table. The kernel
/// needs it for the interrupt controller layout, the PCIe config space and timers.
fn describe_acpi(table: &SystemTable) -> u64 {
    let entries = unsafe { core::slice::from_raw_parts(table.configuration_table, table.number_of_table_entries) };
    let find = |guid: &uefi::Guid| entries.iter().find(|entry| entry.vendor_guid == *guid).map(|entry| entry.vendor_table);
    let (root, version) = match (find(&uefi::ACPI_20_TABLE), find(&uefi::ACPI_10_TABLE)) {
        (Some(root), _) => (root, "2.0+"),
        (None, Some(root)) => (root, "1.0"),
        _ => {
            println!("ACPI: no RSDP in the configuration table.");
            return 0;
        }
    };
    let signature = unsafe { core::slice::from_raw_parts(root as *const u8, 8) };
    println!(
        "ACPI {} RSDP at {:#x}, signature {}.",
        version,
        root as usize,
        if signature == b"RSD PTR " { "valid" } else { "WRONG" }
    );
    root as u64
}

fn halt() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("hlt")
    };
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("wfi")
    };
}

/// Reports a problem the loader cannot recover from and stops (boot services are
/// still alive when this is called, so the message reaches the console).
fn fatal(message: &str) -> ! {
    println!();
    println!("Fosfor OS loader FATAL: {message}");
    loop {
        halt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nFosfor OS loader PANIC: {info}");
    loop {
        halt();
    }
}
