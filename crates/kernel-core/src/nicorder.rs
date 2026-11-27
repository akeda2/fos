//! Which number each network card gets: the order the kernels bring their NICs up in.
//!
//! The number of a physical NIC is its place in the interface table, and the table is filled in the order the cards come up.
//! It used to be driver by driver (every e1000, then every RTL8168, ...), so a card of another make added in any slot moved the
//! numbers of the cards after it, and with them `settings` lines (`nic-mtu N`, `vnic NAME on N`), `run ... &N` and the name a
//! machine announces (`hostname` + the number). Two rules make the numbers stable:
//!
//! * **PCI order.** Cards come up in PCI address order (bus, device, function), whatever their driver. `pci::scan` walks the
//!   buses in that order already, so one walk over `pci::devices()` gives it. A new card still takes the number of where it
//!   sits, but adding a *driver* moves nothing.
//! * **Pins.** `nic N MAC` lines in `settings` give the card with that MAC number N, wherever it sits. The other cards fill the
//!   free numbers in PCI order. To know a card's MAC before it is brought up, the kernel reads it from the card's address
//!   registers, untouched, before the driver resets it (`peek_mac`: e1000 RAL0/RAH0, Realtek IDR0-5, which the hardware
//!   loads from its EEPROM or NVM at power-on). The driver's own index must be the final number (its interrupt tables are
//!   indexed by it, and the 82579/I219 jumbo split, `e1000::prepare_jumbo`, is chosen by number before the reset), hence the
//!   peek rather than renumbering afterwards. A 3C905 (EEPROM behind window commands) and virtio-net (MAC in the device's config
//!   after feature negotiation) have no peek: they cannot be pinned and take free numbers.
//!
//! * **Holes.** A pinned number belongs to its card even while the card is away: the slot stays empty (`nic::skip_to`), so a
//!   card pulled from a lower slot does not move the pinned cards above it, and the `run ... &N` and `vnic ... on N` lines
//!   naming them keep working. Unpinned cards take the lowest numbers no pin claims, so a card that comes back finds its
//!   number free; only when every free number is claimed does an unpinned card take one of an absent card's. A card whose
//!   driver fails to start leaves its number empty too. A pin above the interface table (`nic::MAX_NICS`) cannot be
//!   honoured: it is reported and the card takes a free number. (Until 2026-11-27 the table had no holes: on rack an I350
//!   with pins 2 and 3 was replaced by an RTL8125, the RTL8168 pinned to 4 became nic3, and every service on `&4` failed.)
//!
//! Pins are read only when `settings` has some; without them no card is touched before its driver's `init`.
//!
//! `plan` is the pure part (host tests below); `arrange` walks PCI and reads the MACs.

use crate::pci;

/// Cards and pins considered (more than either kernel's interface table holds).
pub const MAX_CARDS: usize = 16;

/// `nic N MAC` in `settings`: interface `number` (1-based) is the card with `mac`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pin {
    pub number: usize,
    pub mac: [u8; 6],
}

static mut PINS: [Option<Pin>; MAX_CARDS] = [None; MAX_CARDS];

/// `aa:bb:cc:dd:ee:ff` (or with `-`), as `nic` prints MACs.
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split(|c| c == ':' || c == '-');
    for byte in mac.iter_mut() {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    if parts.next().is_some() { None } else { Some(mac) }
}

/// The words after `nic` in a `settings` line: `N MAC`.
pub fn parse_pin<'a>(mut words: impl Iterator<Item = &'a str>) -> Option<Pin> {
    let number = words.next()?.parse::<usize>().ok().filter(|&n| (1..=MAX_CARDS).contains(&n))?;
    let mac = parse_mac(words.next()?)?;
    if words.next().is_some() || mac == [0; 6] || mac[0] & 1 != 0 {
        return None; // a multicast or zero address is no card's
    }
    Some(Pin { number, mac })
}

/// Remembers a pin from `settings` (boot, before the NICs come up). Err with the reason when it clashes or there is no room.
pub fn add_pin(pin: Pin) -> Result<(), &'static str> {
    let pins = unsafe { &mut *core::ptr::addr_of_mut!(PINS) };
    if pins.iter().flatten().any(|other| other.number == pin.number) {
        return Err("that number is pinned already");
    }
    if pins.iter().flatten().any(|other| other.mac == pin.mac) {
        return Err("that MAC is pinned already");
    }
    let slot = pins.iter_mut().find(|slot| slot.is_none()).ok_or("too many pins")?;
    *slot = Some(pin);
    Ok(())
}

/// The pins `settings` gave, in the order of their lines.
pub fn pins() -> ([Pin; MAX_CARDS], usize) {
    let mut out = [Pin { number: 0, mac: [0; 6] }; MAX_CARDS];
    let mut count = 0;
    for pin in unsafe { (*core::ptr::addr_of!(PINS)).iter().flatten() } {
        out[count] = *pin;
        count += 1;
    }
    (out, count)
}

/// Why a pin was not honoured, for the boot log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unhonoured {
    /// No card present has the MAC (or its MAC could not be read before `init`).
    Absent(Pin),
    /// The number is above the interface table (`limit` interfaces).
    BeyondTable(Pin, usize),
}

/// Numbers the cards. `macs[i]` is the MAC of the i-th card in PCI order (`None`: not read); the table has `limit`
/// interfaces. Sets `order[k]` to the index into `macs` of the card that becomes interface k+1, `None` for a hole; reports
/// each pin not honoured through `skipped`. A card left without a number (more cards than `limit`) is not in `order`.
pub fn plan(macs: &[Option<[u8; 6]>], pins: &[Pin], limit: usize, order: &mut [Option<usize>], mut skipped: impl FnMut(Unhonoured)) {
    let count = macs.len().min(MAX_CARDS);
    let limit = limit.min(order.len()).min(MAX_CARDS);
    let mut placed = [false; MAX_CARDS]; // card i has a number
    let mut slot: [Option<usize>; MAX_CARDS] = [None; MAX_CARDS]; // number k+1 -> card
    let mut claimed = [false; MAX_CARDS]; // number k+1 is pinned, whether its card is here or not
    for pin in pins {
        if pin.number > limit {
            skipped(Unhonoured::BeyondTable(*pin, limit));
            continue;
        }
        claimed[pin.number - 1] = true;
        match macs[..count].iter().position(|mac| *mac == Some(pin.mac)) {
            // `add_pin` refuses duplicate numbers and MACs, so neither the slot nor the card is taken here.
            Some(card) if slot[pin.number - 1].is_none() && !placed[card] => {
                slot[pin.number - 1] = Some(card);
                placed[card] = true;
            }
            _ => skipped(Unhonoured::Absent(*pin)),
        }
    }
    // Unpinned cards in PCI order: first the numbers no pin claims, then those of absent pinned cards.
    for card in (0..count).filter(|&card| !placed[card]) {
        let free = (0..limit).find(|&k| slot[k].is_none() && !claimed[k]).or_else(|| (0..limit).find(|&k| slot[k].is_none()));
        if let Some(k) = free {
            slot[k] = Some(card);
        }
    }
    order[..limit].copy_from_slice(&slot[..limit]);
    for entry in &mut order[limit..] {
        *entry = None;
    }
}

/// The drivers' families, as far as ordering needs to know them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    E1000,
    Rtl8168,
    Rtl8125,
    Rtl8139,
    ThreeCom,
    Virtio,
}

impl Kind {
    /// The family of `device`, if a driver takes it.
    pub fn of(device: &pci::Device) -> Option<Kind> {
        let (vendor, id) = (device.vendor, device.id);
        if vendor == crate::e1000::VENDOR_INTEL && crate::e1000::DEVICE_IDS.contains(&id) {
            Some(Kind::E1000)
        } else if vendor == crate::rtl8168::VENDOR_REALTEK && crate::rtl8168::DEVICE_IDS.contains(&id) {
            Some(Kind::Rtl8168)
        } else if vendor == crate::rtl8125::VENDOR_REALTEK && crate::rtl8125::DEVICE_IDS.contains(&id) {
            Some(Kind::Rtl8125)
        } else if vendor == crate::rtl8139::VENDOR_REALTEK && crate::rtl8139::DEVICE_IDS.contains(&id) {
            Some(Kind::Rtl8139)
        } else if vendor == crate::threecom::VENDOR_3COM && crate::threecom::DEVICE_IDS.contains(&id) {
            Some(Kind::ThreeCom)
        } else if vendor == crate::virtio_net::VENDOR_VIRTIO && crate::virtio_net::DEVICE_IDS.contains(&id) {
            Some(Kind::Virtio)
        } else {
            None
        }
    }
}

/// A card to bring up.
#[derive(Clone, Copy)]
pub struct Card {
    pub device: pci::Device,
    pub kind: Kind,
}

/// The MAC a card holds before its driver resets it (see the module comment); `None` where there is no peek or it reads as
/// no address.
fn peek_mac(card: &Card) -> Option<[u8; 6]> {
    let mac = match card.kind {
        Kind::E1000 => crate::e1000::peek_mac(&card.device),
        Kind::Rtl8168 => crate::rtl8168::peek_mac(&card.device),
        Kind::Rtl8125 => crate::rtl8125::peek_mac(&card.device),
        Kind::Rtl8139 => crate::rtl8139::peek_mac(&card.device),
        Kind::ThreeCom | Kind::Virtio => None,
    }?;
    (mac != [0; 6] && mac != [0xff; 6] && mac[0] & 1 == 0).then_some(mac)
}

/// The NIC cards of the families in `kinds` at their interface numbers: entry k is interface k+1, `None` a hole (PCI order,
/// then pins applied). The caller brings each up with index k after `nic::skip_to(k)`. Narrates the pins it cannot honour.
pub fn arrange(kinds: &[Kind]) -> [Option<Card>; MAX_CARDS] {
    let mut cards: [Option<Card>; MAX_CARDS] = [None; MAX_CARDS];
    let mut count = 0;
    for device in pci::devices() {
        let Some(kind) = Kind::of(&device).filter(|kind| kinds.contains(kind)) else { continue };
        if count == MAX_CARDS {
            break;
        }
        cards[count] = Some(Card { device, kind });
        count += 1;
    }
    let (pins, pin_count) = pins();
    let limit = crate::nic::MAX_NICS.min(MAX_CARDS);
    let mut order = [None; MAX_CARDS];
    if pin_count == 0 {
        for (number, entry) in order[..count.min(limit)].iter_mut().enumerate() {
            *entry = Some(number);
        }
    } else {
        let mut macs = [None; MAX_CARDS];
        for (mac, card) in macs.iter_mut().zip(cards[..count].iter().flatten()) {
            *mac = peek_mac(card);
        }
        crate::println!("nic: {pin_count} pinned number(s) in settings; read {} card MAC(s) to place them.", macs.iter().flatten().count());
        plan(&macs[..count], &pins[..pin_count], limit, &mut order, |why| match why {
            Unhonoured::Absent(pin) => crate::println!("nic: no card with MAC {}: nic{} stays empty.", Mac(pin.mac), pin.number),
            Unhonoured::BeyondTable(pin, limit) => {
                crate::println!("nic: pin {} ({}) ignored: the table has {limit} interfaces;", pin.number, Mac(pin.mac));
                crate::println!("     the card takes a free number.");
            }
        });
    }
    let mut out = [None; MAX_CARDS];
    for (slot, card) in out.iter_mut().zip(order) {
        *slot = card.and_then(|card| cards[card]);
    }
    out
}

/// `nic pin`: the `settings` lines that keep every card on the number it has now. The caller holds whatever lock guards the
/// interface table.
pub fn print_pin_lines() {
    crate::println!("# For `settings` (a reboot applies them): each card keeps its number.");
    for index in 0..crate::nic::count() {
        use crate::nic::Nic;
        match crate::nic::at(index) {
            Some(card @ (Nic::E1000(_) | Nic::Rtl8168(_) | Nic::Rtl8125(_) | Nic::Rtl8139(_))) => {
                crate::println!("nic {} {}", index + 1, Mac(card.mac()));
            }
            Some(Nic::Virtual(_)) | None => {} // a vnic is named, not numbered, in settings
            Some(card) => crate::println!("# nic{} ({}): no pin, its MAC is unknown until it starts.", index + 1, card.family()),
        }
    }
}

/// A MAC as `nic` prints it.
pub struct Mac(pub [u8; 6]);

impl core::fmt::Display for Mac {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(last: u8) -> Option<[u8; 6]> {
        Some([0x02, 0, 0, 0, 0, last])
    }

    fn pin(number: usize, last: u8) -> Pin {
        Pin { number, mac: mac(last).unwrap() }
    }

    /// The planned order of `macs.len()` cards and how many pins were not honoured.
    fn run(macs: &[Option<[u8; 6]>], pins: &[Pin]) -> ([Option<usize>; MAX_CARDS], usize) {
        run_limit(macs, pins, 16)
    }

    fn run_limit(macs: &[Option<[u8; 6]>], pins: &[Pin], limit: usize) -> ([Option<usize>; MAX_CARDS], usize) {
        let mut order = [Some(usize::MAX); MAX_CARDS];
        let mut skipped = 0;
        plan(macs, pins, limit, &mut order, |_| skipped += 1);
        (order, skipped)
    }

    #[test]
    fn without_pins_the_pci_order_stands() {
        let (order, skipped) = run(&[mac(1), mac(2), None], &[]);
        assert_eq!(order[..4], [Some(0), Some(1), Some(2), None]);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn a_pin_moves_one_card_and_the_rest_fill_in_pci_order() {
        // rack: I219 00:1f.6, RTL8168 04:00.0, I350 05:00.0 and 05:00.1; the RTL8168 is pinned to 4.
        let (order, _) = run(&[mac(1), mac(8), mac(2), mac(3)], &[pin(4, 8)]);
        assert_eq!(order[..4], [Some(0), Some(2), Some(3), Some(1)]);
    }

    #[test]
    fn a_new_card_in_a_lower_slot_leaves_the_pinned_ones_alone() {
        // A card appears at 03:00.0 (index 1); all four old ones are pinned.
        let macs = [mac(1), mac(9), mac(8), mac(2), mac(3)];
        let (order, _) = run(&macs, &[pin(1, 1), pin(2, 2), pin(3, 3), pin(4, 8)]);
        assert_eq!(order[..5], [Some(0), Some(3), Some(4), Some(2), Some(1)]);
    }

    #[test]
    fn an_absent_or_unread_card_keeps_its_number_empty() {
        let (order, skipped) = run(&[None, mac(2)], &[pin(2, 1)]);
        assert_eq!(order[..3], [Some(0), None, Some(1)]);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn a_pulled_card_leaves_holes_and_a_new_one_takes_an_unclaimed_number() {
        // rack on 2026-11-27: the I350 (pins 2 and 3) replaced by an RTL8125 at 01:00.0; I219 00:1f.6 pinned to 1, the
        // RTL8168 at 04:00.0 to 4. Before, the RTL8168 became nic3 and the services on &4 failed.
        let macs = [mac(1), mac(7), mac(8)];
        let (order, skipped) = run(&macs, &[pin(1, 1), pin(2, 2), pin(3, 3), pin(4, 8)]);
        assert_eq!(order[..6], [Some(0), None, None, Some(2), Some(1), None]);
        assert_eq!(skipped, 2);
    }

    #[test]
    fn with_every_free_number_claimed_an_unpinned_card_takes_an_absent_ones() {
        let (order, _) = run_limit(&[mac(1), mac(7)], &[pin(1, 1), pin(2, 2)], 2);
        assert_eq!(order[..3], [Some(0), Some(1), None]);
    }

    #[test]
    fn a_pin_beyond_the_table_is_reported_and_the_card_takes_a_free_number() {
        let (order, skipped) = run_limit(&[mac(1), mac(2)], &[pin(9, 1)], 8);
        assert_eq!(order[..3], [Some(0), Some(1), None]);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn pins_are_parsed_as_nic_prints_macs() {
        assert_eq!(parse_pin("4 ac:a7:f1:b3:08:5c".split_whitespace()), Some(Pin { number: 4, mac: [0xac, 0xa7, 0xf1, 0xb3, 0x08, 0x5c] }));
        assert_eq!(parse_pin("4 AC-A7-F1-B3-08-5C".split_whitespace()).map(|p| p.number), Some(4));
        assert_eq!(parse_pin("0 ac:a7:f1:b3:08:5c".split_whitespace()), None);
        assert_eq!(parse_pin("4 ac:a7:f1:b3:08".split_whitespace()), None);
        assert_eq!(parse_pin("4 01:00:5e:00:00:01".split_whitespace()), None); // multicast
        assert_eq!(parse_pin("4 ac:a7:f1:b3:08:5c extra".split_whitespace()), None);
    }
}
