//! Which machine runs the program.
//!
//! Two back ends, one interface. The **bytecode VM** is the default: `core` is
//! lowered to the AxCut IR, compiled to registers, and run by `meadow_glade`. The
//! **CEK machine** (`meadow_eval`) is still there behind `--cek`, and is still
//! the specification — if the two disagree the CEK is right and the VM has a
//! bug, so the flag is the first thing to reach for when a program does
//! something inexplicable.
//!
//! Both are held to that by tests rather than by intention: `tests/vm.rs` runs
//! the whole standard library through both and requires every answer to match.
//!
//! # What the VM cannot do yet
//!
//! Discharge an *unhandled* `Fs`, `Process`, `Random` or `Time` operation. The
//! CEK reaches the real world for those; the VM reports them and stops. A
//! program that handles its effects — which is every test in the standard
//! library, by design — is unaffected, and one that reads a file is not, so that
//! is what `--cek` is for until the natives are ported.

use meadow_compiler::{OptLevel, core};
use std::fmt;

/// Which machine to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    /// Bytecode, registers and a copying collector.
    #[default]
    Vm,
    /// The CEK abstract machine — small, slow, and the definition of what a
    /// program means.
    Cek,
    /// The VM, compiling each block to machine code once it has run often
    /// enough -- see `meadow_glade::jit`.
    Jit,
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Engine::Vm => "vm",
            Engine::Cek => "cek",
            Engine::Jit => "jit",
        })
    }
}

/// No practical limit. The bound exists so a test harness can ask for one.
const UNBOUNDED: u64 = u64::MAX;

/// Evaluate the program's entry point, and render the result.
///
/// A string rather than a value because the two machines have different value
/// types, and every caller here wanted the rendering — which the two produce
/// identically on purpose.
pub fn run(program: &core::Program, engine: Engine, opt: OptLevel) -> Result<String, String> {
    match engine {
        Engine::Cek => meadow_eval::run(program)
            .map(|v| v.to_string())
            .map_err(|e| e.msg),
        Engine::Vm | Engine::Jit => {
            let image = compile(program, opt)?;
            let native = native(&image, engine, opt)?;
            let entry = image.entry.ok_or("program has no entry point")?;
            meadow_glade::sched::run_native(
                &image,
                native.as_ref(),
                entry,
                UNBOUNDED,
                meadow_glade::sched::workers(),
            )
            .result
            .map_err(|e| e.msg)
        }
    }
}

/// How long [`run_timed`] spent getting a program ready, and running it.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// Lowering and code generation: `core` to bytecode. `None` on the CEK
    /// machine, which runs `core` as it is.
    pub compile: Option<std::time::Duration>,
    /// The program itself, from its first instruction to its answer -- JIT
    /// compilation included, since that happens as it runs.
    pub run: std::time::Duration,
}

/// [`run`], saying how long each part took.
pub fn run_timed(
    program: &core::Program,
    engine: Engine,
    opt: OptLevel,
) -> (Result<String, String>, Timings) {
    use std::time::Instant;
    match engine {
        Engine::Cek => {
            let start = Instant::now();
            let result = run(program, engine, opt);
            let run = start.elapsed();
            (result, Timings { compile: None, run })
        }
        Engine::Vm | Engine::Jit => {
            let start = Instant::now();
            let image = compile(program, opt);
            let compile = Some(start.elapsed());
            let start = Instant::now();
            let result = image.and_then(|image| {
                let native = native(&image, engine, opt)?;
                let entry = image.entry.ok_or("program has no entry point")?;
                meadow_glade::sched::run_native(
                    &image,
                    native.as_ref(),
                    entry,
                    UNBOUNDED,
                    meadow_glade::sched::workers(),
                )
                .result
                .map_err(|e| e.msg)
            });
            let run = start.elapsed();
            (result, Timings { compile, run })
        }
    }
}

/// A duration the way `Std.Time.formatNanos` writes one: three significant
/// figures in the unit that reads best -- `742ns`, `1.23µs`, `45.7ms`, `3.21s`
/// -- then `2m 05.3s`, `1h 02m 03s` and `3d 04h 05m`.
pub fn format_duration(d: std::time::Duration) -> String {
    let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    let three = |unit: u64| {
        let hundredths = (ns * 100 + unit / 2) / unit;
        if hundredths < 1000 {
            return format!("{}.{:02}", hundredths / 100, hundredths % 100);
        }
        let tenths = (ns * 10 + unit / 2) / unit;
        if tenths < 1000 {
            return format!("{}.{}", tenths / 10, tenths % 10);
        }
        ((ns + unit / 2) / unit).to_string()
    };
    match ns {
        0..1_000 => format!("{ns}ns"),
        1_000..999_500 => format!("{}µs", three(1_000)),
        999_500..999_500_000 => format!("{}ms", three(1_000_000)),
        999_500_000..59_950_000_000 => format!("{}s", three(1_000_000_000)),
        59_950_000_000..3_599_950_000_000 => {
            let tenths = (ns + 50_000_000) / 100_000_000;
            format!(
                "{}m {:02}.{}s",
                tenths / 600,
                tenths % 600 / 10,
                tenths % 10
            )
        }
        3_599_950_000_000..86_399_500_000_000 => {
            let secs = (ns + 500_000_000) / 1_000_000_000;
            format!(
                "{}h {:02}m {:02}s",
                secs / 3600,
                secs % 3600 / 60,
                secs % 60
            )
        }
        _ => {
            let mins = ns / 60_000_000_000 + u64::from(ns % 60_000_000_000 >= 30_000_000_000);
            format!(
                "{}d {:02}h {:02}m",
                mins / 1440,
                mins % 1440 / 60,
                mins % 60
            )
        }
    }
}

/// With [`Engine::Jit`], a JIT for `image`, compiling what gets hot -- after
/// `MEADOW_JIT_THRESHOLD` entries, or the default.
pub fn native(
    image: &meadow_bytecode::Program,
    engine: Engine,
    opt: OptLevel,
) -> Result<Option<meadow_glade::jit::Native<'_>>, String> {
    native_at(
        image,
        engine,
        meadow_glade::jit::Native::threshold_from_env(),
        opt,
    )
}

/// [`native`], compiling a block the `threshold`th time it is entered.
fn native_at(
    image: &meadow_bytecode::Program,
    engine: Engine,
    threshold: u32,
    opt: OptLevel,
) -> Result<Option<meadow_glade::jit::Native<'_>>, String> {
    match engine {
        Engine::Jit => meadow_glade::jit::Native::jit(image, threshold, opt).map(Some),
        _ => Ok(None),
    }
}

/// What the bytecode VM's collector did during a run.
#[derive(Debug, Clone)]
pub struct GcStats {
    /// Green threads that finished, `main` included. Each had a heap of its
    /// own, and the rest of these figures are summed over them.
    pub threads: u64,
    pub collections: u64,
    /// Heap slots allocated and copied by collections, in total.
    pub allocated: u64,
    pub copied: u64,
    pub gc_nanos: u64,
    pub run_nanos: u64,
    /// The main thread's heap -- nursery and old generation -- and slots held in
    /// compact regions, at the end.
    pub heap_slots: usize,
    pub region_slots: usize,
    /// Every pause, across every thread.
    pub pauses: meadow_glade::pauses::Pauses,
    /// Slots promoted to old generations, marking cycles, and the time marking
    /// took on any thread -- mostly not the program's.
    pub promoted: u64,
    pub cycles: u64,
    pub mark_nanos: u64,
    /// Slots and blocks moved out of sparse old blocks.
    pub evacuated: u64,
    pub evacuated_blocks: u64,
    /// The main thread's old generation at the end, and what its last marking
    /// cycle found alive in it.
    pub old_slots: usize,
    pub old_live: usize,
}

impl fmt::Display for GcStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = |slots: u64| human_bytes(slots * meadow_glade::heap::SLOT_BYTES as u64);
        let share = if self.run_nanos == 0 {
            0.0
        } else {
            100.0 * self.gc_nanos as f64 / self.run_nanos as f64
        };
        write!(
            f,
            "gc: {} collections across {} thread{}, {:.1} ms paused ({share:.1}% of {:.1} ms)\n\
             gc: {} allocated, {} copied in nurseries, {} promoted\n\
             gc: {} marking cycle{}, {:.1} ms marking, {} evacuated from {} block{}\n\
             gc: old generation {}, {} alive when last marked\n\
             gc: heap {}, compact regions {}\n\
             gc: pauses {}",
            self.collections,
            self.threads,
            if self.threads == 1 { "" } else { "s" },
            self.gc_nanos as f64 / 1e6,
            self.run_nanos as f64 / 1e6,
            bytes(self.allocated),
            bytes(self.copied),
            bytes(self.promoted),
            self.cycles,
            if self.cycles == 1 { "" } else { "s" },
            self.mark_nanos as f64 / 1e6,
            bytes(self.evacuated),
            self.evacuated_blocks,
            if self.evacuated_blocks == 1 { "" } else { "s" },
            bytes(self.old_slots as u64),
            bytes(self.old_live as u64),
            bytes(self.heap_slots as u64),
            bytes(self.region_slots as u64),
            pauses(&self.pauses),
        )
    }
}

/// The pause percentiles worth knowing when latency matters: the typical one,
/// the tail, and the worst.
fn pauses(p: &meadow_glade::pauses::Pauses) -> String {
    if p.count == 0 {
        return "none".to_string();
    }
    format!(
        "p50 {}, p99 {}, p99.9 {}, max {}",
        human_nanos(p.percentile(0.5)),
        human_nanos(p.percentile(0.99)),
        human_nanos(p.percentile(0.999)),
        human_nanos(p.max_nanos),
    )
}

fn human_nanos(n: u64) -> String {
    match n {
        0..=999 => format!("{n} ns"),
        1_000..=999_999 => format!("{:.1} us", n as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.2} ms", n as f64 / 1e6),
        _ => format!("{:.2} s", n as f64 / 1e9),
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut x = n as f64;
    let mut unit = 0;
    while x >= 1024.0 && unit + 1 < UNITS.len() {
        x /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{x:.1} {}", UNITS[unit])
    }
}

/// [`run`], and on the VM what its collector did. The CEK reference-counts,
/// so it has nothing to report.
pub fn run_with_stats(
    program: &core::Program,
    engine: Engine,
    opt: OptLevel,
) -> (Result<String, String>, Option<GcStats>) {
    match engine {
        Engine::Cek => (run(program, engine, opt), None),
        Engine::Vm | Engine::Jit => match compile(program, opt) {
            Ok(image) => match native(&image, engine, opt) {
                Ok(jit) => run_image_with_stats(&image, jit.as_ref()),
                Err(e) => (Err(e), None),
            },
            Err(e) => (Err(e), None),
        },
    }
}

/// Run a compiled image on the VM, and say what its collector did.
pub fn run_image_with_stats(
    image: &meadow_bytecode::Program,
    native: Option<&meadow_glade::jit::Native>,
) -> (Result<String, String>, Option<GcStats>) {
    let Some(entry) = image.entry else {
        return (Err("program has no entry point".to_string()), None);
    };
    let started = std::time::Instant::now();
    let outcome = meadow_glade::sched::run_native(
        image,
        native,
        entry,
        UNBOUNDED,
        meadow_glade::sched::workers(),
    );
    let s = outcome.stats;
    let stats = GcStats {
        threads: s.threads,
        collections: s.collections,
        allocated: s.allocated,
        copied: s.copied,
        gc_nanos: s.gc_nanos,
        run_nanos: started.elapsed().as_nanos() as u64,
        heap_slots: s.heap_slots,
        region_slots: s.region_slots,
        pauses: s.pauses,
        promoted: s.promoted,
        cycles: s.cycles,
        mark_nanos: s.mark_nanos,
        evacuated: s.evacuated,
        evacuated_blocks: s.evacuated_blocks,
        old_slots: s.old_slots,
        old_live: s.old_live,
    };
    (outcome.result.map_err(|e| e.msg), Some(stats))
}

/// Call each of `tests` with `()`, in order, sharing one build.
///
/// Each gets its own result: a failing test does not stop the rest.
pub fn run_tests(
    program: &core::Program,
    tests: &[core::Var],
    engine: Engine,
    opt: OptLevel,
) -> Result<Vec<Result<String, String>>, String> {
    run_tests_watched(program, tests, engine, opt, &mut |_, _| {})
}

/// [`run_tests`], telling `each` about a result as it lands: which test, by
/// position, and how it went.
///
/// What a progress bar is made of, and what lets a run report a test the
/// moment it finishes rather than the whole lot at the end.
pub fn run_tests_watched(
    program: &core::Program,
    tests: &[core::Var],
    engine: Engine,
    opt: OptLevel,
    each: &mut dyn FnMut(usize, &Result<String, String>),
) -> Result<Vec<Result<String, String>>, String> {
    let threshold = meadow_glade::jit::Native::threshold_from_env();
    run_tests_jit_at_watched(program, tests, engine, opt, threshold, each)
}

/// [`run_tests`], with the JIT compiling a block the `threshold`th time it is
/// entered -- 1 to run all the code that runs natively.
pub fn run_tests_jit_at(
    program: &core::Program,
    tests: &[core::Var],
    engine: Engine,
    opt: OptLevel,
    threshold: u32,
) -> Result<Vec<Result<String, String>>, String> {
    run_tests_jit_at_watched(program, tests, engine, opt, threshold, &mut |_, _| {})
}

/// [`run_tests_jit_at`], watched. See [`run_tests_watched`].
pub fn run_tests_jit_at_watched(
    program: &core::Program,
    tests: &[core::Var],
    engine: Engine,
    opt: OptLevel,
    threshold: u32,
    each: &mut dyn FnMut(usize, &Result<String, String>),
) -> Result<Vec<Result<String, String>>, String> {
    match engine {
        Engine::Cek => Ok(meadow_eval::run_tests_watched(program, tests, &mut |i, r| {
            let told = match r {
                Ok(v) => Ok(v.to_string()),
                Err(e) => Err(e.msg.clone()),
            };
            each(i, &told);
        })
        .map_err(|e| e.msg)?
        .into_iter()
        .map(|r| r.map(|v| v.to_string()).map_err(|e| e.msg))
        .collect()),

        Engine::Vm | Engine::Jit => {
            let (image, base) = test_image(program, tests, opt)?;
            let jit = native_at(&image, engine, threshold, opt)?;

            Ok((0..tests.len())
                .map(|i| {
                    // `lower_program` labels definitions by position and
                    // `entries` is indexed by label, so the nth extra definition
                    // is entry `base + n`.
                    let Some(&entry) = image.entries.get(base + i) else {
                        return Err("a test has no entry point".to_string());
                    };
                    let out = meadow_glade::sched::run_native(
                        &image,
                        jit.as_ref(),
                        entry,
                        UNBOUNDED,
                        meadow_glade::sched::workers(),
                    )
                    .result
                    .map_err(|e| e.msg);
                    each(i, &out);
                    out
                })
                .collect())
        }
    }
}

/// How a test went: its value rendered, or its failure; and what it printed,
/// if that was kept rather than written.
pub struct Told {
    pub result: Result<String, String>,
    pub output: String,
}

/// How many tests to run at once when nobody says: `MEADOW_TEST_THREADS`, or
/// one per core -- as `cargo test` does, and `RUST_TEST_THREADS` for it.
pub fn test_threads() -> usize {
    std::env::var("MEADOW_TEST_THREADS")
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// Call each of `tests` with `()`, `threads` of them at a time, sharing one
/// build -- and one compilation to native code, which every thread adds to.
///
/// Each test is a run of its own: its own main thread, its own heaps, its own
/// `TVar`s and channels. So tests share nothing inside the language, and what
/// they can still collide on is what any two processes can: a file of the
/// same name, a port, the working directory. `threads = 1` is the answer to
/// that, as it is for `cargo test`.
///
/// With `capture`, what a test prints is kept and handed back with its result
/// instead of being written among the others'. `each` hears of a test as it
/// finishes, from whichever thread ran it.
pub fn run_tests_parallel(
    program: &core::Program,
    tests: &[core::Var],
    engine: Engine,
    opt: OptLevel,
    threads: usize,
    capture: bool,
    each: &(dyn Fn(usize, &Told) + Sync),
) -> Result<Vec<Told>, String> {
    let threads = threads.clamp(1, tests.len().max(1));
    if engine == Engine::Cek {
        return Ok(
            meadow_eval::run_tests_parallel(program, tests, threads, capture, &|i, t| {
                each(
                    i,
                    &Told {
                        result: t.result.clone(),
                        output: t.output.clone(),
                    },
                )
            })
            .map_err(|e| e.msg)?
            .into_iter()
            .map(|t| Told {
                result: t.result,
                output: t.output,
            })
            .collect(),
        );
    }
    let (image, base) = test_image(program, tests, opt)?;
    let threshold = meadow_glade::jit::Native::threshold_from_env();
    let jit = native_at(&image, engine, threshold, opt)?;
    // A test that spawns threads gets its share of the cores, and never fewer
    // than two workers: one that expects to run alongside another should.
    let workers = (meadow_glade::sched::workers() / threads).max(2);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<Told>>> =
        tests.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= tests.len() {
                        return;
                    }
                    let told = match image.entries.get(base + i) {
                        None => Told {
                            result: Err("a test has no entry point".to_string()),
                            output: String::new(),
                        },
                        Some(&entry) => {
                            let kept = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
                            let outcome = if capture {
                                let kept = kept.clone();
                                meadow_glade::sched::run_captured(
                                    &image,
                                    jit.as_ref(),
                                    entry,
                                    UNBOUNDED,
                                    workers,
                                    std::sync::Arc::new(move |s: &str| {
                                        kept.lock().unwrap_or_else(|p| p.into_inner()).push_str(s)
                                    }),
                                )
                            } else {
                                meadow_glade::sched::run_native(
                                    &image,
                                    jit.as_ref(),
                                    entry,
                                    UNBOUNDED,
                                    workers,
                                )
                            };
                            let output = std::mem::take(
                                &mut *kept.lock().unwrap_or_else(|p| p.into_inner()),
                            );
                            Told {
                                result: outcome.result.map_err(|e| e.msg),
                                output,
                            }
                        }
                    };
                    each(i, &told);
                    *slots[i].lock().unwrap_or_else(|p| p.into_inner()) = Some(told);
                }
            });
        }
    });
    Ok(slots
        .into_iter()
        .map(|s| {
            s.into_inner()
                .unwrap_or_else(|p| p.into_inner())
                .unwrap_or_else(|| Told {
                    result: Err("the test did not run".into()),
                    output: String::new(),
                })
        })
        .collect())
}

/// `program` with one extra definition per test, whose body applies it to
/// `()`, compiled; and the entry the first of them has. They are compiled with
/// everything else, so the image is built once and each test is simply a
/// different place to start.
/// The test executable of `program`, whose entries are `labels`, compiled
/// and linked in `dir`.
fn link_tests(
    program: &meadow_seq::Program,
    labels: &[meadow_seq::Label],
    opt: OptLevel,
    dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let units = meadow_llvm::compile_tests_with(
        program,
        labels,
        meadow_llvm::UNIT,
        meadow_llvm::CallConv::host(),
        crate::aot::gc(),
    )
    .map_err(|e| e.msg)?;
    let target = crate::aot::Target::host()?.with_runtime(crate::aot::Runtime::Silo);
    let runtime = crate::aot::runtimes(target)?
        .into_iter()
        .next()
        .ok_or("no Silo runtime library")?;
    let modules = crate::aot::write_units(dir, "tests", &units)?;
    let exe = dir.join(format!("tests{}", std::env::consts::EXE_SUFFIX));
    crate::aot::clang_link(&modules, &[], &runtime, &exe, opt, target, false)?;
    Ok(exe)
}

/// Run `tests` compiled by the native backend (`docs/SILO.md`): one executable
/// holding every test, built once, and run once per test -- a process each,
/// `threads` at a time -- with the test's number as its argument. A test
/// passes if its process does; what it printed is its output, and what it
/// said on stderr the failure.
pub fn run_tests_native(
    program: &core::Program,
    tests: &[core::Var],
    opt: OptLevel,
    threads: usize,
    each: &(dyn Fn(usize, &Told) + Sync),
) -> Result<Vec<Told>, String> {
    let (whole, base) = with_tests(program, tests);
    let lowered = meadow_seq::lower_program(&whole, opt);
    if !lowered.unsupported.is_empty() {
        return Err(format!(
            "the back end cannot translate {:?} yet",
            lowered.unsupported
        ));
    }
    let labels: Vec<meadow_seq::Label> = (0..tests.len())
        .map(|i| meadow_seq::Label((base + i) as u32))
        .collect();
    let dir = std::env::temp_dir()
        .join("meadow-silo-tests")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Interpreted: an image of the tests, which Silo's interpreter runs one of
    // by its number, as the executable would.
    let (exe, before) = if crate::aot::interpret() {
        let image = dir.join("tests.image");
        crate::aot::write_image(&image, lowered.program, labels)?;
        (crate::aot::interpreter()?, Some(image))
    } else {
        (link_tests(&lowered.program, &labels, opt, &dir)?, None)
    };

    let threads = threads.clamp(1, tests.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<Told>>> =
        tests.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= tests.len() {
                        return;
                    }
                    let told = match std::process::Command::new(&exe)
                        .args(before.iter())
                        .arg(i.to_string())
                        .output()
                    {
                        Err(e) => Told {
                            result: Err(format!("could not run the test executable: {e}")),
                            output: String::new(),
                        },
                        Ok(out) => {
                            let output = String::from_utf8_lossy(&out.stdout).into_owned();
                            let said = String::from_utf8_lossy(&out.stderr).trim_end().to_string();
                            Told {
                                result: if out.status.success() {
                                    Ok("()".to_string())
                                } else if said.is_empty() {
                                    Err(format!("the test exited with {}", out.status))
                                } else {
                                    Err(said)
                                },
                                output,
                            }
                        }
                    };
                    each(i, &told);
                    *slots[i].lock().unwrap_or_else(|p| p.into_inner()) = Some(told);
                }
            });
        }
    });
    // Kept on request, to run a test again by hand: `tests <number>`.
    if std::env::var_os("MEADOW_SILO_KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(slots
        .into_iter()
        .map(|s| {
            s.into_inner()
                .unwrap_or_else(|p| p.into_inner())
                .unwrap_or(Told {
                    result: Err("the test did not run".into()),
                    output: String::new(),
                })
        })
        .collect())
}

/// `program` with a definition per test that calls it with `()` -- the
/// tests' entry points -- after the program's own, whose number is answered.
fn with_tests(program: &core::Program, tests: &[core::Var]) -> (core::Program, usize) {
    let base = program.defs.len();
    let mut defs = program.defs.clone();
    for (i, var) in tests.iter().enumerate() {
        defs.push(core::Def {
            var: meadow_compiler::hir::VarId::synthetic(i as u32),
            name: "<test>".into(),
            module: Default::default(),
            poly: program.result_of_calling(*var),
            term: core::Term::App(
                std::sync::Arc::new(core::Term::Var(*var)),
                std::sync::Arc::new(core::Term::Lit(core::Lit::Unit)),
            ),
        });
    }
    (
        core::Program {
            defs,
            entry: program.entry,
            ctor_fields: program.ctor_fields.clone(),
            variants: program.variants.clone(),
            origins: Default::default(),
        },
        base,
    )
}

fn test_image(
    program: &core::Program,
    tests: &[core::Var],
    opt: OptLevel,
) -> Result<(meadow_bytecode::Program, usize), String> {
    let base = program.defs.len();
    let mut defs = program.defs.clone();
    for (i, var) in tests.iter().enumerate() {
        defs.push(core::Def {
            // Invented after compilation, so it belongs to no unit —
            // indexed rather than counted, so the same tests over the
            // same program always produce the same image.
            var: meadow_compiler::hir::VarId::synthetic(i as u32),
            name: "<test>".into(),
            module: Default::default(),
            // Whatever the test returns: what calling it has.
            poly: program.result_of_calling(*var),
            term: core::Term::App(
                std::sync::Arc::new(core::Term::Var(*var)),
                std::sync::Arc::new(core::Term::Lit(core::Lit::Unit)),
            ),
        });
    }
    let whole = core::Program {
        defs,
        entry: program.entry,
        ctor_fields: program.ctor_fields.clone(),
        variants: program.variants.clone(),
        origins: Default::default(),
    };
    Ok((compile(&whole, opt)?, base))
}

/// `core` → AxCut → bytecode.
/// [`compile`], also recording the debug info a profile's frames are named by.
///
/// The code is the same instruction for instruction -- see
/// [`meadow_codegen::compile_with_debug_info`] -- so a profile of this is a
/// profile of the program that would have run without it.
pub fn compile_for_profile(
    program: &core::Program,
    opt: OptLevel,
) -> Result<meadow_bytecode::Program, String> {
    let lowered = meadow_seq::lower_program(program, opt);
    if !lowered.unsupported.is_empty() {
        return Err(format!(
            "the back end cannot translate {:?} yet; try --cek",
            lowered.unsupported
        ));
    }
    meadow_codegen::compile_with_debug_info(&lowered.program).map_err(|e| e.msg)
}

/// Run `image` taking a sample every `every` blocks entered, and hand back what
/// the samples came to along with the result.
pub fn run_image_sampled(
    image: &meadow_bytecode::Program,
    native: Option<&meadow_glade::jit::Native>,
    sampling: meadow_glade::sched::Sampling,
) -> (
    Result<String, String>,
    Option<meadow_glade::profile::Profile>,
    meadow_glade::sched::Stats,
) {
    let Some(entry) = image.entry else {
        return (
            Err("program has no entry point".to_string()),
            None,
            Default::default(),
        );
    };
    let outcome = meadow_glade::sched::run_sampled(
        image,
        native,
        entry,
        UNBOUNDED,
        meadow_glade::sched::workers(),
        Some(sampling),
    );
    (
        outcome.result.map_err(|e| e.msg),
        outcome.profile,
        outcome.stats,
    )
}

pub fn compile(program: &core::Program, opt: OptLevel) -> Result<meadow_bytecode::Program, String> {
    let lowered = meadow_seq::lower_program(program, opt);
    if !lowered.unsupported.is_empty() {
        return Err(format!(
            "the back end cannot translate {:?} yet; try --cek",
            lowered.unsupported
        ));
    }
    // What the back ends are handed, for whoever is writing one:
    // `MEADOW_DUMP_AXCUT=1` prints every definition's AxCut to stderr.
    if std::env::var_os("MEADOW_DUMP_AXCUT").is_some() {
        for d in &lowered.program.defs {
            eprintln!("-- {} (L{})\n{}", d.name, d.label.0, d.block);
        }
    }
    meadow_codegen::compile(&lowered.program).map_err(|e| e.msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The same cases as `Std.Time`'s own tests of `formatNanos`, so the REPL's
    /// timings and a program's read alike.
    #[test]
    fn durations_format_as_std_time_does() {
        let cases: &[(u64, &str)] = &[
            (0, "0ns"),
            (742, "742ns"),
            (999, "999ns"),
            (1234, "1.23µs"),
            (9995, "10.0µs"),
            (999_499, "999µs"),
            (999_500, "1.00ms"),
            (45_678_000, "45.7ms"),
            (321_000_000, "321ms"),
            (3_210_000_000, "3.21s"),
            (59_950_000_000, "1m 00.0s"),
            (125_300_000_000, "2m 05.3s"),
            (3_723_000_000_000, "1h 02m 03s"),
            (273_900_000_000_000, "3d 04h 05m"),
        ];
        for &(ns, want) in cases {
            assert_eq!(format_duration(Duration::from_nanos(ns)), want, "{ns}ns");
        }
    }
}
