/* Every v2 tape: a position-independent ELF64 linked at address 0. The kernel
 * (v2/abi/src/elf.rs) copies the PT_LOAD segments into its tape region and applies the
 * R_X86_64_RELATIVE relocations. One section list shared by all tapes. */
ENTRY(_start)

SECTIONS
{
  . = 0;
  .text : ALIGN(4096) {
    KEEP(*(.text.entry))
    *(.text .text.*)
  }
  . = ALIGN(4096);
  .dynsym   : { *(.dynsym) }
  .gnu.hash : { *(.gnu.hash) }
  .hash     : { *(.hash) }
  .dynstr   : { *(.dynstr) }
  .rela.dyn : { *(.rela.dyn) }
  .rodata   : { *(.rodata .rodata.*) }
  .data     : ALIGN(4096) { *(.data .data.*) }
  .dynamic  : { *(.dynamic) }
  .bss      : ALIGN(4096) { *(.bss .bss.*) *(COMMON) }
  /DISCARD/ : { *(.eh_frame*) *(.comment) }
}
