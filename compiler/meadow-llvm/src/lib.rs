//! **AxCut to LLVM IR**: the backend of `meadow build --runtime silo`.
//!
//! A program is compiled all the way to machine code by LLVM and manages its
//! memory by reference counting, the discipline of the AxCut paper -- Schuster,
//! Müller, Ostermann and Brachthäuser, _Compiling Classical Sequent Calculus to
//! Stock Hardware: The Duality of Compilation_, OOPSLA 2025,
//! <https://doi.org/10.1145/3720507>. `docs/SILO.md` is the design; this crate
//! is its compiler half, and `aot/` in a checkout is the runtime it links
//! against.
//!
//! Two passes over `meadow_seq`'s AxCut:
//!
//! - [`linear`] makes it linear: every share and every erase written down.
//! - [`emit`] writes LLVM IR as text, for clang to compile and link.

pub mod emit;
pub mod linear;

use meadow_axcut::Program;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub msg: String,
}

impl From<linear::Error> for Error {
    fn from(e: linear::Error) -> Error {
        Error { msg: e.msg }
    }
}

impl From<emit::Error> for Error {
    fn from(e: emit::Error) -> Error {
        Error { msg: e.msg }
    }
}

/// The symbol a runtime library built from the sources this compiler was
/// defines, and that the emitted module refers to: so that a program links
/// with that runtime and no other. See `build.rs`.
/// How a program that can tie a knot has its cycles found, which is decided
/// when it is built: concurrent tracing (`silo/src/trace.rs`), unless trial
/// deletion (`silo/src/cycles.rs`) is asked for. `docs/SILO.md`, "Cycles".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Cycles {
    Trial,
    #[default]
    Tracing,
}

impl Cycles {
    /// What `@meadow_cycles` says of a program that can tie a knot.
    pub fn mode(self) -> u8 {
        match self {
            Cycles::Trial => 1,
            Cycles::Tracing => 2,
        }
    }

    /// `trial` or `tracing`, as a profile or `MEADOW_CYCLES` says it.
    pub fn named(name: &str) -> Option<Cycles> {
        match name {
            "trial" => Some(Cycles::Trial),
            "tracing" => Some(Cycles::Tracing),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Cycles::Trial => "trial",
            Cycles::Tracing => "tracing",
        }
    }
}

thread_local! {
    static CYCLES: std::cell::Cell<Cycles> = const { std::cell::Cell::new(Cycles::Tracing) };
}

/// The collector the programs this thread compiles get: see [`with_cycles`].
pub fn cycles() -> Cycles {
    CYCLES.with(std::cell::Cell::get)
}

/// Compile, in `f`, with `how` for the cycles: a setting of this thread's,
/// so that what compiles a program need not say it at every call, and two
/// threads compiling two programs do not share it.
pub fn with_cycles<T>(how: Cycles, f: impl FnOnce() -> T) -> T {
    let was = CYCLES.with(|c| c.replace(how));
    let out = f();
    CYCLES.with(|c| c.set(was));
    out
}

pub fn runtime_symbol() -> String {
    format!("meadow_silo_{}", fingerprint())
}

/// The fingerprint of the sources that decide the ABI between the emitted
/// code and the runtime: this crate's and the runtime's. See `build.rs`.
pub fn fingerprint() -> &'static str {
    env!("MEADOW_SILO_FINGERPRINT")
}

pub use emit::CallConv;

/// `program` as an LLVM module, as text, its functions in `conv`.
pub fn compile(program: &Program, conv: CallConv) -> Result<String, Error> {
    Ok(compile_split(program, usize::MAX, conv)?.remove(0).text)
}

/// The most of a program any of the modules [`compile_split`] makes holds:
/// small enough that LLVM optimizes it quickly.
pub const UNIT: usize = 2 << 20;

/// One of the LLVM modules a program is compiled as.
pub struct Unit {
    /// What it is of, for a file's name: the module of the program whose
    /// functions these are -- `Std.Collections.Vector`, and
    /// `Std.Collections.Vector.2` for the rest of one too big for a unit --
    /// or a number, for functions of no module. Empty for the first, which
    /// holds the tables and the entry points.
    pub name: String,
    /// The LLVM IR.
    pub text: String,
}

impl Unit {
    fn all(units: Vec<(String, String)>) -> Vec<Unit> {
        units
            .into_iter()
            .map(|(name, text)| Unit { name, text })
            .collect()
    }
}

/// `program` as LLVM modules to compile apart and link -- one for each module
/// of the program, and more for one with over `unit` bytes of functions: see
/// `emit::Module::units`. Their functions are in `conv`, which is the
/// target's: see [`CallConv`].
pub fn compile_split(program: &Program, unit: usize, conv: CallConv) -> Result<Vec<Unit>, Error> {
    if !program.exports.is_empty() {
        return compile_library(program, unit, conv);
    }
    let entry = program.entry.ok_or_else(|| Error {
        msg: "the program has no entry point".into(),
    })?;
    let mut module = emit::Module::new(program, conv);
    let blocks = linear::program(program, &[entry])?;
    module.name_labels(blocks.iter().map(|(i, label, _)| {
        let d = &program.defs[*i];
        (*label, d.module.to_string(), d.name.to_string())
    }));
    for (i, label, lb) in &blocks {
        module.in_module(
            &program.defs[*i].module.to_string(),
            &program.defs[*i].name.to_string(),
        );
        module.def(*label, lb).map_err(|e| Error {
            msg: format!("in {}: {}", program.defs[*i].name, e.msg),
        })?;
    }
    let result = program
        .results
        .get(&entry)
        .and_then(|r| r.desc())
        .unwrap_or(meadow_rt::desc::ANY);
    report_sizes(&module);
    Ok(Unit::all(module.text_split(
        entry,
        result,
        fingerprint(),
        unit,
    )))
}

/// `MEADOW_SIZES=N`: say the `N` definitions that came to the most LLVM IR,
/// and how much every module did -- see [`emit::Module::sizes`].
fn report_sizes(module: &emit::Module) {
    let Some(n) = std::env::var("MEADOW_SIZES")
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
    else {
        return;
    };
    let sizes = module.sizes();
    let mut by_module: std::collections::BTreeMap<&str, (usize, usize)> = Default::default();
    for (m, _, bytes, funs) in &sizes {
        let e = by_module.entry(m.as_str()).or_default();
        e.0 += bytes;
        e.1 += funs;
    }
    let mut modules: Vec<_> = by_module.into_iter().collect();
    modules.sort_by_key(|(_, (bytes, _))| std::cmp::Reverse(*bytes));
    eprintln!("{:>10} {:>8}  module", "IR KB", "funs");
    for (m, (bytes, funs)) in modules.iter().take(n) {
        eprintln!("{:>10.1} {funs:>8}  {m}", *bytes as f64 / 1024.0);
    }
    eprintln!("{:>10} {:>8}  definition", "IR KB", "funs");
    for (m, d, bytes, funs) in sizes.iter().take(n) {
        eprintln!("{:>10.1} {funs:>8}  {m}.{d}", *bytes as f64 / 1024.0);
    }
}

/// `program` as an LLVM module for a library: a C function for each of its
/// exports, which is all of it that is kept, and no `main`. In modules of
/// about `unit` bytes, as [`compile_split`] makes them.
pub fn compile_library(program: &Program, unit: usize, conv: CallConv) -> Result<Vec<Unit>, Error> {
    let roots: Vec<meadow_axcut::Label> = program.exports.iter().map(|e| e.label).collect();
    let mut module = emit::Module::new(program, conv);
    let blocks = linear::program(program, &roots)?;
    module.name_labels(blocks.iter().map(|(i, label, _)| {
        let d = &program.defs[*i];
        (*label, d.module.to_string(), d.name.to_string())
    }));
    for (i, label, lb) in &blocks {
        module.in_module(
            &program.defs[*i].module.to_string(),
            &program.defs[*i].name.to_string(),
        );
        module.def(*label, lb).map_err(|e| Error {
            msg: format!("in {}: {}", program.defs[*i].name, e.msg),
        })?;
    }
    report_sizes(&module);
    Ok(Unit::all(module.text_library(
        &program.exports,
        fingerprint(),
        unit,
    )))
}

/// `program` as an LLVM module for a test executable: `tests` are the labels
/// of the definitions to run, and the executable's first argument says which,
/// by its place among them. In modules of about `unit` bytes, as
/// [`compile_split`] makes them.
pub fn compile_tests(
    program: &Program,
    tests: &[meadow_axcut::Label],
    unit: usize,
    conv: CallConv,
) -> Result<Vec<Unit>, Error> {
    let mut module = emit::Module::new(program, conv);
    let blocks = linear::program(program, tests)?;
    module.name_labels(blocks.iter().map(|(i, label, _)| {
        let d = &program.defs[*i];
        (*label, d.module.to_string(), d.name.to_string())
    }));
    for (i, label, lb) in &blocks {
        module.in_module(
            &program.defs[*i].module.to_string(),
            &program.defs[*i].name.to_string(),
        );
        module.def(*label, lb).map_err(|e| Error {
            msg: format!("in {}: {}", program.defs[*i].name, e.msg),
        })?;
    }
    Ok(Unit::all(module.text_tests(tests, fingerprint(), unit)))
}
