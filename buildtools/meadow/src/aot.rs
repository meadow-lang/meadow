//! **Native executables**, compiled ahead of time.
//!
//! The bytecode image a build makes is compiled to machine code
//! (`meadow_glade::codegen`), written with the image into an object file, and
//! linked by the system's C compiler with a small `main` and the runtime as a
//! static library. What comes out is an ordinary executable, under the
//! package's `target/<profile>/native/` (see [`crate::artifacts`]) -- or
//! `native/<triple>/`, for a target other than this machine.
//!
//! # The runtime to link
//!
//! `libmeadow_glade.a` (`meadow_glade.lib` on Windows) -- or Silo's,
//! `libmeadow_silo.a` -- built for the target: where `MEADOW_RUNTIME` names
//! it; else, for the host, the one built into this `meadow` (see `build.rs`);
//! else beside this `meadow` binary, as `lib/<triple>/<library>` or plain
//! `<library>` for the host; else, in a checkout, where `cargo build` in
//! `glade` or `silo` leaves it.
//!
//! It must be built from the same runtime sources as this `meadow`, whose code
//! generator assumes that runtime's layout: a library from other sources lacks
//! the symbol the program's `main` refers to (see `glade/build.rs`), so it does
//! not link, and the next place is tried.
//!
//! # The linker
//!
//! The system's C compiler: `cc`, or on Windows the Visual Studio `cl.exe` for
//! the target, found as Rust finds it. `MEADOW_CC` names another.

use crate::artifacts;
use crate::profile::Profile;
use meadow_glade::codegen::{self, Arch, object::Format};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which of Meadow's two runtime systems a program runs on.
///
/// **Glade** (`meadow_glade`) is the one the interpreter, the JIT and the
/// debugger share: bytecode, compiled to machine code block by block as it
/// runs or ahead of time into an executable (`--aot`), with a garbage
/// collector tending the heap. **Silo** (`meadow_silo`) is compiled all the way
/// down by LLVM (`meadow-llvm`) before it runs, counts references instead of
/// collecting, and carries no interpreter: always an executable. See
/// `docs/SILO.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Runtime {
    #[default]
    Glade,
    Silo,
}

impl Runtime {
    pub fn name(self) -> &'static str {
        match self {
            Runtime::Glade => "glade",
            Runtime::Silo => "silo",
        }
    }

    pub fn named(name: &str) -> Option<Runtime> {
        match name {
            "glade" => Some(Runtime::Glade),
            "silo" => Some(Runtime::Silo),
            _ => None,
        }
    }

    /// The symbol a library of this runtime built from the sources this
    /// `meadow` was defines.
    pub fn symbol(self) -> String {
        match self {
            Runtime::Glade => codegen::object::runtime_symbol(),
            Runtime::Silo => meadow_llvm::runtime_symbol(),
        }
    }

    /// The static library's file name, for `format`.
    pub fn library(self, format: Format) -> &'static str {
        match (self, format) {
            (Runtime::Glade, Format::Coff) => "meadow_glade.lib",
            (Runtime::Glade, Format::MachO | Format::Elf) => "libmeadow_glade.a",
            (Runtime::Silo, Format::Coff) => "meadow_silo.lib",
            (Runtime::Silo, Format::MachO | Format::Elf) => "libmeadow_silo.a",
        }
    }
}

/// How a native program's memory is managed: Silo counts references unless
/// told to trace (`docs/RUNTIME-MERGE.md`). `--gc` sets it for a command, and
/// `MEADOW_GC=tracing` for everything that runs.
static GC: std::sync::OnceLock<meadow_llvm::Memory> = std::sync::OnceLock::new();

/// Say how memory is managed, before anything is built: `--gc`.
pub fn set_gc(memory: meadow_llvm::Memory) {
    let _ = GC.set(memory);
}

/// How memory is managed: what `--gc` said, or `MEADOW_GC`, or counting.
pub fn gc() -> meadow_llvm::Memory {
    *GC.get_or_init(|| {
        std::env::var("MEADOW_GC")
            .ok()
            .map_or_else(Default::default, |g| strategy(&g))
    })
}

/// How deep a program for Silo's calls may go: what the profile, the
/// manifest or `--stack` said, or `MEADOW_STACK`.
static STACK: std::sync::OnceLock<meadow_llvm::Stack> = std::sync::OnceLock::new();

/// Say how deep calls may go, before anything is built.
pub fn set_stack(stack: meadow_llvm::Stack) {
    let _ = STACK.set(stack);
}

pub fn stack() -> meadow_llvm::Stack {
    *STACK.get_or_init(|| {
        std::env::var("MEADOW_STACK")
            .ok()
            .and_then(|s| meadow_llvm::Stack::named(&s))
            .unwrap_or_default()
    })
}

/// How a program for Silo is run, by either engine.
pub fn config() -> meadow_llvm::Config {
    meadow_llvm::Config {
        memory: gc(),
        stack: stack(),
    }
}

/// Whether a program for Silo is interpreted rather than compiled: `--interpret`
/// or `MEADOW_INTERPRET`. See [`write_image`].
static INTERPRET: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Interpret, rather than compile, what is built for Silo: `--interpret`.
pub fn set_interpret() {
    let _ = INTERPRET.set(true);
}

pub fn interpret() -> bool {
    *INTERPRET.get_or_init(|| std::env::var_os("MEADOW_INTERPRET").is_some())
}

/// Write `program` as an image for Silo's interpreter (`silo/vm`) to `path`:
/// what a build that interprets makes in place of an executable. Its tests, if
/// it is a program of them, are where each begins.
pub fn write_image(
    path: &Path,
    program: meadow_seq::Program,
    tests: Vec<meadow_seq::Label>,
) -> Result<(), String> {
    write_image_with(path, program, tests, config())
}

/// [`write_image`], run as `config` says rather than as this build is: a
/// debugger's, which wants a stack that grows whatever the build's does.
pub fn write_image_with(
    path: &Path,
    program: meadow_seq::Program,
    tests: Vec<meadow_seq::Label>,
    config: meadow_llvm::Config,
) -> Result<(), String> {
    // The memory pass, done here once rather than by every run: where it
    // starts is the entry, or each test.
    let roots = match (tests.is_empty(), program.entry) {
        (true, Some(e)) => vec![e],
        (true, None) => return Err("the program has no entry point".into()),
        (false, _) => tests.clone(),
    };
    let linear = meadow_llvm::linear::program(&program, &roots).map_err(|e| e.msg)?;
    // What the definitions were before the pass is not read again.
    let mut program = program;
    for d in &mut program.defs {
        d.block.body = meadow_seq::Statement::Error("made linear");
    }
    let image = meadow_seq::image::Image {
        program,
        config,
        tests,
        linear,
    };
    write(path, &meadow_seq::image::encode(&image)?)
}

/// Is `path` an image, which [`interpreter`] runs, rather than an executable?
pub fn is_image(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "image")
}

/// Silo's interpreter: `MEADOW_SILO_VM`, or beside this executable, or where
/// a checkout builds it.
pub fn interpreter() -> Result<PathBuf, String> {
    let exe = format!("meadow-silo-vm{}", std::env::consts::EXE_SUFFIX);
    if let Some(path) = std::env::var_os("MEADOW_SILO_VM") {
        return Ok(PathBuf::from(path));
    }
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join(&exe));
    }
    let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../silo/target");
    for profile in ["release", "debug"] {
        candidates.push(built.join(profile).join(&exe));
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "Silo's interpreter is not built: `cargo build --release --manifest-path              silo/Cargo.toml -p meadow-silo-vm`, or say where it is with MEADOW_SILO_VM"
                .to_string()
        })
}

/// Silo's strategy for a name `--gc` takes: counting for `rc`, and its one
/// tracing collector for any name of one -- Glade's `generational` and
/// `copying` among them, so that a strategy means the same on either runtime.
pub fn strategy(name: &str) -> meadow_llvm::Memory {
    match name {
        "generational" | "copying" => meadow_llvm::Memory::Trace,
        g => meadow_llvm::Memory::named(g).unwrap_or_default(),
    }
}

/// What to compile for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub arch: Arch,
    pub format: Format,
    /// Which runtime library the program links: see [`Runtime`].
    pub runtime: Runtime,
}

impl Target {
    /// The machine this is running on.
    pub fn host() -> Result<Target, String> {
        let arch = Arch::host().ok_or("native code is for aarch64 and x86-64 only")?;
        Ok(Target {
            arch,
            format: Format::host(),
            runtime: Runtime::Glade,
        })
    }

    /// An architecture by name, for this platform's object format:
    /// `aarch64` (or `arm64`), `x86_64` (or `x86-64`).
    pub fn named(name: &str) -> Result<Target, String> {
        let arch = match name {
            "aarch64" | "arm64" => Arch::Aarch64,
            "x86_64" | "x86-64" | "amd64" => Arch::X86_64,
            other => {
                return Err(format!(
                    "no native target `{other}` -- there is `aarch64` and `x86_64`"
                ));
            }
        };
        Ok(Target {
            arch,
            format: Format::host(),
            runtime: Runtime::Glade,
        })
    }

    /// This, linked against `runtime`.
    pub fn with_runtime(self, runtime: Runtime) -> Target {
        Target { runtime, ..self }
    }

    /// Is this the machine this is running on, whichever runtime it links?
    pub fn is_host(self) -> bool {
        Target::host().is_ok_and(|h| (h.arch, h.format) == (self.arch, self.format))
    }

    /// The calling convention Silo's functions are in on this target: see
    /// [`meadow_llvm::CallConv`].
    pub fn call_conv(self) -> meadow_llvm::CallConv {
        meadow_llvm::CallConv::for_arch(match self.arch {
            Arch::Aarch64 => "aarch64",
            Arch::X86_64 => "x86_64",
        })
    }

    /// The Rust target the runtime library for this is built for.
    pub fn triple(self) -> &'static str {
        match (self.arch, self.format) {
            // An ELF target on Android is Android: a native executable is for
            // the machine this runs on, and on a phone that is not glibc Linux.
            (Arch::Aarch64, Format::Elf) if cfg!(target_os = "android") => "aarch64-linux-android",
            (Arch::X86_64, Format::Elf) if cfg!(target_os = "android") => "x86_64-linux-android",
            (Arch::Aarch64, Format::MachO) => "aarch64-apple-darwin",
            (Arch::X86_64, Format::MachO) => "x86_64-apple-darwin",
            (Arch::Aarch64, Format::Elf) => "aarch64-unknown-linux-gnu",
            (Arch::X86_64, Format::Elf) => "x86_64-unknown-linux-gnu",
            (Arch::Aarch64, Format::Coff) => "aarch64-pc-windows-msvc",
            (Arch::X86_64, Format::Coff) => "x86_64-pc-windows-msvc",
        }
    }
}

/// Compile `image` for `target` at `opt`, and link it into package `name`'s executable,
/// answering where it went.
pub fn build(
    root: &Path,
    profile: Profile,
    opt: meadow_compiler::OptLevel,
    name: &str,
    image: &meadow_bytecode::Program,
    target: Target,
    calls: &codegen::Calls,
) -> Result<PathBuf, String> {
    let exe =
        native_dir(root, profile, target).join(format!("{name}{}", target.format.exe_suffix()));
    // An executable that is already the one this image makes is left alone --
    // and that matters more than it sounds. Linking is the slow part of an
    // `aot` build, and on macOS the *first* run of a newly written binary pays
    // for its signature to be checked, which is slower still. Relinking an
    // unchanged program therefore cost about a fifth of a second every time it
    // was run, against twenty milliseconds for the same program on the JIT.
    let stamp = exe.with_extension("stamp");
    let want = made_from(image, opt, target, calls);
    if exe.exists() && std::fs::read_to_string(&stamp).is_ok_and(|had| had == want) {
        return Ok(exe);
    }
    link_image(image, opt, target, calls, &exe)?;
    // After linking, so that a link that failed half-way is not taken for a
    // finished one.
    let _ = std::fs::write(&stamp, &want);
    Ok(exe)
}

/// Compile `program` with the native backend -- AxCut to LLVM IR, compiled and
/// linked with the `meadow_silo` runtime by clang -- into package `name`'s
/// executable, answering where it went. See `docs/SILO.md`.
///
/// `inputs` is what the program was compiled from, as a build's fingerprint
/// says it: given, an executable made of the same is left alone before
/// anything is lowered -- lowering a large program takes a minute, and the IR
/// it writes is not the same twice, since what the compiler interned first
/// depends on which of its threads got there. Without it, the IR is compared.
pub fn build_native(
    root: &Path,
    profile: Profile,
    opt: meadow_compiler::OptLevel,
    name: &str,
    program: &meadow_compiler::core::Program,
    target: Target,
    inputs: Option<u64>,
) -> Result<PathBuf, String> {
    build_native_of(root, profile, opt, name, target, inputs, || {
        let lowered = meadow_seq::lower_program(program, opt);
        if !lowered.unsupported.is_empty() {
            return Err(format!(
                "the back end cannot translate {:?} yet",
                lowered.unsupported
            ));
        }
        Ok(lowered.program)
    })
}

/// [`build_native`] of a program that is AxCut already -- what `lowered`
/// answers, asked for only if the executable there is not this one's: a
/// program of Cut, which a front end other than this compiler's wrote.
pub fn build_native_of(
    root: &Path,
    profile: Profile,
    opt: meadow_compiler::OptLevel,
    name: &str,
    target: Target,
    inputs: Option<u64>,
    lowered: impl FnOnce() -> Result<meadow_seq::Program, String>,
) -> Result<PathBuf, String> {
    let dir = native_dir(root, profile, target);
    let exe = dir.join(format!("{name}{}", target.format.exe_suffix()));
    let runtime = runtimes(target)?
        .into_iter()
        .next()
        .ok_or("no Silo runtime library")?;
    let stamp = exe.with_extension("stamp");
    let meta = std::fs::metadata(&runtime).ok();
    // The runtime library, as how long it is and when it was written.
    let linked_with = meta
        .iter()
        .flat_map(|m| m.len().to_le_bytes())
        .chain(
            meta.iter()
                .filter_map(|m| m.modified().ok())
                .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .flat_map(|d| d.as_nanos().to_le_bytes()),
        )
        .collect::<Vec<u8>>();
    let keyed = inputs.map(|f| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in f
            .to_le_bytes()
            .into_iter()
            .chain(opt.name().bytes())
            .chain(target.triple().bytes())
            .chain(linked_with.iter().copied())
            // Traced and counted, the same program is two executables;
            // with a fixed stack and a growable one, two more.
            .chain([gc() as u8, stack() as u8])
        {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("inputs {h:016x}")
    });
    if interpret() {
        crate::status::status("Lowering", format!("`{name}` for Silo's interpreter"));
        let lowered = lowered()?;
        if !lowered.exports.is_empty() {
            return Err("a library is compiled, not interpreted".into());
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        let image = dir.join(format!("{name}.image"));
        write_image(&image, lowered, Vec::new())?;
        return Ok(image);
    }
    if let Some(want) = &keyed
        && exe.exists()
        && std::fs::read_to_string(&stamp).is_ok_and(|had| had == *want)
    {
        return Ok(exe);
    }
    // Everything from here is the native half of the build, and on a large
    // program the longest part of it: said, so that it is not taken for a
    // hang.
    crate::status::status("Lowering", format!("`{name}` for Silo"));
    let lowered = lowered()?;
    // A program that exports functions to C is a library of them, and is
    // linked as one: no `main`, and named as the platform names a library.
    let library = !lowered.exports.is_empty();
    let exe = if library {
        dir.join(target.format.library_name(name))
    } else {
        exe
    };
    let stamp = exe.with_extension("stamp");
    let units =
        meadow_llvm::compile_split_with(&lowered, meadow_llvm::UNIT, target.call_conv(), config())
            .map_err(|e| e.msg)?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    // Unchanged module and runtime: the executable there is this one.
    let want = keyed.unwrap_or_else(|| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in units
            .iter()
            .flat_map(|u| u.text.bytes())
            .chain(opt.name().bytes())
            .chain(linked_with.iter().copied())
        {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{h:016x}")
    });
    if exe.exists() && std::fs::read_to_string(&stamp).is_ok_and(|had| had == want) {
        prune_units(&dir, name, &unit_paths(&dir, name, &units));
        return Ok(exe);
    }
    // A unit whose text is what its object was compiled from is not compiled
    // again: its module has not changed, and neither has anything it names.
    let modules = unit_paths(&dir, name, &units);
    let kept = dir.join(format!("{name}.units"));
    let had = objects_kept(&kept);
    let prints: Vec<String> = units
        .iter()
        .map(|u| unit_print(&u.text, opt, target))
        .collect();
    let have: Vec<bool> = modules
        .iter()
        .zip(&prints)
        .map(|(m, print)| {
            let object = m.with_extension("o");
            units.len() > 1
                && object.exists()
                && object
                    .file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|f| had.get(f) == Some(print))
        })
        .collect();
    // Forgotten until the objects are there: a build that stops half way
    // leaves nothing that says an object is what it is not.
    let _ = std::fs::remove_file(&kept);
    for ((path, u), have) in modules.iter().zip(&units).zip(&have) {
        if !have {
            write(path, u.text.as_bytes())?;
        }
    }
    let reused = have.iter().filter(|h| **h).count();
    if reused > 0 {
        crate::status::status(
            "Reusing",
            format!("{reused} of {} objects of `{name}`", units.len()),
        );
    }
    clang_link(&modules, &have, &runtime, &exe, opt, target, library)?;
    let _ = std::fs::write(&stamp, &want);
    let listed: String = modules
        .iter()
        .zip(&prints)
        .filter_map(|(m, print)| {
            let object = m.with_extension("o");
            Some(format!("{}\t{print}\n", object.file_name()?.to_str()?))
        })
        .collect();
    let _ = std::fs::write(&kept, listed);
    // The IR was only ever clang's input -- several times the size of the
    // objects it made -- and nothing reads it again: the next build writes it
    // afresh. Kept when the link fails, which is when someone wants to read it,
    // and with `MEADOW_KEEP_IR`, for someone who wants to read it anyway.
    if std::env::var_os("MEADOW_KEEP_IR").is_some() {
        return Ok(exe);
    }
    for m in &modules {
        let _ = std::fs::remove_file(m);
    }
    prune_units(&dir, name, &modules);
    Ok(exe)
}

/// What says a unit's object is the one its text compiles to: the text, and
/// how it was compiled.
fn unit_print(text: &str, opt: meadow_compiler::OptLevel, target: Target) -> String {
    // Two hashes of the same bytes, differently seeded: 128 bits between an
    // object and being taken for another's.
    let mut a: u64 = 0xcbf2_9ce4_8422_2325;
    let mut b: u64 = 0x8422_2325_cbf2_9ce4;
    for byte in text
        .bytes()
        .chain(opt.name().bytes())
        .chain(target.triple().bytes())
    {
        a ^= u64::from(byte);
        a = a.wrapping_mul(0x0100_0000_01b3);
        b = (b ^ u64::from(byte))
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .rotate_left(23);
    }
    format!("{a:016x}{b:016x}")
}

/// The objects the last build left, each with the print of the unit it was
/// compiled from: `<object>\t<print>` a line, in the file at `kept`.
fn objects_kept(kept: &Path) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(kept)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(object, print)| (object.to_string(), print.to_string()))
        .collect()
}

/// Remove what an earlier build of `name` left in `dir` that the executable
/// there was not linked from: its LLVM IR, and the objects of modules that
/// are not among `modules`, this build's -- `name.o` too, when one module was
/// compiled and linked in a single step.
fn prune_units(dir: &Path, name: &str, modules: &[PathBuf]) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let objects: std::collections::HashSet<PathBuf> = if modules.len() == 1 {
        Default::default()
    } else {
        modules.iter().map(|m| m.with_extension("o")).collect()
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file = entry.file_name();
        let Some(rest) = file.to_str().and_then(|f| f.strip_prefix(name)) else {
            continue;
        };
        // `name.ll`, `name.<unit>.ll`, and the same in `.o`: nothing else of
        // the build's is touched.
        let stale = match rest.rsplit_once('.') {
            Some((unit, "ll")) => unit.is_empty() || unit.starts_with('.'),
            Some((unit, "o")) => {
                (unit.is_empty() || unit.starts_with('.')) && !objects.contains(&path)
            }
            _ => false,
        };
        if stale {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Where the LLVM modules `units` of `name` go in `dir`: the first, which
/// holds the tables and the entry points, is `name.ll`, and each of the
/// others is named for what it holds -- `name.Std.Collections.Vector.ll`.
fn unit_paths(dir: &Path, name: &str, units: &[meadow_llvm::Unit]) -> Vec<PathBuf> {
    units
        .iter()
        .map(|u| {
            if u.name.is_empty() {
                dir.join(format!("{name}.ll"))
            } else {
                dir.join(format!("{name}.{}.ll", u.name))
            }
        })
        .collect()
}

/// Write the LLVM modules `units` into `dir`, each where [`unit_paths`] says,
/// and answer their paths.
pub fn write_units(
    dir: &Path,
    name: &str,
    units: &[meadow_llvm::Unit],
) -> Result<Vec<PathBuf>, String> {
    let paths = unit_paths(dir, name, units);
    for (path, u) in paths.iter().zip(units) {
        write(path, u.text.as_bytes())?;
    }
    Ok(paths)
}

/// Compile the LLVM modules at `modules` -- in parallel, when there are
/// several -- and link them with the runtime library `runtime` into `exe`,
/// with clang (or `MEADOW_CLANG`). One that `have` says is compiled already
/// is not compiled again: its object is beside where its text would be.
pub fn clang_link(
    modules: &[PathBuf],
    have: &[bool],
    runtime: &Path,
    exe: &Path,
    opt: meadow_compiler::OptLevel,
    target: Target,
    library: bool,
) -> Result<(), String> {
    let level = match opt {
        meadow_compiler::OptLevel::O0 => "-O0",
        meadow_compiler::OptLevel::O1 => "-O1",
        meadow_compiler::OptLevel::O2 => "-O2",
    };
    let clang = std::env::var("MEADOW_CLANG").unwrap_or_else(|_| "clang".into());
    let run = |cmd: &mut Command, what: &Path| -> Result<(), String> {
        let out = cmd.output().map_err(|e| {
            format!("could not run {clang}: {e} -- the native backend needs clang, or MEADOW_CLANG")
        })?;
        if !out.status.success() {
            // MSVC's linker says what went wrong on stdout.
            return Err(format!(
                "clang could not compile {}:\n{}{}",
                what.display(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(())
    };
    let triple = format!("--target={}", target.triple());
    // One module compiles and links in one step; several are compiled apart
    // first, as many at once as there are cores.
    let inputs: Vec<PathBuf> = if modules.len() == 1 {
        modules.to_vec()
    } else {
        let objects: Vec<PathBuf> = modules.iter().map(|m| m.with_extension("o")).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let failed: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
        let bar = std::sync::Mutex::new(crate::status::Building::new(modules.len()));
        let unit = |i: usize| {
            modules[i]
                .file_name()
                .map_or(String::new(), |n| n.to_string_lossy().into_owned())
        };
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        std::thread::scope(|scope| {
            for _ in 0..cores.min(modules.len()) {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= modules.len() {
                            return;
                        }
                        if have.get(i).copied().unwrap_or(false) {
                            bar.lock().unwrap_or_else(|p| p.into_inner()).step();
                            continue;
                        }
                        bar.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .working_on(&unit(i));
                        let mut cmd = Command::new(&clang);
                        cmd.arg(level)
                            .arg("-c")
                            .arg("-Wno-override-module")
                            .arg(&triple)
                            .arg("-o")
                            .arg(&objects[i])
                            .arg(&modules[i]);
                        if let Err(e) = run(&mut cmd, &modules[i]) {
                            failed
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .get_or_insert(e);
                            return;
                        }
                        let mut bar = bar.lock().unwrap_or_else(|p| p.into_inner());
                        bar.step();
                        bar.working_on(&unit(i));
                    }
                });
            }
        });
        drop(bar);
        if let Some(e) = failed.into_inner().unwrap_or_else(|p| p.into_inner()) {
            return Err(e);
        }
        objects
    };
    crate::status::status(
        "Linking",
        exe.file_name()
            .map_or(String::new(), |n| n.to_string_lossy().into_owned()),
    );
    let mut cmd = Command::new(&clang);
    cmd.arg(level)
        .arg("-Wno-override-module")
        .arg(&triple)
        .arg("-o")
        .arg(exe);
    if library {
        cmd.arg("-shared");
    }
    // Several objects are named in a response file rather than on the command
    // line: a large program is hundreds of them, and a Windows command line
    // holds 32K characters.
    let listed = (inputs.len() > 1).then(|| exe.with_extension("rsp"));
    match &listed {
        Some(rsp) => {
            let text: String = inputs
                .iter()
                .map(|p| format!("\"{}\"\n", p.to_string_lossy().replace('\\', "/")))
                .collect();
            std::fs::write(rsp, text)
                .map_err(|e| format!("could not write {}: {e}", rsp.display()))?;
            cmd.arg(format!("@{}", rsp.display()));
        }
        None => {
            cmd.args(&inputs);
        }
    }
    cmd.arg(runtime);
    for lib in native_system_libs(target.format) {
        cmd.arg(lib);
    }
    let linked = run(&mut cmd, &modules[0]);
    if let Some(rsp) = listed {
        let _ = std::fs::remove_file(rsp);
    }
    linked
}

/// What clang links a program against besides the runtime library.
fn native_system_libs(format: Format) -> &'static [&'static str] {
    match format {
        Format::Coff => &[
            "-lkernel32",
            "-lntdll",
            "-luserenv",
            "-lws2_32",
            "-ldbghelp",
            "-lbcrypt",
            "-ladvapi32",
        ],
        Format::MachO => &["-liconv"],
        Format::Elf => &["-lpthread", "-ldl", "-lm"],
    }
}

/// What an executable was made from, as one line: the image, how it was
/// compiled, what for, and which runtime it was linked against. Anything that
/// would change the executable changes this.
fn made_from(
    image: &meadow_bytecode::Program,
    opt: meadow_compiler::OptLevel,
    target: Target,
    calls: &codegen::Calls,
) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(&meadow_bytecode::image::encode(image));
    feed(opt.name().as_bytes());
    feed(target.triple().as_bytes());
    feed(target.runtime.name().as_bytes());
    // And what a profile said the calls enter, which the code is guarded on.
    let mut sites: Vec<_> = calls.iter().collect();
    sites.sort_by_key(|(pc, _)| **pc);
    for (pc, k) in sites {
        feed(&pc.to_le_bytes());
        feed(&[k.frame as u8]);
        feed(&k.meta.to_le_bytes());
    }
    // The runtime is linked in, so a new one makes a new executable. Its name
    // carries this compiler's identity (see `link_image`), and its file says
    // whether it has been rebuilt since.
    feed(target.runtime.symbol().as_bytes());
    for runtime in runtimes(target).unwrap_or_default() {
        if let Ok(meta) = std::fs::metadata(&runtime) {
            feed(&meta.len().to_le_bytes());
            if let Ok(t) = meta.modified()
                && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
            {
                feed(&d.as_nanos().to_le_bytes());
            }
        }
    }
    format!("{h:016x}")
}

/// Where what is compiled for `target` goes: the host's where `run` looks;
/// another target's beside it, under its triple -- and without `\\?\`, which
/// `cl` takes for the start of a file name.
pub fn native_dir(root: &Path, profile: Profile, target: Target) -> PathBuf {
    let native = artifacts::native_dir(root, profile);
    let mut dir = PathBuf::from(crate::dap::session::plain_path(&native.to_string_lossy()));
    if !target.is_host() {
        dir = dir.join(target.triple());
    }
    // Beside the default runtime's, so the two can be run against each other.
    if target.runtime != Runtime::Glade {
        dir = dir.join(target.runtime.name());
    }
    dir
}

/// Write the native code `image` compiles to for `target` at `opt` as text,
/// under package `name`'s native directory, answering where it went -- what
/// `--emit asm` makes in place of an executable.
pub fn write_asm(
    root: &Path,
    profile: Profile,
    opt: meadow_compiler::OptLevel,
    name: &str,
    image: &meadow_bytecode::Program,
    target: Target,
) -> Result<PathBuf, String> {
    let path = native_dir(root, profile, target).join(format!("{name}.s"));
    artifacts::write_text(&path, &crate::listing::asm(image, target.arch, opt))?;
    Ok(path)
}

/// Compile `image` for `target` at `opt` and link it into the executable
/// `exe`, with the object file and `main` it is linked from beside it --
/// what `meadow link` does with an image from anywhere.
pub fn link_image(
    image: &meadow_bytecode::Program,
    opt: meadow_compiler::OptLevel,
    target: Target,
    calls: &codegen::Calls,
    exe: &Path,
) -> Result<(), String> {
    let runtimes = runtimes(target)?;
    let dir = match exe.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let name = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("{} is not a file name", exe.display()))?;

    let compiled = codegen::compile_guided(image, target.arch, opt, calls);
    let bytes = meadow_bytecode::image::encode(image);
    let object = dir.join(format!("{name}.{}", target.format.object_extension()));
    write(
        &object,
        &codegen::object::write(&compiled, &bytes, target.format),
    )?;
    let main = dir.join(format!("{name}-main.c"));
    write(&main, codegen::object::main_c().as_bytes())?;

    let mut stale = Vec::new();
    for runtime in &runtimes {
        match link(&main, &object, runtime, &exe, target) {
            Ok(()) => return Ok(()),
            Err(e) if e.contains(&codegen::object::runtime_symbol()) => stale.push(runtime),
            Err(e) => return Err(e),
        }
    }
    let mut msg = String::from("no runtime library built from this compiler's sources:");
    for runtime in stale {
        msg.push_str(&format!("\n  {} is from another build", runtime.display()));
    }
    msg.push_str(&format!(
        "\nrebuild it with `cargo build --release --target {}` in `glade`",
        target.triple()
    ));
    Err(msg)
}

/// Link `main`, `object` and `runtime` into `exe`, or say why the linker would
/// not.
fn link(
    main: &Path,
    object: &Path,
    runtime: &Path,
    exe: &Path,
    target: Target,
) -> Result<(), String> {
    let (mut cmd, cc) = compiler(target)?;
    match target.format {
        Format::MachO | Format::Elf => {
            cmd.arg(main).arg(object).arg(runtime).arg("-o").arg(exe);
            if target.format == Format::MachO {
                let arch = match target.arch {
                    Arch::Aarch64 => "arm64",
                    Arch::X86_64 => "x86_64",
                };
                cmd.args(["-arch", arch]);
            }
        }
        Format::Coff => {
            // `main`'s own object goes beside the rest rather than wherever
            // this was run from.
            let mut fo = std::ffi::OsString::from("/Fo");
            fo.push(main.with_extension("obj"));
            let mut fe = std::ffi::OsString::from("/Fe");
            fe.push(exe);
            cmd.args(["/nologo", "/MD"])
                .arg(fo)
                .arg(fe)
                .arg(main)
                .arg(object)
                .arg(runtime);
        }
    }
    cmd.args(target.format.system_libs());
    // On Windows the executable a run just made can stay open for a moment
    // after it exits -- a virus scanner looking at it -- and the linker cannot
    // replace it (LNK1104). That goes away by itself: wait for it, a little.
    let mut out;
    let mut tries = 0;
    loop {
        out = cmd
            .output()
            .map_err(|e| format!("could not run `{cc}` to link: {e}"))?;
        let said = String::from_utf8_lossy(&out.stdout);
        let locked = target.format == Format::Coff
            && !out.status.success()
            && said
                .lines()
                .any(|l| l.contains("LNK1104") && l.contains(&*exe.to_string_lossy()));
        if !locked || tries == 20 {
            break;
        }
        tries += 1;
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if !out.status.success() {
        // The Microsoft tools say what went wrong on standard output.
        return Err(format!(
            "linking {} failed:\n{}{}",
            exe.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// The C compiler that links for `target`, and what to call it in a message:
/// `MEADOW_CC` if it is set -- on Windows, something that takes `cl`'s
/// arguments, like `clang-cl` -- else `cc`, or on Windows the Visual Studio
/// `cl.exe` for the target, with the environment it needs to find its headers
/// and libraries.
fn compiler(target: Target) -> Result<(Command, String), String> {
    if let Ok(cc) = std::env::var("MEADOW_CC") {
        return Ok((Command::new(&cc), cc));
    }
    if target.format != Format::Coff {
        return Ok((Command::new("cc"), "cc".to_string()));
    }
    #[cfg(windows)]
    if let Some(cmd) = find_msvc_tools::find(target.triple(), "cl.exe") {
        return Ok((cmd, "cl.exe".to_string()));
    }
    Err(format!(
        "no C compiler to link for {}: install Visual Studio's C++ build tools, \
         or name a `cl`-compatible compiler with MEADOW_CC",
        target.triple()
    ))
}

/// The runtime libraries built into this `meadow`, for the machine it runs on
/// -- each empty if it was built without it (see `build.rs`).
static EMBEDDED_GLADE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime"));
static EMBEDDED_SILO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime-silo"));

/// `runtime`'s embedded library, as a file a linker can be handed: under
/// `~/.meadow/lib/<its fingerprint>/`, written the first time it is wanted.
/// Its fingerprint names the directory, so a `meadow` from other sources
/// writes its own beside it rather than over it.
fn embedded(runtime: Runtime, lib: &str) -> Option<PathBuf> {
    let bytes = match runtime {
        Runtime::Glade => EMBEDDED_GLADE,
        Runtime::Silo => EMBEDDED_SILO,
    };
    if bytes.is_empty() {
        return None;
    }
    let dir = crate::stdlib::home()?.join("lib").join(runtime.symbol());
    let path = dir.join(lib);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64) {
        return Some(path);
    }
    std::fs::create_dir_all(&dir).ok()?;
    // Whole or not at all: another `meadow` may be linking against it.
    let partial = dir.join(format!("{lib}.{}", std::process::id()));
    std::fs::write(&partial, bytes).ok()?;
    if std::fs::rename(&partial, &path).is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    Some(path).filter(|p| p.is_file())
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Where a runtime library for `target` might be, most wanted first -- see the
/// module docs.
pub fn runtimes(target: Target) -> Result<Vec<PathBuf>, String> {
    let lib = target.runtime.library(target.format);
    // Where a checkout builds it: the runtime's crate, `glade` or `silo`.
    let crate_dir = target.runtime.name();
    if let Some(path) = std::env::var_os("MEADOW_RUNTIME") {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(vec![path])
        } else {
            Err(format!(
                "MEADOW_RUNTIME names {}, which is not there",
                path.display()
            ))
        };
    }
    let host = target.is_host();
    let mut candidates = Vec::new();
    if host && let Some(path) = embedded(target.runtime, lib) {
        candidates.push(path);
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join("lib").join(target.triple()).join(lib));
        if host {
            candidates.push(dir.join(lib));
        }
    }
    // A checkout: the runtime crate's own target directory.
    let built = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(crate_dir)
        .join("target");
    for profile in ["release", "debug"] {
        candidates.push(built.join(target.triple()).join(profile).join(lib));
        if host {
            candidates.push(built.join(profile).join(lib));
        }
    }
    let found: Vec<PathBuf> = candidates.into_iter().filter(|p| p.is_file()).collect();
    if !found.is_empty() {
        return Ok(found);
    }
    Err(format!(
        "no `{crate_dir}` runtime library for {} -- build one with \
         `cargo build --release --target {}` in `{crate_dir}`, or name it with MEADOW_RUNTIME",
        target.triple(),
        target.triple()
    ))
}
