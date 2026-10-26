//! The IPv4 side that network tapes share (`audio-net`, `ptp`): `dns` (turning a host name into an address), `ntp` (asking an NTP server the time), `net` (addressing, ARP, UDP/RTP framing, a DHCP
//! client, all over the tape ABI's raw frame calls) and `igmp` (joining and leaving multicast groups, answering
//! queries). Built on the protocol codecs in `tape-net`. Tapes used to include these files by path; do not do that.

#![no_std]

pub mod dns;
pub mod igmp;
pub mod net;
pub mod tcp;
pub mod ntp;
pub mod wallclock;
