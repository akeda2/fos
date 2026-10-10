//! The PTP clock of `audio-net` (RTP over IPv4 only): listens for a PTPv2 grandmaster (`ptp` tape, `ptp4l`, a
//! studio clock) on 224.0.1.129, measures this machine's clock against it and tells the stream loops what time
//! it is on the grandmaster's timeline. Nothing is steered: the local clock runs as it is and the estimate is
//! extrapolated from it (`ptp_tape::Tracker`).
//!
//! What it is used for: the sender stamps its RTP packets with PTP time (AES67's way, so the stream says
//! `ts-refclk:ptp=...` in its SDP), and a receiver that is also locked reads the one-way delay from the sender's
//! capture to its own arrival off the stamps. Both machines need the same grandmaster.
//!
//! `ptp=auto` (the default) joins the group and looks; with no clock on the network it costs one IGMP report and
//! the occasional frame. `ptp=off` never touches it. The loops call `consume` with every received frame (PTP and
//! its IGMP queries are taken here) and `poll` once per pass (a Delay_Req when due). Timestamps are software
//! ones, taken as the frame is in hand, so the accuracy is tens of microseconds, not the nanoseconds of hardware.

use crate::igmp::Igmp;
use crate::net::{self, NetConfig};
use ptp_tape::{EVENT_PORT, GENERAL_PORT, GROUP, MediaClock, PortIdentity, Slave, Tracker, build_delay_req, port_identity};
use tape_net::{ArpCache, Ipv4Address, MacAddress};
use tape_rt::{TapeApi, println};

struct Ptp {
    slave: Slave,
    tracker: Tracker,
    igmp: Igmp,
    config: NetConfig,
    arp: ArpCache<8>,
    identity: PortIdentity,
    nic: usize,
    users: u8,
    /// Whether an Announce was ever heard (a clock exists on this network).
    found: bool,
}

static mut PTP: Option<Ptp> = None;

fn state() -> Option<&'static mut Ptp> {
    unsafe { (*(&raw mut PTP)).as_mut() }
}

fn local_ns() -> i64 {
    tape_rt::micros() as i64 * 1000
}

/// Starts listening on `nic` (a second caller, such as the other half of a duplex run, shares the first's state).
pub fn start(api: &TapeApi, nic: usize, config: &NetConfig) {
    if let Some(ptp) = state() {
        ptp.users += 1;
        return;
    }
    let mut ptp = Ptp {
        slave: Slave::new(0, port_identity(config.mac.0, 1), 1_000_000_000),
        tracker: Tracker::new(),
        igmp: Igmp::new(GROUP),
        config: *config,
        arp: ArpCache::new(),
        identity: port_identity(config.mac.0, 1),
        nic,
        users: 1,
        found: false,
    };
    ptp.igmp.join(api, nic, config);
    unsafe { PTP = Some(ptp) };
}

/// Stops listening (the last user leaves the group). `others_joined`: another multicast group is still in use, so
/// the NIC's multicast filter stays open.
pub fn stop(api: &TapeApi, nic: usize, others_joined: bool) {
    let Some(ptp) = state() else { return };
    ptp.users -= 1;
    if ptp.users > 0 {
        return;
    }
    ptp.igmp.leave_report(api, nic, &ptp.config);
    if !others_joined {
        (api.nic_set_multicast)(nic, 0);
    }
    unsafe { PTP = None };
}

pub fn active() -> bool {
    state().is_some()
}

/// Does `frame` look like a PTP datagram (IPv4 UDP to the PTP group)? Cheap enough for every frame.
fn is_ptp(frame: &[u8]) -> bool {
    frame.len() >= 42 && frame[12..14] == [0x08, 0x00] && frame[14] == 0x45 && frame[23] == 17 && frame[30..34] == GROUP
}

/// Takes a received frame if it is the clock's (PTP, or an IGMP query for its group); true if consumed. `t2` is
/// the local receive time in microseconds, read as soon as the frame was in hand.
pub fn consume(api: &TapeApi, frame: &[u8], t2: u64) -> bool {
    let Some(ptp) = state() else { return false };
    let t2_ns = t2 as i64 * 1000;
    if is_ptp(frame) {
        if let net::Inbound::Udp { payload, .. } = net::inbound(api, ptp.nic, &ptp.config, &mut ptp.arp, frame, EVENT_PORT..=GENERAL_PORT, 0) {
            if let Some(sample) = ptp.slave.handle(payload, t2_ns) {
                ptp.tracker.update(t2_ns, sample.offset);
            }
            ptp.found |= ptp.slave.master().is_some();
        }
        return true;
    }
    // The audio stream's own IGMP state sees the frame too; both answer a general query.
    ptp.igmp.handle(frame, t2);
    false
}

/// Once per loop pass: a Delay_Req when due, the master's timeout, a pending IGMP answer.
pub fn poll(api: &TapeApi) {
    let Some(ptp) = state() else { return };
    let now = local_ns();
    ptp.slave.tick(now);
    if ptp.slave.master().is_none() && ptp.tracker.tenths_ppm().is_some() {
        ptp.tracker.reset();
    }
    ptp.igmp.poll(api, ptp.nic, &ptp.config, (now / 1000) as u64);
    if let Some(sequence) = ptp.slave.request_due(now) {
        let mut message = [0u8; 64];
        let length = build_delay_req(&mut message, 0, ptp.identity, sequence);
        ptp.slave.request_sent(sequence, local_ns());
        let mut frame = [0u8; 100];
        let destination = MacAddress(audio_wire::multicast_mac(GROUP));
        if let Some(total) = net::build_udp_frame(&mut frame, &ptp.config, destination, Ipv4Address(GROUP), EVENT_PORT, EVENT_PORT, 1, &message[..length]) {
            net::send_frame(api, ptp.nic, &frame[..total]);
        }
    }
}

/// A clock was heard announcing itself.
pub fn found() -> bool {
    state().is_some_and(|ptp| ptp.found)
}

/// Locked: recent measurements and a known rate. Only then is a delay read from a stamp worth showing.
pub fn locked() -> bool {
    state().is_some_and(|ptp| ptp.tracker.locked(local_ns()))
}

/// The grandmaster's time in nanoseconds, if there is an estimate (also in holdover, a few seconds after the
/// last Sync, extrapolated at the measured rate).
pub fn time_ns() -> Option<i64> {
    let ptp = state()?;
    ptp.tracker.tenths_ppm()?;
    Some(ptp.tracker.master_time(local_ns()))
}

/// The grandmaster's time at local time `t_us` (microseconds, an instant earlier read from `tape_rt::micros`).
pub fn time_ns_at(t_us: u64) -> Option<i64> {
    let ptp = state()?;
    ptp.tracker.tenths_ppm()?;
    Some(ptp.tracker.master_time(t_us as i64 * 1000))
}

/// The grandmaster's clock identity (for the SDP), once one is known.
pub fn grandmaster() -> Option<[u8; 8]> {
    let (source, _) = state()?.slave.master()?;
    source[..8].try_into().ok()
}

/// The measured one-way path delay to the grandmaster, microseconds.
pub fn path_delay_us() -> Option<i64> {
    state()?.slave.path_delay().map(|ns| ns / 1000)
}

/// Before a sender starts stamping: waits for a clock to be heard (3 s) and then to lock (10 s), reading the NIC
/// itself, so call it while nothing else does (before a duplex run's receive task starts). True if locked.
pub fn wait_for_lock(api: &TapeApi) -> bool {
    let mut frame = [0u8; net::MAX_FRAME];
    let started = tape_rt::micros();
    let mut announced = false;
    loop {
        if (api.should_abort)() != 0 {
            return false;
        }
        let waited = tape_rt::micros() - started;
        if locked() {
            return true;
        }
        if waited > 3_000_000 && !found() {
            return false;
        }
        if waited > 13_000_000 {
            return false;
        }
        if found() && !announced {
            announced = true;
            println!("PTP clock found; waiting for it to lock (up to 10 s)...");
        }
        pump(api, &mut frame);
        tape_rt::idle();
    }
}

/// Reads and handles whatever the NIC has (the loops that do not read it themselves call this).
pub fn pump(api: &TapeApi, frame: &mut [u8]) {
    let Some(nic) = state().map(|ptp| ptp.nic) else { return };
    for _ in 0..4 {
        let length = (api.nic_receive)(nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            break;
        }
        consume(api, &frame[..length], tape_rt::micros());
    }
    poll(api);
}

/// RTP timestamps on the PTP timeline for the sender's packets.
pub struct Stamper {
    media: MediaClock,
    rate: u32,
}

impl Stamper {
    pub const fn new(rate: u32) -> Stamper {
        Stamper { media: MediaClock::new(), rate }
    }

    /// The stamp of the next packet: its first sample was captured `age_frames` before now. None without a clock.
    pub fn stamp(&mut self, age_frames: u32, step: u32) -> Option<u32> {
        let now = time_ns()?;
        let age = i64::from(age_frames) * 1_000_000_000 / i64::from(self.rate.max(1));
        Some(self.media.next(MediaClock::frames_at(now - age, self.rate), self.rate, step))
    }
}
