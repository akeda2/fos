//! Stand-ins for the modules that exist only on x86 so far, so the shared shell compiles on aarch64 and says what is missing.
//! Each one is replaced by the real module as its hardware (PCI, NICs, USB, the framebuffer) or EL0 support is ported; a stand-in
//! has the same signatures as the real thing, so the shell needs no `cfg`. Nothing here is meant to stay.

/// COM2-COM4 and USB serial adapters (`kernel_core::serialports`) exist on x86 only: a tape sees no ports.
pub mod tapeserial {
    pub fn count() -> u64 {
        0
    }
    pub fn info(_number: u64, _out: u64) -> u64 {
        0
    }
    pub fn open(_number: u64, _baud: u64, _format: u64) -> i64 {
        0
    }
    pub fn close(_number: u64) {}
    pub fn read(_number: u64, _out: u64, _capacity: u64) -> u64 {
        0
    }
    pub fn write(_number: u64, _data: u64, _length: u64) -> u64 {
        0
    }
    pub fn lines(_number: u64) -> u64 {
        0
    }
    pub fn set_lines(_number: u64, _lines: u64) {}
    pub fn set_break(_number: u64, _on: u64) {}
    pub fn process_ended(_pid: u64) {}
}

pub mod ps2 {
    pub fn poll() {}
}
