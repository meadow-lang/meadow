//! Which machine runs the program.
//!
//! The **merged runtime's VM** (`silo/vm`) runs it: `core` lowered to AxCut,
//! written as an image, and run by the VM in a process of its own --
//! interpreted, or with its JIT compiling what runs often. The **CEK
//! machine** (`meadow_eval`) is there behind `--cek`, and is the
//! specification: if the two disagree the CEK is right and the VM has a bug,
//! so the flag is the first thing to reach for when a program does something
//! inexplicable. `tests/differential.rs` holds the two to that.

use meadow_compiler::{OptLevel, core};
use std::fmt;

/// Which machine to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    /// The VM, interpreting.
    #[default]
    Vm,
    /// The CEK abstract machine — small, slow, and the definition of what a
    /// program means.
    Cek,
    /// The VM, compiling what runs often to machine code as it goes -- see
    /// `silo/vm/src/interp/jit.rs`.
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

impl Engine {
    /// What the VM is told, in its environment, to run as this engine.
    pub fn env(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Engine::Jit => &[("MEADOW_JIT", "1")],
            Engine::Vm | Engine::Cek => &[],
        }
    }
}

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
        Engine::Vm | Engine::Jit => run_on_vm_with(program, opt, engine.env()),
    }
}

/// How long [`run_timed`] spent getting a program ready, and running it.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// Lowering, and writing the image: `None` where it is not told apart
    /// from the run.
    pub compile: Option<std::time::Duration>,
    /// The program itself, from its first instruction to its answer -- JIT
    /// compilation included, since that happens as it runs.
    pub run: std::time::Duration,
}

/// `program`, lowered to AxCut and run by Silo's interpreter -- the merged
/// runtime's VM -- in a process of its own, with what it prints going where
/// this process's does: its answer, shown, or why it failed. What the REPL's
/// `:silo` runs an entry on.
pub fn run_on_vm(program: &core::Program, opt: OptLevel) -> (Result<String, String>, Timings) {
    use std::time::Instant;
    let start = Instant::now();
    let dir = std::env::temp_dir().join(format!("meadow-repl-{}", std::process::id()));
    let image = dir.join("entry.image");
    let answer = dir.join("entry.answer");
    let lowered = meadow_seq::lower_program(program, opt);
    let written = if !lowered.unsupported.is_empty() {
        Err(format!(
            "the back end cannot translate {:?} yet",
            lowered.unsupported
        ))
    } else {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))
            .and_then(|()| {
                // A stack that grows, as an edit-run loop's should.
                let config = meadow_llvm::Config {
                    stack: meadow_llvm::Stack::Growable,
                    ..crate::aot::config()
                };
                crate::aot::write_image_with(&image, lowered.program, Vec::new(), config)
            })
    };
    let compile = Some(start.elapsed());
    let start = Instant::now();
    let result = written.and_then(|()| {
        let _ = std::fs::remove_file(&answer);
        let status = std::process::Command::new(crate::aot::interpreter()?)
            .arg("--answer")
            .arg(&answer)
            .arg(&image)
            .status()
            .map_err(|e| format!("could not run Silo's interpreter: {e}"))?;
        match std::fs::read_to_string(&answer) {
            Ok(shown) if status.success() => Ok(shown),
            // What went wrong it has said already, on stderr.
            _ => Err(format!(
                "the program failed ({})",
                status
                    .code()
                    .map_or("killed".to_string(), |c| format!("exit status {c}"))
            )),
        }
    });
    (
        result,
        Timings {
            compile,
            run: start.elapsed(),
        },
    )
}

/// `program` run on the merged runtime's VM, lowered at `opt`, with `env`
/// set for it: what its entry answers, as the VM shows it, or why it did
/// not -- what the VM said on standard error, as the CEK machine says it, or
/// how it ended if it said nothing. What the
/// program printed is left out. Each run has a file of its own, so that runs
/// at once, as a test's are, do not meet.
pub fn run_on_vm_with(
    program: &core::Program,
    opt: OptLevel,
    env: &[(&str, &str)],
) -> Result<String, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("meadow-vm-runs-{}", std::process::id()));
    let n = RUNS.fetch_add(1, Ordering::Relaxed);
    let image = dir.join(format!("run-{n}.image"));
    let answer = dir.join(format!("run-{n}.answer"));
    let lowered = meadow_seq::lower_program(program, opt);
    if !lowered.unsupported.is_empty() {
        return Err(format!(
            "the back end cannot translate {:?} yet",
            lowered.unsupported
        ));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let config = meadow_llvm::Config {
        stack: meadow_llvm::Stack::Growable,
        ..crate::aot::config()
    };
    crate::aot::write_image_with(&image, lowered.program, Vec::new(), config)?;
    let out = std::process::Command::new(crate::aot::interpreter()?)
        .envs(env.iter().copied())
        .arg("--answer")
        .arg(&answer)
        .arg(&image)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run Silo's interpreter: {e}"));
    let shown = std::fs::read_to_string(&answer);
    let _ = std::fs::remove_file(&image);
    let _ = std::fs::remove_file(&answer);
    let out = out?;
    match shown {
        Ok(shown) if out.status.success() => Ok(shown),
        _ => {
            let said = String::from_utf8_lossy(&out.stderr);
            let said = said.trim_end();
            Err(if said.is_empty() {
                format!(
                    "the program failed ({})",
                    out.status
                        .code()
                        .map_or("killed".to_string(), |c| format!("exit status {c}"))
                )
            } else {
                said.to_string()
            })
        }
    }
}

/// [`run_on_vm`], in a session: `program` loaded into the VM the session has
/// kept, as a reload, and its entry run there -- or, if there is none yet, or
/// it cannot take this build, or the last run ended it, run on a new one,
/// kept for the session from then on (REPL-4).
pub fn run_in_session(
    session: &mut Option<crate::served::Served>,
    program: &core::Program,
    opt: OptLevel,
) -> (Result<String, String>, Timings) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;
    static LINES: AtomicUsize = AtomicUsize::new(0);
    let start = Instant::now();
    let dir = std::env::temp_dir().join(format!("meadow-repl-{}", std::process::id()));
    // An image a line: the VM reads each as it is told to load it.
    let image = dir.join(format!(
        "line-{}.image",
        LINES.fetch_add(1, Ordering::Relaxed)
    ));
    // Built to be loaded into the program running, as an interpreted build is.
    let lowered = meadow_seq::lower_program_with(program, opt, true);
    let written = if !lowered.unsupported.is_empty() {
        Err(format!(
            "the back end cannot translate {:?} yet",
            lowered.unsupported
        ))
    } else {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))
            .and_then(|()| {
                // A stack that grows, as an edit-run loop's should.
                let config = meadow_llvm::Config {
                    stack: meadow_llvm::Stack::Growable,
                    ..crate::aot::config()
                };
                crate::aot::write_image_with(&image, lowered.program, Vec::new(), config)
            })
    };
    let compile = Some(start.elapsed());
    let start = Instant::now();
    let result = written.and_then(|()| {
        // Into the VM the session has, if it takes it.
        let loaded = match session {
            Some(s) => s.load(&image).is_ok(),
            None => false,
        };
        if !loaded {
            *session = None;
            *session = Some(crate::served::Served::start(
                &image,
                true,
                Engine::Jit.env(),
            )?);
        }
        let s = session.as_mut().expect("started");
        match s.run(None, None, None) {
            Ok((shown, _)) => Ok(shown),
            Err(e) => {
                *session = None;
                Err(e.said())
            }
        }
    });
    let _ = std::fs::remove_file(&image);
    (
        result,
        Timings {
            compile,
            run: start.elapsed(),
        },
    )
}

/// [`run`], saying how long each part took.
pub fn run_timed(
    program: &core::Program,
    engine: Engine,
    opt: OptLevel,
) -> (Result<String, String>, Timings) {
    let start = std::time::Instant::now();
    let result = run(program, engine, opt);
    let run = start.elapsed();
    (result, Timings { compile: None, run })
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
    each: &mut (dyn FnMut(usize, &Result<String, String>) + Send),
) -> Result<Vec<Result<String, String>>, String> {
    if engine == Engine::Cek {
        return Ok(meadow_eval::run_tests_watched(program, tests, &mut |i, r| {
            let told = match r {
                Ok(v) => Ok(v.to_string()),
                Err(e) => Err(e.msg.clone()),
            };
            each(i, &told);
        })
        .map_err(|e| e.msg)?
        .into_iter()
        .map(|r| r.map(|v| v.to_string()).map_err(|e| e.msg))
        .collect());
    }
    let each = std::sync::Mutex::new(each);
    let told = run_tests_on(program, tests, opt, On::Vm(engine.env()), 1, &|i, t| {
        (each.lock().unwrap_or_else(|p| p.into_inner()))(i, &t.result)
    })?;
    Ok(told.into_iter().map(|t| t.result).collect())
}

/// [`run_tests`], with the JIT compiling what is entered `threshold` times
/// -- 0 to compile everything the first time it is entered.
pub fn run_tests_jit_at(
    program: &core::Program,
    tests: &[core::Var],
    opt: OptLevel,
    threshold: u32,
) -> Result<Vec<Result<String, String>>, String> {
    let threshold = threshold.to_string();
    let env = [
        ("MEADOW_JIT", "1"),
        ("MEADOW_JIT_THRESHOLD", threshold.as_str()),
        ("MEADOW_JIT_TIER2", threshold.as_str()),
    ];
    let told = run_tests_on(program, tests, opt, On::Vm(&env), 1, &|_, _| {})?;
    Ok(told.into_iter().map(|t| t.result).collect())
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
/// build.
///
/// Each test is a run of its own, in a process of its own: its own main
/// thread, its own heaps, its own `TVar`s and channels. So tests share
/// nothing inside the language, and what they can still collide on is what
/// any two processes can: a file of the same name, a port, the working
/// directory. `threads = 1` is the answer to that, as it is for `cargo test`.
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
    let told = run_tests_on(
        program,
        tests,
        opt,
        On::Vm(engine.env()),
        threads,
        &|i, t| {
            // Not kept: written as it comes, a test's all together.
            if !capture && !t.output.is_empty() {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(t.output.as_bytes());
                let _ = out.flush();
            }
            each(i, t)
        },
    )?;
    Ok(told
        .into_iter()
        .map(|t| Told {
            output: if capture { t.output } else { String::new() },
            ..t
        })
        .collect())
}

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
        crate::aot::config(),
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

/// Run `tests` as the build says: on the VM if it is interpreted -- with its
/// JIT, if `MEADOW_JIT` says -- and compiled by the native backend
/// (`docs/SILO.md`) if not. See [`run_tests_on`].
pub fn run_tests_native(
    program: &core::Program,
    tests: &[core::Var],
    opt: OptLevel,
    threads: usize,
    each: &(dyn Fn(usize, &Told) + Sync),
) -> Result<Vec<Told>, String> {
    let on = if crate::aot::interpret() {
        On::Vm(&[])
    } else {
        On::Native
    };
    run_tests_on(program, tests, opt, on, threads, each)
}

/// Where [`run_tests_on`] runs tests: on the VM, with these set in its
/// environment, or as an executable of the native backend's.
#[derive(Clone, Copy)]
pub enum On<'a> {
    Vm(&'a [(&'a str, &'a str)]),
    Native,
}

/// Run `tests`, built once -- an image of them all, or one executable -- and
/// run once per test, a process each, `threads` at a time, with the test's
/// number as its argument. A test passes if its process does; what it printed
/// is its output, and what it said on stderr the failure.
pub fn run_tests_on(
    program: &core::Program,
    tests: &[core::Var],
    opt: OptLevel,
    on: On<'_>,
    threads: usize,
    each: &(dyn Fn(usize, &Told) + Sync),
) -> Result<Vec<Told>, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
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
    // A directory a run, so that runs at once -- a test harness's -- do not
    // meet.
    let dir = std::env::temp_dir().join("meadow-silo-tests").join(format!(
        "{}-{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // An image of the tests, which the VM runs one of by its number, as the
    // executable would.
    let (exe, before, env): (
        std::path::PathBuf,
        Option<std::path::PathBuf>,
        &[(&str, &str)],
    ) = match on {
        On::Vm(env) => {
            let image = dir.join("tests.image");
            let config = meadow_llvm::Config {
                stack: meadow_llvm::Stack::Growable,
                ..crate::aot::config()
            };
            crate::aot::write_image_with(&image, lowered.program, labels, config)?;
            (crate::aot::interpreter()?, Some(image), env)
        }
        On::Native => (link_tests(&lowered.program, &labels, opt, &dir)?, None, &[]),
    };

    let threads = threads.clamp(1, tests.len().max(1));
    let next = AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<Told>>> =
        tests.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= tests.len() {
                        return;
                    }
                    let told = match std::process::Command::new(&exe)
                        .envs(env.iter().copied())
                        .args(before.iter())
                        .arg(i.to_string())
                        .stdin(std::process::Stdio::null())
                        .output()
                    {
                        Err(e) => Told {
                            result: Err(format!("could not run the tests: {e}")),
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
            migrations: Vec::new(),
        },
        base,
    )
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
