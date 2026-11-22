//! Sinks a recording tape writes its files through (`audio_rec::recorder::Sink`), made over this kernel's calls. Behind
//! the `record` feature, so tapes that write no audio files do not build `audio-rec`.
//!
//! - `stream`: the one stream of `fs_stream_*` (FOSFS, FAT32, exFAT; `diskN/NAME` names a FAT disk's file);
//! - `stream_handle`: a stream of its own (`fs_stream_*2`, ABI 30), for writing up to `FS_STREAMS` files at once;
//! - `vfs`: a file by path on an ext volume (`extN/path`, `vfs_*`, ABI 34, placed with `vfs_reserve`, ABI 42);
//! - `out`: whichever of those a file needs, as one `audio_rec::out::Out`.
//!
//! `min_block`: only writes of at least this many bytes count towards the speed figures (a checkpoint's one-sector header
//! rewrite would make a disk look slow); half the tape's block size is the usual choice.

use audio_rec::out::Out;
use audio_rec::stream::StreamSink;
use audio_rec::vfs::{Files, VfsSink};
use fosfor_abi::tape::VFS_FILE;

/// An ext volume's file calls, by path.
pub struct TapeFiles;

impl Files for TapeFiles {
    fn create(&mut self, path: &str) -> Result<(), i32> {
        crate::vfs_create(path, VFS_FILE, 0o644, 0, 0).map_err(|code| code as i32)
    }
    fn reserve(&mut self, path: &str, bytes: u64) -> Result<(), i32> {
        crate::vfs_reserve(path, bytes).map_err(|code| code as i32)
    }
    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<usize, i32> {
        crate::vfs_write(path, offset, data).map_err(|code| code as i32)
    }
    fn truncate(&mut self, path: &str, size: u64) -> Result<(), i32> {
        crate::vfs_truncate(path, size).map_err(|code| code as i32)
    }
    fn sync(&mut self) -> Result<(), i32> {
        crate::vfs_sync().map_err(|code| code as i32)
    }
}

/// The tape's one `fs_stream_*` stream.
pub fn stream(min_block: usize) -> StreamSink {
    let api = crate::api();
    StreamSink::new(api.fs_stream_open, api.fs_stream_write, api.fs_stream_close, crate::micros, min_block)
}

/// A stream of its own (ABI 30), so several files can be written at once; None on a kernel without them. Its `open`
/// must be given the bytes to reserve, never 0 (the first stream would take the largest free run).
pub fn stream_handle(min_block: usize) -> Option<StreamSink> {
    let api = crate::multi_stream_table()?;
    Some(StreamSink::with_handle(api.fs_stream_open2, api.fs_stream_write2, api.fs_stream_close2, crate::micros, min_block))
}

/// A file by path on an ext volume.
pub fn vfs(min_block: usize) -> VfsSink<TapeFiles> {
    VfsSink::new(TapeFiles, crate::micros, min_block)
}

/// Whether `path` is on an ext volume (`ext1/...`), which only the path calls reach.
pub fn is_ext_path(path: &str) -> bool {
    let rest = path.strip_prefix("ext").unwrap_or("");
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest.as_bytes().get(digits) == Some(&b'/')
}

/// The sink for a file at `path`: by path on an ext volume, else a stream (one of its own when `own_stream` and the
/// kernel has them, else the tape's one).
pub fn out(path: &str, own_stream: bool, min_block: usize) -> Out<TapeFiles> {
    if is_ext_path(path) {
        return Out::Vfs(vfs(min_block));
    }
    match own_stream.then(|| stream_handle(min_block)).flatten() {
        Some(sink) => Out::Stream(sink),
        None => Out::Stream(stream(min_block)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_paths_are_told_from_the_rest() {
        assert!(is_ext_path("ext1/recordings/take.flac"));
        assert!(is_ext_path("ext12/a"));
        for path in ["disk3/take.wav", "take.wav", "ext/a", "ext1", "extra/a", "@LABEL/ext1/a"] {
            assert!(!is_ext_path(path), "{path}");
        }
    }
}
