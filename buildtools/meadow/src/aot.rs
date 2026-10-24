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

/// The system a native build is for. The object format follows from it,
/// but not the other way round: Android's is ELF, as Linux's is, and its C
/// library, loader and linker are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    Linux,
    Android,
    MacOs,
    Windows,
}

impl Os {
    /// The system this `meadow` runs on.
    pub fn host() -> Os {
        if cfg!(target_os = "android") {
            Os::Android
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }

    pub fn format(self) -> Format {
        match self {
            Os::Linux | Os::Android => Format::Elf,
            Os::MacOs => Format::MachO,
            Os::Windows => Format::Coff,
        }
    }

    /// As `@cfg(os = "…")` names it.
    pub fn cfg_name(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Android => "android",
            Os::MacOs => "macos",
            Os::Windows => "windows",
        }
    }
}

/// What to compile for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub arch: Arch,
    pub format: Format,
    pub os: Os,
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
            os: Os::host(),
            runtime: Runtime::Glade,
        })
    }

    /// A target by name: an architecture alone -- `aarch64` (or `arm64`),
    /// `x86_64` (or `x86-64`) -- for this system, or a Rust target triple
    /// for another: `aarch64-linux-android`, `x86_64-apple-darwin`.
    pub fn named(name: &str) -> Result<Target, String> {
        let arch_named = |a: &str| match a {
            "aarch64" | "arm64" => Some(Arch::Aarch64),
            "x86_64" | "x86-64" | "amd64" => Some(Arch::X86_64),
            _ => None,
        };
        if let Some(arch) = arch_named(name) {
            return Ok(Target {
                arch,
                format: Format::host(),
                os: Os::host(),
                runtime: Runtime::Glade,
            });
        }
        let os = match name.split_once('-').map(|(_, rest)| rest) {
            Some("linux-android") => Os::Android,
            Some("unknown-linux-gnu") => Os::Linux,
            Some("apple-darwin") => Os::MacOs,
            Some("pc-windows-msvc") => Os::Windows,
            _ => None.ok_or_else(|| unknown_target(name))?,
        };
        let arch = name
            .split('-')
            .next()
            .and_then(arch_named)
            .ok_or_else(|| unknown_target(name))?;
        Ok(Target {
            arch,
            format: os.format(),
            os,
            runtime: Runtime::Glade,
        })
    }

    /// This, linked against `runtime`.
    pub fn with_runtime(self, runtime: Runtime) -> Target {
        Target { runtime, ..self }
    }

    /// Is this the machine this is running on, whichever runtime it links?
    pub fn is_host(self) -> bool {
        Target::host().is_ok_and(|h| (h.arch, h.os) == (self.arch, self.os))
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
        match (self.arch, self.os) {
            (Arch::Aarch64, Os::Android) => "aarch64-linux-android",
            (Arch::X86_64, Os::Android) => "x86_64-linux-android",
            (Arch::Aarch64, Os::MacOs) => "aarch64-apple-darwin",
            (Arch::X86_64, Os::MacOs) => "x86_64-apple-darwin",
            (Arch::Aarch64, Os::Linux) => "aarch64-unknown-linux-gnu",
            (Arch::X86_64, Os::Linux) => "x86_64-unknown-linux-gnu",
            (Arch::Aarch64, Os::Windows) => "aarch64-pc-windows-msvc",
            (Arch::X86_64, Os::Windows) => "x86_64-pc-windows-msvc",
        }
    }
}

fn unknown_target(name: &str) -> String {
    format!(
        "no native target `{name}` -- there is `aarch64` and `x86_64` for this system, and \
         the triples `aarch64-linux-android`, `x86_64-linux-android`, \
         `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, \
         `x86_64-apple-darwin`, `aarch64-pc-windows-msvc` and `x86_64-pc-windows-msvc`"
    )
}

/// The Android API level a build for Android is for: `MEADOW_ANDROID_API`, or
/// 24 -- Android 7, the oldest the release's own Android build runs on.
fn android_api() -> String {
    std::env::var("MEADOW_ANDROID_API")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "24".to_string())
}

/// The NDK's clang, which knows where Android's headers and libraries are:
/// from `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, `ANDROID_NDK_LATEST_HOME`
/// (a GitHub runner's) or `ANDROID_NDK`, whichever is set first.
fn ndk_clang() -> Result<String, String> {
    let ndk = [
        "ANDROID_NDK_HOME",
        "ANDROID_NDK_ROOT",
        "ANDROID_NDK_LATEST_HOME",
        "ANDROID_NDK",
    ]
    .iter()
    .find_map(|v| std::env::var_os(v).filter(|p| !p.is_empty()))
    .ok_or(
        "a build for Android needs its NDK: set ANDROID_NDK_HOME to where it is, \
             or MEADOW_CLANG to its clang",
    )?;
    let host = if cfg!(windows) {
        "windows-x86_64"
    } else if cfg!(target_os = "macos") {
        // The NDK's macOS tools are universal, under this name.
        "darwin-x86_64"
    } else {
        "linux-x86_64"
    };
    let exe = if cfg!(windows) { "clang.exe" } else { "clang" };
    let clang = PathBuf::from(ndk)
        .join("toolchains/llvm/prebuilt")
        .join(host)
        .join("bin")
        .join(exe);
    if !clang.is_file() {
        return Err(format!(
            "the Android NDK has no clang at {}: is ANDROID_NDK_HOME the NDK itself?",
            clang.display()
        ));
    }
    Ok(clang.to_string_lossy().into_owned())
}

/// The clang a native build for `target` runs, and the `--target` it gives
/// it: `MEADOW_CLANG` or the `clang` on `PATH`, except that a build for
/// Android takes the NDK's, and names the API level in the triple.
fn clang_for(target: Target) -> Result<(String, String), String> {
    let set = std::env::var("MEADOW_CLANG").ok().filter(|c| !c.is_empty());
    if target.os == Os::Android {
        let clang = match set {
            Some(c) => c,
            None => ndk_clang()?,
        };
        return Ok((
            clang,
            format!("--target={}{}", target.triple(), android_api()),
        ));
    }
    Ok((
        set.unwrap_or_else(|| "clang".into()),
        format!("--target={}", target.triple()),
    ))
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
        {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("inputs {h:016x}")
    });
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
    let units = meadow_llvm::compile_split(&lowered, meadow_llvm::UNIT, target.call_conv())
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
    let (clang, triple) = clang_for(target)?;
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
    for lib in native_system_libs(target) {
        cmd.arg(lib);
    }
    let linked = run(&mut cmd, &modules[0]);
    if let Some(rsp) = listed {
        let _ = std::fs::remove_file(rsp);
    }
    linked
}

/// What clang links a program against besides the runtime library.
fn native_system_libs(target: Target) -> &'static [&'static str] {
    // Bionic is its own C library: its threads are in it, and what Rust's
    // runtime needs besides is the loader's, the system log's, and the
    // unwinder the NDK carries -- what Glade's runtime links on Android too.
    if target.os == Os::Android {
        return &["-ldl", "-llog", "-lunwind", "-lm"];
    }
    match target.format {
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
    // Glade's executables are linked by this system's C compiler, for this
    // system: another one is Silo's, through clang.
    if target.os != Os::host() {
        return Err(format!(
            "Glade builds ahead of time for {} only; for `{}`, build with `--runtime silo`",
            Os::host().cfg_name(),
            target.triple()
        ));
    }
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
    // Android's needs the NDK's C compiler for the C its crates build, which
    // the script sets up.
    let how = if target.os == Os::Android {
        format!(
            "`scripts/silo-runtime.sh {}` in a checkout",
            target.triple()
        )
    } else {
        format!(
            "`cargo build --release --target {}` in `{crate_dir}`",
            target.triple()
        )
    };
    Err(format!(
        "no `{crate_dir}` runtime library for {} -- build one with {how}, or name it with \
         MEADOW_RUNTIME",
        target.triple()
    ))
}
