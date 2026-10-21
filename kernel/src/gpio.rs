//! GPIO pins: the Raspberry Pi 4's BCM2711 bank in the kernel (both kernels share the policy, the shell command and the
//! remote protocol; only the Pi has pins). Tapes reach it through the `gpio_*` calls of tape ABI 22, the shell through
//! `gpio` (`gpio_shell.rs`), other machines through `gpio_remote.rs`. Why a kernel driver and not register access from
//! tapes: the pins share one block with the console UART and the I2C/SPI muxing, so the kernel can refuse the pins it owns.
//!
//! Registers (BCM2711 ARM Peripherals, checked against Linux' `drivers/pinctrl/bcm/pinctrl-bcm2835.c`), base 0xFE200000:
//! `GPFSEL0..5` (0x00) three bits per pin, ten pins per register; `GPSET0/1` (0x1C), `GPCLR0/1` (0x28) write-one-to-act,
//! so setting an output is never a read-modify-write; `GPLEV0/1` (0x34) the pin level; `GPIO_PUP_PDN_CNTRL_REG0..3`
//! (0xE4) two bits per pin, sixteen pins per register (0 none, 1 up, 2 down). The older `GPPUD`/`GPPUDCLK` sequence does
//! not exist on the BCM2711. Function codes in `GPFSEL`: 0 input, 1 output, 4..7 = alt0..alt3, 3 = alt4, 2 = alt5.
//!
//! Rules taken from Linux: an output's level is written *before* the pin becomes an output (no glitch on a relay); a pin
//! that changes between two non-input functions goes through input; pull is a read-modify-write of its 2-bit field.
//!
//! Functions are numbered `0` input, `1` output, `2..=7` alt0..alt5 everywhere outside this file's register code (tape ABI,
//! wire, shell). 58 pins (GPIO0..57) fit one `u64` mask.
//!
//! Safety policy: only GPIO2..27 minus 14 and 15 (the console UART) can be changed, minus whatever `gpio-deny N...` in
//! `settings` removes; every pin can be read. A pin is never driven unless asked: nothing is configured at boot. Another
//! machine may only write when `settings` has `gpio-remote`. When a tape ends, pins it changed that are not input or
//! output go back to input; outputs stay as they were (as in Linux: a relay must not drop because a tape ended). `gpio-sim`
//! in `settings` gives either kernel a RAM-backed bank, so the shell, the remote protocol and the tape calls can be tested
//! without a Pi (input pins read high with pull-up, low with pull-down, otherwise low).
//! 3.3 V only: the pins are not 5 V tolerant and a few mA each.

pub const PINS: usize = 58;
const ALL: u64 = (1 << PINS) - 1;
/// GPIO2..27 without 14 and 15 (the console UART): the pins `gpio` may change unless `gpio-deny` takes more away.
const USER_PINS: u64 = ((1 << 28) - 1) & !0b11 & !(1 << 14) & !(1 << 15);

pub const FUNC_IN: u8 = 0;
pub const FUNC_OUT: u8 = 1;
const FUNC_TO_CODE: [u8; 8] = [0, 1, 4, 5, 6, 7, 3, 2];
const CODE_TO_FUNC: [u8; 8] = [0, 1, 7, 6, 2, 3, 4, 5];

pub const PULL_NONE: u8 = 0;
pub const PULL_UP: u8 = 1;
pub const PULL_DOWN: u8 = 2;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Error {
    /// No GPIO bank on this machine.
    NoGpio,
    /// The pin is one the kernel does not let anyone change.
    Refused,
    BadArgument,
}

impl Error {
    pub fn text(self) -> &'static str {
        match self {
            Error::NoGpio => "this machine has no GPIO pins",
            Error::Refused => "that pin is not allowed (GPIO2..27 except 14, 15 and `gpio-deny`)",
            Error::BadArgument => "bad pin, function or pull",
        }
    }
}

/// Everything a listing shows, from this machine or decoded from another's answer.
pub struct Snapshot {
    pub levels: u64,
    pub allowed: u64,
    /// Pins another machine may write (`gpio-remote`), `allowed` or 0.
    pub writable: u64,
    pub function: [u8; PINS],
    pub pull: [u8; PINS],
}

enum Bank {
    None,
    Pi,
    Sim,
}

struct State {
    bank: Bank,
    started: bool,
    sim_code: [u8; PINS],
    sim_pull: [u8; PINS],
    sim_latch: u64,
    deny: u64,
    remote_write: bool,
    sim: bool,
    /// Pins a tape changed since it started (`tape_ended` cleans up after them).
    touched: u64,
}

static mut STATE: State = State { bank: Bank::None, started: false, sim_code: [0; PINS], sim_pull: [0; PINS], sim_latch: 0, deny: 0, remote_write: false, sim: false, touched: 0 };

fn state() -> &'static mut State {
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

#[cfg(target_arch = "aarch64")]
const PI4_GPIO: u64 = 0xfe20_0000;
#[cfg(target_arch = "aarch64")]
const PI4_GIC_DISTRIBUTOR: u64 = 0xff84_1000;

/// Finds the bank on first use (the page tables must exist by then).
fn bank() -> &'static mut State {
    let state = state();
    if !state.started {
        state.started = true;
        state.bank = Bank::None;
        #[cfg(target_arch = "aarch64")]
        if crate::arch::aarch64::gic::bases().0 == PI4_GIC_DISTRIBUTOR {
            crate::paging::map_device(PI4_GPIO, 0x1000);
            state.bank = Bank::Pi;
        }
        if matches!(state.bank, Bank::None) && state.sim {
            state.bank = Bank::Sim;
        }
    }
    state
}

/// One `settings` line: `gpio-remote`, `gpio-deny N...`, `gpio-sim`. True if the line was a GPIO one.
pub fn apply_setting(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let state = state();
    match words.next() {
        Some("gpio-remote") => {
            state.remote_write = true;
            crate::println!("settings: gpio-remote -- other machines may change the GPIO pins (with the synckey).");
        }
        Some("gpio-sim") => {
            state.sim = true;
            state.started = false;
            crate::println!("settings: gpio-sim -- a RAM-backed GPIO bank (a test aid).");
        }
        Some("gpio-deny") => {
            for word in words {
                match word.parse::<usize>() {
                    Ok(pin) if pin < PINS => state.deny |= 1 << pin,
                    _ => crate::println!("settings: gpio-deny: `{word}` is not a pin (0..57)."),
                }
            }
        }
        _ => return false,
    }
    true
}

pub fn available() -> bool {
    !matches!(bank().bank, Bank::None)
}

/// Pins that may be changed.
pub fn allowed() -> u64 {
    if available() { USER_PINS & !state().deny } else { 0 }
}

pub fn remote_writes() -> bool {
    state().remote_write
}

// ---- register access -------------------------------------------------------------------------------------------------

#[cfg(target_arch = "aarch64")]
fn reg_read(offset: u64) -> u32 {
    unsafe { ((PI4_GPIO + offset) as *const u32).read_volatile() }
}
#[cfg(target_arch = "aarch64")]
fn reg_write(offset: u64, value: u32) {
    unsafe { ((PI4_GPIO + offset) as *mut u32).write_volatile(value) };
}
#[cfg(not(target_arch = "aarch64"))]
fn reg_read(_: u64) -> u32 {
    0
}
#[cfg(not(target_arch = "aarch64"))]
fn reg_write(_: u64, _: u32) {}

fn code(pin: usize) -> u8 {
    let state = bank();
    match state.bank {
        Bank::Pi => ((reg_read(4 * (pin as u64 / 10)) >> (3 * (pin % 10))) & 7) as u8,
        Bank::Sim => state.sim_code[pin],
        Bank::None => 0,
    }
}

fn set_code(pin: usize, new: u8) {
    let state = bank();
    match state.bank {
        Bank::Pi => {
            let offset = 4 * (pin as u64 / 10);
            let shift = 3 * (pin % 10);
            let current = (reg_read(offset) >> shift) & 7;
            if current == u32::from(new) {
                return;
            }
            let mut value = reg_read(offset);
            if current != 0 && new != 0 {
                value &= !(7 << shift);
                reg_write(offset, value);
            }
            value = value & !(7 << shift) | u32::from(new) << shift;
            reg_write(offset, value);
        }
        Bank::Sim => state.sim_code[pin] = new,
        Bank::None => {}
    }
}

fn pull_of(pin: usize) -> u8 {
    let state = bank();
    match state.bank {
        Bank::Pi => ((reg_read(0xe4 + 4 * (pin as u64 / 16)) >> (2 * (pin % 16))) & 3) as u8,
        Bank::Sim => state.sim_pull[pin],
        Bank::None => 0,
    }
}

fn latch(set: u64, clear: u64) {
    let state = bank();
    match state.bank {
        Bank::Pi => {
            if set & 0xffff_ffff != 0 {
                reg_write(0x1c, set as u32);
            }
            if set >> 32 != 0 {
                reg_write(0x20, (set >> 32) as u32);
            }
            if clear & 0xffff_ffff != 0 {
                reg_write(0x28, clear as u32);
            }
            if clear >> 32 != 0 {
                reg_write(0x2c, (clear >> 32) as u32);
            }
        }
        Bank::Sim => state.sim_latch = state.sim_latch & !clear | set,
        Bank::None => {}
    }
}

/// Pin levels, GPIO0 in bit 0.
pub fn levels() -> u64 {
    let state = bank();
    match state.bank {
        Bank::Pi => (u64::from(reg_read(0x38)) << 32 | u64::from(reg_read(0x34))) & ALL,
        Bank::Sim => {
            let mut levels = 0;
            for pin in 0..PINS {
                let high = match state.sim_code[pin] {
                    1 => state.sim_latch >> pin & 1 != 0,
                    0 => state.sim_pull[pin] == PULL_UP,
                    _ => false,
                };
                levels |= u64::from(high) << pin;
            }
            levels
        }
        Bank::None => 0,
    }
}

// ---- the operations ---------------------------------------------------------------------------------------------------

/// The function of `pin` (0 input, 1 output, 2..=7 alt0..alt5).
pub fn function(pin: usize) -> Option<u8> {
    (pin < PINS && available()).then(|| CODE_TO_FUNC[usize::from(code(pin))])
}

pub fn pull(pin: usize) -> Option<u8> {
    (pin < PINS && available()).then(|| pull_of(pin))
}

fn check(pin: usize) -> Result<(), Error> {
    if !available() {
        Err(Error::NoGpio)
    } else if pin >= PINS {
        Err(Error::BadArgument)
    } else if allowed() >> pin & 1 == 0 {
        Err(Error::Refused)
    } else {
        Ok(())
    }
}

/// Gives `pin` a function. An output starts at `level` (written first, so it never shows the old latch).
pub fn set_function(pin: usize, function: u8, level: bool) -> Result<(), Error> {
    check(pin)?;
    if function > 7 {
        return Err(Error::BadArgument);
    }
    if function == FUNC_OUT {
        if level { latch(1 << pin, 0) } else { latch(0, 1 << pin) }
    }
    set_code(pin, FUNC_TO_CODE[usize::from(function)]);
    Ok(())
}

pub fn set_pull(pin: usize, pull: u8) -> Result<(), Error> {
    check(pin)?;
    if pull > PULL_DOWN {
        return Err(Error::BadArgument);
    }
    let state = bank();
    match state.bank {
        Bank::Pi => {
            let offset = 0xe4 + 4 * (pin as u64 / 16);
            let shift = 2 * (pin % 16);
            reg_write(offset, reg_read(offset) & !(3 << shift) | u32::from(pull) << shift);
        }
        Bank::Sim => state.sim_pull[pin] = pull,
        Bank::None => {}
    }
    Ok(())
}

/// Sets the pins in `mask` to the matching bits of `values`. Returns the pins refused: not allowed, or not an output.
pub fn write(mask: u64, values: u64) -> u64 {
    if !available() {
        return mask;
    }
    let mut refused = mask & !ALL;
    let mut set = 0;
    let mut clear = 0;
    for pin in 0..PINS {
        if mask >> pin & 1 == 0 {
            continue;
        }
        if allowed() >> pin & 1 == 0 || code(pin) != FUNC_TO_CODE[usize::from(FUNC_OUT)] {
            refused |= 1 << pin;
        } else if values >> pin & 1 != 0 {
            set |= 1 << pin;
        } else {
            clear |= 1 << pin;
        }
    }
    latch(set, clear);
    refused
}

pub fn snapshot() -> Option<Snapshot> {
    if !available() {
        return None;
    }
    let mut snapshot = Snapshot {
        levels: levels(),
        allowed: allowed(),
        writable: if remote_writes() { allowed() } else { 0 },
        function: [0; PINS],
        pull: [0; PINS],
    };
    for pin in 0..PINS {
        snapshot.function[pin] = CODE_TO_FUNC[usize::from(code(pin))];
        snapshot.pull[pin] = pull_of(pin);
    }
    Some(snapshot)
}

/// A tape ends: pins it changed that are neither input nor output go back to input (outputs keep their level).
pub fn tape_ended() {
    let state = state();
    let touched = core::mem::take(&mut state.touched);
    if touched == 0 {
        return;
    }
    for pin in 0..PINS {
        if touched >> pin & 1 != 0 && code(pin) > 1 {
            set_code(pin, 0);
        }
    }
}

// ---- tape ABI 22 -----------------------------------------------------------------------------------------------------

/// `gpio_pins()`: the pins a tape may change (0: none here).
pub extern "C" fn api_gpio_pins() -> u64 {
    allowed()
}

/// `gpio_read()`: the level of every pin, GPIO0 in bit 0 (0 without pins).
pub extern "C" fn api_gpio_read() -> u64 {
    levels()
}

/// `gpio_write(mask, values)`: sets output pins; returns the pins that were refused (not allowed or not an output).
pub extern "C" fn api_gpio_write(mask: u64, values: u64) -> u64 {
    write(mask, values)}

/// `gpio_mode(pin, function, level)`: 0, or a negative `-1` refused / `-2` bad argument / `-3` no GPIO.
pub extern "C" fn api_gpio_mode(pin: u32, function: u32, level: u32) -> i32 {
    if function > 255 {
        return -2;
    }
    match set_function(pin as usize, function as u8, level != 0) {
        Ok(()) => {
            state().touched |= 1 << pin;
            0
        }
        Err(error) => status_code(error),
    }
}

/// `gpio_pull(pin, pull)`: 0 none, 1 up, 2 down; results as `gpio_mode`.
pub extern "C" fn api_gpio_pull(pin: u32, pull: u32) -> i32 {
    match set_pull(pin as usize, pull as u8) {
        Ok(()) => 0,
        Err(error) => status_code(error),
    }
}

/// `gpio_function(pin)`: 0 input, 1 output, 2..=7 alt0..alt5; negative for an unknown pin or no GPIO.
pub extern "C" fn api_gpio_function(pin: u32) -> i32 {
    function(pin as usize).map_or(-2, i32::from)
}

fn status_code(error: Error) -> i32 {
    match error {
        Error::Refused => -1,
        Error::BadArgument => -2,
        Error::NoGpio => -3,
    }
}
