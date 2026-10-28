//! **Silo's interpreter**: a program of AxCut, run on Silo's runtime without
//! compiling it (`docs/RUNTIME-MERGE.md`, phase 5).
//!
//! ```text
//! meadow-silo-vm PROGRAM.image [ARGS...]
//! meadow-silo-vm TESTS.image N
//! meadow-silo-vm --dis PROGRAM.image
//! meadow-silo-vm --debug PORT PROGRAM.image
//! meadow-silo-vm --watch PROGRAM.image [ARGS...]
//! meadow-silo-vm --answer FILE PROGRAM.image [ARGS...]
//! meadow-silo-vm --profile FILE [--profile-every N | --profile-hz HZ] PROGRAM.image
//! ```
//!
//! An image of tests runs the one numbered `N`, as a test executable does.
//! `--watch` runs the program and, whenever the image is written anew, loads
//! the new one into it as it runs (`interp::Vm::reload`): what `meadow run
//! --watch` writes. Each new build says on stderr what it replaced, or why it
//! was refused. `--answer FILE` writes what the program answers to `FILE`,
//! shown as `meadow` shows a value, rather than printing it after what the
//! program printed: what the REPL reads an entry's value from. `--profile
//! FILE` samples where the program spends itself and writes folded stacks to
//! `FILE` when it ends (`interp::profile`): by time, a thousand times a
//! second or `--profile-hz`, or every `--profile-every` instructions.
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
//! at run time rather than run as the native call it continues;
//! `MEADOW_VM_WIDE`, lays every instruction down in its wide form; and under
//! a debugger, `MEADOW_VM_TRACE_FILE` writes each boundary a step looks at,
//! and why it went on.
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

/// How the program's answer is described, under a debugger: which reports
/// it itself, so the runtime is told the answer is `()` and prints nothing.
static ANSWER: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(meadow_rt::desc::UNIT);

/// Where `--answer` writes the program's answer.
static ANSWER_TO: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// [`entry`], its answer written where `--answer` says: shown whatever it
/// is, `()` too, since whoever asked for it reads it as a value.
extern "C" fn entry_answered() -> Word {
    let v = interp::vm().run_entry();
    let d = ANSWER.load(std::sync::atomic::Ordering::Relaxed);
    let shown = meadow_silo::show::show(v, d);
    if let Some(to) = ANSWER_TO.get()
        && let Err(e) = std::fs::write(to, shown)
    {
        eprintln!("error: {}: {e}", to.display());
    }
    v
}

/// [`entry`], under a debugger.
extern "C" fn entry_debugged() -> Word {
    let v = interp::vm().run_entry();
    let d = ANSWER.load(std::sync::atomic::Ordering::Relaxed);
    let shown = (d != meadow_rt::desc::UNIT).then(|| meadow_silo::show::show(v, d));
    interp::debug::exited(shown);
    v
}

/// Load the image at `path` into the running program whenever it is written
/// anew, from a thread of its own.
fn watch(path: std::path::PathBuf) {
    let written = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut seen = written(&path);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let now = written(&path);
            if now.is_none() || now == seen {
                continue;
            }
            seen = now;
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            // What the new code is made of, for as long as it may run.
            let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
            match meadow_axcut::image::decode(bytes).and_then(|i| interp::vm().reload(i)) {
                Ok(what) => eprintln!("reloaded {}: {what}", path.display()),
                Err(e) => eprintln!("not reloaded: {e}"),
            }
        }
    });
}

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    // `--dis`: the program's code, as the VM runs it, and nothing run.
    let mut dis = false;
    // `--debug PORT`: run under the debugger listening there.
    let mut debug = None;
    // `--watch`: load the image again whenever it is written anew.
    let mut watching = false;
    // `--profile FILE`: where to write a profile, and how to take it.
    let mut profile_to: Option<std::path::PathBuf> = None;
    let mut how = interp::profile::How {
        every: None,
        hz: 1000,
        depth: 64,
    };
    loop {
        match args.get(1).map(String::as_str) {
            Some("--dis") => {
                dis = true;
                args.remove(1);
            }
            Some("--watch") => {
                watching = true;
                args.remove(1);
            }
            Some(flag @ ("--profile" | "--profile-every" | "--profile-hz")) => {
                let flag = flag.to_string();
                let Some(value) = args.get(2).cloned() else {
                    eprintln!("usage: meadow-silo-vm {flag} VALUE PROGRAM.image");
                    std::process::exit(2);
                };
                let number = || {
                    value.parse::<u64>().unwrap_or_else(|_| {
                        eprintln!("error: {flag} takes a number, not `{value}`");
                        std::process::exit(2);
                    })
                };
                match flag.as_str() {
                    "--profile" => profile_to = Some(std::path::PathBuf::from(&value)),
                    "--profile-every" => how.every = Some(number()),
                    _ => how.hz = number(),
                }
                args.drain(1..3);
            }
            Some("--answer") => {
                let Some(to) = args.get(2).cloned() else {
                    eprintln!("usage: meadow-silo-vm --answer FILE PROGRAM.image");
                    std::process::exit(2);
                };
                let _ = ANSWER_TO.set(std::path::PathBuf::from(to));
                args.drain(1..3);
            }
            Some("--debug") => {
                let port = args.get(2).and_then(|p| p.parse::<u16>().ok());
                args.drain(1..3.min(args.len()));
                match port {
                    Some(p) => debug = Some(p),
                    None => {
                        eprintln!("usage: meadow-silo-vm --debug PORT PROGRAM.image");
                        std::process::exit(2);
                    }
                }
            }
            _ => break,
        }
    }
    if debug.is_some() {
        interp::debugging();
    }
    if profile_to.is_some() {
        interp::profile::start(how);
    }
    let Some(path) = args.get(1) else {
        eprintln!("usage: meadow-silo-vm [--dis | --debug PORT | --watch] PROGRAM.image [ARGS...]");
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
    if dis {
        print!("{}", interp::vm().disassemble());
        return;
    }
    if watching {
        watch(std::path::PathBuf::from(path));
    }
    if ANSWER_TO.get().is_some() && interp::vm().tests().is_none() {
        // Safety: before the runtime starts, which reads it.
        unsafe {
            ANSWER.store(meadow_result_desc, std::sync::atomic::Ordering::Relaxed);
            meadow_result_desc = meadow_rt::desc::UNIT;
        }
        // Safety: as below.
        let status = unsafe { meadow_silo::meadow_run(entry_answered, 0, std::ptr::null()) };
        ended(status, profile_to.as_deref())
    }
    if let Some(port) = debug {
        // Safety: before the runtime starts, which reads it.
        unsafe {
            ANSWER.store(meadow_result_desc, std::sync::atomic::Ordering::Relaxed);
            meadow_result_desc = meadow_rt::desc::UNIT;
        }
        if let Err(e) = interp::debug::start(port) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        // Safety: as below.
        let status = unsafe { meadow_silo::meadow_run(entry_debugged, 0, std::ptr::null()) };
        std::process::exit(status)
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
    ended(status, profile_to.as_deref())
}

/// The program is over, as `status` says: its profile written, if one was
/// taken, and the process ended.
fn ended(status: i32, profile_to: Option<&std::path::Path>) -> ! {
    interp::report_steps();
    if let Some(to) = profile_to {
        let (folded, said) = interp::profile::finish();
        eprintln!("{said}");
        if let Err(e) = std::fs::write(to, folded) {
            eprintln!("profile: {}: {e}", to.display());
        }
    }
    std::process::exit(status)
}
