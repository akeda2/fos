//! The `touch` shell command both kernels share. The pointer code itself is `kernel_core::usb_pointer`; the blocking monitor
//! loop needs the console and NETFS, so it lives here.

pub use kernel_core::usb_pointer::*;

/// `touch` lists the touch panels; `touch monitor` prints each finger event until Escape.
pub fn command<'a>(words: &mut impl Iterator<Item = &'a str>) {
    match words.next() {
        None => list_command(),
        Some("monitor") => {
            if count() == 0 {
                crate::println!("touch: no USB touch panel (`usb rescan` looks again).");
                return;
            }
            crate::println!("touch: showing finger events; Escape stops.");
            crate::netfs::blocking(true);
            while !crate::netfs::break_requested() {
                crate::usb_class::poll();
                crate::storage::pet();
                crate::netfs::drain(|_, _| {}); // a `term net` client's Escape (and our output to it) goes through here
                while let Some(event) = take() {
                    let (x, y) = range(usize::from(event.panel)).unwrap_or((0, 0));
                    crate::println!("touch {} finger {} {} at {},{} (of {x},{y})", event.panel + 1, event.id, if event.down { "down/moved" } else { "up" }, event.x, event.y);
                }
                core::hint::spin_loop();
            }
            crate::netfs::blocking(false);
        }
        Some(_) => crate::println!("Usage: touch | touch monitor"),
    }
}
