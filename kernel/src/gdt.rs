//! The permanent kernel GDT and the emergency double-fault task.
//!
//! An unmapped kernel-stack guard page first raises #PF, but the processor
//! cannot push #PF's frame on that already-exhausted stack. x86 task gates
//! solve this bootstrapping problem: vector 8 switches to a separate TSS and
//! its independent stack before running `double_fault_task`.

const CODE_SELECTOR: u16 = 0x08;
const DATA_SELECTOR: u16 = 0x10;
pub const DOUBLE_FAULT_TSS_SELECTOR: u16 = 0x30;

const TSS_SIZE: u16 = 104;
const DOUBLE_FAULT_STACK_SIZE: usize = 8192;

const TSS_ESP0: usize = 4;
const TSS_SS0: usize = 8;
const TSS_CR3: usize = 28;
const TSS_EIP: usize = 32;
const TSS_EFLAGS: usize = 36;
const TSS_ESP: usize = 56;
const TSS_ES: usize = 72;
const TSS_CS: usize = 76;
const TSS_SS: usize = 80;
const TSS_DS: usize = 84;
const TSS_FS: usize = 88;
const TSS_GS: usize = 92;
const TSS_IO_MAP_BASE: usize = 102;

#[repr(C, align(8))]
struct GlobalDescriptorTable([u64; 7]);

#[repr(C, packed)]
struct GdtDescriptor {
    limit: u16,
    base: u32,
}

#[repr(C, align(4))]
struct TaskStateSegment {
    bytes: [u8; TSS_SIZE as usize],
}

#[repr(C, align(16))]
struct EmergencyStack([u8; DOUBLE_FAULT_STACK_SIZE]);

static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable([
    0,
    0x00cf_9a00_0000_ffff,
    0x00cf_9200_0000_ffff,
    // Keep stage two's 16-bit descriptors: its BIOS disk trampoline enters
    // protected mode through selectors 0x18 and 0x20 before real mode.
    0x0000_9a00_0000_ffff,
    0x0000_9200_0000_ffff,
    0,
    0,
]);
static mut NORMAL_TSS: TaskStateSegment = TaskStateSegment {
    bytes: [0; TSS_SIZE as usize],
};
static mut DOUBLE_FAULT_TSS: TaskStateSegment = TaskStateSegment {
    bytes: [0; TSS_SIZE as usize],
};
static mut DOUBLE_FAULT_STACK: EmergencyStack = EmergencyStack([0; DOUBLE_FAULT_STACK_SIZE]);

unsafe extern "C" {
    fn load_kernel_gdt(descriptor: *const GdtDescriptor);
    fn load_kernel_gdt_and_task_register(descriptor: *const GdtDescriptor);
}

/// Installs code/data/TSS descriptors and loads the normal task register.
///
/// This runs once before the IDT becomes live. The normal TSS is required
/// because a task-gate switch saves the interrupted task's registers through
/// TR before it loads the double-fault TSS.
pub fn initialize() {
    unsafe {
        let normal_tss = core::ptr::addr_of_mut!(NORMAL_TSS);
        let double_fault_tss = core::ptr::addr_of_mut!(DOUBLE_FAULT_TSS);
        let normal_address = normal_tss as u32;
        let double_fault_address = double_fault_tss as u32;

        initialize_tss(normal_tss, crate::paging::directory_address(), 0, 0, 0);

        let stack_top = core::ptr::addr_of_mut!(DOUBLE_FAULT_STACK.0)
            .cast::<u8>()
            .add(DOUBLE_FAULT_STACK_SIZE) as u32;
        initialize_tss(
            double_fault_tss,
            crate::paging::directory_address(),
            double_fault_task as *const () as usize as u32,
            stack_top,
            stack_top,
        );

        GDT.0[5] = tss_descriptor(normal_address);
        GDT.0[6] = tss_descriptor(double_fault_address);
        let descriptor = gdt_descriptor();
        load_kernel_gdt_and_task_register(core::ptr::addr_of!(descriptor));
    }
}

/// Restores the permanent GDT after stage two's BIOS disk trampoline loaded
/// its temporary one. `lgdt` does not invalidate TR's hidden TSS cache, so
/// this intentionally does not execute `ltr` again against the now-busy
/// normal-TSS descriptor.
pub fn restore_after_bios() {
    unsafe {
        let descriptor = gdt_descriptor();
        load_kernel_gdt(core::ptr::addr_of!(descriptor));
    }
}

fn gdt_descriptor() -> GdtDescriptor {
    GdtDescriptor {
        limit: (core::mem::size_of::<GlobalDescriptorTable>() - 1) as u16,
        base: core::ptr::addr_of!(GDT) as u32,
    }
}

unsafe fn initialize_tss(
    tss: *mut TaskStateSegment,
    cr3: u32,
    eip: u32,
    esp: u32,
    esp0: u32,
) {
    unsafe {
        write_tss_u32(tss, TSS_CR3, cr3);
        write_tss_u32(tss, TSS_EIP, eip);
        write_tss_u32(tss, TSS_EFLAGS, 0x2);
        write_tss_u32(tss, TSS_ESP, esp);
        write_tss_u32(tss, TSS_ESP0, esp0);
        write_tss_u16(tss, TSS_SS0, DATA_SELECTOR);
        write_tss_u16(tss, TSS_ES, DATA_SELECTOR);
        write_tss_u16(tss, TSS_CS, CODE_SELECTOR);
        write_tss_u16(tss, TSS_SS, DATA_SELECTOR);
        write_tss_u16(tss, TSS_DS, DATA_SELECTOR);
        write_tss_u16(tss, TSS_FS, DATA_SELECTOR);
        write_tss_u16(tss, TSS_GS, DATA_SELECTOR);
        write_tss_u16(tss, TSS_IO_MAP_BASE, TSS_SIZE);
    }
}

unsafe fn write_tss_u16(tss: *mut TaskStateSegment, offset: usize, value: u16) {
    unsafe {
        (tss.cast::<u8>().add(offset).cast::<u16>()).write_unaligned(value);
    }
}

unsafe fn write_tss_u32(tss: *mut TaskStateSegment, offset: usize, value: u32) {
    unsafe {
        (tss.cast::<u8>().add(offset).cast::<u32>()).write_unaligned(value);
    }
}

fn tss_descriptor(base: u32) -> u64 {
    let limit = u32::from(TSS_SIZE - 1);
    u64::from(limit & 0xffff)
        | (u64::from(base & 0x00ff_ffff) << 16)
        | (u64::from(0x89u8) << 40)
        | (u64::from((limit >> 16) & 0x0f) << 48)
        | (u64::from(base >> 24) << 56)
}

unsafe fn read_tss_u32(tss: *const TaskStateSegment, offset: usize) -> u32 {
    unsafe { (tss.cast::<u8>().add(offset).cast::<u32>()).read_unaligned() }
}

const TSS_EAX: usize = 40;
const TSS_ECX: usize = 44;
const TSS_EDX: usize = 48;
const TSS_EBX: usize = 52;
const TSS_EBP: usize = 60;
const TSS_ESI: usize = 64;
const TSS_EDI: usize = 68;

/// The interrupted task's full register snapshot, as the CPU's own
/// hardware task switch saved it into the normal TSS (referenced by TR at
/// the moment vector 8's task gate fired) before loading the emergency
/// double-fault TSS -- the one case in this kernel where a fatal
/// exception's general-purpose registers are actually recoverable, since
/// `extern "x86-interrupt"` handlers never expose them for an ordinary
/// exception. See `kernel/src/debug.rs`'s `CrashRecord`, the only caller.
pub struct DoubleFaultState {
    pub code_segment: u32,
    pub instruction_pointer: u32,
    pub eflags: u32,
    pub stack_segment: u32,
    pub stack_pointer: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
    pub esi: u32,
    pub edi: u32,
    pub ebp: u32,
}

pub fn double_fault_state() -> DoubleFaultState {
    unsafe {
        let tss = core::ptr::addr_of!(NORMAL_TSS);
        DoubleFaultState {
            code_segment: read_tss_u32(tss, TSS_CS) & 0xffff,
            instruction_pointer: read_tss_u32(tss, TSS_EIP),
            eflags: read_tss_u32(tss, TSS_EFLAGS),
            stack_segment: read_tss_u32(tss, TSS_SS) & 0xffff,
            stack_pointer: read_tss_u32(tss, TSS_ESP),
            eax: read_tss_u32(tss, TSS_EAX),
            ebx: read_tss_u32(tss, TSS_EBX),
            ecx: read_tss_u32(tss, TSS_ECX),
            edx: read_tss_u32(tss, TSS_EDX),
            esi: read_tss_u32(tss, TSS_ESI),
            edi: read_tss_u32(tss, TSS_EDI),
            ebp: read_tss_u32(tss, TSS_EBP),
        }
    }
}

extern "C" fn double_fault_task() -> ! {
    crate::println!(
        "\nDOUBLE FAULT: switched to the emergency task stack.\nHalting before corrupted kernel state can continue."
    );
    let state = double_fault_state();
    let booted_slot = unsafe {
        crate::boot_info::BootInfo::from_address(crate::boot_info::BOOT_INFO_ADDRESS)
    }
    .map_or(0, |info| info.booted_slot);
    crate::debug::record_crash(
        8,
        None,
        state.code_segment,
        state.instruction_pointer,
        state.eflags,
        state.stack_segment,
        state.stack_pointer,
        Some(crate::debug::SavedRegisters {
            eax: state.eax,
            ebx: state.ebx,
            ecx: state.ecx,
            edx: state.edx,
            esi: state.esi,
            edi: state.edi,
            ebp: state.ebp,
        }),
        booted_slot,
    );
    if crate::debug::reboot_on_crash() {
        let info =
            unsafe { crate::boot_info::BootInfo::from_address(crate::boot_info::BOOT_INFO_ADDRESS) };
        crate::debug::handle_crash(info);
    }
    crate::arch::x86::halt_forever();
}
