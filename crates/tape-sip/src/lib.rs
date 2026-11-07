//! SIP over UDP and the SDP of AES67 audio streams, as `no_std` code with no allocation and host tests.
//!
//! AES67 leaves connection management open and names SIP (RFC 3261) with SDP offer/answer (RFC 3264) as one way to do
//! it: an INVITE carries the sender's or receiver's stream description, the 200 OK the other side's, and both then
//! stream RTP. SIP only signals; the media clock is PTP, which SDP names in `a=ts-refclk`. [`sdp`] reads that
//! description, [`msg`] parses and builds SIP messages, [`ua`] is a user agent for one call at a time (answers
//! INVITE, OPTIONS and BYE, places calls, retransmits as RFC 3261 requires over UDP). The caller moves datagrams and
//! owns the clock; nothing here touches a socket.
#![cfg_attr(not(test), no_std)]

pub mod msg;
pub mod sdp;
pub mod ua;
