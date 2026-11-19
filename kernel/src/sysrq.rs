//! SysRq: a state dump on request over the network, for a machine whose shell or tape seems stuck.
//!
//! A host sends one frame with tag `TAG_SYSRQ` (fos ethertype 0x88b6, no payload; `tools/term_net.py --sysrq`, `fosadm
//! sysrq HOST`). Whoever drains the NICs next (the shell's loop, or any tape or service making a tape call) answers with
//! the dump as `term net` DATA frames sent to the requester alone, without HELLO or BYE: an open session is left as it
//! is, and a running tape is not stopped. The dump also goes to the screen and the serial port.
//!
//! It shows what the kakelfix test flake needed and could only be read from QEMU memory dumps: which context runs on
//! whose stack, how often each context made the input, audio and screen calls, who holds the loopback ends, and the
//! most recent trace events (`trace` command). Answered only when the network terminal is open (`deny-term-net`).
//! A machine stuck with interrupts off, or in a loop that makes no tape call, cannot answer: that needs the serial port.

pub const TAG_SYSRQ: u8 = 0x24;

/// Handles a SysRq frame from `source` on interface `index`: prints the dump to it.
pub fn request(index: usize, source: [u8; 6]) {
    if crate::netfs::term_denied() {
        return;
    }
    crate::trace::event(crate::trace::Category::Net, "sysrq", u64::from(source[5]), index as u64);
    crate::termsession::capture(index, source, dump);
}

/// Prints the dump (also the `sysrq` shell command).
pub fn dump() {
    let micros = crate::timer::micros();
    crate::println!("--- sysrq: up {}.{:03} s ---", micros / 1_000_000, micros / 1000 % 1000);
    crate::tasks::describe();
    #[cfg(target_arch = "x86_64")]
    crate::term::describe_input();
    #[cfg(target_arch = "aarch64")]
    crate::tape_aarch64::describe_input();
    for index in 0..crate::loopback::COUNT {
        crate::loopback::describe(index);
    }
    crate::println!("last trace events ({} recorded; `trace` for more):", crate::trace::total());
    crate::trace::print_recent(16);
    crate::println!("--- end sysrq ---");
}
