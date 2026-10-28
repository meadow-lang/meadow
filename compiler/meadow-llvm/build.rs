//! The fingerprint of the sources that decide the ABI between the code this
//! crate emits and the runtime it links against, as `MEADOW_SILO_FINGERPRINT`:
//! this crate's, the LIR's it prints, and the runtime's. The runtime exports a symbol named after it
//! (`silo/build.rs` hashes the same trees with the same function,
//! `compiler/fingerprint.rs`), and the emitted module refers to that symbol, so a
//! program links only with a runtime built from the same sources.

use std::path::Path;

include!("../fingerprint.rs");

fn main() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let roots = [
        here.join("src"),
        here.join("../meadow-lir/src"),
        here.join("../../silo/src"),
    ];
    println!(
        "cargo:rustc-env=MEADOW_SILO_FINGERPRINT={}",
        fingerprint(&roots)
    );
}
