//! Reading the payload partition through the firmware's Block I/O.
//!
//! The loader finds the kernel the same way v1's stage two does: by LBA, with no
//! filesystem. It looks at every whole-disk block device, reads the GPT, and takes
//! the first partition carrying the fos payload type GUID. (M1 takes the first
//! such disk; the kernel's later boot-disk discovery matches the partition GUID
//! the loader passes along instead, see docs/v2-design.md section 8.)

use crate::uefi::{self, BlockIo, BootServices, Handle};
use core::ffi::c_void;
use fos_v2_abi::{PAYLOAD_TYPE_GUID, SECTOR};

/// Scratch for GPT sectors and the partition entry array; aligned because some
/// firmware Block I/O drivers require an aligned buffer.
#[repr(C, align(4096))]
struct Scratch([u8; 32 * SECTOR]);
static mut SCRATCH: Scratch = Scratch([0; 32 * SECTOR]);

#[derive(Clone, Copy)]
pub struct Partition {
    block_io: *mut BlockIo,
    media_id: u32,
    /// First LBA of the partition on its disk, and its length in sectors.
    pub start: u64,
    pub sectors: u64,
}

impl Partition {
    /// Reads `sectors` sectors starting at `sector` (relative to the partition) to
    /// `buffer`, which must be valid for that many bytes.
    pub fn read(&self, sector: u64, buffer: *mut u8, sectors: usize) -> Result<(), uefi::Status> {
        if sector + sectors as u64 > self.sectors {
            return Err(1);
        }
        let status = unsafe {
            ((*self.block_io).read_blocks)(self.block_io, self.media_id, self.start + sector, sectors * SECTOR, buffer as *mut c_void)
        };
        if status == uefi::SUCCESS { Ok(()) } else { Err(status) }
    }
}

fn read_disk(block_io: *mut BlockIo, media_id: u32, lba: u64, sectors: usize) -> Option<&'static [u8]> {
    let buffer = core::ptr::addr_of_mut!(SCRATCH) as *mut u8;
    let status = unsafe { ((*block_io).read_blocks)(block_io, media_id, lba, sectors * SECTOR, buffer as *mut c_void) };
    (status == uefi::SUCCESS).then(|| unsafe { core::slice::from_raw_parts(buffer, sectors * SECTOR) })
}

/// Looks through every whole disk for a GPT partition of the payload type.
pub fn find_payload(boot: &BootServices) -> Option<Partition> {
    let mut count = 0usize;
    let mut handles: *mut Handle = core::ptr::null_mut();
    let status = (boot.locate_handle_buffer)(uefi::BY_PROTOCOL, &uefi::BLOCK_IO_PROTOCOL, core::ptr::null_mut(), &mut count, &mut handles);
    if status != uefi::SUCCESS || handles.is_null() {
        return None;
    }
    for index in 0..count {
        let handle = unsafe { *handles.add(index) };
        let mut interface: *mut c_void = core::ptr::null_mut();
        if (boot.handle_protocol)(handle, &uefi::BLOCK_IO_PROTOCOL, &mut interface) != uefi::SUCCESS || interface.is_null() {
            continue;
        }
        let block_io = interface as *mut BlockIo;
        let media = unsafe { &*(*block_io).media };
        if media.logical_partition || !media.media_present || media.block_size as usize != SECTOR {
            continue;
        }
        // GPT header at LBA 1.
        let Some(header) = read_disk(block_io, media.media_id, 1, 1) else { continue };
        if &header[0..8] != b"EFI PART" {
            continue;
        }
        let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
        let entry_count = u32::from_le_bytes(header[80..84].try_into().unwrap()) as usize;
        let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap()) as usize;
        if entry_size < 128 || entry_size > 512 || entry_count == 0 {
            continue;
        }
        let sectors = (entry_count * entry_size).div_ceil(SECTOR).min(32);
        let Some(entries) = read_disk(block_io, media.media_id, entries_lba, sectors) else { continue };
        for entry in entries.chunks_exact(entry_size).take(entry_count) {
            if entry[0..16] == PAYLOAD_TYPE_GUID {
                let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
                let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
                return Some(Partition { block_io, media_id: media.media_id, start: first, sectors: last - first + 1 });
            }
        }
    }
    None
}
