//! Stand-ins for the modules that exist only on x86 so far, so the shared shell compiles on aarch64 and says what is missing.
//! Each one is replaced by the real module as its hardware (PCI, NICs, USB, the framebuffer) or EL0 support is ported; a stand-in
//! has the same signatures as the real thing, so the shell needs no `cfg`. Nothing here is meant to stay.

macro_rules! words {
    () => { core::str::SplitWhitespace<'_> };
}

fn missing(what: &str) {
    println!("{what}: not available on aarch64 yet.");
}

pub mod audioshell {
    pub fn command(_words: words!()) {
        super::missing("audio");
    }
}

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

pub mod fb {
    /// No framebuffer console in mf on aarch64 yet: tapes see no pixel screen.
    pub fn console_size() -> Option<(usize, usize)> {
        None
    }
    pub fn gfx_info() -> Option<(u32, u32)> {
        None
    }
    pub fn gfx_open() -> bool {
        false
    }
    pub fn gfx_close() {}
    pub fn gfx_fill(_x: u32, _y: u32, _width: u32, _height: u32, _rgb: u32) -> bool {
        false
    }
    pub unsafe fn gfx_blit(_x: u32, _y: u32, _width: u32, _height: u32, _source: *const u32, _stride: u32) -> bool {
        false
    }
    pub fn set_colours(_foreground: Option<&str>, _background: Option<&str>) -> bool {
        false
    }
}

pub mod install {
    pub fn cancel() {}
    pub fn command(_words: words!()) {
        super::missing("install");
    }
}

pub mod bootcode {
    pub fn cancel() {}
    pub fn command(_words: words!()) {
        super::missing("bootcode");
    }
}

pub mod ps2 {
    pub fn poll() {}
}
