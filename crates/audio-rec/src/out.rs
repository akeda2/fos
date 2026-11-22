//! Where a recorded file goes, whatever the volume: `Out` is a stream of the kernel's `fs_stream_*` calls (FOSFS, FAT32,
//! exFAT: `StreamSink`) or a file by path on an ext volume (`VfsSink`), behind the one `Sink` a `Recorder` or a FLAC
//! writer takes. Both time their big writes, so either can say how fast the disk is (`speed`).
//!
//! A tape builds one with `tape_rt::sink` (feature `record`), which has the kernel's calls; this part needs none and
//! is tested on the host.

use crate::recorder::Sink;
use crate::stream::StreamSink;
use crate::vfs::{Files, VfsSink};

pub enum Out<F: Files> {
    Stream(StreamSink),
    Vfs(VfsSink<F>),
}

impl<F: Files> Out<F> {
    /// The big writes so far: bytes, time spent on them, the slowest one (µs).
    pub fn speed(&self) -> (u64, u64, u64) {
        match self {
            Out::Stream(sink) => (sink.bytes, sink.write_us, sink.write_max_us),
            Out::Vfs(sink) => (sink.bytes, sink.write_us, sink.write_max_us),
        }
    }

    /// Kilobytes per second over the big writes so far (0 before any).
    pub fn kib_per_second(&self) -> u64 {
        let (bytes, us, _) = self.speed();
        if us == 0 { 0 } else { bytes * 1000 / 1024 / (us / 1000).max(1) }
    }

    /// Whether it writes by path (an ext volume): such a file grows as it is written, so `open`'s reservation is only a
    /// placement.
    pub fn by_path(&self) -> bool {
        matches!(self, Out::Vfs(_))
    }
}

impl<F: Files> Sink for Out<F> {
    fn open(&mut self, name: &[u8], reserve: u64) -> Result<u64, i32> {
        match self {
            Out::Stream(sink) => sink.open(name, reserve),
            Out::Vfs(sink) => sink.open(name, reserve),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8], length: u64) -> Result<(), i32> {
        match self {
            Out::Stream(sink) => sink.write(offset, data, length),
            Out::Vfs(sink) => sink.write(offset, data, length),
        }
    }

    fn close(&mut self, length: u64) -> Result<(), i32> {
        match self {
            Out::Stream(sink) => sink.close(length),
            Out::Vfs(sink) => sink.close(length),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One file in memory, written in pieces of at most 1000 bytes; the clock moves 10 µs a call.
    #[derive(Default)]
    struct Disk {
        name: String,
        bytes: Vec<u8>,
    }

    impl Files for &mut Disk {
        fn create(&mut self, path: &str) -> Result<(), i32> {
            self.name = path.into();
            Ok(())
        }
        fn reserve(&mut self, _: &str, _: u64) -> Result<(), i32> {
            Ok(())
        }
        fn write(&mut self, _: &str, offset: u64, data: &[u8]) -> Result<usize, i32> {
            let count = data.len().min(1000);
            let end = offset as usize + count;
            if self.bytes.len() < end {
                self.bytes.resize(end, 0);
            }
            self.bytes[offset as usize..end].copy_from_slice(&data[..count]);
            Ok(count)
        }
        fn truncate(&mut self, _: &str, size: u64) -> Result<(), i32> {
            self.bytes.truncate(size as usize);
            Ok(())
        }
        fn sync(&mut self) -> Result<(), i32> {
            Ok(())
        }
    }

    fn clock() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NOW: AtomicU64 = AtomicU64::new(0);
        NOW.fetch_add(10, Ordering::Relaxed)
    }

    #[test]
    fn an_out_by_path_writes_the_file_and_times_the_big_writes() {
        let mut disk = Disk::default();
        let mut out = Out::Vfs(VfsSink::new(&mut disk, clock, 4096));
        assert!(out.by_path());
        out.open(b"ext1/recordings/take.wav", 1 << 20).unwrap();
        let block = [7u8; 8192];
        out.write(0, &block, 8192).unwrap();
        out.write(8192, &block[..100], 8292).unwrap();
        out.close(8292).unwrap();
        let (bytes, us, slowest) = out.speed();
        assert_eq!(bytes, 8192, "only the big write counts");
        assert!(us > 0 && slowest > 0 && slowest <= us);
        assert!(out.kib_per_second() > 0);
        drop(out);
        assert_eq!((disk.name.as_str(), disk.bytes.len()), ("ext1/recordings/take.wav", 8292));
        assert!(disk.bytes.iter().all(|&b| b == 7));
    }
}
