//! Stand-in for `ehci.rs` on aarch64, which has no EHCI driver (the Pi's USB is xHCI only). It gives `usb_audio.rs`, which
//! handles both host controller kinds, the types it names; no controller ever exists, so none of this runs.

pub const ISO_RING: usize = 8;
pub const ITD_BYTES: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    High,
}

#[derive(Clone, Copy)]
pub struct Target {
    pub address: u8,
    pub speed: Speed,
    pub hub_address: u8,
    pub hub_port: u8,
}

#[derive(Debug)]
pub enum Error {
    Timeout,
}

pub struct Controller;

pub fn controller(_index: usize) -> Option<&'static mut Controller> {
    None
}

impl Controller {
    pub fn control_transfer(&mut self, _target: &Target, _request_type: u8, _request: u8, _value: u16, _index: u16, _data: &mut [u8]) -> Result<usize, ()> {
        Err(())
    }
    pub fn get_descriptor(&mut self, _target: &Target, _kind: u8, _index: u8, _buffer: &mut [u8]) -> Result<usize, Error> {
        Err(Error::Timeout)
    }
    pub fn bulk_transfer(&mut self, _target: &Target, _endpoint: u8, _max_packet: u16, _buffer: u32, _length: usize, _data_in: bool, _toggle: &mut bool) -> Result<usize, Error> {
        Err(Error::Timeout)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn bulk_transfer_within(&mut self, _ticks: u64, _target: &Target, _endpoint: u8, _max_packet: u16, _buffer: u32, _length: usize, _data_in: bool, _toggle: &mut bool) -> Result<usize, Error> {
        Err(Error::Timeout)
    }
    pub fn frame_now(&self) -> usize {
        0
    }
    pub fn set_configuration(&mut self, _target: &Target, _value: u8) -> Result<(), ()> {
        Err(())
    }
    pub fn iso_install(&mut self, _which: usize, _itds: usize) {}
    pub fn iso_remove(&mut self, _which: usize) {}
    pub fn enable_interrupts(&mut self, _index: usize) -> bool {
        false
    }
    pub fn disable_interrupts(&mut self) {}
    pub fn status_word(&self) -> u32 {
        0
    }
    pub fn interrupts_enabled(&self) -> bool {
        false
    }
}

pub fn itd_init(_at: usize, _target: &Target, _endpoint: u8, _max_packet: u16, _capture: bool, _buffer: u64, _pages: usize) {}
pub fn itd_arm(_at: usize, _step: usize, _lengths: &[u16], _max_packet: u16) {}
pub fn itd_active(_itd: usize, _step: usize, _count: usize) -> bool {
    false
}
pub fn itd_result(_itd: usize, _step: usize, _j: usize) -> (usize, u32) {
    (0, 0)
}
