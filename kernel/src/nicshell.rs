//! The `nic` command both shells share: the interface list, `nic sniff [N]`, `nic promisc N on|off`, `nic peers`, `nic discover`, `nic use N`, `nic map`, `nic send`, `nic recv`.
//!
//! The wording and the loops live here once; what differs is behind `imp`, one small block per kernel (as in
//! `service_host.rs`): the x86 kernel reads its NIC drivers (`nic.rs`), the Pi its one Genet. x86-only subcommands
//! (`park`, `phy`, `ts`, `csum`, `vlan`, `mcast`, `regs`: they drive x86 NIC chips) stay in `term.rs`.

use crate::netdump::{describe_frame, Line, Mac};

/// What every interface shows in the list.
pub struct Summary {
    pub family: &'static str,
    pub mac: [u8; 6],
    pub link_up: bool,
    /// Negotiated speed in Mbit/s and whether the link is full duplex.
    pub speed: Option<(u32, bool)>,
    pub received: u64,
    pub sent: u64,
    pub dropped: bool,
    /// The remote terminal session runs over this interface.
    pub terminal: bool,
}

pub fn command(rest: &str) {
    let mut words = rest.split_whitespace();
    match words.next() {
        None | Some("list") => list(),
        Some("sniff") => sniff(words.next()),
        Some("promisc") => promisc(words.next(), words.next()),
        Some("use") => use_nic(words.next()),
        Some("map") => map(),
        Some("send") => send_test(),
        Some("recv") => recv_test(),
        Some("peers") => crate::netfs::print_peers(),
        Some("discover") => crate::netfs::nic_discover(),
        Some(_) => crate::println!("Usage: nic [list | sniff [N] | promisc N on|off | peers | discover | use N | map | send | recv]"),
    }
}

fn list() {
    if imp::count() == 0 {
        return crate::println!("No network interface.");
    }
    for index in 0..imp::count() {
        let Some(nic) = imp::summary(index) else { continue };
        crate::println!("{}: {} {}  {}{}", index + 1, nic.family, Mac(&nic.mac), if nic.link_up { "link up" } else { "link DOWN" }, if nic.terminal { "  (terminal)" } else { "" });
        if let Some((speed, full)) = nic.speed {
            crate::println!("   negotiated {speed} Mbit/s {}", if full { "full duplex" } else { "half duplex" });
        }
        crate::println!("   frames: {} received, {} sent{}", nic.received, nic.sent, if nic.dropped { " (some dropped: no link)" } else { "" });
        imp::details(index);
    }
}

/// `nic use N`: the interface `nic sniff` and the single-interface tape calls default to.
fn use_nic(argument: Option<&str>) {
    let count = crate::netfs::count();
    match argument.and_then(|word| word.parse::<usize>().ok()) {
        Some(number) if number >= 1 && number <= count => {
            imp::set_active(number - 1);
            crate::println!("Using NIC {number} for nic sniff and the single-interface tape calls.");
        }
        _ => crate::println!("Usage: nic use N   (N from 1 to {count}; `nic` lists them). Now: NIC {}.", imp::active() + 1),
    }
}

/// `nic map`: which interfaces hear which. Every interface with a link sends a probe and the others listen; with two
/// ports cabled together this shows the cable. It retries up to three rounds while nothing is heard (links coming up
/// take a moment).
fn map() {
    const MAX: usize = 8;
    let count = crate::netfs::count().min(MAX);
    if count == 0 {
        return crate::println!("No network interface.");
    }
    let mut heard = [[false; MAX]; MAX];
    crate::netfs::blocking(true);
    for round in 0..3 {
        for from in 0..count {
            let Some(mac) = crate::netfs::mac(from).filter(|_| crate::netfs::link_up(from)) else { continue };
            let mut frame = [0u8; 14 + 8];
            frame[0..6].copy_from_slice(&[0xff; 6]);
            frame[6..12].copy_from_slice(&mac);
            frame[12..14].copy_from_slice(&[0x88, 0xb9]);
            frame[14..20].copy_from_slice(b"FOSMAP");
            frame[20] = from as u8;
            crate::netfs::send_frame(from, &frame);
            let until = crate::timer::ticks() + crate::timer::TICK_HZ / 5;
            while crate::timer::ticks() < until {
                crate::netfs::drain(|to, received| {
                    if received.len() >= 21 && received[12..14] == [0x88, 0xb9] && &received[14..20] == b"FOSMAP" {
                        let sender = usize::from(received[20]);
                        // Only our own interfaces' probes count (another machine on the same network sends some too).
                        if sender < count && sender != to && crate::netfs::mac(sender).is_some_and(|own| own == received[6..12]) {
                            heard[sender][to] = true;
                        }
                    }
                });
                imp::idle();
            }
        }
        if heard.iter().any(|row| row.iter().any(|&seen| seen)) || crate::netfs::break_requested() {
            break;
        }
        crate::println!("Nothing heard yet (round {})...", round + 1);
    }
    crate::netfs::blocking(false);
    for from in 0..count {
        let Some(mac) = crate::netfs::mac(from) else { continue };
        crate::print(format_args!("NIC {} {} ({}): ", from + 1, Mac(&mac), if crate::netfs::link_up(from) { "link up" } else { "link DOWN" }));
        let mut any = false;
        for to in 0..count {
            if heard[from][to] {
                crate::print(format_args!("{}NIC {}", if any { ", " } else { "heard by " }, to + 1));
                any = true;
            }
        }
        crate::println!("{}", if any { "" } else { "heard by nobody" });
    }
}

/// `nic send`: one raw broadcast test frame (ethertype 0x88b5) out of the first interface.
fn send_test() {
    let Some(mac) = crate::netfs::mac(0) else { return crate::println!("No network interface.") };
    let mut frame = [0u8; 14 + 27];
    frame[0..6].copy_from_slice(&[0xff; 6]);
    frame[6..12].copy_from_slice(&mac);
    frame[12..14].copy_from_slice(&[0x88, 0xb5]);
    frame[14..].copy_from_slice(b"fos raw ethernet test frame");
    if crate::netfs::send_frame(0, &frame) {
        crate::println!("Sent {} bytes: broadcast test frame (ethertype 0x88b5).", frame.len());
    } else {
        crate::println!("Send failed.");
    }
}

/// `nic recv`: waits up to three seconds for one frame the machine does not answer itself, shows its header and dumps it.
fn recv_test() {
    if crate::netfs::count() == 0 {
        return crate::println!("No network interface.");
    }
    let mut buffer = [0u8; 1600];
    let mut length = 0;
    crate::netfs::blocking(true);
    let deadline = crate::timer::ticks() + 3 * crate::timer::TICK_HZ;
    while length == 0 && crate::timer::ticks() < deadline && !crate::netfs::break_requested() {
        crate::netfs::drain(|_, frame| {
            if length == 0 && frame.len() >= 14 {
                length = frame.len().min(buffer.len());
                buffer[..length].copy_from_slice(&frame[..length]);
            }
        });
        if length == 0 {
            imp::idle();
        }
    }
    crate::netfs::blocking(false);
    if length == 0 {
        return crate::println!("No frame received (timed out waiting).");
    }
    let f = &buffer[..length];
    crate::println!("Received {length} bytes: dst {} src {} ethertype 0x{:02x}{:02x}", Mac(&f[0..6].try_into().unwrap()), Mac(&f[6..12].try_into().unwrap()), f[12], f[13]);
    crate::shellcmds::dump_bytes(0, f);
}

/// `nic promisc N on|off`: the interface keeps every frame it sees, not only its own and broadcasts.
fn promisc(number: Option<&str>, state: Option<&str>) {
    let index = number.and_then(|word| word.parse::<usize>().ok()).filter(|n| (1..=imp::count()).contains(n));
    let on = match state {
        Some("on") => true,
        Some("off") => false,
        _ => return crate::println!("Usage: nic promisc N on|off   (N from 1 to {}).", imp::count()),
    };
    match index {
        Some(number) if imp::set_promiscuous(number - 1, on) => crate::println!("NIC {number}: promiscuous {}.", if on { "on" } else { "off" }),
        _ => crate::println!("nic promisc: no such interface (`nic` lists them)."),
    }
}

/// `nic sniff [N]`: one line per frame the interface receives, promiscuous, until Escape.
fn sniff(argument: Option<&str>) {
    let index = match argument.and_then(|word| word.parse::<usize>().ok()) {
        Some(number) if number >= 1 && number <= imp::count() => number - 1,
        Some(_) => return crate::println!("nic sniff: no such interface (`nic` lists them)."),
        None => imp::active().min(imp::count().saturating_sub(1)),
    };
    if imp::count() == 0 {
        return crate::println!("No network interface.");
    }
    if imp::busy(index) {
        return crate::println!("nic sniff: NIC {} belongs to a background tape.", index + 1);
    }
    imp::set_promiscuous(index, true);
    crate::println!("Sniffing NIC {} (promiscuous). Escape stops.", index + 1);
    imp::sniff_begin();
    let (mut shown, mut skipped) = (0u32, 0u32);
    while !imp::interrupted() {
        let mut budget = 40u32; // lines per pass: a busy network must not make the screen the bottleneck
        imp::sniff_pass(index, &mut |frame| {
            if budget == 0 {
                skipped += 1;
                return;
            }
            budget -= 1;
            shown += 1;
            let mut line = Line::new();
            describe_frame(frame, &mut line);
            crate::println!("{shown:>5} {:>4}B {}", frame.len(), line.as_str());
        });
        imp::idle();
    }
    imp::sniff_end();
    imp::set_promiscuous(index, false);
    crate::println!("Stopped: {shown} frame(s) shown{}.", if skipped > 0 { ", some skipped to keep up" } else { "" });
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use super::Summary;
    use crate::term;

    pub fn count() -> usize {
        term::nic_count()
    }

    pub fn active() -> usize {
        term::active_nic()
    }

    pub fn busy(_index: usize) -> bool {
        false
    }

    pub fn summary(index: usize) -> Option<Summary> {
        let nic = term::nic_at(index)?;
        let (received, sent, dropped) = nic.counters();
        Some(Summary {
            family: nic.family(),
            mac: nic.mac(),
            link_up: nic.link_up(),
            speed: nic.link_speed().map(|(speed, full)| (u32::from(speed), full)),
            received,
            sent,
            dropped: dropped != 0,
            terminal: term::session_nic() == Some(index),
        })
    }

    /// Interrupt delivery, driver details, parking and the tape frame queue.
    pub fn details(index: usize) {
        let Some(nic) = term::nic_at(index) else { return };
        // v1's `nic` reported "NIC IRQn: N notifications"; keep that line (tools grep for it).
        if let crate::e1000::Delivery::Msi(v) | crate::e1000::Delivery::Msix(v) | crate::e1000::Delivery::Intx { vector: v, .. } = nic.delivery() {
            crate::println!("   NIC IRQ{v}: {} notifications", crate::e1000::ISR[index].interrupts.load(core::sync::atomic::Ordering::Relaxed));
        }
        match nic.delivery() {
            crate::e1000::Delivery::Polled => crate::println!("   interrupts: polled"),
            crate::e1000::Delivery::Intx { gsi, vector } => {
                let state = &crate::e1000::ISR[index];
                crate::println!(
                    "   interrupts: INTx pin {gsi}, vector {vector:#x}: {} taken ({} storms masked)",
                    state.interrupts.load(core::sync::atomic::Ordering::Relaxed),
                    crate::msi::storms()
                );
            }
            crate::e1000::Delivery::Msi(vector) | crate::e1000::Delivery::Msix(vector) => {
                let state = &crate::e1000::ISR[index];
                crate::println!(
                    "   interrupts: {} vector {vector:#x}: {} taken, {} receive overruns, {} link changes",
                    if matches!(nic.delivery(), crate::e1000::Delivery::Msix(_)) { "MSI-X" } else { "MSI" },
                    state.interrupts.load(core::sync::atomic::Ordering::Relaxed),
                    state.overruns.load(core::sync::atomic::Ordering::Relaxed),
                    state.link_changes.load(core::sync::atomic::Ordering::Relaxed)
                );
            }
        }
        nic.describe();
        crate::nicpark::describe(index);
        if index == 0 {
            let (queued, dropped) = term::frame_queue_counts();
            crate::println!("   tape frame queue: {queued} queued, {dropped} dropped (all interfaces)");
        }
    }

    pub fn set_promiscuous(index: usize, on: bool) -> bool {
        term::nic_set_promiscuous(index, on)
    }

    /// The terminal's and discovery's own frames are handled as usual so the session stays alive; the rest is shown.
    pub fn sniff_begin() {
        term::blocking(true);
    }

    pub fn sniff_pass(index: usize, show: &mut dyn FnMut(&[u8])) {
        term::drain_nics(|from, frame| {
            if from == index {
                show(frame);
            }
        });
    }

    pub fn set_active(index: usize) {
        term::set_active_nic(index);
    }

    pub fn interrupted() -> bool {
        term::break_requested()
    }

    pub fn idle() {
        crate::debug::pet();
        crate::cpu::sleep();
    }

    pub fn sniff_end() {
        term::blocking(false);
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use super::Summary;
    use crate::genet;

    pub fn count() -> usize {
        usize::from(genet::present())
    }

    pub fn active() -> usize {
        0
    }

    pub fn busy(index: usize) -> bool {
        crate::tasks::service_owns(index)
    }

    pub fn summary(index: usize) -> Option<Summary> {
        let info = genet::info().filter(|_| index == 0)?;
        Some(Summary {
            family: "Genet",
            mac: info.mac,
            link_up: info.link_up,
            speed: info.link_up.then_some((info.speed, info.full_duplex)),
            received: u64::from(info.received),
            sent: u64::from(info.sent),
            dropped: info.dropped != 0,
            terminal: false,
        })
    }

    pub fn details(_index: usize) {
        genet::details();
    }

    pub fn set_promiscuous(_index: usize, on: bool) -> bool {
        genet::set_promiscuous(on)
    }

    pub fn sniff_begin() {}

    /// The shell's idle loop (discovery, kernel update) is not running meanwhile: sniffing owns the ring.
    pub fn sniff_pass(_index: usize, show: &mut dyn FnMut(&[u8])) {
        let mut buffer = [0u8; 1600];
        loop {
            let length = genet::receive(&mut buffer);
            if length == 0 {
                break;
            }
            show(&buffer[..length]);
        }
    }

    pub fn set_active(_index: usize) {}

    pub fn interrupted() -> bool {
        crate::usb_kbd::poll();
        let mut escape = false;
        while let Some(key) = crate::arch::aarch64::pl011::read_byte().or_else(crate::usb_kbd::pop) {
            escape |= key == 0x1b;
        }
        escape
    }

    pub fn idle() {
        crate::cpu::sleep();
    }

    pub fn sniff_end() {}
}
