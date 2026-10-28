//! A fingerprint of the sources that decide the native ABI -- the layout of
//! `Vm` and `Heap` the generated code reads, the calls it makes, the image it
//! is handed -- as `MEADOW_GLADE_FINGERPRINT`.
//!
//! The runtime library exports a symbol named after it, and the `main` of every
//! program compiled ahead of time refers to that symbol (see
//! `codegen::object::main_c`), so linking a program against a runtime library
//! from other sources fails at link time instead of crashing when it runs.
//!
//! The hashing is `fingerprint.rs`, which `meadow-llvm` and `aot` -- the other
//! backend and its runtime -- share for their own fingerprint.

use std::path::Path;

include!("../compiler/fingerprint.rs");

fn main() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let roots = [
        here.join("src"),
        here.join("../compiler/meadow-bytecode/src"),
        here.join("../compiler/meadow-rt/src"),
    ];
    println!(
        "cargo:rustc-env=MEADOW_GLADE_FINGERPRINT={}",
        fingerprint(&roots)
    );
}
