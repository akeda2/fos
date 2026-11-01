//! The text engine of the `ebb` editor (a small, embedded cousin of `ebba`).
//!
//! Everything works on byte slices the caller provides (a tape gets them from `TapeApi::alloc`), so
//! there is no global allocator and no `alloc` crate: the text lives in a gap buffer, the undo log
//! in a second slice and the clipboard in a third. Text is bytes; the engine reads UTF-8 and treats
//! a byte that does not start a valid sequence as one Latin-1 character, so a file is saved back
//! exactly as it was loaded.
#![cfg_attr(not(test), no_std)]

#[cfg(test)]
extern crate std;

mod buffer;
mod doc;
mod history;
pub mod layout;

pub use buffer::Buffer;
pub use doc::Doc;
pub use history::History;
