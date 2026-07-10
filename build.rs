fn main() {
    // Expose the compile-time target triple (e.g. x86_64-unknown-linux-gnu) so
    // `forest update` can select the matching asset from the release manifest
    // (latest.json's `files[].target`). Using the real Cargo TARGET avoids
    // brittle OS/ARCH guessing (gnu vs musl, apple x86_64 vs aarch64, ...).
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=FOREST_TARGET={target}");
}
