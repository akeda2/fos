/* Fixes this tape's load address at pmem::TAPE_BASE (see
 * kernel/src/pmem.rs), the one physical address every tape is always
 * loaded at. Identical to tapes/vga-colors/linker.ld -- see that file's
 * own comments for the full reasoning.
 */
ENTRY(_start)

SECTIONS
{
  . = 0x100000;
  __tape_start = .;

  .text : ALIGN(16) {
    KEEP(*(.text.entry))
    *(.text .text.*)
  }

  .rodata : ALIGN(16) {
    *(.rodata .rodata.*)
  }

  .data : ALIGN(16) {
    *(.data .data.*)
  }

  .bss : ALIGN(16) {
    __bss_start = .;
    *(.bss .bss.* COMMON)
    __bss_end = .;
  }

  __tape_end = .;
  /* 4 MiB, matching kernel/src/pmem.rs's TAPE_LEN. */
  ASSERT(__tape_end - __tape_start <= 0x400000, "tape exceeds the fixed 4 MiB tape region (see pmem::TAPE_LEN)")
}
