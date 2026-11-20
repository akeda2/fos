//! A `Sink` over path-based file calls (the tape ABI's `vfs_*`, for ext volumes named `extN/path`), where `stream` is over
//! the `fs_stream_*` handles of FOSFS and FAT. The differences that matter for a recorder:
//!
//! - an ext file is not reserved up front; it grows as it is written, and `reserve` only places it (`vfs_reserve`: a
//!   region of the volume of its own, so files written side by side stay contiguous);
//! - a write may take less than asked (the server kernel takes up to 1 MiB a call): the sink loops;
//! - the kernel keeps metadata changes of a stream of writes open and commits them together (`vfs_sync`): the sink commits
//!   whenever the recorder moves the file's length (its first block, a checkpoint, the end), so a power cut keeps the take
//!   up to the last checkpoint, as on the other volumes;
//! - the file's length is wherever the last write ended, so `close` truncates to the length the recorder gives.

use crate::recorder::Sink;

/// The file calls the sink needs, by path. Errors are the kernel's negative codes.
pub trait Files {
    fn create(&mut self, path: &str) -> Result<(), i32>;
    /// A placement for a file that will grow to about `bytes`; only a goal, so a failure is not an error for the take.
    fn reserve(&mut self, path: &str, bytes: u64) -> Result<(), i32>;
    /// Writes at `offset`; the count taken, which may be less than `data.len()`.
    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<usize, i32>;
    fn truncate(&mut self, path: &str, size: u64) -> Result<(), i32>;
    /// Commits the writes so far (metadata included).
    fn sync(&mut self) -> Result<(), i32>;
}

/// What `open` offers as the file's largest size: no limit of its own (the volume's free space is the limit, and running
/// out is a write error).
pub const NO_LIMIT: u64 = u64::MAX / 2;
const MAX_PATH: usize = 128;
/// A write that takes nothing at all is an error rather than a loop.
const STALLED: i32 = -3;

pub struct VfsSink<F: Files> {
    pub files: F,
    path: [u8; MAX_PATH],
    path_len: usize,
    micros: fn() -> u64,
    /// Only writes of at least this many bytes count towards the speed figures (as `StreamSink`).
    min_block: usize,
    pub write_us: u64,
    pub write_max_us: u64,
    pub writes: u64,
    pub bytes: u64,
    /// Commits made (`sync`): one per length change.
    pub syncs: u64,
}

impl<F: Files> VfsSink<F> {
    pub const fn new(files: F, micros: fn() -> u64, min_block: usize) -> Self {
        VfsSink { files, path: [0; MAX_PATH], path_len: 0, micros, min_block, write_us: 0, write_max_us: 0, writes: 0, bytes: 0, syncs: 0 }
    }

    fn sync(&mut self) -> Result<(), i32> {
        self.syncs += 1;
        self.files.sync()
    }
}

impl<F: Files> Sink for VfsSink<F> {
    fn open(&mut self, name: &[u8], reserve: u64) -> Result<u64, i32> {
        if name.len() > MAX_PATH || core::str::from_utf8(name).is_err() {
            return Err(-4); // FS_BAD_NAME
        }
        self.path[..name.len()].copy_from_slice(name);
        self.path_len = name.len();
        let path = core::str::from_utf8(name).unwrap_or("");
        self.files.create(path)?;
        if reserve > 0 {
            let _ = self.files.reserve(path, reserve);
        }
        Ok(NO_LIMIT)
    }

    fn write(&mut self, offset: u64, data: &[u8], length: u64) -> Result<(), i32> {
        let began = (self.micros)();
        let path = core::str::from_utf8(&self.path[..self.path_len]).unwrap_or("");
        let mut done = 0;
        while done < data.len() {
            match self.files.write(path, offset + done as u64, &data[done..])? {
                0 => return Err(STALLED),
                count => done += count,
            }
        }
        let spent = (self.micros)() - began;
        if data.len() >= self.min_block {
            (self.write_us, self.write_max_us, self.writes, self.bytes) = (self.write_us + spent, self.write_max_us.max(spent), self.writes + 1, self.bytes + data.len() as u64);
        }
        if length != 0 { self.sync() } else { Ok(()) }
    }

    fn close(&mut self, length: u64) -> Result<(), i32> {
        let path = core::str::from_utf8(&self.path[..self.path_len]).unwrap_or("");
        self.files.truncate(path, length)?;
        self.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::Recorder;
    use crate::wav::Format;

    /// One file in memory; writes take at most 64 KiB (less than asked, which a kernel may do); what was committed is kept apart, so a
    /// power cut can be simulated.
    #[derive(Default)]
    struct Memory {
        path: String,
        data: Vec<u8>,
        committed: Vec<u8>,
        reserved: Option<u64>,
        calls: usize,
    }

    impl Files for &mut Memory {
        fn create(&mut self, path: &str) -> Result<(), i32> {
            if !self.path.is_empty() {
                return Err(-6);
            }
            self.path = path.to_string();
            Ok(())
        }
        fn reserve(&mut self, path: &str, bytes: u64) -> Result<(), i32> {
            assert_eq!(path, self.path);
            self.reserved = Some(bytes);
            Ok(())
        }
        fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<usize, i32> {
            assert_eq!(path, self.path);
            self.calls += 1;
            let count = data.len().min(64 * 1024);
            let end = offset as usize + count;
            if self.data.len() < end {
                self.data.resize(end, 0);
            }
            self.data[offset as usize..end].copy_from_slice(&data[..count]);
            Ok(count)
        }
        fn truncate(&mut self, _path: &str, size: u64) -> Result<(), i32> {
            self.data.truncate(size as usize);
            Ok(())
        }
        fn sync(&mut self) -> Result<(), i32> {
            self.committed = self.data.clone();
            Ok(())
        }
    }

    fn now() -> u64 {
        0
    }

    fn data_len(file: &[u8]) -> u32 {
        let at = file.windows(4).position(|window| window == b"data").unwrap();
        u32::from_le_bytes(file[at + 4..at + 8].try_into().unwrap())
    }

    const FORMAT: Format = Format { channels: 2, rate: 48_000, slot: 4, bits: 32, float: true };

    #[test]
    fn a_take_through_short_writes_is_whole_and_committed_at_checkpoints() {
        let mut memory = Memory::default();
        let mut sink = VfsSink::new(&mut memory, now, 1024);
        let mut staging = vec![0u8; 256 * 1024];
        let mut recorder = Recorder::begin(&mut sink, &mut staging, FORMAT, b"ext2/take001.wav", 8 << 20).unwrap();
        let audio: Vec<u8> = (0..600_000u32).map(|n| (n % 253) as u8).collect();
        assert_eq!(recorder.push(&audio).unwrap(), audio.len());
        recorder.checkpoint().unwrap();
        let summary = recorder.finish().unwrap();
        // Commits: the first block, the checkpoint, and at the end the last block, the final header and the close (each
        // moves the length). The full blocks in between do not.
        assert_eq!(sink.syncs, 5);
        assert_eq!(sink.writes, 3, "the speed counts the blocks (two of 256 KiB, the last 74 KiB), not the one-sector headers");
        drop(sink);
        assert_eq!((memory.path.as_str(), memory.reserved), ("ext2/take001.wav", Some(8 << 20)));
        assert!(memory.calls >= 10, "256 KiB blocks went 64 KiB a call ({})", memory.calls);
        let header = summary.bytes as usize - audio.len();
        assert_eq!(memory.committed.len(), summary.bytes as usize);
        assert_eq!(data_len(&memory.committed) as usize, audio.len());
        assert_eq!(&memory.committed[header..], &audio[..]);
    }

    #[test]
    fn a_power_cut_keeps_the_take_up_to_the_checkpoint() {
        let mut memory = Memory::default();
        let mut sink = VfsSink::new(&mut memory, now, 1024);
        let mut staging = vec![0u8; 64 * 1024];
        let mut recorder = Recorder::begin(&mut sink, &mut staging, FORMAT, b"ext2/cut.wav", 0).unwrap();
        recorder.push(&[5u8; 200_000]).unwrap();
        recorder.checkpoint().unwrap();
        recorder.push(&[6u8; 200_000]).unwrap();
        drop(recorder);
        drop(sink);
        assert_eq!(memory.reserved, None, "no reservation was asked for");
        // Committed: three 64 KiB blocks, and a header whose data length is everything after it.
        assert_eq!(memory.committed.len(), 3 * 64 * 1024);
        assert_eq!(data_len(&memory.committed) as usize, 3 * 64 * 1024 - crate::wav::header_len(&FORMAT));
        assert!(memory.data.len() > memory.committed.len(), "later blocks were written but not committed");
    }
}
