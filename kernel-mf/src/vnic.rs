//! Virtual network interfaces: the `vnic` command, the `vnic NAME on N` lines of `settings`, and the names `run ... &NAME` accepts.
//!
//! A virtual interface is a NIC number with its own MAC on a physical NIC (`kernel_core::nic::Virtual`); the physical card does
//! layer 2 and `nicctl::drain_nic_to_fifo` hands each interface the frames addressed to it, so a tape on one sees an ordinary
//! NIC (own DHCP lease, ARP, ports). Several services can thereby share one card while the audio NICs stay reserved for tapes
//! with direct access. A virtual interface refuses a parent that a tape owns; the card stays promiscuous while virtual
//! interfaces exist (`nicctl::nic_set_promiscuous` ignores "off").
//!
//! The MAC is locally administered and derived from the parent's MAC and the name, so DHCP leases survive a reboot. `settings`
//! is read before the NICs exist, so its lines are queued and `apply` creates them right after `net::init`.

use kernel_core::nic::{self, MAX_NICS};

use kernel_core::netdump::Mac;

use crate::net;

const NAME_MAX: usize = 15;
const QUEUE: usize = 8;

#[derive(Clone, Copy)]
struct Spec {
    name: [u8; NAME_MAX],
    length: usize,
    /// 0-based physical NIC.
    parent: usize,
    mac: Option<[u8; 6]>,
}

static mut PENDING: [Option<Spec>; QUEUE] = [None; QUEUE];
/// The name of each virtual interface by NIC index (length 0: none).
static mut NAMES: [([u8; NAME_MAX], usize); MAX_NICS] = [([0; NAME_MAX], 0); MAX_NICS];

fn name_of(index: usize) -> Option<&'static str> {
    let (bytes, length) = unsafe { &(*core::ptr::addr_of!(NAMES))[index] };
    (*length != 0).then(|| core::str::from_utf8(&bytes[..*length]).unwrap_or("?"))
}

/// The 0-based NIC index of the live virtual interface called `name`.
pub fn lookup(name: &str) -> Option<usize> {
    (0..nic::count().min(MAX_NICS)).find(|&index| nic::virtual_parent(index).is_some() && name_of(index).is_some_and(|own| own.eq_ignore_ascii_case(name)))
}

/// Names are letters, digits and `-`, and not a number (`&N` would be ambiguous).
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= NAME_MAX && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') && name.parse::<u64>().is_err()
}

/// FNV-1a over the parent's MAC and the name: five bytes under a fixed first byte (0x02: locally administered, unicast).
fn derived_mac(parent: [u8; 6], name: &str) -> [u8; 6] {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in parent.iter().copied().chain(name.bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let bytes = hash.to_be_bytes();
    [0x02, bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]
}

/// Creates virtual interface `name` on physical NIC `parent` (0-based); returns its NIC index.
fn add(name: &str, parent: usize, mac: Option<[u8; 6]>) -> Result<usize, &'static str> {
    if !valid_name(name) {
        return Err("a name is 1-15 letters, digits or `-`, and not a number");
    }
    if lookup(name).is_some() {
        return Err("that name is taken");
    }
    if !crate::tapenet::exists(parent) || nic::is_virtual(parent) {
        return Err("the parent must be a physical NIC");
    }
    if crate::tapenet::owner(parent) != 0 {
        return Err("a tape owns that NIC");
    }
    let _nics = net::lock_nics();
    let Some(parent_mac) = nic::at(parent).map(|card| card.mac()) else { return Err("no such NIC") };
    let mac = mac.unwrap_or_else(|| derived_mac(parent_mac, name));
    if mac[0] & 1 != 0 {
        return Err("a multicast MAC cannot be an interface address");
    }
    let Some(index) = nic::add_virtual(parent, mac) else { return Err("the NIC table is full, or the MAC is already in use") };
    kernel_core::nicctl::clear_frame_queue_nic(index);
    let take = name.len();
    unsafe {
        let slot = &mut (*core::ptr::addr_of_mut!(NAMES))[index];
        slot.0[..take].copy_from_slice(name.as_bytes());
        slot.1 = take;
    }
    Ok(index)
}

/// Removes virtual interface `name`; its number is reused by the next one.
fn remove(name: &str) -> Result<(), &'static str> {
    let Some(index) = lookup(name) else { return Err("no such virtual interface") };
    if crate::tapenet::owner(index) != 0 {
        return Err("a tape is running on it (`kill` it first)");
    }
    let _nics = net::lock_nics();
    nic::remove_virtual(index);
    kernel_core::nicctl::clear_frame_queue_nic(index);
    unsafe { (*core::ptr::addr_of_mut!(NAMES))[index].1 = 0 };
    Ok(())
}

fn parse_options<'a>(words: impl Iterator<Item = &'a str>) -> Result<Option<[u8; 6]>, &'static str> {
    let mut mac = None;
    for word in words {
        match word.strip_prefix("mac=") {
            Some(text) => mac = Some(crate::netclient::parse_mac(text).ok_or("mac= takes six hex bytes: 02:00:00:00:00:01")?),
            None => return Err("only mac=... is known"),
        }
    }
    Ok(mac)
}

/// A `vnic NAME on N [mac=...]` line of `settings` (the NICs do not exist yet: queued for `apply`).
pub fn queue(line: &str) {
    let mut words = line.split_whitespace().skip(1);
    let parsed = (|| {
        let name = words.next().ok_or("vnic NAME on N [mac=...]")?;
        if words.next() != Some("on") {
            return Err("vnic NAME on N [mac=...]");
        }
        let parent = words.next().and_then(|word| word.parse::<usize>().ok()).filter(|&n| n >= 1).ok_or("N is the physical NIC number, from 1")?;
        let mac = parse_options(words.by_ref())?;
        if !valid_name(name) {
            return Err("a name is 1-15 letters, digits or `-`, and not a number");
        }
        let mut spec = Spec { name: [0; NAME_MAX], length: name.len(), parent: parent - 1, mac };
        spec.name[..name.len()].copy_from_slice(name.as_bytes());
        Ok(spec)
    })();
    match parsed {
        Ok(spec) => {
            let queue = unsafe { &mut *core::ptr::addr_of_mut!(PENDING) };
            match queue.iter_mut().find(|slot| slot.is_none()) {
                Some(slot) => *slot = Some(spec),
                None => println!("settings: `{line}` ignored (at most {QUEUE} vnic lines)."),
            }
        }
        Err(reason) => println!("settings: `{line}` ignored ({reason})."),
    }
}

/// Creates the interfaces `settings` asked for; call once the NICs are up.
pub fn apply() {
    let queue = unsafe { &mut *core::ptr::addr_of_mut!(PENDING) };
    for spec in queue.iter_mut().filter_map(Option::take) {
        let name = core::str::from_utf8(&spec.name[..spec.length]).unwrap_or("?");
        match add(name, spec.parent, spec.mac) {
            Ok(index) => println!("vnic {name}: nic{} on nic{} (MAC {}).", index + 1, spec.parent + 1, Mac(&nic::at(index).map_or([0; 6], |card| card.mac()))),
            Err(reason) => println!("settings: vnic {name} on {}: {reason}.", spec.parent + 1),
        }
    }
}

/// The name of virtual interface `index`, for `nic`.
pub fn name(index: usize) -> Option<&'static str> {
    nic::virtual_parent(index).and_then(|_| name_of(index))
}

/// The `vnic` command: `vnic` lists, `vnic add NAME on N [mac=...]`, `vnic del NAME`.
pub fn command<'a>(mut words: impl Iterator<Item = &'a str> + Clone) {
    match words.next() {
        None => {
            let mut any = false;
            for index in 0..nic::count().min(MAX_NICS) {
                if let (Some(parent), Some(name)) = (nic::virtual_parent(index), name_of(index)) {
                    any = true;
                    let mac = nic::at(index).map_or([0; 6], |card| card.mac());
                    println!("{name}: nic{} on nic{} MAC {} link {}", index + 1, parent + 1, Mac(&mac), if nic::at(index).is_some_and(|card| card.link_up()) { "up" } else { "DOWN" });
                }
            }
            if !any {
                println!("No virtual interfaces. `vnic add NAME on N` makes one on physical NIC N.");
            }
        }
        Some("add") => {
            let result = (|| {
                let name = words.next().ok_or("vnic add NAME on N [mac=...]")?;
                if words.next() != Some("on") {
                    return Err("vnic add NAME on N [mac=...]");
                }
                let parent = words.next().and_then(|word| word.parse::<usize>().ok()).filter(|&n| n >= 1).ok_or("N is the physical NIC number, from 1")?;
                let mac = parse_options(words.by_ref())?;
                add(name, parent - 1, mac).map(|index| (name, index, parent))
            })();
            match result {
                Ok((name, index, parent)) => println!("vnic {name}: nic{} on nic{parent}. Use it with `run TAPE &{name}`.", index + 1),
                Err(reason) => println!("vnic: {reason}."),
            }
        }
        Some("del" | "delete" | "remove") => match words.next() {
            Some(name) => match remove(name) {
                Ok(()) => println!("vnic {name}: removed."),
                Err(reason) => println!("vnic: {reason}."),
            },
            None => println!("vnic del NAME"),
        },
        Some(_) => println!("vnic [add NAME on N [mac=...] | del NAME]"),
    }
}
