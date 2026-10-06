//! A `Sink` over the kernel's `fs_stream_*` tape calls (a file that grows as it is written, on FOSFS or on a FAT32 /
//! exFAT disk named `diskN/NAME`). It also times the big block writes: a disk that is slower than the audio is what a
//! recorder has to tell its user about.

use crate::recorder::Sink;

pub struct StreamSink {
    open: extern "C" fn(*const u8, usize, u64) -> isize,
    write: extern "C" fn(u64, *const u8, usize, u64) -> isize,
    close: extern "C" fn(u64) -> isize,
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
        StreamSink { open, write, close, micros, min_block, write_us: 0, write_max_us: 0, writes: 0, bytes: 0 }
    }

    /// Kilobytes per second over the big writes so far (0 before any).
    pub fn kib_per_second(&self) -> u64 {
        if self.write_us == 0 { 0 } else { self.bytes * 1000 / 1024 / (self.write_us / 1000).max(1) }
    }
}

impl Sink for StreamSink {
    fn open(&mut self, name: &[u8], reserve: u64) -> Result<u64, i32> {
        let capacity = (self.open)(name.as_ptr(), name.len(), reserve);
        if capacity < 0 { Err(capacity as i32) } else { Ok(capacity as u64) }
    }

    fn write(&mut self, offset: u64, data: &[u8], length: u64) -> Result<(), i32> {
        let began = (self.micros)();
        let status = (self.write)(offset, data.as_ptr(), data.len(), length);
        let spent = (self.micros)() - began;
        if data.len() >= self.min_block {
            (self.write_us, self.write_max_us, self.writes, self.bytes) = (self.write_us + spent, self.write_max_us.max(spent), self.writes + 1, self.bytes + data.len() as u64);
        }
        if status == 0 { Ok(()) } else { Err(status as i32) }
    }

    fn close(&mut self, length: u64) -> Result<(), i32> {
        let status = (self.close)(length);
        if status == 0 { Ok(()) } else { Err(status as i32) }
    }
}
