/* This tape is loaded at pmem::TAPE_BASE (0x100000). */
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
  ASSERT(__tape_end - __tape_start <= 0x400000, "audio-rtp tape exceeds the fixed 4 MiB tape region")
}
