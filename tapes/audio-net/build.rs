// Links the vendored libopus archive built by `make v2-opus-lib` (build/v2/opus-fixed). A build script is used
// instead of rustflags so the link flags apply to this crate only, not to the -Z build-std crates.
fn main() {
    let dir = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../build/v2/opus-fixed");
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=opus_fixed");
    println!("cargo:rerun-if-changed={}", dir.join("libopus_fixed.a").display());
}
