//! `i2cscan`: scans the Raspberry Pi 4's header I2C bus (GPIO2/3) or reads a device register through the tape ABI 23 calls
//! (`tape_rt::i2c_transfer`). It exists to test those calls and as the starting point for a tape that talks to a device.
#![no_std]
#![no_main]

use tape_rt::{TapeApi, println};

tape_rt::tape_help!(br#"i2cscan: the Raspberry Pi 4's header I2C bus (GPIO2 SDA, GPIO3 SCL).

  run i2cscan [speed=KHZ]                scan 08..77 with a one-byte read
  run i2cscan [speed=KHZ] get ADDR REG [N]
                                     write REG to the device, then read N
                                     bytes (default 1; a STOP in between)
  ADDR, REG are hex; speed is 10..1000 kHz (default 100).
"#);

fn hex(word: Option<&str>) -> Option<u8> {
    u8::from_str_radix(word?.trim_start_matches("0x"), 16).ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let mut words = arguments.split_whitespace().peekable();
    if let Some(khz) = words.peek().and_then(|word| word.strip_prefix("speed=")) {
        let Some(khz) = khz.parse::<u32>().ok() else { return println!("i2c: bad speed") };
        if let Err(code) = tape_rt::i2c_set_speed(khz.saturating_mul(1000)) {
            return println!("i2c: cannot set {khz} kHz (code {code}).");
        }
        words.next();
    }
    match tape_rt::i2c_speed() {
        Ok(hz) => println!("I2C bus at {} kHz.", hz / 1000),
        Err(code) => return println!("i2c: no I2C bus here (code {code})."),
    }
    match words.next() {
        None | Some("scan") => {
            let started = tape_rt::micros();
            let mut found = 0;
            let mut byte = [0u8; 1];
            for address in 0x08..=0x77u8 {
                match tape_rt::i2c_transfer(address, &[], &mut byte) {
                    Ok(()) => {
                        println!("  {address:02x}: answers");
                        found += 1;
                    }
                    Err(-1) => {}
                    Err(code) => return println!("i2c: error {code} at {address:02x}."),
                }
            }
            println!("{found} device(s) in {} ms.", (tape_rt::micros() - started) / 1000);
        }
        Some("get") => {
            let (Some(address), Some(register)) = (hex(words.next()), hex(words.next())) else { return println!("i2c: get ADDR REG [N]") };
            let count = words.next().map_or(Some(1), |word| word.parse::<usize>().ok().filter(|&n| (1..=64).contains(&n)));
            let Some(count) = count else { return println!("i2c: N is 1..64") };
            let mut data = [0u8; 64];
            match tape_rt::i2c_transfer(address, &[register], &mut data[..count]) {
                Ok(()) => {
                    for byte in &data[..count] {
                        tape_rt::print!(" {byte:02x}");
                    }
                    println!();
                }
                Err(code) => println!("i2c: error {code} (-1 no acknowledge, -2 timeout)."),
            }
        }
        Some(_) => println!("i2c: run i2cscan [speed=KHZ] [scan | get ADDR REG [N]]"),
    }
}
