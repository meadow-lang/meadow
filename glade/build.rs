//! The fingerprint of the sources that decide the ABI between the code
//! `meadow-llvm` emits and this runtime, as `MEADOW_GLADE_FINGERPRINT`: that
//! crate's, the LIR's it prints, and this one's. The library exports a symbol named after it, and
//! the emitted module refers to that symbol, so a program links only with a
//! runtime built from the sources its compiler was.
//!
//! `compiler/meadow-llvm/build.rs` hashes the same trees with the same
//! function, `compiler/fingerprint.rs`.

use std::path::Path;

include!("../compiler/fingerprint.rs");

fn main() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let roots = [
        here.join("../compiler/meadow-llvm/src"),
        here.join("../compiler/meadow-lir/src"),
        here.join("src"),
    ];
    // Every tree it reads, the code generator's among them, outside this
    // package: cargo watches only the package unless it is told.
    for root in &roots {
        println!("cargo:rerun-if-changed={}", root.display());
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../compiler/fingerprint.rs");
    println!(
        "cargo:rustc-env=MEADOW_GLADE_FINGERPRINT={}",
        fingerprint(&roots)
    );
}
