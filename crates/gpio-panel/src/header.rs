//! The 40-pin header of a Raspberry Pi 4: which pad is power, ground or which GPIO, and what a GPIO does by default
//! (alt0 on a stock Pi) so that the picture can say "SDA1" beside GPIO2.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pad {
    V3,
    V5,
    Ground,
    Gpio(u8),
}

use Pad::{Gpio as G, Ground as GND, V3, V5};

/// Pad 1 first. The connector has two columns: odd pads on the left, even pads on the right, pad 1 at the top.
pub const HEADER: [Pad; 40] = [
    V3, V5, G(2), V5, G(3), GND, G(4), G(14), GND, G(15), //
    G(17), G(18), G(27), GND, G(22), G(23), V3, G(24), G(10), GND, //
    G(9), G(25), G(11), G(8), GND, G(7), G(0), G(1), G(5), GND, //
    G(6), G(12), G(13), GND, G(19), G(16), G(26), G(20), GND, G(21),
];

pub const ROWS: usize = 20;

/// Pad `number` (1-40).
pub fn pad(number: usize) -> Option<Pad> {
    number.checked_sub(1).and_then(|index| HEADER.get(index)).copied()
}

/// (row, column) of a pad on the connector: row 0 at the top, column 0 for odd pads.
pub fn position(gpio: u8) -> Option<(usize, usize)> {
    HEADER.iter().position(|pad| *pad == Pad::Gpio(gpio)).map(|index| (index / 2, index % 2))
}

pub fn pad_at(row: usize, column: usize) -> Pad {
    HEADER[row * 2 + column]
}

/// The pad number (1-40) of a GPIO.
pub fn number_of(gpio: u8) -> Option<usize> {
    HEADER.iter().position(|pad| *pad == Pad::Gpio(gpio)).map(|index| index + 1)
}

/// What alt0 is on a stock Pi 4 (BCM2711 datasheet table 5-5); empty where alt0 is nothing useful on the header.
pub fn alt0_name(gpio: u8) -> &'static str {
    match gpio {
        0 => "ID_SD",
        1 => "ID_SC",
        2 => "SDA1",
        3 => "SCL1",
        4 => "GPCLK0",
        5 => "GPCLK1",
        6 => "GPCLK2",
        7 => "SPI0 CE1",
        8 => "SPI0 CE0",
        9 => "SPI0 MISO",
        10 => "SPI0 MOSI",
        11 => "SPI0 SCLK",
        12 => "PWM0",
        13 => "PWM1",
        14 => "TXD0",
        15 => "RXD0",
        18 => "PCM CLK",
        19 => "PCM FS",
        20 => "PCM DIN",
        21 => "PCM DOUT",
        _ => "",
    }
}

/// `function` bytes as the kernel reports them: 0 input, 1 output, 2..=7 alt0..alt5.
pub const FUNCTION_NAMES: [&str; 8] = ["IN", "OUT", "ALT0", "ALT1", "ALT2", "ALT3", "ALT4", "ALT5"];
pub const PULL_NAMES: [&str; 3] = ["none", "up", "down"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connector_is_the_pi_one() {
        assert_eq!(pad(1), Some(Pad::V3));
        assert_eq!(pad(2), Some(Pad::V5));
        assert_eq!(pad(3), Some(Pad::Gpio(2)));
        assert_eq!(pad(11), Some(Pad::Gpio(17)));
        assert_eq!(pad(27), Some(Pad::Gpio(0)));
        assert_eq!(pad(40), Some(Pad::Gpio(21)));
        assert_eq!(pad(0), None);
        assert_eq!(pad(41), None);
    }

    #[test]
    fn every_gpio_from_0_to_27_is_there_once() {
        for gpio in 0..=27u8 {
            assert_eq!(HEADER.iter().filter(|pad| **pad == Pad::Gpio(gpio)).count(), 1, "GPIO{gpio}");
        }
        assert_eq!(HEADER.iter().filter(|pad| matches!(pad, Pad::Gpio(_))).count(), 28);
        assert_eq!(HEADER.iter().filter(|pad| **pad == Pad::Ground).count(), 8);
        assert_eq!((position(17), number_of(17)), (Some((5, 0)), Some(11)));
    }
}
