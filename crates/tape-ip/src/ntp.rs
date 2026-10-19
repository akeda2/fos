//! Asking an NTP server the time over the tape ABI's frame calls. The arithmetic is `tape_net::ntp`.

use crate::net::{self, NetConfig};
use tape_net::ntp::{self, Sample};
use tape_net::{ArpCache, Ipv4Address};
use tape_rt::TapeApi;

const SOURCE_PORT: u16 = 40123;
const TRIES: u32 = 4;

fn local_ns() -> i64 {
    tape_rt::micros() as i64 * 1000
}

enum Phase {
    Idle,
    /// Waiting for the pause to end, then the next request goes out.
    Pause { until: u64 },
    /// A request is out: its cookie, the local time it left, and when to give up.
    Waiting { cookie: u64, sent: i64, deadline: u64 },
}

/// A query that does not block: `begin` it, then call `poll` every pass of a loop that has other work. It sends `TRIES` requests,
/// one at a time (each waits up to a second, 100 ms apart) and ends with the sample of shortest delay: the one least spoiled by
/// queueing. A caller that wants good receive times should poll without sleeping while `busy()`.
pub struct Client {
    server: Ipv4Address,
    phase: Phase,
    attempt: u32,
    best: Option<Sample>,
}

impl Client {
    pub fn new(server: Ipv4Address) -> Client {
        Client { server, phase: Phase::Idle, attempt: 0, best: None }
    }

    pub fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// Starts a query (dropping one in progress).
    pub fn begin(&mut self) {
        self.attempt = 0;
        self.best = None;
        self.phase = Phase::Pause { until: 0 };
    }

    /// Does what is due. None while the query runs; Some(result) once, when it ends (None inside: the server never answered or
    /// cannot be reached). The first request needs the server's (or gateway's) MAC address: when it is not in `arp`, that
    /// asks and waits up to two seconds, so resolve it first with a blocking `query` or `net::resolve`.
    pub fn poll(&mut self, api: &TapeApi, nic: usize, config: &NetConfig, arp: &mut ArpCache<8>) -> Option<Option<Sample>> {
        let mut frame = [0u8; net::MAX_FRAME];
        match self.phase {
            Phase::Idle => None,
            Phase::Pause { until } => {
                if tape_rt::micros() < until {
                    return None;
                }
                if self.attempt >= TRIES {
                    self.phase = Phase::Idle;
                    return Some(self.best);
                }
                let Some(mac) = net::resolve(api, nic, config, arp, self.server) else { return self.finish() };
                let cookie = (tape_rt::micros() << 8) | u64::from(self.attempt);
                let mut request = [0u8; ntp::PACKET_LEN];
                ntp::build_request(&mut request, cookie);
                let Some(length) = net::build_udp_frame(&mut frame, config, mac, self.server, SOURCE_PORT, ntp::PORT, 64, &request) else { return self.finish() };
                let sent = local_ns();
                net::send_frame(api, nic, &frame[..length]);
                self.phase = Phase::Waiting { cookie, sent, deadline: tape_rt::micros() + 1_000_000 };
                None
            }
            Phase::Waiting { cookie, sent, deadline } => {
                for _ in 0..4 {
                    let received = (api.nic_receive)(nic, frame.as_mut_ptr(), frame.len());
                    if received == 0 {
                        break;
                    }
                    let arrived = local_ns();
                    if let net::Inbound::Udp { source, source_port, payload, .. } = net::inbound(api, nic, config, arp, &frame[..received], SOURCE_PORT..=SOURCE_PORT, 0) {
                        if source == self.server && source_port == ntp::PORT {
                            if let Some(reply) = ntp::parse_reply(payload, cookie) {
                                let sample = ntp::sample(&reply, sent, arrived);
                                if self.best.is_none_or(|b| sample.delay < b.delay) {
                                    self.best = Some(sample);
                                }
                                self.next_attempt();
                                return None;
                            }
                        }
                    }
                }
                if tape_rt::micros() >= deadline {
                    self.next_attempt();
                }
                None
            }
        }
    }

    // A little apart, so the samples do not share one queueing episode.
    fn next_attempt(&mut self) {
        self.attempt += 1;
        self.phase = Phase::Pause { until: tape_rt::micros() + 100_000 };
    }

    fn finish(&mut self) -> Option<Option<Sample>> {
        self.phase = Phase::Idle;
        Some(self.best)
    }
}

/// Runs a whole query and waits for it: the best of `TRIES` samples, None if the server never answered or the address cannot be
/// reached. Blocks for up to about 6 seconds, so call it before anything time-critical starts. The Escape key is not read here.
pub fn query(api: &TapeApi, nic: usize, config: &NetConfig, arp: &mut ArpCache<8>, server: Ipv4Address) -> Option<Sample> {
    let mut client = Client::new(server);
    client.begin();
    loop {
        if let Some(result) = client.poll(api, nic, config, arp) {
            return result;
        }
        core::hint::spin_loop();
    }
}
