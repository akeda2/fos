/* The v2 kernel: a position-independent ELF64 image linked at address 0. The
 * loader (v2/loader/src/elf.rs) allocates whatever physical pages the firmware has
 * free, copies the segments there, applies the R_X86_64_RELATIVE relocations and
 * jumps in; everything is identity-mapped, so virtual = physical afterwards. A
 * fixed link address was tried first and failed on the very first firmware tested:
 * OVMF keeps its own boot-services data at 16 MiB. There is no low-memory budget
 * any more: the image may be as large as a payload slot (8 MiB) allows.
 */
ENTRY(_start)

SECTIONS
{
  . = 0;
  __kernel_start = .;

  .text : ALIGN(4096) {
    KEEP(*(.text.entry))
    *(.text .text.*)
  }

  .rodata : ALIGN(4096) {
    *(.rodata .rodata.*)
  }

  .data : ALIGN(4096) {
    *(.data .data.*)
  }

  .bss : ALIGN(4096) {
    __bss_start = .;
    *(.bss .bss.*)
    *(COMMON)
    /* The boot stack lives at the end of .bss: 64 KiB, growing down. */
    . = ALIGN(16);
    __stack_bottom = .;
    . += 64K;
    __stack_top = .;
    __bss_end = .;
  }

  . = ALIGN(4096);
  __kernel_end = .;

  /DISCARD/ : { *(.eh_frame*) *(.comment) }
}
