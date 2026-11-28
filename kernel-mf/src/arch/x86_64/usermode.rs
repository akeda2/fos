//! x86-64 ring 3 entry and exit: the `syscall` instruction path, SMEP/SMAP and the jump into user mode (`process.rs` and
//! `tapeapi.rs` are the portable halves).
//!
//! Entering the kernel from ring 3:
//! * interrupts and exceptions use the TSS's rsp0 (the running thread's kernel stack, set by the scheduler at every switch) and
//!   the shared stubs, which `swapgs` when CS says ring 3 (`isr_stubs.S`, `tools/gen_isr_stubs.py --swapgs`);
//! * `syscall` (the fast path) lands in `syscall_entry`, which swaps GS the same way, switches to the kernel stack and builds
//!   the same frame an interrupt would, so one return path (`iretq`) serves both. SFMASK clears IF, DF, TF and AC.
//!
//! What protects the kernel: SMEP stops it from executing user pages and SMAP from touching them except in the `user_access`
//! windows (the places that copy to or from a process).

use crate::{cpu, gdt};

const MSR_EFER: u32 = 0xc000_0080;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_SFMASK: u32 = 0xc000_0084;

/// A vector number no CPU delivers: what the `syscall` stub puts in `Frame::vector`.
pub const SYSCALL_VECTOR: u64 = 0x1000;

core::arch::global_asm!(
    r#"
    .text
    .global syscall_entry
syscall_entry:
    swapgs
    mov gs:[8], rsp                  # Cpu::scratch_rsp
    mov rsp, gs:[16]                 # Cpu::kernel_rsp
    push 0x1b                        # ss  (user data)
    push qword ptr gs:[8]            # the user's rsp
    push r11                         # rflags, as syscall saved it
    push 0x23                        # cs  (user code)
    push rcx                         # rip, as syscall saved it
    push 0                           # error code
    push 0x1000                      # SYSCALL_VECTOR
    jmp isr_common_body              # GS is already the kernel's: skip the stub's swapgs

    .global enter_user_mode
enter_user_mode:                     # (rdi = entry, rsi = user stack top, rdx, rcx = arguments): never returns
    cli                              # between swapgs and iretq an interrupt would find the wrong GS
    push 0x1b
    push rsi
    push 0x202                       # IF on
    push 0x23
    push rdi
    mov rdi, rdx                     # the entry's first two arguments
    mov r8, rcx
    xor eax, eax
    xor ebx, ebx
    xor ecx, ecx
    xor edx, edx
    xor esi, esi
    xor ebp, ebp
    xor r9d, r9d
    xor r10d, r10d
    xor r11d, r11d
    xor r12d, r12d
    xor r13d, r13d
    xor r14d, r14d
    xor r15d, r15d
    mov rsi, r8
    xor r8d, r8d
    swapgs
    iretq
"#
);

unsafe extern "C" {
    fn syscall_entry();
    fn enter_user_mode(entry: u64, stack_top: u64, argument: u64, second: u64) -> !;
}

/// Starts running user code: `entry(argument, second)` on the stack ending at `stack_top`. Never returns.
pub fn enter(entry: u64, stack_top: u64, argument: u64, second: u64) -> ! {
    unsafe { enter_user_mode(entry, stack_top, argument, second) }
}

/// Per CPU: `syscall` on, SMEP on, and the entry point and flag mask set.
pub fn init_cpu() {
    cpu::write_msr(MSR_EFER, cpu::read_msr(MSR_EFER) | 1); // SCE
    // STAR: bits 47:32 the kernel CS (SS is the next descriptor); 63:48 the base `sysret` derives the user selectors from.
    cpu::write_msr(MSR_STAR, u64::from(0x10u16) << 48 | u64::from(gdt::KERNEL_CODE) << 32);
    cpu::write_msr(MSR_LSTAR, syscall_entry as *const () as u64);
    cpu::write_msr(MSR_SFMASK, 0x200 | 0x100 | 0x400 | 0x4_0000); // IF, TF, DF, AC
    let features = cpu::cpuid(7, 0).1;
    // CR4 bit 20 SMEP (no execution of user pages), bit 21 SMAP (no access to them unless RFLAGS.AC is set: `user_access`).
    let wanted = (features & (1 << 7) != 0) as u64 * (1 << 20) | (features & (1 << 20) != 0) as u64 * (1 << 21);
    if wanted != 0 {
        unsafe {
            let cr4: u64;
            core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
            core::arch::asm!("mov cr4, {}", in(reg) cr4 | wanted, options(nomem, nostack, preserves_flags));
        }
    }
    if features & (1 << 20) != 0 && !SMAP_ON.swap(true, core::sync::atomic::Ordering::AcqRel) {
        println!("SMAP: the kernel reaches user memory only through stac/clac windows (copy_to_user, user_bytes, gfx_blit).");
    }
}

/// Set once the CPUs run with CR4.SMAP (they all have the same features); `stac`/`clac` are #UD without it.
static SMAP_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Runs `f`, which touches user memory, with SMAP's check lifted (RFLAGS.AC set) for exactly that long. Callers check the range
/// first (`vm::user_range_ok`). An interrupt clears AC on entry and `iretq` restores it, so a preemption inside is safe.
pub fn user_access<R>(f: impl FnOnce() -> R) -> R {
    let on = SMAP_ON.load(core::sync::atomic::Ordering::Relaxed);
    if on {
        unsafe { core::arch::asm!("stac", options(nostack)) };
    }
    let result = f();
    if on {
        unsafe { core::arch::asm!("clac", options(nostack)) };
    }
    result
}


/// Nothing to do: x86 keeps instruction fetch coherent with stores.
pub fn sync_code(_address: u64, _length: u64) {}

// The tape shim, mapped into every tape's address space by `tapeapi::map_support` (entry wrapper, thread start, one stub per
// table slot). The stub numbering and layout are the contract with `tapeapi`; the instructions are this CPU's.
core::arch::global_asm!(
    r#"
    .text
    .balign 16
    .global tape_shim_start, tape_shim_stubs, tape_shim_end, tape_thread_start
tape_shim_start:                     # entered with rdi = the table, rsi = the tape's entry point
    call rsi
    mov eax, 1                       # the tape returned: exit(0)
    xor edi, edi
    syscall
    ud2
    .balign 16
tape_thread_start:                   # another thread: rdi = its entry point, rsi = its argument (the stack is 16-aligned)
    mov rax, rdi
    mov rdi, rsi
    call rax
    mov eax, 5                       # the entry returned: end this thread only
    syscall
    ud2
    .balign 16
tape_shim_stubs:
    .set slot, 0
    .rept 192
    mov r10, rcx
    mov eax, 0x100 + slot
    syscall
    ret
    .balign 16
    .set slot, slot + 1
    .endr
tape_shim_end:
"#
);


/// ELF `e_machine` and the RELATIVE relocation type of tapes for this CPU (the self-test builds an image by hand).
pub const ELF_MACHINE: u16 = 62;
pub const RELATIVE_RELOCATION: u64 = 8;
/// The exception classes `How::Fault` carries: a page fault, and what an undefined or privileged instruction raises (#GP).
pub const PAGE_FAULT: u64 = 14;
pub const ILLEGAL_INSTRUCTION: u64 = 13;

/// How the self-test names things on this CPU.
pub const PRIVILEGE: &str = "Ring 3";
pub const ILLEGAL_NAME: &str = "#GP";
pub const FP_NAME: &str = "SSE";

/// Lets one timer tick arrive: enable interrupts and halt until one does.
pub fn halt_a_tick() {
    unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
}
