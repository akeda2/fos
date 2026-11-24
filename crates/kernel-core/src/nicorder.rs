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
//! The table stays dense: a pinned card that is absent leaves no hole, so a pin above the number of cards present cannot be
//! honoured (it is reported, and the card takes a free number). Pins are read only when `settings` has some; without them no
//! card is touched before its driver's `init`.
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
    /// Fewer cards than the number: the table has no holes.
    BeyondCards(Pin, usize),
}

/// Numbers the cards. `macs[i]` is the MAC of the i-th card in PCI order (`None`: not read). Writes to `order[k]` the index
/// into `macs` of the card that becomes interface k+1, for every card; reports each pin not honoured through `skipped`.
pub fn plan(macs: &[Option<[u8; 6]>], pins: &[Pin], order: &mut [usize], mut skipped: impl FnMut(Unhonoured)) {
    let count = macs.len().min(order.len()).min(MAX_CARDS);
    let mut placed = [false; MAX_CARDS]; // card i has a number
    let mut slot: [Option<usize>; MAX_CARDS] = [None; MAX_CARDS]; // number k+1 -> card
    for pin in pins {
        match macs[..count].iter().position(|mac| *mac == Some(pin.mac)) {
            None => skipped(Unhonoured::Absent(*pin)),
            Some(_) if pin.number > count => skipped(Unhonoured::BeyondCards(*pin, count)),
            // `add_pin` refuses duplicate numbers and MACs, so neither the slot nor the card is taken here.
            Some(card) if slot[pin.number - 1].is_none() && !placed[card] => {
                slot[pin.number - 1] = Some(card);
                placed[card] = true;
            }
            Some(_) => skipped(Unhonoured::Absent(*pin)),
        }
    }
    let mut unplaced = (0..count).filter(|&card| !placed[card]);
    for (number, entry) in slot[..count].iter().enumerate() {
        order[number] = match entry {
            Some(card) => *card,
            None => unplaced.next().expect("one free number per unplaced card"),
        };
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

/// The NIC cards of the families in `kinds`, in the order to bring them up (PCI order, then pins applied), `None` after the
/// last. Narrates the pins it cannot honour.
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
    let mut order = [0usize; MAX_CARDS];
    if pin_count == 0 {
        for (number, entry) in order.iter_mut().enumerate() {
            *entry = number;
        }
    } else {
        let mut macs = [None; MAX_CARDS];
        for (mac, card) in macs.iter_mut().zip(cards[..count].iter().flatten()) {
            *mac = peek_mac(card);
        }
        crate::println!("nic: {pin_count} pinned number(s) in settings; read {} card MAC(s) to place them.", macs.iter().flatten().count());
        plan(&macs[..count], &pins[..pin_count], &mut order, |why| match why {
            Unhonoured::Absent(pin) => crate::println!("nic: pin {} ignored: no card with MAC {}.", pin.number, Mac(pin.mac)),
            Unhonoured::BeyondCards(pin, cards) => {
                crate::println!("nic: pin {} ({}) ignored: only {cards} card(s),", pin.number, Mac(pin.mac));
                crate::println!("     and the numbers have no gaps; it takes a free one.");
            }
        });
    }
    let mut out = [None; MAX_CARDS];
    for (slot, &card) in out.iter_mut().zip(&order[..count]) {
        *slot = cards[card];
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
    fn run(macs: &[Option<[u8; 6]>], pins: &[Pin]) -> ([usize; MAX_CARDS], usize) {
        let mut order = [usize::MAX; MAX_CARDS];
        let mut skipped = 0;
        plan(macs, pins, &mut order, |_| skipped += 1);
        (order, skipped)
    }

    #[test]
    fn without_pins_the_pci_order_stands() {
        let (order, skipped) = run(&[mac(1), mac(2), None], &[]);
        assert_eq!(order[..3], [0, 1, 2]);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn a_pin_moves_one_card_and_the_rest_fill_in_pci_order() {
        // rack: I219 00:1f.6, RTL8168 04:00.0, I350 05:00.0 and 05:00.1; the RTL8168 is pinned to 4.
        let (order, _) = run(&[mac(1), mac(8), mac(2), mac(3)], &[pin(4, 8)]);
        assert_eq!(order[..4], [0, 2, 3, 1]);
    }

    #[test]
    fn a_new_card_in_a_lower_slot_leaves_the_pinned_ones_alone() {
        // A card appears at 03:00.0 (index 1); all four old ones are pinned.
        let macs = [mac(1), mac(9), mac(8), mac(2), mac(3)];
        let (order, _) = run(&macs, &[pin(1, 1), pin(2, 2), pin(3, 3), pin(4, 8)]);
        assert_eq!(order[..5], [0, 3, 4, 2, 1]);
    }

    #[test]
    fn an_absent_or_unread_card_leaves_its_pin_unused() {
        let (order, skipped) = run(&[None, mac(2)], &[pin(2, 1)]);
        assert_eq!(order[..2], [0, 1]);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn a_pin_beyond_the_cards_present_is_reported_and_the_card_takes_a_free_number() {
        let (order, skipped) = run(&[mac(1), mac(2)], &[pin(5, 1)]);
        assert_eq!(order[..2], [0, 1]);
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
