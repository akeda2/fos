.section .text.entry, "ax"
.global _start
.extern kmain
.extern __bss_start
.extern __bss_end

# These helpers install the kernel-owned GDT after boot and after a BIOS disk
# transfer. The latter returns with stage two's smaller GDT loaded, which has
# no TSS descriptors for the double-fault task gate.
.global load_kernel_gdt
.global load_kernel_gdt_and_task_register

_start:
    cli
    mov esp, offset kernel_stack_top
    xor ebp, ebp

    # Soft reboot (see kernel/src/reboot.rs) re-enters this exact entry point
    # after stage two has just re-read a fresh kernel image from disk into
    # this same physical memory (0x10000) that this VM instance was already
    # executing as code moments earlier -- a pattern no other boot path in
    # this project exercises (an ordinary cold boot never overwrites
    # already-executing code). Empirically, without this loop, a soft reboot
    # reliably jumps into the wild shortly after landing here, even when the
    # freshly-read bytes are byte-for-byte identical to what was already
    # resident (ruling out a stale/incomplete disk read as the cause). A
    # bare CPU delay loop of the same length does not fix it; a loop that
    # performs real port I/O does, every time. The leading theory is a
    # QEMU/TCG quirk around invalidating a cached code translation for this
    # physical range after a BIOS-emulated disk write lands underneath it --
    # `out` forces TCG to exit the current translation block, which appears
    # to force it to pick up the fresh bytes for what follows. This has not
    # been proven against QEMU internals or real hardware; it is recorded
    # here as the best available explanation for a fix confirmed reliable
    # across many repeated runs. Port 0x80 is the same conventionally-unused
    # POST-diagnostic port `interrupts.rs`'s `io_wait()` already uses for
    # PIC/PIT command pacing, so this has no observable side effect.
    mov ecx, 2000
.soft_reboot_resync:
    out 0x80, al
    loop .soft_reboot_resync

    mov edi, offset __bss_start
    mov ecx, offset __bss_end
    sub ecx, edi
    xor eax, eax
    rep stosb

    push ebx
    call kmain

.halt:
    cli
    hlt
    jmp .halt

load_kernel_gdt:
    mov eax, [esp + 4]
    lgdt [eax]
    push 0x08
    mov eax, offset load_kernel_gdt_after
    push eax
    retf
load_kernel_gdt_after:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    ret

load_kernel_gdt_and_task_register:
    mov eax, [esp + 4]
    push eax
    call load_kernel_gdt
    add esp, 4
    mov ax, 0x28
    ltr ax
    ret

.section .bss
.align 4096
.global kernel_stack_guard
kernel_stack_guard:
    # `paging.rs` deliberately leaves this page unmapped after early boot.
    # A downward stack overflow faults here instead of corrupting adjacent
    # state; the double-fault task uses a separate emergency stack.
    .skip 4096
.global kernel_stack_canary
kernel_stack_canary:
    # `stack.rs` arms and IRQ0 checks this tripwire. Keep it immediately
    # below the downward-growing kernel stack so an overflow damages it first.
    .skip 16
.global kernel_stack
kernel_stack:
    # Networking tapes can run nested inside `term net` and borrow this
    # stack for their fixed packet buffers and TCP state.  Keep enough
    # headroom for that call chain without introducing a tape-specific ABI.
    #
    # Grown from 32 KiB to 64 KiB after a real double fault: the interactive
    # `start` shell command re-enters `execute()` one level deeper than the
    # boot-time start-script path (`execute("start")` -> `start_command()`
    # -> `execute_start_file()` -> `execute(script line)`), and a heavy
    # script line such as `tape run router @router.cfg` (multiple NICs,
    # DHCP, health checks, NAT) pushes the combined depth of that extra
    # `execute()`/`tape_command`/`tape_run` chain past 32 KiB even after
    # `execute()`'s own frame was shrunk (see the `#[inline(never)]` note
    # on `execute()` in `shell.rs`). Unlike when this was first sized,
    # `__kernel_end` no longer sits comfortably below the reserved region's
    # ceiling (`linker.ld`'s `ASSERT`, now `0x9f000`) -- confirmed by
    # measurement (`size build/kernel.elf`), current `.bss` usage leaves
    # only a few tens of KiB of headroom before it, not room to casually
    # double this reservation again. Growing this further would likely
    # need bumping that ceiling too (see AGENTS.md's "Low-memory map").
    .skip 65536
kernel_stack_top:
