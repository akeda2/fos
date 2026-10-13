//! `term net` as a link: another fos machine's shell over Ethernet, the same wire protocol `tools/term_net.py` speaks
//! (ethertype 0x88b6, tag, payload): HELLO opens the session, DATA carries keys one way and screen bytes the other, BYE
//! ends it. The other machine's `deny-term-net` setting keeps it silent, which shows as "no answer".

use crate::link::{Link, MAX_FRAME, Nic};
use tape_rt::println;

const ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const TAG_HELLO: u8 = 0x20;
const TAG_DATA: u8 = 0x21;
const TAG_BYE: u8 = 0x22;
/// Payload bytes per DATA frame the kernel's server sends (`term.rs` DATA_CAPACITY); a smaller buffer here cut the end
/// of a long `ls`, prompt included.
const DATA_CAPACITY: usize = 1024;
/// Payload bytes per DATA frame we send: keys, so a small frame is plenty.
const SEND_CAPACITY: usize = 512;
/// How long to wait for the first byte of the remote shell before saying so, in ticks.
const SILENCE_TICKS: u64 = 300;

pub struct TermNetLink {
    nic: Nic,
    peer: [u8; 6],
    open: bool,
    heard: bool,
    started: u64,
    frame: [u8; MAX_FRAME],
    pending: [u8; DATA_CAPACITY],
    pending_len: usize,
    sent: u32,
    received: u32,
}

impl TermNetLink {
    pub fn open(nic: Nic, peer: [u8; 6]) -> Option<TermNetLink> {
        let mut link = TermNetLink {
            nic,
            peer,
            open: true,
            heard: false,
            started: tape_rt::ticks(),
            frame: [0; MAX_FRAME],
            pending: [0; DATA_CAPACITY],
            pending_len: 0,
            sent: 0,
            received: 0,
        };
        link.send(TAG_HELLO, &[]).then_some(link)
    }

    fn send(&mut self, tag: u8, payload: &[u8]) -> bool {
        let mut frame = [0u8; 15 + SEND_CAPACITY];
        frame[..6].copy_from_slice(&self.peer);
        frame[6..12].copy_from_slice(&self.nic.mac.0);
        frame[12..14].copy_from_slice(&ETHERTYPE);
        frame[14] = tag;
        frame[15..15 + payload.len()].copy_from_slice(payload);
        self.nic.send(&frame[..15 + payload.len()])
    }
}

impl Link for TermNetLink {
    fn poll(&mut self) -> bool {
        if !self.open {
            return false;
        }
        if !self.heard && tape_rt::ticks().wrapping_sub(self.started) > SILENCE_TICKS {
            self.heard = true;
            println!("\n[no answer yet: is `term net` allowed on that machine? Ctrl-A x leaves]");
        }
        // Leave a frame in the NIC queue until the screen has taken the last one.
        if self.pending_len != 0 {
            return false;
        }
        let length = self.nic.receive(&mut self.frame);
        if length < 15 || self.frame[6..12] != self.peer || self.frame[12..14] != ETHERTYPE {
            return length != 0;
        }
        match self.frame[14] {
            TAG_DATA => {
                // The frame has no length: Ethernet padding shows as trailing zeros, which are never real text.
                let payload = &self.frame[15..length];
                let end = payload.iter().rposition(|&byte| byte != 0).map_or(0, |last| last + 1).min(DATA_CAPACITY);
                self.pending[..end].copy_from_slice(&payload[..end]);
                self.pending_len = end;
                self.received = self.received.wrapping_add(end as u32);
                self.heard = true;
            }
            TAG_BYE => {
                self.open = false;
                println!("\n[the remote terminal ended the session]");
            }
            _ => {}
        }
        true
    }

    fn read(&mut self, out: &mut [u8]) -> usize {
        let count = self.pending_len.min(out.len());
        out[..count].copy_from_slice(&self.pending[..count]);
        self.pending.copy_within(count..self.pending_len, 0);
        self.pending_len -= count;
        count
    }

    fn write(&mut self, data: &[u8]) -> usize {
        let count = data.len().min(SEND_CAPACITY);
        if count == 0 || !self.send(TAG_DATA, &data[..count]) {
            return 0;
        }
        self.sent = self.sent.wrapping_add(count as u32);
        count
    }

    fn is_open(&self) -> bool {
        self.open
    }

    fn status(&mut self) {
        let mac = self.peer;
        println!(
            "\n[fterm: term net {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} on interface {}, sent {} received {}]",
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5],
            self.nic.index + 1,
            self.sent,
            self.received
        );
    }

    fn close(&mut self) {
        if self.open {
            self.send(TAG_BYE, &[]);
            self.open = false;
        }
    }
}
