//! What `fterm` talks to. A link moves bytes; the terminal loop in `main.rs` owns the keyboard and the screen and never
//! blocks, so every link is polled: it does its timers and receiving in `poll`, hands screen bytes out through `read` and
//! takes typed bytes through `write` (which says how many it took; the loop keeps the rest).

use fterm::Action;
use tape_net::MacAddress;
use tape_rt::compat;

pub trait Link {
    /// Background work for one pass (timers, receiving, sending queued bytes). True if anything happened.
    fn poll(&mut self) -> bool;
    /// Moves received bytes into `out`; the number moved.
    fn read(&mut self, out: &mut [u8]) -> usize;
    /// Takes as many of `data` as there is room for; the number taken.
    fn write(&mut self, data: &[u8]) -> usize;
    /// False once the other end has gone (the loop drains `read`, then ends the session).
    fn is_open(&self) -> bool {
        true
    }
    /// Link-specific commands (break, DTR, RTS). False if this link has no such thing.
    fn command(&mut self, _action: &Action) -> bool {
        false
    }
    /// The status lines for Ctrl-A s.
    fn status(&mut self);
    fn close(&mut self);
}

/// One Ethernet interface, by the numbering of the tape ABI (zero-based).
#[derive(Clone, Copy)]
pub struct Nic {
    pub index: u32,
    pub mac: MacAddress,
    max_frame: usize,
}

pub const MAX_FRAME: usize = 1514;

impl Nic {
    pub fn open(index: u32) -> Option<Nic> {
        let mut mac = [0u8; 6];
        if compat::nic_mac_at(index, mac.as_mut_ptr() as usize) == 0 {
            return None;
        }
        let max_frame = (compat::nic_max_frame_len() as usize).min(MAX_FRAME);
        (max_frame >= 60).then_some(Nic { index, mac: MacAddress::new(mac), max_frame })
    }

    /// The kernel pads short frames; the tape only builds the headers and payload.
    pub fn send(&self, frame: &[u8]) -> bool {
        frame.len() >= 14 && frame.len() <= self.max_frame && compat::nic_send_at(self.index, frame.as_ptr() as usize, frame.len() as u32) != 0
    }

    /// One waiting frame, if any; its length (0 for none).
    pub fn receive(&self, buffer: &mut [u8; MAX_FRAME]) -> usize {
        (compat::nic_receive_at(self.index, buffer.as_mut_ptr() as usize, self.max_frame as u32) as usize).min(MAX_FRAME)
    }
}
