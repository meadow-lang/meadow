//! **The runtime of a program compiled by `meadow-llvm`.**
//!
//! `meadow build --runtime silo` compiles a program all the way to machine code
//! with LLVM, straight from AxCut: no bytecode, no interpreter, no collector.
//! Memory is counted by reference, as in the AxCut paper -- Schuster, Müller,
//! Ostermann and Brachthäuser, _Compiling Classical Sequent Calculus to Stock
//! Hardware: The Duality of Compilation_, OOPSLA 2025,
//! <https://doi.org/10.1145/3720507> -- and the program runs on the native
//! stack. `docs/SILO.md` is the design.
//!
//! This library is what such a program links against: the heap ([`heap`]),
//! the primitives the emitted code does not do inline ([`prims`]), printing
//! ([`show`]), and the entry point, [`meadow_run`]. The emitted module refers
//! to [`FINGERPRINT`], so it links only with a runtime built from the sources
//! its compiler was.
//!
//! Every `extern "C"` function here is called by emitted code, and takes and
//! answers words: references are counted as `docs/SILO.md` says, a primitive
//! borrows its arguments, and what one answers is owned by the caller.

pub mod ctx;
pub mod cycles;
pub mod heap;
pub mod memory;
pub mod native;
pub mod parcel;
pub mod prims;
pub mod profile;
pub mod region;
pub mod sched;
pub mod segments;
pub mod shadow;
pub mod show;
pub mod trace;
pub mod value;

use ctx::Ctx;
use heap::Word;
use std::ffi::c_char;

/// Defined only by a library of this runtime built from the sources the
/// compiler was: see `build.rs`, and `meadow_llvm::runtime_symbol`.
#[unsafe(export_name = concat!("meadow_silo_", env!("MEADOW_SILO_FINGERPRINT")))]
pub static FINGERPRINT: u8 = 0;

unsafe extern "C" {
    /// The descriptor of what the program's entry point answers.
    static meadow_result_desc: i64;
}

// The emitted code's hottest entries, each handed the running thread's
// context -- which a program that can spawn keeps in a register, so that
// nothing is looked up per block -- or `null` from one that cannot, whose one
// context is a static. See `meadow_llvm::emit`'s `CTX`.

/// A block of `words` words for a `let` or a `new`, count zero.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_acquire(c: *mut ctx::Ctx, words: i64) -> Word {
    heap::acquire_in(ctx::given(c), words as usize)
}

/// The last reference to the block at `v` was erased: it is block `v` and its
/// count is zero, which the emitted code has checked. Its fields are erased
/// later, a few at a time: see [`heap`].
#[unsafe(no_mangle)]
pub extern "C" fn meadow_free(c: *mut ctx::Ctx, v: Word) {
    heap::died_in(ctx::given(c), v);
}

/// The block at `v` had its fields moved out by its last reference: it is
/// reusable.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_clean(c: *mut ctx::Ctx, v: Word) {
    heap::clean_in(ctx::given(c), v);
}

/// Fail with the message at `msg`: an `error`, a failed assertion, a division
/// by zero.
///
/// # Safety
///
/// `msg` must point at `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_fail(msg: *const u8, len: i64) -> ! {
    // Safety: the caller's.
    let text = unsafe { std::slice::from_raw_parts(msg, len as usize) };
    fail(&String::from_utf8_lossy(text))
}

/// Stop the program with `msg` on stderr and a failing status -- or, in a
/// thread other than `main`, just that thread: see [`sched::fail_thread`].
pub fn fail(msg: &str) -> ! {
    sched::fail_thread(msg);
    use std::io::Write;
    let _ = std::io::stdout().flush();
    eprintln!("{msg}");
    std::process::exit(1)
}

/// Field `i` of the data, record or array at `v`, shared.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_field(v: Word, i: i64) -> Word {
    let i = i as usize;
    if !heap::is_block(v) || i >= heap::len(v) {
        fail(&format!("field {i} of a value that has none"));
    }
    let x = heap::field(v, i);
    heap::share(x, heap::field_desc(v, i));
    x
}

/// The string literal at `text`: made once, kept for the rest of the run, and
/// shared with each use.
///
/// # Safety
///
/// `text` must point at `len` bytes that live as long as the program.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_text(text: *const u8, len: i64) -> Word {
    // Safety: the running thread's context -- each thread makes its own, in
    // its own heap.
    let lits = unsafe { &mut (*crate::ctx::get()).literals };
    let v = match lits.get(&(text as usize)) {
        Some(v) => *v,
        None => {
            // Safety: the caller's.
            let bytes = unsafe { std::slice::from_raw_parts(text, len as usize) };
            let s = heap::string(bytes);
            prims::keep(s, meadow_rt::desc::REF);
            // Safety: as above; `keep` and `string` do not touch the literals.
            unsafe { (*crate::ctx::get()).literals.insert(text as usize, s) };
            s
        }
    };
    heap::share(v, meadow_rt::desc::REF);
    v
}

/// The running thread's spill area: where the emitted code puts the
/// arguments of a call that do not fit in registers. See
/// `meadow_llvm::emit::spill`.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_spill_area() -> *mut Word {
    // Safety: the running thread's context.
    unsafe { (*crate::ctx::get()).spill.as_mut_ptr() }
}

/// A `BigInt` literal.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_bigint(n: i64) -> Word {
    value::new_bigint(&num_bigint::BigInt::from(n))
}

/// Run the program's entry point and answer what it returned, with the
/// context its heap is in -- current on this OS thread, for reading the
/// answer.
///
/// A program that never spawns needs none of the scheduler: it runs here, on
/// this thread's stack, with one context and no thread-locals at all. See
/// `ctx::threaded`.
fn run_main(entry: extern "C" fn() -> Word) -> (Box<Ctx>, Word) {
    if ctx::threaded() {
        let (mut c, v) = sched::main(move || entry());
        ctx::set(&mut *c);
        return (c, v);
    }
    let mut c = Ctx::new(0);
    ctx::set(&mut *c);
    let v = entry();
    (c, v)
}

/// Make the runtime ready for a call from C into a library of Meadow's: what
/// every exported function does first (`meadow_llvm::emit`'s `export_fn`).
/// The first call sets the runtime up, as a program's start does, and the
/// first from each OS thread gives that thread a context of its own, which
/// stays for its later calls: a host calls a plugin's systems from one
/// thread, again and again, and what the library keeps between calls lives
/// in that context's heap.
///
/// A library whose code spawns threads is not ready this way yet: its
/// scheduler is not started.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_export_enter() {
    static READY: std::sync::Once = std::sync::Once::new();
    READY.call_once(|| {
        allocator();
        memory::start();
    });
    thread_local! {
        static HAS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    HAS.with(|has| {
        if !has.get() {
            // Kept for the life of the thread: the library's values live in
            // its heap from one call to the next.
            let c: &'static mut Ctx = Box::leak(Ctx::new(0));
            ctx::set(c);
            has.set(true);
        }
    });
}

/// Run `body` of `env`, a call C made into the library, so that a failure in
/// it ends the call and not the process: 0 when it returned, 1 when it
/// failed. The emitted C entry of each export calls it: see
/// `meadow_llvm::emit`'s `export_fn` and [`sched::export_run`].
#[unsafe(no_mangle)]
pub extern "C" fn meadow_export_run(
    body: extern "C" fn(*mut std::ffi::c_void),
    env: *mut std::ffi::c_void,
) -> i32 {
    sched::export_run(body, env)
}

/// Run one test of a test executable: the entry of `tests` numbered by the
/// first argument. A test passes by returning -- status 0 -- and fails by
/// failing, with its message on stderr, as `Test.fail` makes it.
///
/// # Safety
///
/// `tests` must be the emitted module's table of `n` entries, and `argv` the
/// process's arguments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_run_test(
    tests: *const extern "C" fn() -> Word,
    n: i64,
    _argc: i32,
    _argv: *const *const c_char,
) -> i32 {
    let which = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<usize>().ok())
        .filter(|i| *i < n as usize);
    let Some(i) = which else {
        eprintln!("usage: a test's number, below {n}");
        return 2;
    };
    // Safety: the caller's.
    run_one_test(unsafe { *tests.add(i) })
}

/// Run `entry` as a test: on a stack of its own, status 0 if it returns and 1
/// if it fails. What a test executable does with the test it was asked for,
/// and what Silo's interpreter does with one of an image's tests.
pub fn run_one_test(entry: extern "C" fn() -> Word) -> i32 {
    let ran = std::thread::Builder::new()
        .name("main".into())
        .stack_size(1 << 30)
        .spawn(move || {
            allocator();
            let (c, _) = run_main(entry);
            drop(c);
            use std::io::Write;
            let _ = std::io::stdout().flush();
        })
        .expect("the test's thread starts")
        .join();
    i32::from(ran.is_err())
}

/// Tell the C allocator, which every heap's chunks and every region's come
/// from, to give memory back when it is freed.
///
/// glibc's keeps an arena for every thread that allocates, and raises the
/// size from which it asks the system directly each time a block that big is
/// freed -- so after a while a megabyte freed is a megabyte kept, in the
/// arena of whichever thread freed it. A program of many threads that each
/// build something large and let it go, as a compiler's units are, was at
/// its end holding a third more than anything in it was using. With the
/// size fixed, a heap's chunks are always the system's and go back to it;
/// with two arenas, what is smaller is at least kept in one of two places.
fn allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::ffi::c_int;
        unsafe extern "C" {
            fn mallopt(param: c_int, value: c_int) -> c_int;
        }
        const M_MMAP_THRESHOLD: c_int = -3;
        const M_ARENA_MAX: c_int = -8;
        // Safety: two settings of the allocator, made before the program
        // has a second thread.
        unsafe {
            mallopt(M_MMAP_THRESHOLD, 128 * 1024);
            mallopt(M_ARENA_MAX, 2);
        }
    }
}

/// Run the program: its entry point, on a stack deep enough for the
/// recursion Meadow programs do, then its answer printed -- unless it is `()`
/// -- and a status for the process.
///
/// # Safety
///
/// `entry` must be the emitted module's `meadow_entry`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_run(
    entry: extern "C" fn() -> Word,
    _argc: i32,
    _argv: *const *const c_char,
) -> i32 {
    // Safety: the emitted module defines it.
    let d = unsafe { meadow_result_desc };
    let run = move || {
        // What `main` answers is in its heap, which stays current here.
        allocator();
        memory::start();
        heap::catch_faults();
        profile::start();
        let (mut main_ctx, v) = run_main(entry);
        profile::finish();
        let text = show::show(v, d);
        prims::report();
        cycles::report();
        heap::quarantined();
        heap::erase(v, d);
        if std::env::var_os("MEADOW_SILO_LEAKS").is_some() {
            // Every cycle collected and everything pending erased, so that
            // what is counted as left behind really is left behind.
            heap::settle();
            let (left, kinds) = heap::leaked(&prims::roots());
            eprintln!("aot: {left} blocks live at exit");
            eprintln!("aot: {} blocks acquired", heap::acquired());
            eprintln!("aot: {} segments live at exit", segments::live());
            // What `main` counted for itself goes into the regions' counts
            // first, as it would when its context goes.
            main_ctx.regions.settle();
            // And the answers of threads nobody can await any more, which
            // the scheduler lets go of when it next looks.
            sched::forget();
            eprintln!("aot: {} regions live at exit", region::live());
            for (refs, n, words) in region::census().iter().take(12) {
                eprintln!(
                    "aot: {n:>8}  regions with {refs} references, {} KB",
                    words / 128
                );
            }
            // What threads ended without erasing: their heaps went whole, and
            // these with them, still holding whatever they held outside.
            let ended = heap::ended_with();
            let total: u64 = ended.iter().map(|(_, n)| n).sum();
            if total > 0 {
                eprintln!("aot: {total} blocks held by threads when they ended");
                for ((k, m), n) in ended.iter().take(24) {
                    let what = match *k {
                        heap::DATA => show::ctor_name(*m as usize),
                        heap::CLOSURE => format!("closure, methods from {m}"),
                        heap::COMPACT => "a compact".to_string(),
                        heap::ARRAY => "array".to_string(),
                        heap::STRING => "string".to_string(),
                        heap::RECORD => "record".to_string(),
                        heap::CELL => "ref".to_string(),
                        k => format!("kind {k}"),
                    };
                    eprintln!("aot: {n:>8}  ended holding: {what}");
                }
            }
            if left > 0 {
                for (n, k, m) in kinds.iter().take(12) {
                    let what = match *k {
                        heap::DATA => show::ctor_name(*m as usize),
                        heap::CLOSURE => format!("closure, methods from {m}"),
                        k => format!("kind {k}"),
                    };
                    eprintln!("aot: {n:>8}  {what}");
                }
            }
        }
        text
    };
    // A native stack for non-tail recursion over a long list: reserved, and
    // committed only as it is touched.
    let answer = std::thread::Builder::new()
        .name("main".into())
        .stack_size(1 << 30)
        .spawn(run)
        .expect("the main thread starts")
        .join();
    let status = match answer {
        Ok(text) => {
            if text != "()" {
                println!("{text}");
            }
            0
        }
        Err(_) => 1,
    };
    // What the program wrote after its last newline is still in stdout's
    // buffer, and nothing else empties it: the process is the emitted
    // module's `main`, not Rust's, so Rust's cleanup at exit never runs.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    status
}
