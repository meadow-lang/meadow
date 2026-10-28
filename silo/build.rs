//! The fingerprint of the sources that decide the ABI between the code
//! `meadow-llvm` emits and this runtime, as `MEADOW_SILO_FINGERPRINT`: that
//! crate's, the LIR's it prints, and this one's. The library exports a symbol named after it, and
//! the emitted module refers to that symbol, so a program links only with a
//! runtime built from the sources its compiler was.
//!
//! `compiler/meadow-llvm/build.rs` hashes the same trees with the same
//! function, `glade/fingerprint.rs`.

use std::path::Path;

include!("../glade/fingerprint.rs");

fn main() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let roots = [
        here.join("../compiler/meadow-llvm/src"),
        here.join("../compiler/meadow-lir/src"),
        here.join("src"),
    ];
    println!(
        "cargo:rustc-env=MEADOW_SILO_FINGERPRINT={}",
        fingerprint(&roots)
    );
}
