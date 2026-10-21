//! Setting the machine's wall clock from an NTP server, for tapes that show the time of day but are not clocks (`kaffe`).
//!
//! `WallSync::connect` brings an interface up (DHCP or a fixed address) and finds the server (the one DHCP names, an
//! address or a name); `query` or `begin`/`poll` ask it and set the wall clock (and the RTC, where there is one) to local
//! time when it differs by a second or more. The zone is `tz=` if given, else guessed from the machine's own clock the first
//! time (as the `clock` tape does), else UTC. Arguments read: `ntp=dhcp|A.B.C.D|NAME|off` `ip=dhcp|A.B.C.D/N` `gateway=`
//! `dns=` `nic=N` `tz=+H[:MM]`.

use crate::{dns, net, ntp};
use tape_net::ntp::{self as codec, Civil};
use tape_net::{ArpCache, Ipv4Address, MacAddress};
use tape_rt::{TapeApi, arg_value as value, println};

pub struct WallSync {
    nic: usize,
    config: net::NetConfig,
    arp: ArpCache<8>,
    client: ntp::Client,
    pub server: Ipv4Address,
    /// Minutes east of UTC, once known.
    pub zone: Option<i32>,
    /// `micros()` of the last answer that was adopted.
    pub last_ok_us: Option<u64>,
    /// Round-trip delay of that answer, in nanoseconds.
    pub delay_ns: i64,
}

/// `tz=` as minutes east of UTC: `Ok(None)` when absent, `Err` when it cannot be read.
pub fn zone_arg(arguments: &str) -> Result<Option<i32>, ()> {
    match value(arguments, "tz") {
        Some(text) => codec::parse_zone(text).map(Some).ok_or(()),
        None => Ok(None),
    }
}

fn wall_seconds() -> Option<i64> {
    let w = tape_rt::wall_clock()?;
    Some(codec::unix_from_civil(&Civil { year: w.year, month: w.month, day: w.day, hour: w.hour, minute: w.minute, second: w.second }))
}

impl WallSync {
    /// `who` prefixes what is printed. None (with the reason printed) when no interface or server could be found, or
    /// `ntp=off`.
    pub fn connect(api: &TapeApi, who: &str, arguments: &str) -> Option<WallSync> {
        let spec = value(arguments, "ntp").unwrap_or("dhcp");
        if spec == "off" {
            return None;
        }
        let zone = match zone_arg(arguments) {
            Ok(zone) => zone,
            Err(()) => {
                println!("{who}: tz= is hours east of UTC, like +2, -5 or +5:30.");
                return None;
            }
        };
        let nic = value(arguments, "nic").and_then(|v| v.parse::<usize>().ok()).map_or(tape_rt::active_nic(), |n| n.saturating_sub(1));
        if (api.nic_count)() <= nic {
            println!("{who}: this machine has no NIC {}.", nic + 1);
            return None;
        }
        let mut mac = [0u8; 6];
        (api.nic_mac)(nic, mac.as_mut_ptr());
        let ip = value(arguments, "ip").unwrap_or("dhcp");
        let (config, dhcp_ntp, dhcp_dns) = if ip == "dhcp" {
            println!("{who}: asking DHCP for an address on NIC {}...", nic + 1);
            match net::dhcp_with_servers(api, nic, MacAddress(mac)) {
                Some(found) => found,
                None => {
                    println!("{who}: no DHCP answer; give ip=A.B.C.D/N, or ntp=off. Using the machine's clock.");
                    return None;
                }
            }
        } else {
            match net::static_config(MacAddress(mac), ip, value(arguments, "gateway").unwrap_or("")) {
                Ok(config) => (config, None, None),
                Err(reason) => {
                    println!("{who}: {reason}.");
                    return None;
                }
            }
        };
        let mut arp = ArpCache::<8>::new();
        let have_gateway = config.gateway.0 != [0; 4];
        let server = if spec == "dhcp" {
            match dhcp_ntp {
                Some(server) => server,
                None if have_gateway => {
                    println!("{who}: DHCP names no NTP server (option 42); trying the gateway.");
                    config.gateway
                }
                None => {
                    println!("{who}: DHCP names no NTP server and there is no gateway; give ntp=A.B.C.D. Using the machine's clock.");
                    return None;
                }
            }
        } else if let Some(server) = net::parse_ip(spec) {
            server
        } else {
            let Some(resolver) = value(arguments, "dns").and_then(net::parse_ip).or(dhcp_dns).or(have_gateway.then_some(config.gateway)) else {
                println!("{who}: {spec} is a name and no DNS server is known (dns=A.B.C.D). Using the machine's clock.");
                return None;
            };
            match dns::lookup(api, nic, &config, &mut arp, resolver, tape_net::dns::PORT, spec) {
                Ok(server) => server,
                Err(reason) => {
                    println!("{who}: {spec}: {reason}. Using the machine's clock.");
                    return None;
                }
            }
        };
        let s = server.0;
        println!("{who}: NTP server {}.{}.{}.{}.", s[0], s[1], s[2], s[3]);
        Some(WallSync { nic, config, arp, client: ntp::Client::new(server), server, zone, last_ok_us: None, delay_ns: 0 })
    }

    /// Asks the server and waits (up to about 6 seconds), then sets the clock. The reason for a failure is ready to show.
    pub fn query(&mut self, api: &TapeApi) -> Result<(), &'static str> {
        let sample = ntp::query(api, self.nic, &self.config, &mut self.arp, self.server).ok_or("no answer from the NTP server")?;
        self.adopt(sample)
    }

    /// Starts a query that `poll` carries on without blocking.
    pub fn begin(&mut self) {
        self.client.begin();
    }

    /// None while the query is under way, then its outcome.
    pub fn poll(&mut self, api: &TapeApi) -> Option<Result<(), &'static str>> {
        let result = self.client.poll(api, self.nic, &self.config, &mut self.arp)?;
        Some(match result {
            Some(sample) => self.adopt(sample),
            None => Err("no answer from the NTP server"),
        })
    }

    fn adopt(&mut self, sample: codec::Sample) -> Result<(), &'static str> {
        let local_ns = tape_rt::micros() as i64 * 1000;
        let unix = (local_ns + sample.offset).div_euclid(1_000_000_000);
        if codec::civil_from_unix(unix).year < 2024 {
            return Err("the server's date cannot be right");
        }
        self.last_ok_us = Some(tape_rt::micros());
        self.delay_ns = sample.delay;
        let zone = *self.zone.get_or_insert_with(|| wall_seconds().and_then(|wall| codec::guess_zone(wall, unix)).unwrap_or(0));
        let local = (local_ns + sample.offset + 500_000_000).div_euclid(1_000_000_000) + i64::from(zone) * 60;
        if wall_seconds().is_none_or(|wall| (wall - local).abs() >= 1) {
            let date = codec::civil_from_unix(local);
            tape_rt::set_wall_clock(&tape_rt::WallClock { year: date.year, month: date.month, day: date.day, hour: date.hour, minute: date.minute, second: date.second, reserved: 0 });
        }
        Ok(())
    }
}
