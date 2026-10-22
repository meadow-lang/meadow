//! **Silo's interpreter**: a program of AxCut, run on Silo's runtime without
//! compiling it (`docs/RUNTIME-MERGE.md`, phase 5).
//!
//! ```text
//! meadow-silo-vm PROGRAM.image [ARGS...]
//! meadow-silo-vm TESTS.image N
//! ```
//!
//! An image of tests runs the one numbered `N`, as a test executable does.
//!
//! `meadow` writes the image (`meadow_axcut::image`): the program made linear
//! (`meadow_axcut::linear`, the same pass Silo's compiler runs, done once when
//! the image is written), and whether its memory is counted or traced. This
//! reads it, compiles each block to slot-numbered code (`interp`), and runs it
//! on the runtime a compiled program links: the same heap, collector,
//! scheduler, segments, primitives and natives. Nothing goes near LLVM, so a
//! program starts as soon as it is read.
//!
//! `MEADOW_VM_STATS` reports how long the image took to read and load, and
//! what loading made of it; `MEADOW_VM_FRAMES`, each frame left to be built
//! at run time rather than run as the native call it continues.
//!
//! The runtime expects an emitted module beside it, defining a few constants
//! and tables and two ways back into the program's code. This executable
//! defines them instead -- the constants set from the program before it runs,
//! the tables given to the runtime whole (`meadow_silo::show::install`), and
//! the ways back into the interpreter (`interp`).

mod interp;

use meadow_silo::heap::Word;

// --- what an emitted module defines --------------------------------------
//
// Set before the runtime reads any of them: see `interp::load`.

#[unsafe(no_mangle)]
pub static mut meadow_threaded: u8 = 0;
#[unsafe(no_mangle)]
pub static mut meadow_cycles: u8 = 0;
#[unsafe(no_mangle)]
pub static mut meadow_gc: u8 = 0;
#[unsafe(no_mangle)]
pub static mut meadow_spill_words: i64 = 256;
#[unsafe(no_mangle)]
pub static mut meadow_result_desc: i64 = meadow_rt::desc::ANY;

// The runtime reads its tables from these only when none were installed,
// which here they always are.
#[unsafe(no_mangle)]
pub static meadow_ctor_names: [usize; 0] = [];
#[unsafe(no_mangle)]
pub static meadow_ctor_fields: [usize; 0] = [];
#[unsafe(no_mangle)]
pub static meadow_ctor_count: i64 = 0;
#[unsafe(no_mangle)]
pub static meadow_role_names: [usize; 0] = [];
#[unsafe(no_mangle)]
pub static meadow_sym_names: [usize; 0] = [];
#[unsafe(no_mangle)]
pub static meadow_sym_ranks: [i64; 0] = [];
#[unsafe(no_mangle)]
pub static meadow_sym_count: i64 = 0;

/// Method 0 of the closure `obj`, given `arg`: how the runtime runs the code
/// after a stack-segment primitive.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_invoke1(obj: Word, arg: Word) -> Word {
    interp::vm().invoke(obj, 0, &[arg])
}

/// A thread's function `obj`, applied to `arg` with the evidence `ev`: how the
/// scheduler starts a spawned thread.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_apply(obj: Word, arg: Word, ev: Word) -> Word {
    interp::vm().invoke(obj, 0, &[arg, 0, ev])
}

/// The program's entry point: what `meadow_run` starts.
extern "C" fn entry() -> Word {
    interp::vm().run_entry()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: meadow-silo-vm PROGRAM.image [ARGS...]");
        std::process::exit(2);
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {path}: {e}");
            std::process::exit(2);
        }
    };
    // The program borrows from these for as long as it runs.
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let began = std::time::Instant::now();
    let decoded = meadow_axcut::image::decode(bytes);
    if std::env::var_os("MEADOW_VM_STATS").is_some() {
        eprintln!("vm: {} bytes decoded in {:?}", bytes.len(), began.elapsed());
    }
    let image = match decoded {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {path}: {e}");
            std::process::exit(2);
        }
    };
    // What `Process.argv` answers: what followed the image.
    meadow_rt::args::set(args[2..].to_vec());
    if let Err(e) = interp::load(image) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    if let Some(n) = interp::vm().tests() {
        let Some(i) = args
            .get(2)
            .and_then(|a| a.parse::<usize>().ok())
            .filter(|i| *i < n)
        else {
            eprintln!("usage: a test's number, below {n}");
            std::process::exit(2);
        };
        interp::choose(i);
        std::process::exit(meadow_silo::run_one_test(entry));
    }
    // Safety: `entry` is the program's entry point, as an emitted module's
    // `meadow_entry` would be.
    let status = unsafe { meadow_silo::meadow_run(entry, 0, std::ptr::null()) };
    std::process::exit(status)
}
