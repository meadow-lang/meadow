//! `meadow test` — build a package and run its `@test` functions.
//!
//! A test is `@test fun name u = …`. The runner calls each with `()`, in
//! declaration order, sharing one load of the program: top-level definitions are
//! evaluated once, as they are for `meadow run`.
//!
//! A test fails by performing `Std.Test.fail` — which `Std.Test`'s `assert` /
//! `assertEq` do for you. With no handler in the way the runtime turns that into
//! a stopped test carrying the message, so a failure reads like any other runtime
//! error and one failing test does not stop the others.

use crate::runtime::{self, Engine};
use crate::status;
use crate::workspace::Selection;
use crate::{Resolved, pipeline};
use meadow_compiler::intern::InternedString;
use std::path::Path;

pub struct Options {
    /// Package directory (or a single `.mw` file).
    pub path: std::path::PathBuf,
    /// Run only tests whose name contains this -- or, with `exact`, is it.
    ///
    /// The name is the qualified one, `Module.test`, so a bare name still
    /// matches as a substring of it.
    pub filter: Option<String>,
    /// Require the whole name to equal `filter`.
    ///
    /// What running *one* test needs: `parse` is contained in `parseInt`, and
    /// in a package where two modules each declare `works` only the qualified
    /// `A.works` picks one. The editor's Test lens always asks this way.
    pub exact: bool,
    /// Also run the standard library's own tests.
    pub std: bool,
    /// Which workspace members to test -- see [`Selection::select`]. Only the
    /// selected packages' tests run, not their dependencies'.
    pub packages: Selection,
    pub profile: Resolved,
    /// Which machine runs them. The bytecode VM by default; `--cek` for the
    /// specification.
    pub engine: Engine,
    /// How many tests run at once. `None` is [`runtime::test_threads`]: one a
    /// core, unless `MEADOW_TEST_THREADS` says otherwise.
    pub threads: Option<usize>,
    /// Write what tests print as they print it, rather than keeping it to show
    /// beside the ones that fail.
    pub no_capture: bool,
    /// Compile the tests with the native backend (`--runtime silo`) rather than
    /// run them on `engine`.
    pub native: bool,
}

/// Returns `true` if everything that ran passed.
pub fn run(opts: &Options) -> Result<bool, String> {
    // `--std` is a request to test the standard library, and there need not be a
    // package to hang it off — `meadow test --std` from anywhere should work.
    // Without this, the default path of `.` would sweep every `.mw` file below
    // the working directory into one package and fail in a hundred ways.
    // `@cfg(test)` holds while testing.
    let mut options = opts.profile.options;
    options.cfg.test = true;
    let started = std::time::Instant::now();
    let (linked, tested) = if opts.std && !is_package(Path::new(&opts.path)) {
        let (packages, diags) = crate::stdlib::std_packages(options);
        for d in &diags {
            status::error(format!("{}: {}", d.filename, d.msg));
        }
        if !diags.is_empty() {
            return Err("the standard library did not compile".into());
        }
        (crate::linker::Linker::link(packages), Vec::new())
    } else {
        let selected = opts.packages.select(&opts.path)?;
        let paths: Vec<&Path> = selected.paths.iter().map(|p| p.as_path()).collect();
        let (out, names) = pipeline::build_together(&paths, options);
        for d in &out.diagnostics {
            status::diagnostic(d);
        }
        let Some(linked) = out.linked else {
            return Err("could not build the package".into());
        };
        if !out.diagnostics.is_empty() {
            return Err("build failed".into());
        }
        (linked, names)
    };

    // The packages asked for, and not what they depend on -- the standard
    // library least of all, which carries tests of its own that a package
    // build should not be made to wait on unless they were asked for.
    //
    // Several packages' tests are told apart by the package's name in front,
    // which makes the name the `use` path of the test: `util.Parse.works`.
    let qualify = tested.len() > 1;
    let cases: Vec<_> = linked
        .tests
        .iter()
        .filter(|t| tested.contains(&t.package) || (opts.std && &*t.package == "Std"))
        .map(|t| {
            let name = if qualify && &*t.package != "Std" {
                InternedString::from(format!("{}.{}", t.package, t.name))
            } else {
                t.name
            };
            (name, t.var)
        })
        .filter(|(name, _)| {
            opts.filter.as_ref().is_none_or(|f| {
                let name = name.to_string();
                if opts.exact {
                    name == *f
                } else {
                    name.contains(f.as_str())
                }
            })
        })
        .collect();

    let total = cases.len();
    // As cargo closes a build before it runs anything.
    status::status(
        "Finished",
        format!(
            "`test` profile [{}, {}] in {}",
            opts.profile.opt().name(),
            match opts.engine {
                Engine::Cek => "cek",
                Engine::Vm => "vm",
                Engine::Jit => "jit",
            },
            status::elapsed(started.elapsed())
        ),
    );
    status::status(
        "Running",
        format!("{total} test{}", if total == 1 { "" } else { "s" }),
    );
    println!("running {total} test{}", if total == 1 { "" } else { "s" });
    if total == 0 {
        println!();
        println!(
            "test result: {}. 0 passed; 0 failed",
            status::paint("ok", "32")
        );
        return Ok(true);
    }

    let vars: Vec<_> = cases.iter().map(|(_, var)| *var).collect();
    let opt = opts.profile.options.opt;
    // Each test is reported the moment it finishes, with a bar saying how far
    // through the run is and what it is on. A thousand tests otherwise say
    // nothing at all until the last one is done.
    //
    // Tests run side by side, as `cargo test`'s do, so they are reported in the
    // order they finish and the bar says how many are done rather than which
    // one is running. What a test prints is kept, and shown with it if it
    // fails: written as it came, it would land among the other tests' lines.
    let bar = std::sync::Mutex::new(status::Testing::new(total));
    let names: Vec<String> = cases.iter().map(|(name, _)| name.to_string()).collect();
    let report = |i: usize, told: &runtime::Told| {
        let name = names.get(i).map(String::as_str).unwrap_or("?");
        let mut bar = bar.lock().unwrap_or_else(|p| p.into_inner());
        bar.step(told.result.is_ok());
        status::say(&match told.result {
            Ok(_) => format!("test {name} ... {}", status::paint("ok", "32")),
            Err(_) => format!("test {name} ... {}", status::paint("FAILED", "31")),
        });
    };
    let threads = opts.threads.unwrap_or_else(runtime::test_threads);
    let results = if opts.native {
        runtime::run_tests_native(&linked.program, &vars, opt, threads, &report)?
    } else {
        runtime::run_tests_parallel(
            &linked.program,
            &vars,
            opts.engine,
            opt,
            threads,
            !opts.no_capture,
            &report,
        )?
    };
    drop(bar);

    // In declaration order, whatever order they finished in.
    let mut failures = Vec::new();
    for ((name, _), told) in cases.iter().zip(&results) {
        if let Err(msg) = &told.result {
            // The message alone: a failed assertion is not a "runtime error".
            failures.push((*name, msg.clone(), told.output.clone()));
        }
    }

    if !failures.is_empty() {
        println!();
        println!("failures:");
        println!();
        for (name, msg, output) in &failures {
            println!("---- {name} ----");
            println!("{msg}");
            if !output.is_empty() {
                println!("---- {name} output ----");
                print!("{output}");
                if !output.ends_with('\n') {
                    println!();
                }
            }
            println!();
        }
    }

    let passed = total - failures.len();
    println!();
    println!(
        "test result: {}. {passed} passed; {} failed",
        if failures.is_empty() {
            status::paint("ok", "32")
        } else {
            status::paint("FAILED", "31")
        },
        failures.len()
    );
    Ok(failures.is_empty())
}

/// Whether `path` names something the build system would recognise: a single
/// `.mw` file, or a directory with a manifest or a `src/`.
fn is_package(path: &Path) -> bool {
    if path.extension().is_some_and(|e| e == "mw") {
        return true;
    }
    path.join("Meadow.toml").is_file() || path.join("src").is_dir()
}

/// Link a set of packages without building from disk — used by the integration
/// tests, which assemble a package in memory. `package` selects whose tests to
/// run (`"Std"` for the standard library's own).
pub fn run_linked_in(
    linked: crate::linker::LinkedProgram,
    package: &str,
    engine: Engine,
    opt: meadow_compiler::OptLevel,
) -> Result<Vec<(String, Option<String>)>, String> {
    let cases: Vec<_> = linked
        .tests
        .iter()
        .filter(|t| &*t.package == package)
        .collect();
    let vars: Vec<_> = cases.iter().map(|t| t.var).collect();
    let results = runtime::run_tests(&linked.program, &vars, engine, opt)?;
    Ok(cases
        .iter()
        .zip(results)
        .map(|(c, r)| (c.name.to_string(), r.err()))
        .collect())
}

/// Every test that is not the standard library's.
pub fn run_linked(
    linked: crate::linker::LinkedProgram,
    engine: Engine,
    opt: meadow_compiler::OptLevel,
) -> Result<Vec<(String, Option<String>)>, String> {
    let cases: Vec<_> = linked
        .tests
        .iter()
        .filter(|t| &*t.package != "Std")
        .collect();
    let vars: Vec<_> = cases.iter().map(|t| t.var).collect();
    let results = runtime::run_tests(&linked.program, &vars, engine, opt)?;
    Ok(cases
        .iter()
        .zip(results)
        .map(|(c, r)| (c.name.to_string(), r.err()))
        .collect())
}
