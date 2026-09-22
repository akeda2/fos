ENTRY(tape_main)

SECTIONS {
    . = 0x100000;

    .text : {
        *(.longmode_shim)
        *(.longmode_shim.*)
        *(.text)
        *(.text.*)
    }

    .rodata : {
        *(.rodata)
        *(.rodata.*)
    }

    .data : {
        *(.data)
        *(.data.*)
    }

    .bss : {
        *(.bss)
        *(.bss.*)
        *(COMMON)
    }
}
