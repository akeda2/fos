//! `mkimage FILE SIZE`: a zero-filled disk image on a FOSFS or ext volume, made to be exported (`aoe export FILE ... rw`).
//!
//! **Why zeros, not a sparse file.** A file export reads the file's block map once and serves it (`aoetarget`, "A file as
//! an export"); a write into a hole would need the file system to allocate under a running export, so `rw` refuses a file
//! with holes or unwritten extents. Writing every sector also takes the space up front: an initiator never meets a full
//! volume in the middle of its work. The cost is the time to write it, once (docs/block-sharing.md "mkimage").
//!
//! **Contiguous.** FOSFS reserves one extent of the whole size (`Volume::reserve`) and fills it. ext asks for a region
//! with room for the file (`tape_vfs::reserve`, the recorder's placement) before the first write; later blocks follow the
//! file's own last extent, so the file is a few long runs at most, and `aoe export` says how many.
//!
//! **In the background.** Filling tens of GiB takes minutes, and an mf shell command cannot read the keyboard or the
//! network terminal while it runs, so a thread (`mkimage`) does it, one chunk per hold of the disk lock (other disk users
//! and AoE exports interleave). `mkimage` shows how far it is, `mkimage stop` ends it. A stopped or failed image is
//! removed: an image of the wrong size, or half of it still old data, is no use.
//!
//! **While it is being made** the file must not be removed or exported: FOSFS's entry is looked up again before every
//! chunk and the fill stops if its extent moved; ext resolves the path on every write. An export of the half-made file is
//! guarded like any other (`aoetarget::guarded`), so the fill's next write is refused and it stops. A volume whose disk
//! is exported whole and writable is refused, and the fill stops if that happens while it runs: another machine may be
//! changing that file system, and two writers of one file system corrupt it.

use crate::sched;
use crate::sync::SpinLock;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use fosfor_abi::tape::{VfsMake, VfsStat, VFS_FILE};
use kernel_core::{ext4vol, tape_fs, tape_vfs};

/// Bytes written per hold of the disk lock: long enough for the drive to stream, short enough that the shell and the AoE
/// workers waiting for the lock are not held up (a few ms at SSD speeds).
const CHUNK: usize = 1 << 20;
/// ext: the journal transaction the deferred writes keep open is committed this often, so it never grows with the file.
const COMMIT_EVERY: u64 = 256 << 20;
/// The smallest image: one MiB. Sizes are whole MiB.
const MIB: u64 = 1 << 20;

/// The image being made: what, how big, how far.
struct Job {
    reference: String,
    size: u64,
    ext: bool,
    /// The disk index the volume is on.
    disk: usize,
    started: u64,
}

static JOB: SpinLock<Option<Job>> = SpinLock::new(None);
static DONE: AtomicU64 = AtomicU64::new(0);
static STOP: AtomicBool = AtomicBool::new(false);

const USAGE: &str = "Usage: mkimage FILE SIZE   (SIZE: 64M, 20G, 1T; whole MiB)\n\
                     \x20      mkimage            how far the image being made is\n\
                     \x20      mkimage stop       stop it (the half-made file is removed)\n\
                     A zero-filled image on a FOSFS or ext volume, for `aoe export FILE`.";

/// `SIZE`: a number with M, G or T (MiB, GiB, TiB), or bytes that are whole MiB.
fn parse_size(text: &str) -> Option<u64> {
    let upper = text.trim_end_matches(['i', 'B', 'b']);
    let (digits, unit) = match upper.as_bytes().last()? {
        b'M' | b'm' => (&upper[..upper.len() - 1], MIB),
        b'G' | b'g' => (&upper[..upper.len() - 1], MIB << 10),
        b'T' | b't' => (&upper[..upper.len() - 1], MIB << 20),
        _ => (upper, 1),
    };
    let size = digits.parse::<u64>().ok()?.checked_mul(unit)?;
    (size >= MIB && size % MIB == 0).then_some(size)
}

pub fn command(rest: &str) {
    let mut words = rest.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (None, ..) => status(),
        (Some("stop"), None, _) => match JOB.lock().as_ref() {
            Some(job) => {
                STOP.store(true, Ordering::Release);
                crate::println!("mkimage: stopping {}; it is removed when the thread sees it.", job.reference);
            }
            None => crate::println!("mkimage: no image is being made."),
        },
        (Some(reference), Some(size), None) => match parse_size(size) {
            Some(size) => start(reference, size),
            None => crate::println!("mkimage: SIZE is whole MiB, at least 1M: 64M, 20G, 1T."),
        },
        _ => crate::println!("{USAGE}"),
    }
}

fn status() {
    let job = JOB.lock();
    let Some(job) = job.as_ref() else { return crate::println!("mkimage: no image is being made.") };
    let done = DONE.load(Ordering::Relaxed);
    let seconds = crate::arch::micros().saturating_sub(job.started) / 1_000_000;
    let rate = done / seconds.max(1) >> 20;
    crate::println!("mkimage: {}: {} of {} MiB ({}%), {seconds} s, {rate} MiB/s.", job.reference, done >> 20, job.size >> 20, done * 100 / job.size);
}

/// Checks the target, makes the file (empty, placed) and starts the thread.
fn start(reference: &str, size: u64) {
    if JOB.lock().is_some() {
        return crate::println!("mkimage: an image is being made already (`mkimage` shows it).");
    }
    let bytes = reference.as_bytes();
    if kernel_core::remote::is_remote(bytes) || tape_fs::is_fat(bytes) {
        crate::println!("mkimage: {reference}: only FOSFS and ext volumes; a FAT, exFAT or");
        return crate::println!("     remote file cannot be exported.");
    }
    let ext = tape_vfs::is_ext_reference(bytes);
    let made = {
        let _disks = crate::tapefs::lock();
        if ext { make_ext(bytes, size) } else { make_fosfs(bytes, size) }
    };
    let disk = match made {
        Ok(disk) => disk,
        Err(why) => return crate::println!("mkimage: {reference}: {why}."),
    };
    DONE.store(0, Ordering::Relaxed);
    STOP.store(false, Ordering::Release);
    *JOB.lock() = Some(Job { reference: String::from(reference), size, ext, disk, started: crate::arch::micros() });
    if sched::spawn_any("mkimage", fill_thread, 0).is_none() {
        *JOB.lock() = None;
        remove(bytes, ext);
        return crate::println!("mkimage: could not start the thread; nothing was made.");
    }
    crate::println!("mkimage: writing {} MiB of zeros to {reference} in the background.", size >> 20);
    crate::println!("     `mkimage` shows how far it is, `mkimage stop` ends it.");
}

/// The reason to refuse a volume on disk index `disk`: exported whole and writable, another machine may be writing it,
/// and this machine's changes to the same file system would collide with its own (two writers).
const SHARED: &str = "its disk is exported rw as a whole (see `aoe`): another\n     machine may be writing it";

/// ext: refuses an existing name and a size above the free space, creates the file and places it; the disk index.
fn make_ext(reference: &[u8], size: u64) -> Result<usize, &'static str> {
    let mut stat = VfsStat::default();
    if tape_vfs::stat(reference, &mut stat) == 0 {
        return Err("exists already (remove it first)");
    }
    let disk = match tape_vfs::ext_space(reference) {
        Some((disk, _)) if crate::aoetarget::exported_writable(disk) => return Err(SHARED),
        Some((_, free)) if free < size => return Err("the volume has less free space than that"),
        Some((disk, _)) => disk,
        None => return Err("not found (no such ext volume)"),
    };
    let make = VfsMake { kind: VFS_FILE, mode: 0o644, uid: 0, gid: 0 };
    match tape_vfs::create(reference, &make) {
        0 => {}
        fosfor_abi::tape::FS_NOT_FOUND => return Err("its folder does not exist"),
        fosfor_abi::tape::FS_READ_ONLY => return Err("the volume cannot be written here"),
        _ => return Err("could not be created"),
    }
    // Only a goal for the first allocation: a failure costs contiguity, not the image.
    let _ = tape_vfs::reserve(reference, size);
    Ok(disk)
}

/// FOSFS: refuses an existing name (`reserve` would replace it) and reserves one extent of the whole size.
fn make_fosfs(reference: &[u8], size: u64) -> Result<usize, &'static str> {
    let (volume, name) = tape_fs::resolve_file(reference, false).map_err(|_| "no such volume")?;
    if crate::aoetarget::exported_writable(volume.disk) {
        return Err(SHARED);
    }
    if !matches!(volume.find(name), Ok(None)) {
        return Err("exists already (remove it first)");
    }
    match volume.reserve(name, size) {
        Ok(_) => Ok(volume.disk),
        Err(kernel_core::fosfs::Error::DataFull) => Err("no free run of that size on the volume"),
        Err(kernel_core::fosfs::Error::InvalidName) => Err("not a valid name on that volume"),
        Err(kernel_core::fosfs::Error::NoDirectory) => Err("its folder does not exist"),
        Err(_) => Err("could not be reserved"),
    }
}

fn remove(reference: &[u8], ext: bool) {
    let _disks = crate::tapefs::lock();
    if ext {
        let _ = tape_vfs::remove(reference);
        let _ = ext4vol::commit_pending();
    } else {
        let _ = tape_fs::remove(reference);
    }
}

/// One chunk of zeros at `offset`; Err says why the fill stops.
fn write_chunk(reference: &[u8], ext: bool, disk: usize, offset: u64, zeros: &[u8], size: u64) -> Result<(), &'static str> {
    // Exported whole and rw since the fill began: stop before writing the file system another machine may be changing.
    if crate::aoetarget::exported_writable(disk) {
        return Err("its disk was exported rw as a whole");
    }
    let _disks = crate::tapefs::lock();
    if ext {
        if tape_vfs::write(reference, offset, zeros) != zeros.len() as isize {
            return Err("a write failed (the file was removed or exported, or the disk failed)");
        }
        let end = offset + zeros.len() as u64;
        if end % COMMIT_EVERY == 0 || end == size {
            ext4vol::commit_pending().map_err(|_| "committing the volume's changes failed")?;
        }
        return Ok(());
    }
    let (volume, name) = tape_fs::resolve_file(reference, false).map_err(|_| "the volume went away")?;
    // The entry again: the file may have been removed (and its extent given to another file) since the last chunk.
    let entry = match volume.find(name) {
        Ok(Some(entry)) if !entry.is_dir => entry,
        _ => return Err("the file was removed"),
    };
    let end = offset + zeros.len() as u64;
    let length = if end == size { size } else { 0 };
    volume.write_extent(&entry, offset, zeros, length).map_err(|_| "a write failed (the file is exported, or the disk failed)")?;
    if end == size {
        volume.trim_extent(&entry, size).map_err(|_| "recording its length failed")?;
    }
    Ok(())
}

extern "C" fn fill_thread(_: usize) {
    let (reference, size, ext, disk, started) = match JOB.lock().as_ref() {
        Some(job) => (job.reference.clone(), job.size, job.ext, job.disk, job.started),
        None => sched::exit(),
    };
    let zeros = alloc::vec![0u8; CHUNK];
    let mut offset = 0;
    let mut failed = None;
    while offset < size {
        if STOP.load(Ordering::Acquire) {
            failed = Some("stopped");
            break;
        }
        let take = (size - offset).min(CHUNK as u64) as usize;
        if let Err(why) = write_chunk(reference.as_bytes(), ext, disk, offset, &zeros[..take], size) {
            failed = Some(why);
            break;
        }
        offset += take as u64;
        DONE.store(offset, Ordering::Relaxed);
    }
    let seconds = crate::arch::micros().saturating_sub(started) / 1_000_000;
    match failed {
        None => {
            crate::println!("mkimage: {reference} done: {} MiB in {seconds} s.", size >> 20);
            crate::println!("     `aoe export {reference} rw for MAC` serves it.");
        }
        Some(why) => {
            remove(reference.as_bytes(), ext);
            crate::println!("mkimage: {reference}: {why}; the file is removed.");
        }
    }
    *JOB.lock() = None;
    sched::exit();
}
