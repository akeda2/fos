//! What the USB enumerators need from the kernel's console side: where a typed key goes, which keymap is active and
//! whether disks already have their final numbers. The kernel registers it once at boot (`set`).

use fosfor_abi::keymap::Layout;

pub struct Env {
    /// Queues one typed byte for the shell (callable from interrupt context).
    pub key_push: fn(u8),
    pub key_layout: fn() -> Layout,
    /// True once the boot disk is known, so `diskN` numbers are final.
    pub boot_disk_known: fn() -> bool,
}

static mut ENV: Option<&'static Env> = None;

pub fn set(env: &'static Env) {
    unsafe { *core::ptr::addr_of_mut!(ENV) = Some(env) };
}

fn env() -> Option<&'static Env> {
    unsafe { *core::ptr::addr_of!(ENV) }
}

pub fn push(byte: u8) {
    if let Some(env) = env() {
        (env.key_push)(byte);
    }
}

pub fn layout() -> Layout {
    env().map_or(Layout::Us, |env| (env.key_layout)())
}

/// A USB disk's number as the boot messages print it: the boot disk is `disk1`, so a stick found before the boot disk is
/// known cannot honestly be given a number yet; `disks` lists the final numbering.
pub struct DiskNumber(pub usize);

impl core::fmt::Display for DiskNumber {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if env().is_some_and(|env| (env.boot_disk_known)()) {
            write!(formatter, "disk{}", self.0 + 1)
        } else {
            write!(formatter, "attached (numbered after the boot disk is found; see `disks`)")
        }
    }
}
