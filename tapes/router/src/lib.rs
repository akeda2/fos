//! Pure configuration and route-selection helpers for the IPv4 router tape.
//!
//! The executable keeps all NIC state and packet buffers in `main.rs`.  This
//! small library makes the fixed-buffer command-line grammar and longest
//! prefix matching testable without a target-specific build or a NIC.
#![no_std]

use tape_net::Ipv4Address;

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
        if position == input.len() {
            break;
        }
        let start = position;
        while position < input.len() && !input[position].is_ascii_whitespace() {
            position += 1;
        }
        if count == words.len() {
            return count + 1;
        }
        words[count] = Some(&input[start..position]);
        count += 1;
    }
    count
}

fn valid_netmask(mask: Ipv4Address) -> bool {
    let value = u32::from_be_bytes(mask.0);
    if value == 0 {
        return false;
    }
    let inverted = !value;
    inverted & inverted.wrapping_add(1) == 0
}

/// Parses `[dhcp] NIC IP NETMASK [NIC IP NETMASK ...] [default NIC GATEWAY]`.
///
/// NIC numbers are one-based in the command line, matching `nic list`.
/// The optional leading `router` word is accepted so both
/// `tape run 1 ...` and `tape run router 1 ...` are readable.
///
/// `dhcp` reserves NIC 1 (callback index zero) for a DHCP lease.  Static
/// triples then configure the remaining interfaces.  The optional `default`
/// suffix selects the one-based egress NIC and its next-hop gateway.
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
    let dhcp = words[first] == Some(&b"dhcp"[..]);
    if dhcp {
        first += 1;
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

    let static_words = static_end.saturating_sub(first);
    if static_words % 3 != 0 {
        return Err(ConfigError::WrongArgumentCount);
    }
    let interface_count = static_words / 3;
    let total_interfaces = interface_count + usize::from(dhcp);
    if total_interfaces < 2 {
        return Err(ConfigError::MissingInterface);
    }
    if interface_count > MAX_INTERFACES || total_interfaces > MAX_INTERFACES {
        return Err(ConfigError::TooManyInterfaces);
    }
    let mut configs: [Option<InterfaceConfig>; MAX_INTERFACES] = [None; MAX_INTERFACES];
    for index in 0..interface_count {
        let nic = parse_number(words[first + index * 3].unwrap())
            .filter(|number| *number != 0)
            .ok_or(ConfigError::BadNicNumber)?
            - 1;
        if configs[..index]
            .iter()
            .flatten()
            .any(|config| config.nic == nic)
        {
            return Err(ConfigError::DuplicateNic);
        }
        if dhcp && nic == 0 {
            return Err(ConfigError::DuplicateNic);
        }
        let address =
            parse_ipv4(words[first + index * 3 + 1].unwrap()).ok_or(ConfigError::BadAddress)?;
        let netmask =
            parse_ipv4(words[first + index * 3 + 2].unwrap()).ok_or(ConfigError::BadNetmask)?;
        if !valid_netmask(netmask) {
            return Err(ConfigError::BadNetmask);
        }
        configs[index] = Some(InterfaceConfig {
            nic,
            address,
            netmask,
        });
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
