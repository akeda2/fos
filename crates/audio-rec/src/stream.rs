//! A `Sink` over the kernel's `fs_stream_*` tape calls (a file that grows as it is written, on FOSFS or on a FAT32 /
//! exFAT disk named `diskN/NAME`). It also times the big block writes: a disk that is slower than the audio is what a
//! recorder has to tell its user about.

use crate::recorder::Sink;

/// Which kernel calls the sink uses: the single stream of `fs_stream_*`, or a handle of ABI 30's `fs_stream_*2`.
#[derive(Clone, Copy)]
enum Calls {
    Single {
        open: extern "C" fn(*const u8, usize, u64) -> isize,
        write: extern "C" fn(u64, *const u8, usize, u64) -> isize,
        close: extern "C" fn(u64) -> isize,
    },
    Handle {
        open: extern "C" fn(*const u8, usize, u64, *mut u64) -> isize,
        write: extern "C" fn(isize, u64, *const u8, usize, u64) -> isize,
        close: extern "C" fn(isize, u64) -> isize,
        handle: isize,
    },
}

pub struct StreamSink {
    calls: Calls,
    micros: fn() -> u64,
    /// Only writes of at least this many bytes count towards the speed figures (a checkpoint is one sector).
    min_block: usize,
    pub write_us: u64,
    pub write_max_us: u64,
    pub writes: u64,
    pub bytes: u64,
}

impl StreamSink {
    pub const fn new(
        open: extern "C" fn(*const u8, usize, u64) -> isize,
        write: extern "C" fn(u64, *const u8, usize, u64) -> isize,
        close: extern "C" fn(u64) -> isize,
        micros: fn() -> u64,
        min_block: usize,
    ) -> StreamSink {
        StreamSink { calls: Calls::Single { open, write, close }, micros, min_block, write_us: 0, write_max_us: 0, writes: 0, bytes: 0 }
    }

    /// A sink on its own stream handle (kernel ABI 30), so several can be open at once. `open` must be given the number of
    /// bytes to reserve, never 0 (the first stream would take the largest free run and starve the others).
    pub const fn with_handle(
        open: extern "C" fn(*const u8, usize, u64, *mut u64) -> isize,
        write: extern "C" fn(isize, u64, *const u8, usize, u64) -> isize,
        close: extern "C" fn(isize, u64) -> isize,
        micros: fn() -> u64,
        min_block: usize,
    ) -> StreamSink {
        StreamSink { calls: Calls::Handle { open, write, close, handle: -1 }, micros, min_block, write_us: 0, write_max_us: 0, writes: 0, bytes: 0 }
    }

    /// Kilobytes per second over the big writes so far (0 before any).
    pub fn kib_per_second(&self) -> u64 {
        if self.write_us == 0 { 0 } else { self.bytes * 1000 / 1024 / (self.write_us / 1000).max(1) }
    }
}

impl Sink for StreamSink {
    fn open(&mut self, name: &[u8], reserve: u64) -> Result<u64, i32> {
        match &mut self.calls {
            Calls::Single { open, .. } => {
                let capacity = open(name.as_ptr(), name.len(), reserve);
                if capacity < 0 { Err(capacity as i32) } else { Ok(capacity as u64) }
            }
            Calls::Handle { open, handle, .. } => {
                let mut capacity = 0u64;
                let opened = open(name.as_ptr(), name.len(), reserve, &mut capacity);
                if opened < 0 {
                    return Err(opened as i32);
                }
                *handle = opened;
                Ok(capacity)
            }
        }
    }

    fn write(&mut self, offset: u64, data: &[u8], length: u64) -> Result<(), i32> {
        let began = (self.micros)();
        let status = match self.calls {
            Calls::Single { write, .. } => write(offset, data.as_ptr(), data.len(), length),
            Calls::Handle { write, handle, .. } => write(handle, offset, data.as_ptr(), data.len(), length),
        };
        let spent = (self.micros)() - began;
        if data.len() >= self.min_block {
            (self.write_us, self.write_max_us, self.writes, self.bytes) = (self.write_us + spent, self.write_max_us.max(spent), self.writes + 1, self.bytes + data.len() as u64);
        }
        if status == 0 { Ok(()) } else { Err(status as i32) }
    }

    fn close(&mut self, length: u64) -> Result<(), i32> {
        let status = match &mut self.calls {
            Calls::Single { close, .. } => close(length),
            Calls::Handle { close, handle, .. } => {
                let status = close(*handle, length);
                *handle = -1;
                status
            }
        };
        if status == 0 { Ok(()) } else { Err(status as i32) }
    }
}
