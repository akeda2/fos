pub mod x86 {
    #[inline]
    pub unsafe fn inb(port: u16) -> u8 {
        let value: u8;
        unsafe {
            core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    #[inline]
    pub unsafe fn outb(port: u16, value: u8) {
        unsafe {
            core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
        }
    }

    /// 32-bit input, used for PCI configuration-space access (the
    /// `0xcf8`/`0xcfc` address/data port pair only accepts full
    /// double-word transfers).
    #[inline]
    pub unsafe fn inl(port: u16) -> u32 {
        let value: u32;
        unsafe {
            core::arch::asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// 32-bit output; see `inl`.
    #[inline]
    pub unsafe fn outl(port: u16, value: u32) {
        unsafe {
            core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
        }
    }

    /// 16-bit input, used for the RTL8139 NIC's 16-bit registers (`CAPR`,
    /// `CBR`, `IMR`, `ISR`). Kept alongside `outw` for symmetry even
    /// though this driver only currently writes those registers.
    #[inline]
    #[allow(dead_code)]
    pub unsafe fn inw(port: u16) -> u16 {
        let value: u16;
        unsafe {
            core::arch::asm!("in ax, dx", in("dx") port, out("ax") value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// 16-bit output; see `inw`.
    #[inline]
    pub unsafe fn outw(port: u16, value: u16) {
        unsafe {
            core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
        }
    }

    pub fn halt_forever() -> ! {
        loop {
            unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)); }
        }
    }
}

