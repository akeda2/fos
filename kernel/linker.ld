ENTRY(_start)

SECTIONS
{
  . = 0x10000;
  __kernel_start = .;

  . = ALIGN(4096);
  __text_start = .;
  .text : {
    KEEP(*(.text.entry))
    *(.text .text.*)
  }
  __text_end = .;

  . = ALIGN(4096);
  __rodata_start = .;
  .rodata : {
    *(.rodata .rodata.*)
  }
  __rodata_end = .;

  . = ALIGN(4096);
  .data : {
    *(.data .data.*)
  }

  .bss : ALIGN(16) {
    __bss_start = .;
    *(.bss .bss.* COMMON)
    __bss_end = .;
  }

  __kernel_end = .;
  ASSERT(__kernel_end < 0x98000, "kernel exceeds the reserved low-memory load region")
}
