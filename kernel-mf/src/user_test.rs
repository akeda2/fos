//! Ring 3 self-test (slice 8b): five tiny processes, hand-assembled and position independent, exercise the system calls, the
//! pointer checks, exception containment and preemption of user code.

use crate::process::{self, How};
use crate::user_blobs::*;
use crate::{fpu, pmm, sched, time};

fn blob(start: &'static u8, end: &'static u8) -> &'static [u8] {
    let start = start as *const u8;
    unsafe { core::slice::from_raw_parts(start, end as *const u8 as usize - start as usize) }
}

fn halt_a_tick() {
    crate::usermode::halt_a_tick();
}

/// A minimal ET_DYN image: ELF header, two program headers (PT_LOAD read/write/execute, PT_DYNAMIC), the dynamic table, one
/// RELATIVE relocation that points the blob's message pointer at the message, then the blob. Entry is the blob's first byte.
fn build_elf() -> alloc::vec::Vec<u8> {
    let code = blob(unsafe { &user_elf_start }, unsafe { &user_elf_end });
    let start = unsafe { &user_elf_start } as *const u8 as usize;
    let pointer_at = unsafe { &user_elf_pointer } as *const u8 as usize - start;
    let message_at = unsafe { &user_elf_message } as *const u8 as usize - start;
    let (dynamic, rela, blob_at) = (64u64 + 2 * 56, 64u64 + 2 * 56 + 48, 64u64 + 2 * 56 + 48 + 24);
    let total = blob_at + code.len() as u64;
    let mut out = alloc::vec::Vec::new();
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&3u16.to_le_bytes()); // ET_DYN
    out.extend_from_slice(&crate::usermode::ELF_MACHINE.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&blob_at.to_le_bytes()); // entry
    out.extend_from_slice(&64u64.to_le_bytes()); // program headers
    out.extend_from_slice(&0u64.to_le_bytes()); // no sections
    out.extend_from_slice(&0u32.to_le_bytes());
    for half in [64u16, 56, 2, 0, 0, 0] {
        out.extend_from_slice(&half.to_le_bytes());
    }
    let phdr = |out: &mut alloc::vec::Vec<u8>, kind: u32, flags: u32, from: u64, size: u64| {
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        for value in [from, from, from, size, size, 0x1000] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    };
    phdr(&mut out, 1, 7, 0, total);
    phdr(&mut out, 2, 6, dynamic, 48);
    for value in [7u64, rela, 8, 24, 0, 0] {
        out.extend_from_slice(&value.to_le_bytes()); // DT_RELA, DT_RELASZ, DT_NULL
    }
    for value in [blob_at + pointer_at as u64, crate::usermode::RELATIVE_RELOCATION, blob_at + message_at as u64] {
        out.extend_from_slice(&value.to_le_bytes()); // offset, the CPU's RELATIVE relocation type, addend
    }
    out.extend_from_slice(code);
    out
}

pub fn run() {
    let free_before = pmm::stats().free;
    let hello = process::spawn("hello", blob(unsafe { &user_hello_start }, unsafe { &user_hello_end })).expect("hello");
    let badptr = process::spawn("badptr", blob(unsafe { &user_badptr_start }, unsafe { &user_badptr_end })).expect("badptr");
    let fault = process::spawn("fault", blob(unsafe { &user_fault_start }, unsafe { &user_fault_end })).expect("fault");
    let overflow = process::spawn("overflow", blob(unsafe { &user_overflow_start }, unsafe { &user_overflow_end })).expect("overflow");
    let gp = process::spawn("gp", blob(unsafe { &user_gp_start }, unsafe { &user_gp_end })).expect("gp");
    let spin = process::spawn("spin", blob(unsafe { &user_spin_start }, unsafe { &user_spin_end })).expect("spin");
    let elf = process::spawn_elf("elf", &build_elf(), b"").expect("elf");
    let fp_a = blob(unsafe { &user_fp_a_start }, unsafe { &user_fp_a_end });
    let fp_b = blob(unsafe { &user_fp_b_start }, unsafe { &user_fp_b_end });
    let mut fp_ids = alloc::vec![];
    for (name, code) in [("fp-a1", fp_a), ("fp-b1", fp_b), ("fp-a2", fp_a), ("fp-b2", fp_b)] {
        fp_ids.push(process::spawn(name, code).expect("fp process"));
    }
    #[cfg(target_arch = "x86_64")]
    let avx = (fpu::level() == fosfor_abi::tape::FP_AVX2)
        .then(|| process::spawn("avx", blob(unsafe { &user_avx_start }, unsafe { &user_avx_end })).expect("avx process"));
    #[cfg(not(target_arch = "x86_64"))]
    let avx: Option<u64> = None;
    let expected = 7 + fp_ids.len() + usize::from(avx.is_some());
    let start = time::ticks();
    while time::ticks() - start < 4 {
        halt_a_tick();
    }
    sched::kill(spin);
    let mut waited = 0;
    while process::outcomes().len() < expected && waited < 300 {
        halt_a_tick();
        waited += 1;
    }
    // A dead thread's stack is freed by the next thread that runs on its CPU.
    for _ in 0..4 {
        sched::yield_now();
        halt_a_tick();
    }
    let outcomes = process::outcomes();
    let find = |id: u64| outcomes.iter().find(|o| o.pid == id);
    let hello_ok = matches!(find(hello), Some(o) if o.how == How::Exited(7) && o.written == 2 * HELLO_BYTES);
    let badptr_ok = matches!(find(badptr), Some(o) if o.how == How::Exited(-1));
    let fault_ok = matches!(find(fault), Some(o) if matches!(o.how, How::Fault(crate::usermode::PAGE_FAULT, 0xffff_8000_0000_0000)));
    let guard = process::STACK_TOP - process::STACK_BYTES;
    let overflow_ok = matches!(find(overflow), Some(o) if matches!(o.how, How::Fault(crate::usermode::PAGE_FAULT, address) if (guard - 16..guard).contains(&address)));
    let gp_ok = matches!(find(gp), Some(o) if matches!(o.how, How::Fault(crate::usermode::ILLEGAL_INSTRUCTION, _)));
    let elf_ok = matches!(find(elf), Some(o) if o.how == How::Exited(0) && o.written == 19);
    let fp_ok = fp_ids.iter().all(|&id| matches!(find(id), Some(o) if o.how == How::Exited(0)))
        && avx.is_none_or(|id| matches!(find(id), Some(o) if o.how == How::Exited(0)));
    let spin_ok = matches!(find(spin), Some(o) if o.how == How::Killed);
    // Fresh stack slots can need a new page-table page (128 KiB stacks: one per ~15 threads); tables are kept for reuse, not leaked.
    // A dead thread's stack is released by the CPU that switches away from it, which may be a moment after the outcome was seen.
    // The boot thread cannot sleep (`sleep_ticks` returns at once when nothing else is eligible on CPU 0), so wait the way the
    // loops above do. Under QEMU's TCG with 8 vCPUs a thread's exit can come a second after its outcome: poll up to 10 s.
    for _ in 0..1000 {
        if free_before.saturating_sub(pmm::stats().free) <= 2 {
            break;
        }
        sched::yield_now();
        halt_a_tick();
    }
    let leaked = free_before.saturating_sub(pmm::stats().free).saturating_sub(2);
    let ok = |b: bool| if b { "ok" } else { "FAILED" };
    // An image that would run into the stack is refused before anything is allocated.
    let mut big = build_elf();
    big[104..112].copy_from_slice(&(process::MAX_IMAGE + 4096).to_le_bytes());
    let big_ok = matches!(process::spawn_elf("big", &big, b""), Err(process::SpawnError::TooLarge));
    println!(
        "{}: hello {}, bad pointer {}, page fault {}, stack guard {}, {} {}, preempted spinner {}, ELF {}, big image {}, {}{} state {}, {} frames not returned.",
        crate::usermode::PRIVILEGE, ok(hello_ok), ok(badptr_ok), ok(fault_ok), ok(overflow_ok), crate::usermode::ILLEGAL_NAME, ok(gp_ok), ok(spin_ok), ok(elf_ok), ok(big_ok), crate::usermode::FP_NAME, if avx.is_some() { "+AVX" } else { "" }, ok(fp_ok), leaked
    );
}
