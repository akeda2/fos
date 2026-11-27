//! IGMP host side (RFC 2236 and RFC 3376), just enough to receive a multicast stream through a switch with IGMP
//! snooping: join the group (an unsolicited report), answer the querier's membership queries, leave at the end.
//!
//! Without a join, a snooping switch forwards a multicast group only to the ports that asked for it, so a
//! receiver would hear nothing. A query is answered after a random delay of up to the query's maximum response
//! time, so that the hosts on a segment do not all answer at once. The unsolicited join is an IGMPv2 report
//! (every querier accepts it, a v3 one in compatibility mode); answers to a query use the version of the query.
//!
//! **No querier.** IGMP hosts report only when asked, which assumes a querier on the segment. A snooping switch with no
//! querier still learns members from the unsolicited join, but forgets them when its membership timer runs out
//! (RFC 2236's Group Membership Interval, 260 s by default; vendors differ) and then stops forwarding the group to
//! that port. A PTP master on the test network lost its slaves' Delay_Req after a few minutes that way (2026-11-27:
//! no IGMP query in 280 s of capture). So while no query has been heard for a membership interval, a joined group is
//! reported again every `REREPORT_US`; once a querier speaks, only its queries are answered, as the RFCs have it.
//!
//! An IGMP packet is an IPv4 packet of protocol 2, TTL 1, with the Router Alert option (RFC 2113), sent to the
//! group itself (v2 report), to 224.0.0.2 (v2 leave) or to 224.0.0.22 (v3 report).

use crate::net::NetConfig;
use audio_wire::multicast_mac;
use tape_rt::TapeApi;

const ALL_HOSTS: [u8; 4] = [224, 0, 0, 1];
const ALL_ROUTERS: [u8; 4] = [224, 0, 0, 2];
const IGMPV3_REPORTS: [u8; 4] = [224, 0, 0, 22];
/// RFC 2236 8.4, Group Membership Interval (2 x 125 s + 10 s): how long a querier or switch keeps a membership
/// that nobody reports again; also how long a heard querier counts as present.
const MEMBERSHIP_US: u64 = 260_000_000;
/// With no querier heard, a joined group is reported again this often: well inside any membership timeout.
const REREPORT_US: u64 = 60_000_000;

pub struct Igmp {
    group: [u8; 4],
    /// When (micros) a pending answer to a query is due, and whether it is a v3 report.
    due: Option<(u64, bool)>,
    /// Between `join` and `leave`.
    joined: bool,
    /// When the last query for us arrived, and when we last reported, on the caller's clock (`handle`, `poll`).
    /// `last_report` is `None` right after `join`, until `poll` gives it a time.
    last_query: Option<u64>,
    last_report: Option<u64>,
    pub queries: u32,
    pub reports: u32,
}

/// RFC 1071 checksum.
fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in bytes.chunks(2) {
        sum += u32::from(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Builds Ethernet + IPv4 (with Router Alert) + `igmp` into `out`; returns the length.
fn build(out: &mut [u8; 80], config: &NetConfig, destination: [u8; 4], igmp: &[u8]) -> usize {
    out[0..6].copy_from_slice(&multicast_mac(destination));
    out[6..12].copy_from_slice(&config.mac.0);
    out[12..14].copy_from_slice(&[0x08, 0x00]);
    let ip = &mut out[14..];
    let total = 24 + igmp.len();
    ip[0] = 0x46; // version 4, header of six 32-bit words (the option makes it 24 bytes)
    ip[1] = 0xc0; // internetwork control
    ip[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    ip[4..8].copy_from_slice(&[0, 0, 0x40, 0]); // no fragmentation
    ip[8] = 1; // TTL 1: never leaves the segment
    ip[9] = 2; // IGMP
    ip[10..12].copy_from_slice(&[0, 0]);
    ip[12..16].copy_from_slice(&config.ip.0);
    ip[16..20].copy_from_slice(&destination);
    ip[20..24].copy_from_slice(&[0x94, 0x04, 0, 0]); // Router Alert
    let sum = checksum(&ip[..24]);
    ip[10..12].copy_from_slice(&sum.to_be_bytes());
    ip[24..24 + igmp.len()].copy_from_slice(igmp);
    14 + total
}

impl Igmp {
    pub fn new(group: [u8; 4]) -> Igmp {
        Igmp { group, due: None, joined: false, last_query: None, last_report: None, queries: 0, reports: 0 }
    }

    fn send(&mut self, api: &TapeApi, nic: usize, config: &NetConfig, v3: bool, leave: bool) {
        let mut message = [0u8; 20];
        let (destination, length) = if v3 {
            // Report with one record: MODE_IS_EXCLUDE with no sources (= "I want everything from the group").
            message[0] = 0x22;
            message[6..8].copy_from_slice(&1u16.to_be_bytes());
            message[8] = 2;
            message[12..16].copy_from_slice(&self.group);
            (IGMPV3_REPORTS, 16)
        } else {
            message[0] = if leave { 0x17 } else { 0x16 };
            message[4..8].copy_from_slice(&self.group);
            (if leave { ALL_ROUTERS } else { self.group }, 8)
        };
        let sum = checksum(&message[..length]);
        message[2..4].copy_from_slice(&sum.to_be_bytes());
        let mut frame = [0u8; 80];
        let total = build(&mut frame, config, destination, &message[..length]);
        (api.nic_send)(nic, frame.as_ptr(), total);
        self.reports += 1;
    }

    /// Starts receiving the group: the group and 224.0.0.1 through the card's hash filter (all multicast on a card
    /// without one), and an unsolicited join (sent twice, as
    /// RFC 2236 suggests, in case the first is lost).
    pub fn join(&mut self, api: &TapeApi, nic: usize, config: &NetConfig) {
        if !self.filter(nic, true) {
            (api.nic_set_multicast)(nic, 1);
        }
        self.joined = true;
        self.last_report = None;
        self.send(api, nic, config, false, false);
        let until = tape_rt::micros() + 10_000;
        while tape_rt::micros() < until {
            core::hint::spin_loop();
        }
        self.send(api, nic, config, false, false);
    }

    /// A leave that keeps the NIC's multicast filter open: another group is still joined.
    pub fn leave_report(&mut self, api: &TapeApi, nic: usize, config: &NetConfig) {
        self.joined = false;
        self.send(api, nic, config, false, true);
        self.filter(nic, false);
    }

    pub fn leave(&mut self, api: &TapeApi, nic: usize, config: &NetConfig) {
        self.joined = false;
        self.send(api, nic, config, false, true);
        if !self.filter(nic, false) {
            (api.nic_set_multicast)(nic, 0);
        }
    }

    /// The hardware filter for the group and for 224.0.0.1 (the queries): false if the card has none, so the caller
    /// opens all multicast instead.
    fn filter(&self, nic: usize, on: bool) -> bool {
        tape_rt::multicast_group(nic, multicast_mac(ALL_HOSTS), on) && tape_rt::multicast_group(nic, multicast_mac(self.group), on)
    }

    /// Looks at one received frame; true if it was IGMP (so the caller need not look at it again). A query for
    /// all hosts, or for our group, schedules an answer.
    pub fn handle(&mut self, frame: &[u8], now_micros: u64) -> bool {
        if frame.len() < 14 + 20 + 8 || frame[12..14] != [0x08, 0x00] || frame[14 + 9] != 2 {
            return false;
        }
        let header = usize::from(frame[14] & 0x0f) * 4;
        let Some(message) = frame.get(14 + header..) else { return true };
        if message.len() < 8 || message[0] != 0x11 {
            return true; // someone else's report
        }
        let target = &message[4..8];
        let general = target == [0, 0, 0, 0];
        if !general && target != self.group {
            return true;
        }
        self.queries += 1;
        self.last_query = Some(now_micros);
        let v3 = message.len() >= 12;
        // Maximum response code: tenths of a second (v2); v3 codes of 128 and over are a floating-point format.
        let code = u64::from(message[1]);
        let tenths = if code < 128 { code } else { ((code & 0x0f) | 0x10) << (((code >> 4) & 7) + 3) };
        let max = tenths.max(1) * 100_000;
        // Pseudo-random delay in [0, max): the low bits of the clock are good enough to spread hosts apart.
        let delay = now_micros.wrapping_mul(2_654_435_761) % max;
        if self.due.is_none_or(|(at, _)| now_micros + delay < at) {
            self.due = Some((now_micros + delay, v3));
        }
        true
    }

    /// Sends a due answer, or a repeated join when no querier is heard; call once per loop pass, always with the
    /// same clock as `handle`.
    pub fn poll(&mut self, api: &TapeApi, nic: usize, config: &NetConfig, now_micros: u64) {
        if let Some((at, v3)) = self.due {
            if now_micros >= at {
                self.due = None;
                self.last_report = Some(now_micros);
                self.send(api, nic, config, v3, false);
            }
        } else if self.rereport_due(now_micros) {
            self.last_report = Some(now_micros);
            self.send(api, nic, config, false, false);
        }
    }

    /// True when a joined group should be reported again unasked: no query for a membership interval and no report
    /// for `REREPORT_US`. The first call after `join` only starts the count (the join itself was the report).
    fn rereport_due(&mut self, now_micros: u64) -> bool {
        if !self.joined {
            return false;
        }
        let Some(last) = self.last_report else {
            self.last_report = Some(now_micros);
            return false;
        };
        let querier_heard = self.last_query.is_some_and(|at| now_micros.saturating_sub(at) < MEMBERSHIP_US);
        !querier_heard && now_micros.saturating_sub(last) >= REREPORT_US
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined() -> Igmp {
        let mut igmp = Igmp::new([224, 0, 1, 129]);
        igmp.joined = true;
        igmp
    }

    /// A general IGMPv2 query (maximum response 1 s) as a raw frame.
    fn query() -> [u8; 46] {
        let mut frame = [0u8; 46];
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        frame[14] = 0x45;
        frame[14 + 9] = 2;
        frame[34] = 0x11;
        frame[35] = 10;
        frame
    }

    #[test]
    fn without_a_querier_a_joined_group_is_reported_every_minute() {
        let mut igmp = joined();
        assert!(!igmp.rereport_due(1_000), "the first poll after the join only starts the count");
        assert!(!igmp.rereport_due(1_000 + REREPORT_US - 1));
        assert!(igmp.rereport_due(1_000 + REREPORT_US));
    }

    #[test]
    fn a_heard_querier_takes_over_until_it_falls_silent() {
        let mut igmp = joined();
        igmp.rereport_due(0);
        assert!(igmp.handle(&query(), 5_000_000));
        assert_eq!(igmp.queries, 1);
        igmp.due = None; // its answer was sent
        assert!(!igmp.rereport_due(5_000_000 + 2 * REREPORT_US), "the querier asks; no unasked reports");
        assert!(igmp.rereport_due(5_000_000 + MEMBERSHIP_US), "a querier silent for a membership interval is gone");
    }

    #[test]
    fn a_left_or_never_joined_group_is_not_reported() {
        let mut igmp = Igmp::new([224, 0, 1, 129]);
        igmp.rereport_due(0);
        assert!(!igmp.rereport_due(10 * REREPORT_US));
    }

    #[test]
    fn a_clock_that_starts_far_from_zero_works_the_same() {
        // ptp passes TAI microseconds (about 1.8e15): nothing may assume the caller's clock starts near zero.
        let start = 1_800_000_000_000_000;
        let mut igmp = joined();
        igmp.rereport_due(start);
        assert!(!igmp.rereport_due(start + 1));
        assert!(igmp.rereport_due(start + REREPORT_US));
    }
}
