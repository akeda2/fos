//! A file on disk as a decoder's `audio_decode::Source`, for tapes that play or look at what is recorded. Behind the
//! `decode` feature, so tapes that decode nothing do not build `audio-decode`.
//!
//! Two ways in, the same reads:
//!
//! - `FileSource::vfs`: by path (`vfs_read`, ABI 32): `ext1/recordings/...`, `disk3/...`, any volume, folders included;
//! - `FileSource::fs`: by name (`fs_read_at`, ABI 29): a FOSFS or FAT root file, where the embedded kernels keep takes.
//!
//! A read asks for at most `READ_CHUNK` bytes, so one call never holds the disk for long; the decoders join short reads.

use crate::text::Line;
use audio_decode::{Source, SourceError};
use core::fmt::Write;

/// The longest path or name a source keeps (FOSFS's limit; KakelFix's `ROOT/PROJECT/TAKE/FILE` reaches 175).
pub const MAX_PATH: usize = 192;
/// The most bytes one read asks the kernel for.
pub const READ_CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Calls {
    Vfs,
    Fs,
}

pub struct FileSource {
    path: Line<MAX_PATH>,
    size: u64,
    calls: Calls,
}

impl FileSource {
    /// No file (for a decoder in a static, before a file is opened): every read is the end of the file.
    pub const fn none() -> FileSource {
        FileSource { path: Line::new(), size: 0, calls: Calls::Vfs }
    }

    fn with(path: &str, size: u64, calls: Calls) -> Option<FileSource> {
        let mut source = FileSource { path: Line::new(), size, calls };
        let _ = source.path.write_str(path);
        (source.path.length == path.len()).then_some(source)
    }

    /// The file at `path` (any volume), `size` bytes long; None when the path is longer than `MAX_PATH`.
    pub fn vfs(path: &str, size: u64) -> Option<FileSource> {
        FileSource::with(path, size, Calls::Vfs)
    }

    /// The file at `path`, its size asked of the kernel (`vfs_stat`).
    pub fn open_vfs(path: &str) -> Result<FileSource, isize> {
        let stat = crate::vfs_stat(path)?;
        FileSource::vfs(path, stat.size).ok_or(fosfor_abi::tape::FS_ERROR)
    }

    /// The root file `name` on a FOSFS or FAT volume (`fs_read_at`), `size` bytes long.
    pub fn fs(name: &str, size: u64) -> Option<FileSource> {
        FileSource::with(name, size, Calls::Fs)
    }

    pub fn path(&self) -> &str {
        self.path.as_str()
    }
}

impl Source for FileSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        if self.path.is_empty() || offset >= self.size {
            return Ok(0);
        }
        let length = out.len().min(READ_CHUNK).min((self.size - offset).min(usize::MAX as u64) as usize);
        let out = &mut out[..length];
        match self.calls {
            Calls::Vfs => crate::vfs_read(self.path.as_str(), offset, out),
            Calls::Fs => crate::read_file_at(self.path.as_str(), offset, out),
        }
        .map_err(|_| SourceError)
    }
}
