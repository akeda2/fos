//! The mount table lives in `kernel-core` (`mounts.rs` there); this adds the two questions only the kernel can answer:
//! which volume is the boot volume, and which labelled volumes are present right now.

pub use kernel_core::mounts::*;

/// Reads `fstab` from the boot volume into the table; see `kernel_core::mounts::load_from`.
pub fn load() -> (usize, usize) {
    load_from(crate::storage::boot_volume())
}

/// How many mounts name a volume that is not present right now (neither a FOSFS nor a FAT volume of that label).
pub fn absent() -> usize {
    absent_with(|label| crate::storage::volume_labelled(label).is_some() || crate::tape_fs::fat_labelled(label).is_some())
}
