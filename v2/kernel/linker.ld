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

  /* Code: read + execute. */
  .text : ALIGN(4096) {
    __text_start = .;
    KEEP(*(.text.entry))
    *(.text .text.*)
    . = ALIGN(4096);
    __text_end = .;
  }

  /* Read-only data, including what the linker generates for the relocations the
   * loader applies. Listed explicitly so the range [__rodata_start, __rodata_end)
   * is exactly the read-only pages. */
  . = ALIGN(4096);
  __rodata_start = .;
  .dynsym   : { *(.dynsym) }
  .gnu.hash : { *(.gnu.hash) }
  .hash     : { *(.hash) }
  .dynstr   : { *(.dynstr) }
  .rela.dyn : { *(.rela.dyn) }
  .rodata   : { *(.rodata .rodata.*) }
  . = ALIGN(4096);
  __rodata_end = .;

  /* Writable data and zero-initialized data, mapped read + write + no-execute. */
  .data : ALIGN(4096) {
    __data_start = .;
    *(.data .data.*)
  }

  /* The dynamic section is a section of its own type; it stays writable. */
  .dynamic : { *(.dynamic) }

  .bss : ALIGN(4096) {
    __bss_start = .;
    *(.bss .bss.*)
    *(COMMON)
    /* Stacks sit last, each with a guard page below it that is left unmapped, so
     * an overflow faults instead of corrupting whatever lies beneath. The boot
     * stack is 64 KiB; the double-fault stack (used through the TSS's IST1) 16 KiB. */
    . = ALIGN(4096);
    __stack_guard = .;
    . += 4096;
    __stack_bottom = .;
    . += 64K;
    __stack_top = .;
    __ist_guard = .;
    . += 4096;
    __ist_bottom = .;
    . += 16K;
    __ist_top = .;
    __bss_end = .;
  }

  . = ALIGN(4096);
  __kernel_end = .;

  /DISCARD/ : { *(.eh_frame*) *(.comment) }
}
