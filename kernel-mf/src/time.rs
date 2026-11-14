//! Time as the shared kernel sees it: the scheduler tick, a microsecond clock and a bounded busy wait. The sources are the
//! architecture's (the local APIC timer and PM timer / TSC on x86, the generic timer and GIC on aarch64).

pub use crate::arch::{TICK_HZ, busy_wait_ms, micros, ticks};
