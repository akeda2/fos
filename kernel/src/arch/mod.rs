//! The architecture layer. Everything that is specific to one CPU family lives under `arch/NAME/`: the boot entry
//! stub, CPU control, descriptor tables or exception vectors, the interrupt controller, page-table format and the
//! firmware table readers. `main.rs` re-exports the active architecture's modules at the crate root, so drivers and the
//! shell say `crate::cpu`, `crate::paging` and so on. A second architecture (docs/aarch64-plan.md) adds a sibling directory.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
