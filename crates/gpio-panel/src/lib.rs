//! What the `gpiopanel` tape is made of, apart from the hardware calls: `header` (the 40-pin connector of a Raspberry Pi),
//! `wire` (the frames of GPIO over the network), `client` (a non-blocking requester that speaks them), `panel` (what the
//! buttons do) and `layout` (where everything is on the screen).
#![cfg_attr(not(test), no_std)]

pub mod client;
pub mod header;
pub mod layout;
pub mod panel;
pub mod wire;

pub use client::{Client, Event};
pub use layout::{Hit, Layout, Rect};
pub use panel::Panel;
pub use wire::{PINS, PULL_UNKNOWN, Request, State};
