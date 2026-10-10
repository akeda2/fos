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

/// Sends `TRIES` requests, one at a time (each waits up to a second), and returns the sample with the shortest delay:
/// the one least spoiled by queueing. None if the server never answered or the address cannot be reached. Blocks for up
/// to about 6 seconds, so call it before anything time-critical starts. The Escape key is not read here.
pub fn query(api: &TapeApi, nic: usize, config: &NetConfig, arp: &mut ArpCache<8>, server: Ipv4Address) -> Option<Sample> {
    let mac = net::resolve(api, nic, config, arp, server)?;
    let mut best: Option<Sample> = None;
    let mut frame = [0u8; net::MAX_FRAME];
    for attempt in 0..TRIES {
        let cookie = (tape_rt::micros() << 8) | u64::from(attempt);
        let mut request = [0u8; ntp::PACKET_LEN];
        ntp::build_request(&mut request, cookie);
        let Some(length) = net::build_udp_frame(&mut frame, config, mac, server, SOURCE_PORT, ntp::PORT, 64, &request) else { return None };
        let sent = local_ns();
        net::send_frame(api, nic, &frame[..length]);
        let deadline = tape_rt::micros() + 1_000_000;
        while tape_rt::micros() < deadline {
            let received = (api.nic_receive)(nic, frame.as_mut_ptr(), frame.len());
            if received == 0 {
                core::hint::spin_loop();
                continue;
            }
            let arrived = local_ns();
            if let net::Inbound::Udp { source, source_port, payload, .. } = net::inbound(api, nic, config, arp, &frame[..received], SOURCE_PORT..=SOURCE_PORT, 0) {
                if source == server && source_port == ntp::PORT {
                    if let Some(reply) = ntp::parse_reply(payload, cookie) {
                        let sample = ntp::sample(&reply, sent, arrived);
                        if best.is_none_or(|b| sample.delay < b.delay) {
                            best = Some(sample);
                        }
                        break;
                    }
                }
            }
        }
        // A little apart, so the samples do not share one queueing episode.
        let pause = tape_rt::micros() + 100_000;
        while tape_rt::micros() < pause {
            tape_rt::idle();
        }
    }
    best
}
