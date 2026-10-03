//! What happens when the CPU raises an exception the kernel did not expect: say what,
//! where and why, in words, then stop. (Later milestones add the crash record and the
//! optional reboot of v1's debug mode; the legible report is the part that must exist
//! from the first fault on.)

use crate::{cpu, paging, println};

/// The register state at the moment of an interrupt, as `isr_common` builds it.
#[repr(C)]
pub struct Frame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

fn exception_name(vector: u64) -> &'static str {
    match vector {
        0 => "#DE divide error",
        1 => "#DB debug",
        2 => "NMI non-maskable interrupt",
        3 => "#BP breakpoint",
        4 => "#OF overflow",
        5 => "#BR bound range exceeded",
        6 => "#UD invalid opcode",
        7 => "#NM device not available",
        8 => "#DF double fault",
        10 => "#TS invalid TSS",
        11 => "#NP segment not present",
        12 => "#SS stack-segment fault",
        13 => "#GP general protection fault",
        14 => "#PF page fault",
        16 => "#MF x87 floating-point error",
        17 => "#AC alignment check",
        18 => "#MC machine check",
        19 => "#XM SIMD floating-point error",
        20 => "#VE virtualization exception",
        _ => "reserved exception",
    }
}

/// Reports a fatal exception and stops the machine.
pub fn fatal(frame: &Frame) -> ! {
    // The kernel may have been in the middle of printing; start on a fresh line.
    println!();
    println!("*** CPU EXCEPTION: {} (vector {}), error code {:#x}", exception_name(frame.vector), frame.vector, frame.error);
    println!("    at RIP {:#018x}  CS {:#x}  RFLAGS {:#x}", frame.rip, frame.cs, frame.rflags);
    println!("    RSP {:#018x}  SS {:#x}  ({})", frame.rsp, frame.ss, paging::describe(frame.rsp));
    println!("    code at RIP is {}", paging::describe(frame.rip));
    println!(
        "    RAX {:#018x} RBX {:#018x} RCX {:#018x} RDX {:#018x}",
        frame.rax, frame.rbx, frame.rcx, frame.rdx
    );
    println!(
        "    RSI {:#018x} RDI {:#018x} RBP {:#018x} R8  {:#018x}",
        frame.rsi, frame.rdi, frame.rbp, frame.r8
    );
    match frame.vector {
        14 => {
            let address = cpu::read_cr2();
            let error = frame.error;
            println!("    page fault at {:#018x}: {}", address, paging::describe(address));
            println!(
                "    cause: {} a {} page by {} code ({}{}{})",
                if error & 1 != 0 { "a protection violation on" } else { "access to a not-present" },
                if error & 16 != 0 { "an instruction fetch from" } else if error & 2 != 0 { "a write to" } else { "a read from" },
                if error & 4 != 0 { "user" } else { "kernel" },
                if error & 1 != 0 { "page present" } else { "page not present" },
                if error & 8 != 0 { ", reserved bit set" } else { "" },
                if error & 16 != 0 { ", NX violation" } else { "" },
            );
        }
        8 => {
            println!("    a double fault means the CPU could not deliver an earlier exception.");
            if paging::describe(frame.rsp).contains("stack guard") {
                println!("    RSP is in a stack guard page: the kernel stack overflowed.");
            }
        }
        13 => println!("    selector/error detail: {:#x}", frame.error),
        _ => {}
    }
    cpu::interrupts_disable();
    let debug = crate::config::debug();
    if debug.reboot_on_crash {
        println!("*** debug file: reboot-on-crash; resetting in {} s", debug.reboot_delay_seconds);
        crate::apic::busy_wait_ms(u64::from(debug.reboot_delay_seconds) * 1000);
        crate::power::reset(crate::power::runtime_services());
    }
    println!("*** halting");
    cpu::halt()
}
