//! The fingerprint of the sources that decide the ABI between the code this
//! crate emits and the runtime it links against, as `MEADOW_GLADE_FINGERPRINT`:
//! this crate's, the LIR's it prints, and the runtime's. The runtime exports a
//! symbol named after it (`glade/build.rs` hashes the same trees with the same function,
//! `compiler/fingerprint.rs`), and the emitted module refers to that symbol, so a
//! program links only with a runtime built from the same sources.

use std::path::Path;

include!("../fingerprint.rs");

fn main() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let roots = [
        here.join("src"),
        here.join("../meadow-lir/src"),
        here.join("../../glade/src"),
    ];
    // Every tree it reads: the runtime's is outside this package, which is
    // all cargo would watch otherwise, and a fingerprint stale by a change
    // there is an executable that does not link.
    for root in &roots {
        println!("cargo:rerun-if-changed={}", root.display());
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../fingerprint.rs");
    println!(
        "cargo:rustc-env=MEADOW_GLADE_FINGERPRINT={}",
        fingerprint(&roots)
    );
}
