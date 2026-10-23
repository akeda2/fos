// Links the vendored libopus archive built by `make opus-lib` (build/fosfor/opus-float, build/fosfor-aarch64/opus-float on aarch64). A build script is used
// instead of rustflags so the link flags apply to this crate only, not to the -Z build-std crates.
fn main() {
    let build = if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") { "fosfor-aarch64" } else { "fosfor" };
    let dir = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../build").join(build).join("opus-float");
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=opus_float");
    println!("cargo:rerun-if-changed={}", dir.join("libopus_float.a").display());
}
