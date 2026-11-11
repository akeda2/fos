//! What a trap from EL0 (or a fatal path) saw: the registers at the exception, laid out as `usermode.rs` saves them. The
//! shared kernel reads it through the accessors; the layout is the contract with the assembly there.

#[repr(C)]
pub struct Frame {
    /// x0-x30.
    pub x: [u64; 31],
    /// SP_EL0 at the exception.
    pub sp: u64,
    /// ELR_EL1: where the user code continues (after an `svc`, the next instruction; for a fault, the instruction).
    pub rip: u64,
    pub spsr: u64,
    pub esr: u64,
    pub far: u64,
    /// Which vector slot it came in on (8: synchronous from EL0, 9: IRQ from EL0).
    pub vector: u64,
    _pad: u64,
}

pub const FRAME_BYTES: usize = core::mem::size_of::<Frame>();
const _: () = assert!(FRAME_BYTES == 304 && core::mem::offset_of!(Frame, vector) == 288);

impl Frame {
    /// The system call number of an `svc` frame (x8).
    pub fn syscall_number(&self) -> u64 {
        self.x[8]
    }

    /// System call argument `index` (0-5): x0-x5, as the C ABI passes them.
    pub fn arg(&self, index: usize) -> u64 {
        self.x[index]
    }

    pub fn set_result(&mut self, value: u64) {
        self.x[0] = value;
    }

    /// Where the seventh and later arguments of a call start in user memory: the top of the caller's stack (no return address
    /// is pushed on this CPU).
    pub fn stack_args(&self) -> u64 {
        self.sp
    }

    /// What killed a process at EL0: the ESR exception class and the address involved (FAR for an abort, else the pc).
    pub fn fault(&self) -> (u64, u64) {
        let class = self.esr >> 26;
        (class, if matches!(class, 0x20 | 0x21 | 0x24 | 0x25) { self.far } else { self.rip })
    }
}

pub fn fatal_reset(_frame: &Frame) -> ! {
    crate::println!("*** fatal: halting.");
    crate::halt()
}
