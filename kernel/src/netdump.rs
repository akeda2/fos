//! Shared by both kernels' shells: a MAC address for printing and one line of text about an Ethernet frame (`nic sniff`).

pub struct Mac<'a>(pub &'a [u8; 6]);
impl core::fmt::Display for Mac<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
        let b = self.0;
        write!(formatter, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2], b[3], b[4], b[5])
    }
}

/// A short line of text on the stack.
pub struct Line {
    bytes: [u8; 110],
    length: usize,
}

impl Line {
    pub fn new() -> Line {
        Line { bytes: [0; 110], length: 0 }
    }
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let take = text.len().min(self.bytes.len() - self.length);
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        Ok(())
    }
}

/// One line about a frame: the addresses, the type and what is inside, as far as the common protocols go.
pub fn describe_frame(frame: &[u8], line: &mut Line) {
    use core::fmt::Write;
    if frame.len() < 14 {
        let _ = write!(line, "runt frame");
        return;
    }
    let ip = |at: usize| -> (u8, u8, u8, u8) { (frame[at], frame[at + 1], frame[at + 2], frame[at + 3]) };
    let kind = u16::from_be_bytes([frame[12], frame[13]]);
    let _ = write!(line, "{} > {} ", Mac(&frame[6..12].try_into().unwrap()), Mac(&frame[0..6].try_into().unwrap()));
    match kind {
        0x0806 if frame.len() >= 42 => {
            let (sender, target) = (ip(28), ip(38));
            if frame[21] == 1 {
                let _ = write!(line, "ARP who has {}.{}.{}.{}? tell {}.{}.{}.{}", target.0, target.1, target.2, target.3, sender.0, sender.1, sender.2, sender.3);
            } else {
                let _ = write!(line, "ARP {}.{}.{}.{} is at {}", sender.0, sender.1, sender.2, sender.3, Mac(&frame[22..28].try_into().unwrap()));
            }
        }
        0x0800 if frame.len() >= 34 => {
            let (source, destination) = (ip(26), ip(30));
            let header = usize::from(frame[14] & 15) * 4;
            let _ = write!(line, "IPv4 {}.{}.{}.{} > {}.{}.{}.{} ", source.0, source.1, source.2, source.3, destination.0, destination.1, destination.2, destination.3);
            let ports = |frame: &[u8]| {
                let at = 14 + header;
                (frame.get(at..at + 4).map_or(0, |bytes| u16::from_be_bytes([bytes[0], bytes[1]])), frame.get(at..at + 4).map_or(0, |bytes| u16::from_be_bytes([bytes[2], bytes[3]])))
            };
            match frame[23] {
                1 => {
                    let _ = write!(line, "ICMP type {}", frame.get(14 + header).copied().unwrap_or(0));
                }
                6 => {
                    let (a, b) = ports(frame);
                    let _ = write!(line, "TCP {a} > {b}");
                }
                17 => {
                    let (a, b) = ports(frame);
                    let _ = write!(line, "UDP {a} > {b}");
                    match (a, b) {
                        (67 | 68, _) | (_, 67 | 68) => {
                            let _ = write!(line, " (DHCP)");
                        }
                        (53, _) | (_, 53) => {
                            let _ = write!(line, " (DNS)");
                        }
                        (9875, _) | (_, 9875) => {
                            let _ = write!(line, " (SAP)");
                        }
                        (5004..=5100, _) | (_, 5004..=5100) => {
                            let _ = write!(line, " (RTP?)");
                        }
                        _ => {}
                    }
                }
                other => {
                    let _ = write!(line, "protocol {other}");
                }
            }
        }
        0x86dd => {
            let _ = write!(line, "IPv6");
        }
        0x8100 => {
            let _ = write!(line, "VLAN tag");
        }
        0x22f0 => {
            let _ = write!(line, "AVTP (IEEE 1722)");
        }
        0x88b5 => {
            let _ = write!(line, "fos test frame");
        }
        0x88b6 => {
            let _ = write!(line, "fos terminal/chat tag {:#04x}", frame.get(14).copied().unwrap_or(0));
        }
        0x88ba => {
            let _ = write!(line, "audio-net raw audio");
        }
        0x88cc => {
            let _ = write!(line, "LLDP");
        }
        other => {
            let _ = write!(line, "type {other:#06x}");
        }
    }
}
