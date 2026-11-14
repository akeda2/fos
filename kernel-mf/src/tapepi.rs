//! Tape calls for the Raspberry Pi 4's header: `gpio_*` (ABI 22) and `i2c_*` (ABI 23), on the embedded kernel's drivers
//! (`gpio.rs`, `i2c.rs`). The GPIO calls take and return plain numbers and go straight to the driver. An I2C transfer moves
//! bytes between a tape's memory and the bus, so it goes through kernel buffers, and runs pinned to CPU 0 where the bus's
//! interrupt is handled. Everything a tape changed is undone when it ends (`process_ended`), as on the embedded kernel.

use crate::{gpio, i2c, process, sched};

pub fn i2c_speed(hz: u64) -> i64 {
    sched::pin_current(0);
    i64::from(i2c::api_i2c_speed(hz as u32))
}

pub fn i2c_transfer(address: u64, out: u64, out_len: u64, input: u64, in_len: u64) -> i64 {
    if !i2c::available() {
        return -4;
    }
    if address > 0x7f || (out_len == 0 && in_len == 0) || out_len > 0xffff || in_len > 0xffff {
        return -3;
    }
    let Some(request) = (if out_len > 0 { process::user_bytes_up_to(out, out_len, 0xffff) } else { Some(alloc::vec::Vec::new()) }) else { return -3 };
    sched::pin_current(0);
    let mut reply = alloc::vec![0u8; in_len as usize];
    let status = i2c::api_i2c_transfer(address as u32, request.as_ptr(), request.len() as u32, reply.as_mut_ptr(), in_len as u32);
    if status == 0 && in_len > 0 && !process::copy_to_user(input, &reply) {
        return -3;
    }
    i64::from(status)
}

/// A tape ended: its pin modes and the bus speed go back to what the kernel had.
pub fn process_ended(_pid: u64) {
    gpio::tape_ended();
    i2c::tape_ended();
}
