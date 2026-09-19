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

    /// A short, port-I/O-based delay: a throwaway write to port `0x80`,
    /// conventionally unused (the original IBM PC used it for POST debug
    /// codes) but still wired to the ISA/LPC bus on essentially all real
    /// PC-compatible chipsets, so an `out` to it takes roughly the same
    /// bus cycle time as any other I/O port access. Some real hardware --
    /// unlike QEMU's emulated 8259, which accepts back-to-back writes with
    /// no delay -- needs a short gap between successive command bytes sent
    /// to slower legacy devices (the PIC being the classic example); this
    /// gives them one without needing a calibrated timer this early in
    /// boot, before the PIT has even been programmed.
    #[inline]
    pub unsafe fn io_wait() {
        unsafe { outb(0x80, 0) }
    }

    pub fn halt_forever() -> ! {
        loop {
            unsafe {
                core::arch::asm!("cli; hlt", options(nomem, nostack));
            }
        }
    }

    /// Asks the 8042 keyboard controller to pulse the CPU's own RESET
    /// line -- the classic real-mode/protected-mode-era way to reboot a
    /// PC-compatible without BIOS or ACPI help, still honored by QEMU's
    /// emulated 8042 and by essentially every real chipset for backward
    /// compatibility. `0x64` is the controller's status/command port
    /// (see `STATUS_PORT` in `keyboard.rs`); bit 1 of a status read means
    /// its input buffer still holds a byte the controller hasn't consumed
    /// yet, so this polls that clear first (the same convention
    /// `keyboard.rs`'s own status-port polling uses) before sending
    /// `0xfe` ("pulse output port bit 0", which is wired to RESET) to
    /// that same port as a command.
    pub fn reset() -> ! {
        loop {
            let status = unsafe { inb(0x64) };
            if status & 0x02 == 0 {
                break;
            }
        }
        unsafe { outb(0x64, 0xfe) };
        // A real reset never returns control here; this is only reached
        // if the controller somehow didn't respond (never observed in
        // QEMU), so fail safe by halting rather than falling through to
        // whatever undefined state the caller didn't expect.
        halt_forever()
    }
}
