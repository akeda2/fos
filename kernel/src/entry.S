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
    # on `execute()` in `shell.rs`). `__kernel_end` sits far enough below
    # the reserved region's `0x50000` ceiling (`linker.ld`'s `ASSERT`) that
    # doubling this reservation still leaves headroom.
    .skip 65536
kernel_stack_top:
