//! IGMP host side (RFC 2236 and RFC 3376), just enough to receive a multicast stream through a switch with IGMP
//! snooping: join the group (an unsolicited report), answer the querier's membership queries, leave at the end.
//!
//! Without a join, a snooping switch forwards a multicast group only to the ports that asked for it, so a
//! receiver would hear nothing. A query is answered after a random delay of up to the query's maximum response
//! time, so that the hosts on a segment do not all answer at once.
//!
//! **Version.** Reports are IGMPv3 (RFC 3376), sent to 224.0.0.22, unless an IGMPv2 querier has been heard within a
//! membership interval: then v2, as RFC 3376 7.2.1 has it (v2 before 2026-11-28). The version did not change what
//! reaches whom on the test network: clock's PTP group reaches a fos receiver on rack or aneby only while the
//! workstation (Linux) is joined, with v2 and v3 fos reports alike, and the reports are byte for byte the ones Linux
//! sends. That looks like a switch with no querier that never learns of members behind another snooping switch; a
//! querier on the network (a switch setting) is the usual cure.
//!
//! **No querier.** IGMP hosts report only when asked, which assumes a querier on the segment. A snooping switch with no
//! querier still learns members from the unsolicited join, but forgets them when its membership timer runs out
//! (RFC 2236's Group Membership Interval, 260 s by default; vendors differ) and then stops forwarding the group to
//! that port. A PTP master on the test network lost its slaves' Delay_Req after a few minutes that way (2026-11-27:
//! no IGMP query in 280 s of capture). So while no query has been heard for a membership interval, a joined group is
//! reported again every `REREPORT_US`; once a querier speaks, only its queries are answered, as the RFCs have it.
//!
//! An IGMP packet is an IPv4 packet of protocol 2, TTL 1, with the Router Alert option (RFC 2113), sent to the
//! group itself (v2 report), to 224.0.0.2 (v2 leave) or to 224.0.0.22 (v3 report and leave).

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

/// What a report says.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Report {
    /// The unsolicited join: v3 record CHANGE_TO_EXCLUDE_MODE with no sources.
    Join,
    /// An answer to a query, or a repeated join: v3 MODE_IS_EXCLUDE with no sources ("everything from the group").
    Current,
    /// The leave: v3 CHANGE_TO_INCLUDE_MODE with no sources, or a v2 leave to 224.0.0.2.
    Leave,
}

pub struct Igmp {
    group: [u8; 4],
    /// When (micros) a pending answer to a query is due.
    due: Option<u64>,
    /// When an IGMPv2 querier was last heard (a query of 8 bytes): while it counts as present, reports are v2.
    v2_query: Option<u64>,
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
        Igmp { group, due: None, v2_query: None, joined: false, last_query: None, last_report: None, queries: 0, reports: 0 }
    }

    /// The IGMP message for `report` into `message`: (destination, length). v2 while a v2 querier counts as present.
    fn message(&self, report: Report, message: &mut [u8; 16]) -> ([u8; 4], usize) {
        *message = [0; 16];
        let (destination, length) = if self.v2_query.is_none() {
            // A v3 report with one group record and no sources.
            message[0] = 0x22;
            message[6..8].copy_from_slice(&1u16.to_be_bytes());
            message[8] = match report {
                Report::Current => 2,
                Report::Leave => 3,
                Report::Join => 4,
            };
            message[12..16].copy_from_slice(&self.group);
            (IGMPV3_REPORTS, 16)
        } else {
            let leave = report == Report::Leave;
            message[0] = if leave { 0x17 } else { 0x16 };
            message[4..8].copy_from_slice(&self.group);
            (if leave { ALL_ROUTERS } else { self.group }, 8)
        };
        let sum = checksum(&message[..length]);
        message[2..4].copy_from_slice(&sum.to_be_bytes());
        (destination, length)
    }

    fn send(&mut self, api: &TapeApi, nic: usize, config: &NetConfig, report: Report) {
        let mut message = [0u8; 16];
        let (destination, length) = self.message(report, &mut message);
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
        self.send(api, nic, config, Report::Join);
        let until = tape_rt::micros() + 10_000;
        while tape_rt::micros() < until {
            core::hint::spin_loop();
        }
        self.send(api, nic, config, Report::Join);
    }

    /// A leave that keeps the NIC's multicast filter open: another group is still joined.
    pub fn leave_report(&mut self, api: &TapeApi, nic: usize, config: &NetConfig) {
        self.joined = false;
        self.send(api, nic, config, Report::Leave);
        self.filter(nic, false);
    }

    pub fn leave(&mut self, api: &TapeApi, nic: usize, config: &NetConfig) {
        self.joined = false;
        self.send(api, nic, config, Report::Leave);
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
        // A v2 query is 8 bytes, a v3 one at least 12 (RFC 3376 7.1; Ethernet padding can make a v2 one longer, but the
        // IP total length cannot).
        let total = usize::from(u16::from_be_bytes([frame[16], frame[17]]));
        if total.saturating_sub(header) < 12 {
            self.v2_query = Some(now_micros);
        }
        // Maximum response code: tenths of a second (v2); v3 codes of 128 and over are a floating-point format.
        let code = u64::from(message[1]);
        let tenths = if code < 128 { code } else { ((code & 0x0f) | 0x10) << (((code >> 4) & 7) + 3) };
        let max = tenths.max(1) * 100_000;
        // Pseudo-random delay in [0, max): the low bits of the clock are good enough to spread hosts apart.
        let delay = now_micros.wrapping_mul(2_654_435_761) % max;
        if self.due.is_none_or(|at| now_micros + delay < at) {
            self.due = Some(now_micros + delay);
        }
        true
    }

    /// Sends a due answer, or a repeated join when no querier is heard; call once per loop pass, always with the
    /// same clock as `handle`.
    pub fn poll(&mut self, api: &TapeApi, nic: usize, config: &NetConfig, now_micros: u64) {
        self.forget_old_querier(now_micros);
        if let Some(at) = self.due {
            if now_micros >= at {
                self.due = None;
                self.last_report = Some(now_micros);
                self.send(api, nic, config, Report::Current);
            }
        } else if self.rereport_due(now_micros) {
            self.last_report = Some(now_micros);
            self.send(api, nic, config, Report::Current);
        }
    }

    /// Back to v3 once the v2 querier has been silent for a membership interval (RFC 3376 8.12's Older Version
    /// Querier Present Timeout is about that long).
    fn forget_old_querier(&mut self, now_micros: u64) {
        if self.v2_query.is_some_and(|at| now_micros.saturating_sub(at) >= MEMBERSHIP_US) {
            self.v2_query = None;
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

    /// A general IGMPv2 query (maximum response 1 s) as a raw frame, padded to Ethernet's minimum like a real one.
    fn query() -> [u8; 60] {
        let mut frame = [0u8; 60];
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&28u16.to_be_bytes());
        frame[14 + 9] = 2;
        frame[34] = 0x11;
        frame[35] = 10;
        frame
    }

    /// The same as an IGMPv3 query (12 bytes: the v2 fields plus QRV/QQIC and no sources).
    fn query_v3() -> [u8; 60] {
        let mut frame = query();
        frame[16..18].copy_from_slice(&32u16.to_be_bytes());
        frame
    }

    /// (destination, the message padded with zeros, its length).
    fn sent(igmp: &Igmp, report: Report) -> ([u8; 4], [u8; 16], usize) {
        let mut message = [0u8; 16];
        let (destination, length) = igmp.message(report, &mut message);
        assert_eq!(checksum(&message[..length]), 0, "the checksum covers the message");
        (destination, message, length)
    }

    #[test]
    fn with_no_querier_reports_are_v3_to_224_0_0_22() {
        let igmp = joined();
        for (report, record) in [(Report::Join, 4), (Report::Current, 2), (Report::Leave, 3)] {
            let (destination, message, length) = sent(&igmp, report);
            assert_eq!(destination, IGMPV3_REPORTS);
            assert_eq!((message[0], length, message[7], message[8]), (0x22, 16, 1, record));
            assert_eq!(message[12..16], [224, 0, 1, 129]);
        }
    }

    #[test]
    fn a_v2_querier_makes_the_reports_v2_until_it_falls_silent() {
        let mut igmp = joined();
        assert!(igmp.handle(&query(), 1_000_000));
        let (destination, message, length) = sent(&igmp, Report::Join);
        assert_eq!((destination, &message[..length]), ([224, 0, 1, 129], &[0x16, 0, 0x08, 0x7e, 224, 0, 1, 129][..]));
        assert_eq!(sent(&igmp, Report::Leave).0, ALL_ROUTERS);
        assert_eq!(sent(&igmp, Report::Leave).1[0], 0x17);
        igmp.forget_old_querier(1_000_000 + MEMBERSHIP_US - 1);
        assert_eq!(sent(&igmp, Report::Current).1[0], 0x16);
        igmp.forget_old_querier(1_000_000 + MEMBERSHIP_US);
        assert_eq!(sent(&igmp, Report::Current).1[0], 0x22);
    }

    #[test]
    fn a_v3_querier_keeps_v3() {
        let mut igmp = joined();
        assert!(igmp.handle(&query_v3(), 1_000_000));
        assert!(igmp.v2_query.is_none());
        assert_eq!(sent(&igmp, Report::Current).1[0], 0x22);
        assert!(igmp.due.is_some(), "the query is answered");
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
