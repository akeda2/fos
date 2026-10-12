//! Turning a host name into an IPv4 address over the tape ABI's frame calls. The wire format is `tape_net::dns`.
//! Blocking, like `ntp::query`: call it before anything time-critical starts. The Escape key is not read here.

use crate::net::{self, NetConfig};
use tape_net::dns::{self, Failure};
use tape_net::{ArpCache, Ipv4Address};
use tape_rt::TapeApi;

const SOURCE_PORT: u16 = 40124;
const TRIES: u32 = 3;
const WAIT_MICROS: u64 = 1_500_000;

/// An address if `text` is a dotted quad, else the first A record `server` (`port`, normally `dns::PORT`) gives for it
/// as a name. The error is a sentence for the tape to print.
pub fn lookup(api: &TapeApi, nic: usize, config: &NetConfig, arp: &mut ArpCache<8>, server: Ipv4Address, port: u16, text: &str) -> Result<Ipv4Address, &'static str> {
    if let Some(address) = net::parse_ip(text) {
        return Ok(address);
    }
    if !dns::valid_name(text) {
        return Err("not an IP address or a valid host name");
    }
    if server == Ipv4Address([0; 4]) {
        return Err("no DNS server known (DHCP named none; give one)");
    }
    let mac = net::resolve(api, nic, config, arp, server).ok_or("the DNS server cannot be reached (no ARP answer)")?;
    let mut frame = [0u8; net::MAX_FRAME];
    let mut query = [0u8; dns::MAX_QUERY];
    for attempt in 0..TRIES {
        let id = (tape_rt::micros() as u16) ^ (attempt as u16).wrapping_mul(0x9E37);
        let length = dns::build_query(&mut query, id, text).ok_or("not a valid host name")?;
        let total = net::build_udp_frame(&mut frame, config, mac, server, SOURCE_PORT, port, 64, &query[..length]).ok_or("DNS query did not fit a frame")?;
        net::send_frame(api, nic, &frame[..total]);
        let deadline = tape_rt::micros() + WAIT_MICROS;
        while tape_rt::micros() < deadline {
            let received = (api.nic_receive)(nic, frame.as_mut_ptr(), frame.len());
            if received == 0 {
                tape_rt::idle();
                continue;
            }
            if let net::Inbound::Udp { source, source_port, payload, .. } = net::inbound(api, nic, config, arp, &frame[..received], SOURCE_PORT..=SOURCE_PORT, 0) {
                if source == server && source_port == port {
                    match dns::parse_reply(payload, id, text) {
                        Ok(octets) => return Ok(Ipv4Address(octets)),
                        Err(Failure::NotOurs) => {}
                        Err(failure) => return Err(dns::describe(failure)),
                    }
                }
            }
        }
    }
    Err("the DNS server did not answer")
}
