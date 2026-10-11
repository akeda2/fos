//! The serial port as a link (COM2-COM4 through the `serial_*` tape calls).

use crate::link::Link;
use fterm::{Action, format_name};
use tape_rt::{
    SERIAL_LINE_CTS, SERIAL_LINE_DCD, SERIAL_LINE_DSR, SERIAL_LINE_DTR, SERIAL_LINE_RI, SERIAL_LINE_RTS, SerialInfo, TapeApi, println,
};

/// How long a Ctrl-A b break lasts, in 10 ms ticks.
const BREAK_TICKS: u64 = 25;

pub struct SerialLink {
    api: &'static TapeApi,
    port: usize,
    lines: u32,
    break_until: u64,
}

impl SerialLink {
    /// Opens the port; the error is the `SERIAL_*` code.
    pub fn open(api: &'static TapeApi, port: usize, baud: u32, format: u32) -> Result<SerialLink, i32> {
        match (api.serial_open)(port, baud, format) {
            0 => Ok(SerialLink { api, port, lines: (api.serial_lines)(port), break_until: 0 }),
            code => Err(code),
        }
    }
}

impl Link for SerialLink {
    fn poll(&mut self) -> bool {
        if self.break_until != 0 && (self.api.ticks)() >= self.break_until {
            (self.api.serial_break)(self.port, 0);
            self.break_until = 0;
        }
        false
    }

    fn read(&mut self, out: &mut [u8]) -> usize {
        let received = (self.api.serial_read)(self.port, out.as_mut_ptr(), out.len());
        // NUL is what an idle line or a baud-rate mismatch produces; the console has no use for it.
        let mut kept = 0;
        for index in 0..received {
            if out[index] != 0 {
                out[kept] = out[index];
                kept += 1;
            }
        }
        kept
    }

    fn write(&mut self, data: &[u8]) -> usize {
        // A break holds the transmitter; typed bytes wait in the loop's queue.
        if self.break_until != 0 {
            return 0;
        }
        (self.api.serial_write)(self.port, data.as_ptr(), data.len().min(16))
    }

    fn command(&mut self, action: &Action) -> bool {
        match action {
            Action::Break => {
                (self.api.serial_break)(self.port, 1);
                self.break_until = (self.api.ticks)() + BREAK_TICKS;
                println!("\n[break]");
            }
            Action::ToggleDtr => {
                self.lines ^= SERIAL_LINE_DTR;
                (self.api.serial_set_lines)(self.port, self.lines);
                println!("\n[DTR {}]", if self.lines & SERIAL_LINE_DTR != 0 { "up" } else { "down" });
            }
            Action::ToggleRts => {
                self.lines ^= SERIAL_LINE_RTS;
                (self.api.serial_set_lines)(self.port, self.lines);
                println!("\n[RTS {}]", if self.lines & SERIAL_LINE_RTS != 0 { "up" } else { "down" });
            }
            _ => return false,
        }
        true
    }

    fn status(&mut self) {
        let mut info = SerialInfo::default();
        (self.api.serial_info)(self.port, &mut info);
        let lines = (self.api.serial_lines)(self.port);
        let mut format = [0u8; 3];
        format_name(info.format, &mut format);
        let on = |line: u32, name: &'static str| if lines & line != 0 { name } else { "-" };
        println!(
            "\n[fterm: serial {} baud {}, rx {} tx {}, errors: overrun {} frame {} parity {} break {}]",
            info.baud,
            core::str::from_utf8(&format).unwrap_or(""),
            info.rx_bytes,
            info.tx_bytes,
            info.overruns,
            info.framing_errors,
            info.parity_errors,
            info.breaks,
        );
        println!(
            "[lines: {} {} {} {} {} {}]",
            on(SERIAL_LINE_CTS, "CTS"),
            on(SERIAL_LINE_DSR, "DSR"),
            on(SERIAL_LINE_RI, "RI"),
            on(SERIAL_LINE_DCD, "DCD"),
            on(SERIAL_LINE_DTR, "DTR"),
            on(SERIAL_LINE_RTS, "RTS"),
        );
    }

    fn close(&mut self) {
        (self.api.serial_break)(self.port, 0);
        (self.api.serial_close)(self.port);
    }
}
