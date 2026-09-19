//! Pure configuration and route-selection helpers for the IPv4 router tape.
//!
//! The executable keeps all NIC state and packet buffers in `main.rs`.  This
//! small library makes the fixed-buffer command-line grammar and longest
//! prefix matching testable without a target-specific build or a NIC.
#![no_std]

use tape_net::{DHCP_MAX_DNS_SERVERS, Ipv4Address};

pub const MAX_INTERFACES: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterfaceConfig {
    pub nic: u32,
    pub address: Ipv4Address,
    pub netmask: Ipv4Address,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DefaultRoute {
    pub nic: u32,
    pub gateway: Ipv4Address,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouterConfig {
    pub interfaces: [Option<InterfaceConfig>; MAX_INTERFACES],
    pub interface_count: usize,
    pub dhcp: bool,
    pub dhcp_server: bool,
    pub dns_servers: [Option<Ipv4Address>; DHCP_MAX_DNS_SERVERS],
    pub dns_server_count: usize,
    pub default_route: Option<DefaultRoute>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    MissingInterface,
    TooManyInterfaces,
    WrongArgumentCount,
    BadNicNumber,
    DuplicateNic,
    BadAddress,
    BadNetmask,
    MissingDefaultRouteValue,
    BadDefaultNic,
    BadGateway,
    DefaultNicNotConfigured,
    MissingDnsValue,
    BadDnsServer,
    TooManyDnsServers,
}

pub fn parse_ipv4(text: &[u8]) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut component = 0usize;
    let mut value = 0u16;
    let mut digits = 0usize;

    for &byte in text.iter().chain(core::iter::once(&b'.')) {
        match byte {
            b'0'..=b'9' => {
                value = value.checked_mul(10)?.checked_add(u16::from(byte - b'0'))?;
                if value > 255 {
                    return None;
                }
                digits += 1;
            }
            b'.' if digits != 0 && component < 4 => {
                octets[component] = value as u8;
                component += 1;
                value = 0;
                digits = 0;
            }
            _ => return None,
        }
    }

    (component == 4).then_some(Ipv4Address::new(octets))
}

fn parse_prefix_mask(text: &[u8]) -> Option<Ipv4Address> {
    let prefix = parse_number(text)?;
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Some(Ipv4Address::new(mask.to_be_bytes()))
}

fn parse_cidr(text: &[u8]) -> core::result::Result<(Ipv4Address, Ipv4Address), ConfigError> {
    let Some(slash) = text.iter().position(|&byte| byte == b'/') else {
        return Err(ConfigError::BadAddress);
    };
    let address = parse_ipv4(&text[..slash]).ok_or(ConfigError::BadAddress)?;
    let mask = parse_prefix_mask(&text[slash + 1..]).ok_or(ConfigError::BadNetmask)?;
    Ok((address, mask))
}

fn parse_netmask(text: &[u8]) -> Option<Ipv4Address> {
    text.strip_prefix(b"/")
        .map_or_else(|| parse_ipv4(text), parse_prefix_mask)
}

fn parse_number(text: &[u8]) -> Option<u32> {
    if text.is_empty() {
        return None;
    }
    text.iter().try_fold(0u32, |value, &byte| {
        (b'0'..=b'9')
            .contains(&byte)
            .then_some(value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?)
    })
}

fn split_words<'a>(input: &'a [u8], words: &mut [Option<&'a [u8]>]) -> usize {
    let mut count = 0;
    let mut position = 0;
    while position < input.len() {
        while position < input.len() && input[position].is_ascii_whitespace() {
            position += 1;
        }
        if position < input.len() && input[position] == b'#' {
            while position < input.len() && input[position] != b'\n' {
                position += 1;
            }
            continue;
        }
        if position == input.len() {
            break;
        }
        let start = position;
        while position < input.len()
            && !input[position].is_ascii_whitespace()
            && input[position] != b'#'
        {
            position += 1;
        }
        if count == words.len() {
            return count + 1;
        }
        words[count] = Some(&input[start..position]);
        count += 1;
        if position < input.len() && input[position] == b'#' {
            while position < input.len() && input[position] != b'\n' {
                position += 1;
            }
        }
    }
    count
}

fn valid_netmask(mask: Ipv4Address) -> bool {
    let value = u32::from_be_bytes(mask.0);
    let inverted = !value;
    inverted & inverted.wrapping_add(1) == 0
}

/// Parses `[dhcp] [dhcp-server] [dns SERVER [SERVER]] NIC IP NETMASK
/// [NIC IP NETMASK ...] [default NIC GATEWAY]`.
///
/// NIC numbers are one-based in the command line, matching `nic list`.
/// The optional leading `router` word is accepted so both
/// `tape run 1 ...` and `tape run router 1 ...` are readable.
/// Interfaces may use either the legacy `NIC IP NETMASK` form, the compact
/// `NIC IP/PREFIX` form, or `NIC IP /PREFIX`. For example, all of these
/// describe the same interface: `1 10.0.1.1 255.255.255.0`,
/// `1 10.0.1.1/24`, and `1 10.0.1.1 /24`.
///
/// `dhcp` reserves NIC 1 (callback index zero) for a DHCP lease.  Static
/// triples then configure the remaining interfaces.  The optional `default`
/// suffix selects the one-based egress NIC and its next-hop gateway.
///
/// `dhcp-server` enables the built-in DHCP server on every configured
/// interface except NIC 1. Its pool is derived from each interface subnet.
///
/// `dns SERVER [SERVER]` overrides DNS servers learned from upstream DHCP.
/// Up to two IPv4 DNS server addresses may be supplied.
///
/// Whitespace may include newlines, and `#` starts a comment through the end
/// of its line, so the same grammar can be kept as a readable FOSFS file.
pub fn parse_config(input: &[u8]) -> core::result::Result<RouterConfig, ConfigError> {
    let mut words = [None; 20];
    let count = split_words(input, &mut words);
    if count == 0 || count > words.len() {
        return Err(ConfigError::WrongArgumentCount);
    }
    let mut first = 0;
    if words[0] == Some(&b"router"[..]) {
        first = 1;
    }
    if first == count {
        return Err(ConfigError::MissingInterface);
    }
    let mut dhcp = false;
    let mut dhcp_server = false;
    let mut dns_servers = [None; DHCP_MAX_DNS_SERVERS];
    let mut dns_server_count = 0usize;
    loop {
        match words[first] {
            Some(b"dhcp") => {
                dhcp = true;
                first += 1;
            }
            Some(b"dhcp-server") => {
                dhcp_server = true;
                first += 1;
            }
            Some(b"dns") => {
                first += 1;
                let dns = words
                    .get(first)
                    .and_then(|word| *word)
                    .ok_or(ConfigError::MissingDnsValue)?;
                let address = parse_ipv4(dns)
                    .filter(|address| *address != Ipv4Address::UNSPECIFIED)
                    .ok_or(ConfigError::BadDnsServer)?;
                if dns_server_count == DHCP_MAX_DNS_SERVERS {
                    return Err(ConfigError::TooManyDnsServers);
                }
                dns_servers[dns_server_count] = Some(address);
                dns_server_count += 1;
                first += 1;
                if let Some(next) = words.get(first).and_then(|word| *word) {
                    if let Some(address) = parse_ipv4(next)
                        .filter(|address| *address != Ipv4Address::UNSPECIFIED)
                    {
                        if dns_server_count == DHCP_MAX_DNS_SERVERS {
                            return Err(ConfigError::TooManyDnsServers);
                        }
                        dns_servers[dns_server_count] = Some(address);
                        dns_server_count += 1;
                        first += 1;
                    }
                }
            }
            _ => break,
        }
        if first == count {
            return Err(ConfigError::MissingInterface);
        }
    }

    let mut static_end = count;
    let mut default_route = None;
    if count >= first + 3 && words[count - 3] == Some(&b"default"[..]) {
        let nic = parse_number(words[count - 2].unwrap())
            .filter(|number| *number != 0)
            .ok_or(ConfigError::BadDefaultNic)?
            - 1;
        let gateway = parse_ipv4(words[count - 1].unwrap()).ok_or(ConfigError::BadGateway)?;
        if gateway == Ipv4Address::UNSPECIFIED {
            return Err(ConfigError::BadGateway);
        }
        default_route = Some(DefaultRoute { nic, gateway });
        static_end -= 3;
    } else if words[first..count]
        .iter()
        .flatten()
        .any(|word| *word == b"default")
    {
        return Err(ConfigError::MissingDefaultRouteValue);
    }

    let mut configs: [Option<InterfaceConfig>; MAX_INTERFACES] = [None; MAX_INTERFACES];
    let mut interface_count = 0usize;
    let mut position = first;
    while position < static_end {
        if interface_count == MAX_INTERFACES {
            return Err(ConfigError::TooManyInterfaces);
        }
        let nic = parse_number(words[position].ok_or(ConfigError::WrongArgumentCount)?)
            .filter(|number| *number != 0)
            .ok_or(ConfigError::BadNicNumber)?
            - 1;
        let address_word = words
            .get(position + 1)
            .and_then(|word| *word)
            .ok_or(ConfigError::WrongArgumentCount)?;
        let (address, netmask, consumed) = if address_word.contains(&b'/') {
            let (address, netmask) = parse_cidr(address_word)?;
            (address, netmask, 2)
        } else {
            let address = parse_ipv4(address_word).ok_or(ConfigError::BadAddress)?;
            let netmask_word = words
                .get(position + 2)
                .and_then(|word| *word)
                .ok_or(ConfigError::WrongArgumentCount)?;
            let netmask = parse_netmask(netmask_word).ok_or(ConfigError::BadNetmask)?;
            (address, netmask, 3)
        };
        if !valid_netmask(netmask) {
            return Err(ConfigError::BadNetmask);
        }
        if configs[..interface_count]
            .iter()
            .flatten()
            .any(|config| config.nic == nic)
        {
            return Err(ConfigError::DuplicateNic);
        }
        if dhcp && nic == 0 {
            return Err(ConfigError::DuplicateNic);
        }
        configs[interface_count] = Some(InterfaceConfig {
            nic,
            address,
            netmask,
        });
        interface_count += 1;
        position += consumed;
    }
    let total_interfaces = interface_count + usize::from(dhcp);
    if total_interfaces < 2 {
        return Err(ConfigError::MissingInterface);
    }
    if total_interfaces > MAX_INTERFACES {
        return Err(ConfigError::TooManyInterfaces);
    }
    if let Some(route) = default_route {
        let configured = (dhcp && route.nic == 0)
            || configs[..interface_count]
                .iter()
                .flatten()
                .any(|config| config.nic == route.nic);
        if !configured {
            return Err(ConfigError::DefaultNicNotConfigured);
        }
        if let Some(config) = configs[..interface_count]
            .iter()
            .flatten()
            .find(|config| config.nic == route.nic)
        {
            if !same_subnet(config.address, config.netmask, route.gateway) {
                return Err(ConfigError::BadGateway);
            }
        }
    }
    Ok(RouterConfig {
        interfaces: configs,
        interface_count,
        dhcp,
        dhcp_server,
        dns_servers,
        dns_server_count,
        default_route,
    })
}

pub fn as_u32(address: Ipv4Address) -> u32 {
    u32::from_be_bytes(address.0)
}

pub fn same_subnet(local: Ipv4Address, netmask: Ipv4Address, other: Ipv4Address) -> bool {
    as_u32(local) & as_u32(netmask) == as_u32(other) & as_u32(netmask)
}

pub fn prefix_length(netmask: Ipv4Address) -> u32 {
    as_u32(netmask).count_ones()
}

/// Returns the most-specific directly connected interface for an address.
pub fn longest_match(
    configs: &[Option<InterfaceConfig>; MAX_INTERFACES],
    count: usize,
    destination: Ipv4Address,
) -> Option<usize> {
    let mut selected = None;
    let mut selected_prefix = 0;
    for (index, config) in configs[..count].iter().enumerate() {
        let Some(config) = config else { continue };
        let prefix = prefix_length(config.netmask);
        if same_subnet(config.address, config.netmask, destination)
            && (selected.is_none() || prefix > selected_prefix)
        {
            selected = Some(index);
            selected_prefix = prefix;
        }
    }
    selected
}

/// Selects a directly connected route, or the configured default route.
///
/// The returned tuple is the interface-array index and the IPv4 next hop.
/// Direct routes ARP for the destination itself; a default route ARPs for its
/// configured gateway instead.
pub fn route_next_hop(
    configs: &[Option<InterfaceConfig>; MAX_INTERFACES],
    count: usize,
    destination: Ipv4Address,
    default_route: Option<DefaultRoute>,
) -> Option<(usize, Ipv4Address)> {
    if let Some(index) = longest_match(configs, count, destination) {
        return Some((index, destination));
    }
    let route = default_route?;
    let index = configs[..count]
        .iter()
        .position(|config| config.is_some_and(|config| config.nic == route.nic))?;
    Some((index, route.gateway))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_interface_configurations() {
        let config = parse_config(b"1 10.0.1.1 255.255.255.0 2 10.0.2.1 255.255.255.0").unwrap();
        assert_eq!(config.interface_count, 2);
        assert_eq!(
            config.interfaces[0],
            Some(InterfaceConfig {
                nic: 0,
                address: Ipv4Address::new([10, 0, 1, 1]),
                netmask: Ipv4Address::new([255, 255, 255, 0]),
            })
        );
        assert_eq!(
            longest_match(
                &config.interfaces,
                config.interface_count,
                Ipv4Address::new([10, 0, 2, 9])
            ),
            Some(1)
        );
    }

    #[test]
    fn accepts_optional_router_word_and_rejects_bad_configs() {
        assert_eq!(
            parse_config(b"router 1 192.0.2.1 255.255.255.0 2 198.51.100.1 255.255.255.0")
                .unwrap()
                .interface_count,
            2
        );
        assert_eq!(
            parse_config(b"1 192.0.2.1 255.0.255.0 2 198.51.100.1 255.255.255.0"),
            Err(ConfigError::BadNetmask)
        );
        assert_eq!(
            parse_config(b"1 192.0.2.1 255.255.255.0 1 198.51.100.1 255.255.255.0"),
            Err(ConfigError::DuplicateNic)
        );
    }

    #[test]
    fn accepts_cidr_and_prefix_only_netmask_syntax() {
        let compact = parse_config(b"router 1 10.0.1.1/24 2 10.0.2.1/24").expect("compact CIDR");
        let separate =
            parse_config(b"router 1 10.0.1.1 /24 2 10.0.2.1 /24").expect("separate prefix");
        let dotted = parse_config(b"router 1 10.0.1.1 255.255.255.0 2 10.0.2.1 255.255.255.0")
            .expect("dotted netmask");
        assert_eq!(compact, dotted);
        assert_eq!(separate, dotted);
        assert_eq!(
            compact.interfaces[0].unwrap().netmask,
            Ipv4Address::new([255, 255, 255, 0])
        );
    }

    #[test]
    fn accepts_zero_prefix_and_rejects_invalid_cidr() {
        assert!(parse_config(b"1 10.0.1.1/0 2 10.0.2.1/24").is_ok());
        assert_eq!(
            parse_config(b"1 10.0.1.1/33 2 10.0.2.1/24"),
            Err(ConfigError::BadNetmask)
        );
        assert_eq!(
            parse_config(b"1 10.0.1.1/x 2 10.0.2.1/24"),
            Err(ConfigError::BadNetmask)
        );
    }

    #[test]
    fn parses_dhcp_and_explicit_default_route() {
        let config =
            parse_config(b"router dhcp 2 10.0.2.1 255.255.255.0 default 1 10.0.0.1").unwrap();
        assert!(config.dhcp);
        assert_eq!(config.interface_count, 1);
        assert_eq!(
            config.default_route,
            Some(DefaultRoute {
                nic: 0,
                gateway: Ipv4Address::new([10, 0, 0, 1]),
            })
        );
        let mut runtime = config.interfaces;
        runtime[1] = Some(InterfaceConfig {
            nic: 0,
            address: Ipv4Address::new([10, 0, 0, 2]),
            netmask: Ipv4Address::new([255, 255, 255, 0]),
        });
        assert_eq!(
            route_next_hop(
                &runtime,
                2,
                Ipv4Address::new([192, 0, 2, 1]),
                config.default_route
            ),
            Some((1, Ipv4Address::new([10, 0, 0, 1])))
        );
        assert_eq!(
            route_next_hop(
                &runtime,
                2,
                Ipv4Address::new([10, 0, 2, 9]),
                config.default_route
            ),
            Some((0, Ipv4Address::new([10, 0, 2, 9])))
        );
    }

    #[test]
    fn parses_dhcp_server_mode_without_reserving_nic_one() {
        let config = parse_config(b"router dhcp-server 1 10.0.1.1/24 2 10.0.2.1/24").unwrap();
        assert!(!config.dhcp);
        assert!(config.dhcp_server);
        assert_eq!(config.interface_count, 2);
    }

    #[test]
    fn parses_dns_override_for_upstream_dhcp() {
        let config =
            parse_config(b"router dhcp dhcp-server dns 10.0.1.53 1.1.1.1 2 10.0.2.1/24")
                .unwrap();
        assert!(config.dhcp);
        assert!(config.dhcp_server);
        assert_eq!(config.dns_server_count, 2);
        assert_eq!(
            config.dns_servers,
            [
                Some(Ipv4Address::new([10, 0, 1, 53])),
                Some(Ipv4Address::new([1, 1, 1, 1])),
            ]
        );
        assert_eq!(
            parse_config(b"router dhcp dns 0.0.0.0 2 10.0.2.1/24"),
            Err(ConfigError::BadDnsServer)
        );
    }

    #[test]
    fn parses_multiline_configuration_with_comments() {
        let config = parse_config(
            b"# Serve the downstream LANs\n\
              dhcp-server\n\
              1 10.0.1.1/24 # primary, never serves DHCP\n\
              2 10.0.2.1/24\n\
              default 1 10.0.1.254\n",
        )
        .unwrap();
        assert!(config.dhcp_server);
        assert_eq!(config.interface_count, 2);
        assert_eq!(
            config.default_route,
            Some(DefaultRoute {
                nic: 0,
                gateway: Ipv4Address::new([10, 0, 1, 254]),
            })
        );
    }

    #[test]
    fn rejects_invalid_default_and_minimum_configurations() {
        assert_eq!(
            parse_config(b"1 192.0.2.1 255.255.255.0"),
            Err(ConfigError::MissingInterface)
        );
        assert_eq!(
            parse_config(
                b"1 192.0.2.1 255.255.255.0 2 198.51.100.1 255.255.255.0 default 3 192.0.2.254"
            ),
            Err(ConfigError::DefaultNicNotConfigured)
        );
        assert_eq!(
            parse_config(
                b"1 192.0.2.1 255.255.255.0 2 198.51.100.1 255.255.255.0 default 1 203.0.113.1"
            ),
            Err(ConfigError::BadGateway)
        );
        assert_eq!(
            parse_config(b"router dhcp 1 192.0.2.1 255.255.255.0 2 198.51.100.1 255.255.255.0"),
            Err(ConfigError::DuplicateNic)
        );
        assert_eq!(
            parse_config(b"1 192.0.2.1 255.255.255.0 2 198.51.100.1 255.255.255.0 default"),
            Err(ConfigError::MissingDefaultRouteValue)
        );
        assert_eq!(
            parse_config(
                b"1 192.0.2.1 255.255.255.0 2 198.51.100.1 255.255.255.0 default x 192.0.2.254"
            ),
            Err(ConfigError::BadDefaultNic)
        );
    }
}
