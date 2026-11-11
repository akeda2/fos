//! Stand-ins for the modules that exist only on x86 so far, so the shared shell compiles on aarch64 and says what is missing.
//! Each one is replaced by the real module as its hardware (PCI, NICs, USB, the framebuffer) or EL0 support is ported; a stand-in
//! has the same signatures as the real thing, so the shell needs no `cfg`. Nothing here is meant to stay.

macro_rules! words {
    () => { core::str::SplitWhitespace<'_> };
}

fn missing(what: &str) {
    println!("{what}: not available on aarch64 yet.");
}

pub mod process {
    use alloc::vec::Vec;
    pub use crate::sched::How;

    #[derive(Clone, Copy)]
    pub struct Outcome {
        pub pid: u64,
        pub how: How,
        pub written: u64,
    }
    pub fn outcomes() -> Vec<Outcome> {
        Vec::new()
    }
    pub static START_NICS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    #[derive(Debug)]
    pub enum SpawnError {
        /// No EL0 yet: a tape has nowhere to run.
        Unsupported,
    }
    pub fn spawn_elf(_name: &str, _image: &[u8], _args: &[u8]) -> Result<u64, SpawnError> {
        Err(SpawnError::Unsupported)
    }
    pub fn screen_owner() -> u64 {
        0
    }
    pub fn user_bytes(_address: u64, _length: u64) -> Option<Vec<u8>> {
        None
    }
    pub fn user_bytes_up_to(_address: u64, _length: u64, _limit: u64) -> Option<Vec<u8>> {
        None
    }
    pub fn copy_to_user(_address: u64, _data: &[u8]) -> bool {
        false
    }
}

pub mod tapenet {
    pub fn exists(_index: usize) -> bool {
        false
    }
    pub fn owner(_index: usize) -> u64 {
        0
    }
    pub fn address_of_owner(_pid: u64) -> Option<[u8; 4]> {
        None
    }
}

pub mod tapeaudio {
    pub fn owner() -> u64 {
        0
    }
}

pub mod net {
    pub fn exit_session(_session: usize) -> bool {
        false
    }
    pub fn print_list() {
        println!("No network interfaces (no NIC driver on aarch64 yet).");
    }
    pub fn print_registers(_number: usize) {
        super::missing("nic regs");
    }
    pub fn poll() {}
    pub fn housekeeping() {}
}

pub mod netclient {
    pub fn print_peers() {
        super::missing("nic peers");
    }
    pub fn nic_discover() {
        super::missing("nic discover");
    }
    pub fn tapesync(_words: words!()) {
        super::missing("tapesync");
    }
    pub fn gpio(_rest: &str) {
        super::missing("gpio");
    }
}

pub mod vnic {
    pub struct Fresh;
    impl Fresh {
        pub fn keep(self) {}
        pub fn index(&self) -> usize {
            0
        }
    }
    pub fn lookup(_name: &str) -> Option<usize> {
        None
    }
    pub fn add_ephemeral(_tape: &str, _parent: usize) -> Result<Fresh, &'static str> {
        Err("no network interfaces on aarch64 yet")
    }
    pub fn queue(_line: &str) {}
    pub fn command<'a>(_words: impl Iterator<Item = &'a str> + Clone) {
        super::missing("vnic");
    }
}

pub mod service {
    pub fn poll() {}
    pub fn command<'a>(_words: impl Iterator<Item = &'a str> + Clone) {
        super::missing("service");
    }
}

pub mod audioshell {
    pub fn command(_words: words!()) {
        super::missing("audio");
    }
}

pub mod usbshell {
    pub fn command(_words: words!()) {
        super::missing("usb");
    }
}

pub mod usb {
    pub fn poll() {}
    pub fn storage_read(_device: usize, _lba: u64, _buffer: &mut [u8]) -> Result<(), ()> {
        Err(())
    }
    pub fn storage_write(_device: usize, _lba: u64, _buffer: &[u8]) -> Result<(), ()> {
        Err(())
    }
}

pub mod fb {
    pub fn set_colours(_foreground: Option<&str>, _background: Option<&str>) -> bool {
        false
    }
}

pub mod install {
    pub fn cancel() {}
    pub fn command(_words: words!()) {
        super::missing("install");
    }
}

pub mod bootcode {
    pub fn cancel() {}
    pub fn command(_words: words!()) {
        super::missing("bootcode");
    }
}

pub mod ps2 {
    pub fn poll() {}
}
