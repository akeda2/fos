//! What the buttons do. A `Panel` holds the newest state of the pins, the selected pin and whether changes are armed, and
//! turns a press into at most one `Request` for whoever owns the pins (the tape's local calls or a network `Client`). Nothing is
//! driven by surprise: changes need ARM, a pin the machine does not allow is refused here already, and an output always
//! starts low.

use crate::header::{self, Pad};
use crate::layout::Hit;
use crate::wire::{Request, State};

/// How long PULSE holds the opposite level.
pub const PULSE_MS: u64 = 200;

pub const NOTE_ARM: &str = "Press ARM first: changes are locked";
pub const NOTE_LOCKED: &str = "That pin is not allowed to change";
pub const NOTE_READ_ONLY: &str = "That machine takes no writes (gpio-remote)";
pub const NOTE_NOT_OUTPUT: &str = "Not an output: press OUT first";
pub const NOTE_NO_STATE: &str = "No pin state yet";

pub struct Panel {
    pub state: Option<State>,
    pub selected: u8,
    pub armed: bool,
    pub note: &'static str,
    /// (pin, level to go back to, when).
    pulse: Option<(u8, bool, u64)>,
}

impl Panel {
    pub fn new() -> Panel {
        Panel { state: None, selected: 17, armed: false, note: "", pulse: None }
    }

    pub fn set_state(&mut self, state: State) {
        self.state = Some(state);
    }

    pub fn pulsing(&self) -> bool {
        self.pulse.is_some()
    }

    /// May we change `pin` now? The reason if not.
    fn check(&self, pin: u8) -> Result<&State, &'static str> {
        let state = self.state.as_ref().ok_or(NOTE_NO_STATE)?;
        if !self.armed {
            return Err(NOTE_ARM);
        }
        if !state.is_allowed(pin) {
            return Err(NOTE_LOCKED);
        }
        if !state.is_writable(pin) {
            return Err(NOTE_READ_ONLY);
        }
        Ok(state)
    }

    fn output_level(&self, pin: u8) -> Result<bool, &'static str> {
        let state = self.check(pin)?;
        if state.function_of(pin) != 1 {
            return Err(NOTE_NOT_OUTPUT);
        }
        Ok(state.level(pin))
    }

    /// A press; the request to carry out, if any. A refusal sets `note`.
    pub fn press(&mut self, hit: Hit, now: u64) -> Option<Request> {
        self.note = "";
        match self.act(hit, now) {
            Ok(request) => request,
            Err(note) => {
                self.note = note;
                None
            }
        }
    }

    fn act(&mut self, hit: Hit, now: u64) -> Result<Option<Request>, &'static str> {
        let pin = self.selected;
        match hit {
            Hit::Pin(gpio) => {
                // A second tap on an output that is armed is a switch.
                let again = gpio == self.selected;
                self.selected = gpio;
                if again && self.state.as_ref().is_some_and(|state| state.function_of(gpio) == 1) && self.armed {
                    return self.act(Hit::Toggle, now);
                }
                Ok(None)
            }
            Hit::Arm => {
                self.armed = !self.armed;
                Ok(None)
            }
            Hit::Function(function) => {
                self.check(pin)?;
                self.pulse = None;
                Ok(Some(Request::Function { pin, function, level: false }))
            }
            Hit::Pull(pull) => {
                self.check(pin)?;
                Ok(Some(Request::Pull { pin, pull }))
            }
            Hit::Level(level) => {
                self.output_level(pin)?;
                self.pulse = None;
                Ok(Some(Request::Set { mask: 1 << pin, values: u64::from(level) << pin }))
            }
            Hit::Toggle => {
                let level = self.output_level(pin)?;
                self.pulse = None;
                Ok(Some(Request::Set { mask: 1 << pin, values: u64::from(!level) << pin }))
            }
            Hit::Pulse => {
                let level = self.output_level(pin)?;
                self.pulse = Some((pin, level, now + PULSE_MS));
                Ok(Some(Request::Set { mask: 1 << pin, values: u64::from(!level) << pin }))
            }
        }
    }

    /// Every pass: the end of a pulse.
    pub fn tick(&mut self, now: u64) -> Option<Request> {
        let (pin, back, until) = self.pulse?;
        if now < until {
            return None;
        }
        self.pulse = None;
        Some(Request::Set { mask: 1 << pin, values: u64::from(back) << pin })
    }

    /// An arrow key: the selection moves over the connector to the next GPIO pad in that direction.
    pub fn step(&mut self, rows: i32, columns: i32) {
        let Some((mut row, column)) = header::position(self.selected) else { return };
        if columns != 0 {
            let other = 1 - column;
            if let Pad::Gpio(gpio) = header::pad_at(row, other) {
                self.selected = gpio;
            }
            return;
        }
        for _ in 0..header::ROWS {
            row = (row as i32 + rows).rem_euclid(header::ROWS as i32) as usize;
            if let Pad::Gpio(gpio) = header::pad_at(row, column) {
                self.selected = gpio;
                return;
            }
        }
    }
}

impl Default for Panel {
    fn default() -> Self {
        Panel::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::PINS;

    fn state(function17: u8, writable: bool) -> State {
        let mut state = State { levels: 0, allowed: 0x0fff_3ffc, writable: if writable { 0x0fff_3ffc } else { 0 }, function: [0; PINS], pull: [0; PINS] };
        state.function[17] = function17;
        state
    }

    #[test]
    fn nothing_changes_until_armed() {
        let mut panel = Panel::new();
        panel.set_state(state(1, true));
        assert_eq!(panel.press(Hit::Level(true), 0), None);
        assert_eq!(panel.note, NOTE_ARM);
        panel.press(Hit::Arm, 0);
        assert_eq!(panel.press(Hit::Level(true), 0), Some(Request::Set { mask: 1 << 17, values: 1 << 17 }));
        assert_eq!(panel.press(Hit::Toggle, 0), Some(Request::Set { mask: 1 << 17, values: 1 << 17 }));
    }

    #[test]
    fn an_output_starts_low_and_levels_need_an_output() {
        let mut panel = Panel::new();
        panel.set_state(state(0, true));
        panel.press(Hit::Arm, 0);
        assert_eq!(panel.press(Hit::Function(1), 0), Some(Request::Function { pin: 17, function: 1, level: false }));
        assert_eq!(panel.press(Hit::Level(true), 0), None);
        assert_eq!(panel.note, NOTE_NOT_OUTPUT);
        assert_eq!(panel.press(Hit::Pull(1), 0), Some(Request::Pull { pin: 17, pull: 1 }));
    }

    #[test]
    fn pins_the_machine_refuses_are_refused_here_too() {
        let mut panel = Panel::new();
        panel.set_state(state(1, true));
        panel.press(Hit::Arm, 0);
        panel.press(Hit::Pin(14), 0);
        assert_eq!(panel.press(Hit::Function(0), 0), None);
        assert_eq!(panel.note, NOTE_LOCKED);
        panel.set_state(state(1, false));
        panel.press(Hit::Pin(17), 0);
        assert_eq!(panel.press(Hit::Function(0), 0), None);
        assert_eq!(panel.note, NOTE_READ_ONLY);
    }

    #[test]
    fn a_second_tap_on_an_armed_output_switches_it_and_a_pulse_comes_back() {
        let mut panel = Panel::new();
        panel.set_state(state(1, true));
        panel.press(Hit::Arm, 0);
        panel.press(Hit::Pin(4), 0);
        panel.press(Hit::Pin(17), 0);
        assert_eq!(panel.press(Hit::Pin(17), 0), Some(Request::Set { mask: 1 << 17, values: 1 << 17 }));
        assert_eq!(panel.press(Hit::Pulse, 1000), Some(Request::Set { mask: 1 << 17, values: 1 << 17 }));
        assert_eq!(panel.tick(1000 + PULSE_MS - 1), None);
        assert_eq!(panel.tick(1000 + PULSE_MS), Some(Request::Set { mask: 1 << 17, values: 0 }));
        assert_eq!(panel.tick(5000), None);
    }

    #[test]
    fn the_arrow_keys_skip_power_and_ground() {
        let mut panel = Panel::new();
        panel.selected = 2;
        panel.step(-1, 0);
        assert_eq!(panel.selected, 26, "up from the top wraps round, past ground");
        panel.step(0, 1);
        assert_eq!(panel.selected, 20);
        panel.step(1, 0);
        assert_eq!(panel.selected, 21);
        panel.step(1, 0);
        assert_eq!(panel.selected, 14, "down from the last pad wraps past 5 V and ground");
    }
}
