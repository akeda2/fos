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

    pub fn halt_forever() -> ! {
        loop {
            unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)); }
        }
    }
}

