//! The `meadow` command-line entry point. The real work lives in this crate's
//! library (`lib.rs`) plus the `meadow-compiler` and `meadow-eval` crates; this
//! file only parses arguments, wires things together, and prints results.

mod repl;

use clap::{Parser, Subcommand};
use meadow::{
    Backend, Engine, OptLevel, Profile, Resolved, Strictness, aot, artifacts, format, init,
    listing::{self, Emit},
    package::ProfileConfig,
    pipeline, runtime, status, test,
    workspace::{Selected, Selection},
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "meadow", about = "Meadow language compiler", version = meadow::channel::VERSION)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Never fetch a dependency. What is already in the cache is used; anything
    /// else is an error saying what would have had to be fetched.
    #[arg(long, global = true)]
    offline: bool,
    /// Refuse anything that would change `Meadow.lock`. What CI wants: a build
    /// that quietly re-pins a dependency is a build of something nobody
    /// reviewed.
    #[arg(long, global = true)]
    locked: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Type-check and link a package, printing the annotated result.
    Build {
        /// Package directory (or a single `.mw` file), or a workspace.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Print the type of every top-level binding in your packages.
        #[arg(long)]
        types: bool,
        /// Also print every node's inferred type.
        #[arg(long)]
        annotations: bool,
        /// What to write, in place of what a build writes by default -- the
        /// bytecode image, and for an `aot` build an executable: `image`,
        /// `bytecode` (the image as text), `asm` (the native code as text),
        /// `exe`, or `expanded` (the sources with every macro call replaced
        /// by what it produced). Comma-separated, or repeated.
        #[arg(long, value_name = "KIND", value_delimiter = ',', value_parser = emit)]
        emit: Vec<Emit>,
        #[command(flatten)]
        packages: PackageArgs,
        #[command(flatten)]
        profile: ProfileArgs,
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Build a package, then evaluate its `main` entry point.
    Run {
        /// Package directory (or a single `.mw` file), or a workspace.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Print the type of every top-level binding in your packages first.
        #[arg(long)]
        types: bool,
        /// In a workspace: the member to run.
        #[arg(short = 'p', long = "package", value_name = "NAME")]
        package: Option<String>,
        #[command(flatten)]
        profile: ProfileArgs,
        #[command(flatten)]
        engine: EngineArgs,
        /// After running, report what the garbage collector did (VM only): how
        /// many collections, how long they took, and how much they copied.
        #[arg(long)]
        gc_stats: bool,
        /// Sample where the program spends itself and write folded stacks to
        /// this file, for `flamegraph.pl` or speedscope. Needs a build that
        /// carries debug info, which is what a debug build is.
        #[arg(long, value_name = "FILE")]
        profile_to: Option<std::path::PathBuf>,
        /// Block entries between samples, with `--profile-to`: a profile of
        /// *work*, the same twice for the same program. Without it the profile
        /// is of *time*, which is what sees a cache miss or a collection.
        #[arg(long, value_name = "N")]
        sample_every: Option<u64>,
        /// Samples a second, for the profile of time. Ignored with
        /// `--sample-every`.
        #[arg(long, value_name = "HZ", default_value_t = meadow_glade::profile::HZ)]
        sample_hz: u64,
        /// Run on the JIT and write what each call site was seen to enter to
        /// `target/<profile>/calls.pgo`, for an `aot` build of the same
        /// program to guard its calls on: profile-guided optimization.
        #[arg(long)]
        train: bool,
        /// Which collector the VM uses: `generational` (the default: a nursery,
        /// and an old generation marked concurrently, for short pauses) or
        /// `copying` (one space, copied whole). `MEADOW_GC` sets the same.
        #[arg(long, value_parser = ["generational", "copying"])]
        gc: Option<String>,
        #[command(flatten)]
        target: TargetArgs,
        /// Arguments for the program, after `--`: what `Process.argv` gives
        /// it. `meadow run . -- in.mw -o out` passes `in.mw -o out`.
        #[arg(last = true, value_name = "ARGS")]
        args: Vec<String>,
    },
    /// Run a bytecode image: what `meadow build` writes under
    /// `target/<profile>/bytecode/`, or what a compiler written in Meadow
    /// emits.
    Exec {
        /// The `.mbc` file.
        image: PathBuf,
        /// `vm` (the interpreter alone) or `jit` (the default). For an
        /// executable, `meadow link` the image.
        #[arg(long, value_name = "BACKEND", value_parser = backend)]
        backend: Option<Backend>,
        /// Optimization level for the JIT: 0, 1 or 2.
        #[arg(short = 'O', long = "opt-level", value_name = "LEVEL", value_parser = opt_level)]
        opt: Option<OptLevel>,
        /// Arguments for the program, after `--`.
        #[arg(last = true, value_name = "ARGS")]
        args: Vec<String>,
    },
    /// Compile a bytecode image to machine code and link it into an
    /// executable.
    Link {
        /// The `.mbc` file.
        image: PathBuf,
        /// The file to write. The image's name beside it, by default.
        #[arg(short = 'o', long = "output", value_name = "FILE")]
        output: Option<PathBuf>,
        /// `exe` (the default), or `asm`: the native code as text, in place of
        /// the executable.
        #[arg(long, value_name = "KIND", value_parser = ["exe", "asm"], default_value = "exe")]
        emit: String,
        /// Optimization level: 0, 1 or 2 (the default).
        #[arg(short = 'O', long = "opt-level", value_name = "LEVEL", value_parser = opt_level)]
        opt: Option<OptLevel>,
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Build a package and run its `@test` functions.
    Test {
        /// Package directory (or a single `.mw` file).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Only run tests whose name contains this.
        filter: Option<String>,
        /// Only the test whose name is exactly FILTER -- `Module.test`, or the
        /// bare name in the root module.
        #[arg(long, requires = "filter")]
        exact: bool,
        /// Also run the standard library's own tests.
        #[arg(long)]
        std: bool,
        /// How many tests run at once: one per core unless this, or
        /// `MEADOW_TEST_THREADS`, says otherwise. `1` runs them in order,
        /// for tests that share a file or a port.
        #[arg(long, value_name = "N")]
        test_threads: Option<usize>,
        /// Write what tests print as they print it. By default it is kept,
        /// and shown beside a test that fails.
        #[arg(long, visible_alias = "nocapture")]
        no_capture: bool,
        #[command(flatten)]
        packages: PackageArgs,
        #[command(flatten)]
        profile: ProfileArgs,
        #[command(flatten)]
        engine: EngineArgs,
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Disassemble a package: the bytecode the VM would run.
    Dis {
        /// Package directory (or a single `.mw` file).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// The native code an `aot` build compiles the bytecode to, instead.
        #[arg(long)]
        asm: bool,
        #[command(flatten)]
        packages: PackageArgs,
        #[command(flatten)]
        profile: ProfileArgs,
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Run the language server, speaking LSP over stdin and stdout.
    ///
    /// Editors start this; there is no reason to run it by hand.
    Lsp {
        /// Ignored: clients pass this to select the stdio transport, which is
        /// the only one spoken.
        ///
        /// It has to be *accepted* rather than merely ignored. `vscode-languageclient`
        /// appends `--stdio` to the command it was given, and refusing an unknown
        /// argument made the server exit with code 2 before reading a byte — which
        /// reached the user as `write EPIPE`, a message about the editor's failed
        /// write that says nothing about the argument it passed.
        #[arg(long)]
        stdio: bool,
        /// Ignored: some clients name their own process id so a server can exit
        /// when the editor goes away. This one exits when its input closes.
        #[arg(long = "clientProcessId", value_name = "PID")]
        client_process_id: Option<String>,
        /// Fetch what a package depends on when a file of it is opened, as a
        /// build would -- cloning, compiling, and running its macros. Editors
        /// pass it only for a workspace the user trusts; without it, only
        /// what a build already fetched is used.
        #[arg(long)]
        fetch: bool,
    },
    /// Run the debug adapter, speaking the Debug Adapter Protocol over stdin
    /// and stdout.
    ///
    /// Editors start this when you debug a Meadow program; there is no reason
    /// to run it by hand.
    Dap,
    /// Re-indent `.mw` sources in place.
    Fmt {
        /// Files or directories to format. Defaults to the current directory.
        paths: Vec<PathBuf>,
        /// List the files that would change and exit 1, without writing.
        #[arg(long)]
        check: bool,
        /// Write the result to stdout instead of back to the file.
        #[arg(long)]
        stdout: bool,
        /// How many characters a line may be: what fits is put on one line,
        /// and what does not is cut at its outermost joint. A string or a
        /// comment is never cut. `0` only indents: no token leaves its line.
        /// 100 unless it is given, as rustfmt has it.
        #[arg(long, value_name = "COLUMNS", default_value_t = meadow_fmt::WIDTH)]
        width: usize,
    },
    /// Create a package: a `Meadow.toml` and a `src/Main.mw` that runs.
    ///
    /// Inside a workspace, the new package is added to its `members`.
    Init {
        /// Where to put it, created if it does not exist.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// The package's name. Defaults to the directory's.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Create a workspace instead: a `Meadow.toml` with an empty
        /// `[workspace]`, for packages made inside it to join.
        #[arg(long, conflicts_with = "name")]
        workspace: bool,
    },
    /// Add a dependency to a package's manifest.
    ///
    /// Takes a git URL, a GitHub `owner/name`, or a directory. There is no
    /// registry to look a name up in, so the repository is fetched and its own
    /// `Meadow.toml` says what the package is called.
    Add {
        /// `https://github.com/owner/name`, `owner/name`, or `../a/directory`.
        #[arg(value_name = "WHAT")]
        what: String,
        /// Take this branch. Without one, the repository's default branch.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["tag", "rev"])]
        branch: Option<String>,
        /// Take this tag.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["branch", "rev"])]
        tag: Option<String>,
        /// Take this commit, which cannot come to mean anything else.
        #[arg(long, value_name = "COMMIT", conflicts_with_all = ["branch", "tag"])]
        rev: Option<String>,
        /// Call it this, rather than what it calls itself.
        #[arg(long, value_name = "NAME")]
        rename: Option<String>,
        /// The package to add it to.
        #[arg(long, default_value = ".", value_name = "DIR")]
        path: PathBuf,
    },
    /// Run a program of Cut, the IR a front end hands the back end
    /// (`docs/CUT.md`): lowered to AxCut and run on Glade, or built for Silo
    /// and run as an executable.
    ///
    /// For whoever writes a front end, Meadow's own in Meadow among them.
    /// Effects a program handles are not lowered yet.
    Cut {
        /// The program, as the text `docs/CUT.md` describes.
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// `glade`, on its bytecode, or `silo`, as native code.
        #[arg(long, default_value = "glade", value_name = "NAME")]
        runtime: String,
        /// On Glade, compile the bytecode to machine code ahead of time and
        /// run that executable, rather than interpreting it.
        #[arg(long)]
        aot: bool,
        /// How hard an executable is optimised: 0, 1 or 2.
        #[arg(
            short = 'O',
            long = "opt-level",
            default_value = "2",
            value_name = "LEVEL"
        )]
        opt: u8,
        /// Build what would be run -- the bytecode image, or the executable
        /// -- and stop: what a front end's `build` asks for.
        #[arg(long)]
        no_run: bool,
        /// Print what the program is made into, and run nothing: `inlined`,
        /// the Cut it is lowered from once its small definitions are inlined;
        /// `axcut`, the AxCut it is lowered to; or `llvm`, the LLVM IR a Silo
        /// build compiles.
        #[arg(long, value_name = "WHAT")]
        emit: Option<String>,
        /// What the program is given, after `--`: what its `argv` answers.
        #[arg(last = true, value_name = "ARGS")]
        args: Vec<String>,
    },
    /// Remove what a build wrote: the package's `target` directory, with the
    /// images, executables and incremental cache in it.
    ///
    /// Sources, `Meadow.toml` and `Meadow.lock` are not touched, and neither is
    /// anything fetched into the dependency cache, which other packages share.
    Clean {
        /// The package, or a workspace, whose `target` to remove.
        #[arg(default_value = ".", value_name = "PATH")]
        path: PathBuf,
        /// Only this profile's, rather than all of them.
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        /// Say what would be removed without removing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Bring a package's dependencies forward to what their branches and tags
    /// now name, rewriting `Meadow.lock`.
    ///
    /// The manifest is not touched: a dependency keeps the branch or tag it
    /// follows. Updating the toolchain is `meadow self update`.
    Update {
        /// Only these, by name. Without any, every git dependency.
        #[arg(value_name = "NAME")]
        only: Vec<String>,
        /// The package, or a member of the workspace holding the lockfile.
        #[arg(long, default_value = ".", value_name = "DIR")]
        path: PathBuf,
        /// Say what would change without changing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Where toolchain management went. Hidden: it is here only to say so to
    /// anyone whose fingers still type it.
    #[command(name = "self", hide = true)]
    Toolchain {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
    /// Compile the standard library ahead of time into OUT, for a build of
    /// `meadow` to embed with `MEADOW_PRECOMPILED_STD=OUT`. Hidden: it is the
    /// release's business, not a user's.
    #[command(name = "__precompile-std", hide = true)]
    PrecompileStd {
        out: PathBuf,
        /// The Rust target triple the `meadow` embedding it is built for;
        /// this machine's by default.
        #[arg(long)]
        target: Option<String>,
    },
}

/// `-p`, `--workspace` and `--exclude` -- which members of a workspace a
/// command means.
#[derive(clap::Args)]
struct PackageArgs {
    /// In a workspace: the member to use, by name. Repeat it for more.
    #[arg(short = 'p', long = "package", value_name = "NAME")]
    package: Vec<String>,
    /// Every member of the workspace.
    #[arg(long, visible_alias = "all", conflicts_with = "package")]
    workspace: bool,
    /// With `--workspace`: leave this member out. Repeat it for more.
    #[arg(long, value_name = "NAME", requires = "workspace")]
    exclude: Vec<String>,
}

impl PackageArgs {
    fn selection(&self) -> Selection {
        Selection {
            packages: self.package.clone(),
            workspace: self.workspace,
            exclude: self.exclude.clone(),
        }
    }
}

/// What `selection` means at `path`, exiting with the reason when it means
/// nothing -- after warning about any member profile the workspace ignores.
fn select(selection: &Selection, path: &std::path::Path) -> Selected {
    let selected = selection.select(path).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });
    if let Some(ws) = &selected.workspace {
        for m in ws.ignored_profiles(&selected.paths) {
            eprintln!(
                "warning: the `[profile]` sections of `{}` are ignored: a workspace member \
                 builds with the profiles in {}",
                m.name,
                meadow::workspace::shown(&ws.root.join("Meadow.toml"))
            );
        }
    }
    selected
}

/// `--target` -- what a native build compiles for -- and how the runtime runs.
#[derive(clap::Args, Clone)]
struct TargetArgs {
    /// The architecture a native build compiles for: `aarch64` or `x86_64`.
    /// The host's by default; `target = "…"` in a `[profile.<name>]` of
    /// `Meadow.toml` sets it for a package.
    #[arg(long, value_name = "ARCH")]
    target: Option<String>,
    /// The runtime system, from the profile: see `--runtime`.
    #[arg(skip)]
    runtime: aot::Runtime,
    /// How many OS threads run the program's green threads. One per core by
    /// default, and `1` runs them all on one, in turn. `MEADOW_THREADS` says
    /// the same, and `threads = N` in a profile; both runtimes read it.
    #[arg(long, short = 'j', value_name = "N")]
    threads: Option<usize>,
    /// How large Glade's collector lets a thread's nursery grow: `8M`, `512K`,
    /// `1G`, or bytes, from 256K to 1G. 2M by default. A larger one collects
    /// less often and promotes less, which a program that allocates a great
    /// deal runs faster for, and its nursery pauses are longer.
    /// `MEADOW_GC_NURSERY` says the same in words of eight bytes, and
    /// `nursery = "…"` in a profile. Silo has no nursery.
    #[arg(long, value_name = "SIZE", value_parser = meadow::profile::nursery_size)]
    nursery: Option<u64>,
    /// After running, report the blocks the program left behind: what it
    /// still held at exit, by kind, and what held them. Silo counts by
    /// reference, so this is where a cycle shows up. `leaks = true` in a
    /// profile turns it on for a package.
    #[arg(long)]
    leaks: bool,
    /// Features of the package to build with, by the names its `[features]`
    /// gives them: `--features gpu,simd`. What a feature turns on is what
    /// `@cfg(feature = "gpu")` is written on.
    #[arg(long, value_name = "FEATURES", value_delimiter = ',')]
    features: Vec<String>,
    /// Build without the package's `default` features.
    #[arg(long)]
    no_default_features: bool,
    /// Build with every feature the package has.
    #[arg(long)]
    all_features: bool,
}

impl TargetArgs {
    /// These, with what the resolved profile says for whatever the command
    /// line left out -- the runtime above all, which only the profile knows.
    fn with(&self, profile: &Resolved) -> TargetArgs {
        // Said here, where what is to be built is settled and nothing has
        // been built: which of the package's features the build is with.
        meadow::package::ask_features(meadow::package::AskedFeatures {
            named: self.features.clone(),
            no_default: self.no_default_features,
            all: self.all_features,
        });
        TargetArgs {
            target: self
                .target
                .clone()
                .or_else(|| profile.target.map(|t| t.to_string())),
            runtime: profile.runtime,
            threads: self.threads.or(profile.threads),
            nursery: self.nursery.or(profile.nursery),
            leaks: self.leaks || profile.leaks,
            features: self.features.clone(),
            no_default_features: self.no_default_features,
            all_features: self.all_features,
        }
    }

    /// Tell the runtime what these ask for. The environment is how both
    /// runtimes are told, and a program Silo runs is a process
    /// of its own, which inherits it.
    fn configure(&self) {
        if let Some(n) = self.threads.filter(|n| *n > 0) {
            // Safety: before the program, its threads or any child of it.
            unsafe { std::env::set_var("MEADOW_THREADS", n.to_string()) };
        }
        if let Some(bytes) = self.nursery {
            // The collector counts in words. Safety: as above.
            unsafe { std::env::set_var("MEADOW_GC_NURSERY", (bytes / 8).to_string()) };
        }
        if self.leaks {
            // Safety: as above.
            unsafe { std::env::set_var("MEADOW_SILO_LEAKS", "1") };
        }
    }

    fn target(&self) -> Result<aot::Target, String> {
        let target = match &self.target {
            Some(name) => aot::Target::named(name),
            None => aot::Target::host(),
        }?;
        Ok(target.with_runtime(self.runtime))
    }
}

/// `--release` / `--debug` — the build profile — plus the individual switches
/// that override whatever the profile chose.
///
/// Debug is the default: `-O1`, and no exhaustiveness check, so a half-written
/// `match` still runs. Release is `-O2` and strict. Either can be overridden
/// per switch, here or in the package's `Meadow.toml`.
#[derive(clap::Args)]
struct ProfileArgs {
    /// Build with the release profile: `-O2`, and `match` must be exhaustive.
    #[arg(long, conflicts_with = "debug")]
    release: bool,
    /// Build with the debug profile (the default).
    #[arg(long)]
    debug: bool,
    /// Optimization level: 0, 1 or 2. Overrides the profile.
    #[arg(short = 'O', long = "opt-level", value_name = "LEVEL", value_parser = opt_level)]
    opt: Option<OptLevel>,
    /// Reject a non-exhaustive `match`, whatever the profile says.
    #[arg(long, conflicts_with = "lenient")]
    strict: bool,
    /// Allow a non-exhaustive `match`, whatever the profile says.
    #[arg(long)]
    lenient: bool,
    /// The runtime system: `glade` (the default) -- bytecode, run by the
    /// interpreter and the JIT or compiled ahead of time with `--aot`, and a
    /// garbage collector -- or `silo`, compiled all the way down by LLVM,
    /// counting references, and always an executable. `runtime = "…"` in a
    /// `[profile.<name>]` of `Meadow.toml` sets it for a package; this
    /// overrides that.
    #[arg(long, value_name = "RUNTIME", value_parser = ["glade", "silo"])]
    runtime: Option<String>,
    /// How Glade runs the program: `vm` (the bytecode interpreter), `jit`
    /// (which compiles what runs often to machine code as it goes) or `aot`
    /// (machine code compiled ahead of time, into an executable). Debug builds
    /// default to `jit` and release builds to `aot`; `backend = "..."` in a
    /// `[profile.<name>]` of `Meadow.toml` overrides that, and this overrides
    /// both. Silo is always compiled ahead of time.
    #[arg(long, value_name = "BACKEND", value_parser = backend, conflicts_with_all = ["jit", "aot"])]
    backend: Option<Backend>,
    /// `--backend jit`.
    #[arg(long, conflicts_with = "aot")]
    jit: bool,
    /// `--backend aot`: Glade, compiled ahead of time into an executable.
    #[arg(long, visible_alias = "native")]
    aot: bool,
    /// Turn on a flag for `@cfg(…)` to test: `--cfg fast`, `--cfg feature=gpu`.
    /// Repeat it for more; a manifest's `cfg = "…"` adds to these.
    #[arg(long = "cfg", value_name = "FLAG")]
    cfg: Vec<String>,
    /// Compile every definition of the package and its dependencies, not only
    /// what `main` reaches. `prune = false` in a `[profile.<name>]` of
    /// `Meadow.toml` does the same.
    #[arg(long)]
    no_prune: bool,
}

fn emit(s: &str) -> Result<Emit, String> {
    Emit::parse(s)
        .ok_or_else(|| format!("expected image, bytecode, asm, exe or expanded, got `{s}`"))
}

fn backend(s: &str) -> Result<Backend, String> {
    Backend::parse(s).ok_or_else(|| format!("expected vm, jit or aot, got `{s}`"))
}

fn opt_level(s: &str) -> Result<OptLevel, String> {
    OptLevel::parse(s).ok_or_else(|| format!("expected 0, 1 or 2, got `{s}`"))
}

impl ProfileArgs {
    fn profile(&self) -> Profile {
        if self.release {
            Profile::Release
        } else {
            Profile::Debug
        }
    }

    /// The switches named on the command line, which win over everything.
    fn overrides(&self) -> ProfileConfig {
        ProfileConfig {
            opt: self.opt,
            strictness: match (self.strict, self.lenient) {
                (true, _) => Some(Strictness::Strict),
                (_, true) => Some(Strictness::Lenient),
                _ => None,
            },
            backend: self
                .backend
                .or(self.jit.then_some(Backend::Jit))
                .or(self.aot.then_some(Backend::Aot)),
            prune: self.no_prune.then_some(false),
            cfg: (!self.cfg.is_empty())
                .then(|| meadow_compiler::intern::InternedString::from(self.cfg.join(","))),
            // Asked for on the command line with `--profile-to`, not here.
            profile: None,
            runtime: self.runtime.as_deref().and_then(aot::Runtime::named),
            // `-j`, `--leaks` and `--target` belong to the command, and win
            // over these when they are given: see `TargetArgs::with`.
            threads: None,
            // Asked for with `--nursery`, which `TargetArgs` reads.
            nursery: None,
            leaks: None,
            target: None,
        }
    }

    /// The profile, the package's `Meadow.toml`, and these flags, in that order
    /// of increasing authority.
    fn resolve(&self, path: &std::path::Path) -> Resolved {
        let silo = self.runtime.as_deref() == Some("silo");
        if silo && (self.backend.is_some() || self.jit || self.aot) {
            eprintln!(
                "error: Silo is compiled ahead of time by what it is, and has no backend to \
                 choose; `--backend`, `--jit` and `--aot` are Glade's"
            );
            std::process::exit(2);
        }
        Resolved::resolve(self.profile(), path, self.overrides())
    }
}

/// `--cek` — run on the CEK abstract machine instead of the bytecode VM.
///
/// The VM, with its JIT, is the default. The CEK is the specification of what a Meadow program
/// means, so if the two disagree it is right and the VM has a bug; this flag is
/// what makes that comparison available without a rebuild.
#[derive(clap::Args)]
struct EngineArgs {
    /// Evaluate with the CEK machine rather than the bytecode VM.
    #[arg(long, conflicts_with_all = ["jit", "aot", "backend"])]
    cek: bool,
}

impl EngineArgs {
    /// `profile`, told whether the CEK machine runs the program -- which
    /// `@cfg(backend = "cek")` asks.
    fn resolve(&self, mut profile: Resolved) -> Resolved {
        if self.cek {
            profile.options.cfg.backend = "cek";
        }
        profile
    }

    /// The machine that runs a program in this process, for `backend`. An
    /// `aot` backend's native code runs in a process of its own, so here --
    /// where tests run -- it is the JIT's.
    fn engine(&self, backend: Backend) -> Engine {
        if self.cek {
            return Engine::Cek;
        }
        match backend {
            Backend::Vm => Engine::Vm,
            Backend::Jit | Backend::Aot => Engine::Jit,
        }
    }
}

/// The stack the whole command runs on.
///
/// The compiler walks a program's structure recursively -- resolving,
/// inferring, expanding macros, lowering -- and a main thread's stack is 1 MB
/// on Windows: a few hundred nested `let`s, three hundred chained `+`s, or a
/// macro a hundred calls deep ran out of it and killed the process. This is
/// address space reserved, and committed only as deep as a program needs.
const STACK: usize = 256 << 20;

fn main() {
    let run = std::thread::Builder::new()
        .name("meadow".into())
        .stack_size(STACK)
        .spawn(command)
        .expect("the main thread starts");
    // A panic has said what it was already; the exit status is Rust's own.
    if run.join().is_err() {
        std::process::exit(101);
    }
}

fn command() {
    let cli = Cli::parse();
    // Said once, before anything resolves a dependency.
    meadow::package::set_policy(meadow::package::Policy {
        net: if cli.offline {
            meadow::git::Net::Offline
        } else {
            meadow::git::Net::Allowed
        },
        locked: cli.locked,
        update: false,
    });
    // Say what is being done -- `Compiling`, `Finished` -- except where the
    // terminal is not ours to write to: the REPL's, and an editor's, whose
    // language server and debugger would pour it into an output panel on every
    // keystroke.
    if !matches!(&cli.cmd, None | Some(Cmd::Lsp { .. }) | Some(Cmd::Dap)) {
        meadow::status::enable();
    }
    match cli.cmd {
        // No subcommand → interactive REPL.
        None => repl::Session::new().run(),
        Some(Cmd::Build {
            path,
            types,
            annotations,
            emit,
            packages,
            profile,
            target,
        }) => {
            // Asked for before anything compiles: the expander writes down
            // what each call produces as it goes, and the packages built are
            // compiled rather than read back.
            if emit.contains(&Emit::Expanded) {
                meadow_compiler::expand::record::start_produced();
            }
            let selected = select(&packages.selection(), &path);
            let profile = profile.resolve(&path);
            let target = target.with(&profile);
            match &selected.paths[..] {
                [one] => build(
                    one,
                    None,
                    Listing { types, annotations },
                    false,
                    None,
                    &emit,
                    profile,
                    &target,
                    false,
                ),
                many => build_many(
                    many,
                    Listing { types, annotations },
                    &emit,
                    profile,
                    &target,
                ),
            }
        }
        Some(Cmd::Run {
            path,
            types,
            package,
            profile,
            engine,
            gc_stats,
            profile_to,
            sample_every,
            sample_hz,
            train,
            gc,
            target,
            args,
        }) => {
            meadow_compiler::core::args::set(args);
            let resolved = profile.resolve(&path);
            let target = target.with(&resolved);
            target.configure();
            if let Some(gc) = gc {
                meadow_glade::heap::configure(meadow_glade::heap::GcConfig {
                    collector: match gc.as_str() {
                        "copying" => meadow_glade::heap::Collector::Copying,
                        _ => meadow_glade::heap::Collector::Generational,
                    },
                    ..meadow_glade::heap::GcConfig::from_env()
                });
            }
            let selection = Selection {
                packages: package.into_iter().collect(),
                ..Selection::default()
            };
            let selected = select(&selection, &path);
            let one = selected.one("run").unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(1);
            });
            let mut profile = engine.resolve(resolved);
            // Training is the JIT watching, whatever the profile would run.
            if train {
                profile.backend = Backend::Jit;
            }
            build(
                one,
                Some(if train {
                    Engine::Jit
                } else {
                    engine.engine(profile.backend)
                }),
                Listing {
                    types,
                    annotations: false,
                },
                gc_stats,
                profile_to.map(|to| {
                    (
                        to,
                        meadow_glade::sched::Sampling {
                            every: sample_every,
                            hz: sample_hz,
                            depth: meadow_glade::profile::DEPTH,
                        },
                    )
                }),
                &[],
                profile,
                &target,
                train,
            )
        }
        Some(Cmd::Exec {
            image,
            backend,
            opt,
            args,
        }) => {
            meadow_compiler::core::args::set(args);
            exec(
                &image,
                backend.unwrap_or(Backend::Jit),
                opt.unwrap_or(OptLevel::O1),
            );
        }
        Some(Cmd::Link {
            image,
            output,
            emit,
            opt,
            target,
        }) => link(
            &image,
            output,
            emit == "asm",
            opt.unwrap_or(OptLevel::O2),
            &target,
        ),
        Some(Cmd::Dis {
            path,
            asm,
            packages,
            profile,
            target,
        }) => {
            let selected = select(&packages.selection(), &path);
            let resolved = profile.resolve(&path);
            let arch = asm.then(|| exit_on_error(target.with(&resolved).target()).arch);
            disassemble(&selected.paths, resolved, arch)
        }
        Some(Cmd::Test {
            path,
            filter,
            exact,
            std,
            test_threads,
            no_capture,
            packages,
            profile,
            engine,
            target,
        }) => {
            let resolved = profile.resolve(&path);
            let target = target.with(&resolved);
            target.configure();
            match test::run(&test::Options {
                native: target
                    .target()
                    .is_ok_and(|t| t.runtime == aot::Runtime::Silo),
                threads: test_threads.filter(|n| *n > 0),
                no_capture,
                packages: packages.selection(),
                engine: engine.engine(resolved.backend),
                profile: engine.resolve(resolved),
                path,
                filter,
                exact,
                std,
            }) {
                Ok(true) => {}
                Ok(false) => std::process::exit(1),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Cmd::Dap) => {
            if let Err(e) = meadow::dap::run() {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Some(Cmd::Lsp { fetch, .. }) => {
            if fetch {
                meadow::editor::allow_fetching();
            }
            // Both shapes of the standard library: the bundle a package depends
            // on, and the modules it was bundled from — which is what lets a
            // `Std` source file be analysed as itself. One compile serves both.
            let (packages, _) = meadow::stdlib::std_packages(meadow::Options::debug());
            let (modules, _) = meadow::stdlib::std_modules(meadow::Options::debug());
            let modules = modules
                .into_iter()
                .map(|(dotted, pkg)| (dotted.to_string(), pkg))
                .collect();
            // And the sources on disk, so a definition inside `Std` is a file
            // the editor can open. Best-effort: without it, navigation stops at
            // the edge of the open document, which is where it used to stop
            // anyway.
            let src_root = meadow::stdlib::extract_sources();
            if let Err(e) = meadow_lsp::server::run(
                packages,
                modules,
                src_root,
                meadow::stdlib::MODULES,
                Some(meadow::editor::find_package),
            ) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Some(Cmd::Fmt {
            paths,
            check,
            stdout,
            width,
        }) => {
            let paths = if paths.is_empty() {
                vec![PathBuf::from(".")]
            } else {
                paths
            };
            match format::run(&format::Options {
                paths,
                check,
                stdout,
                width: (width > 0).then_some(width),
            }) {
                // `--check` is for CI: a file that needs formatting is a failure.
                Ok(changed) => {
                    if check && changed > 0 {
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Cmd::Add {
            what,
            branch,
            tag,
            rev,
            rename,
            path,
        }) => {
            let reference = match (branch, tag, rev) {
                (Some(b), _, _) => meadow::package::GitRef::Branch(b),
                (_, Some(t), _) => meadow::package::GitRef::Tag(t),
                (_, _, Some(r)) => meadow::package::GitRef::Rev(r),
                _ => meadow::package::GitRef::Default,
            };
            if let Err(e) = meadow::add::run(&meadow::add::Options {
                what,
                reference,
                rename,
                dir: path,
            }) {
                status::error(e);
                std::process::exit(1);
            }
        }
        Some(Cmd::Init {
            path,
            name,
            workspace,
        }) => match init::run(&init::Options {
            path,
            name,
            workspace,
        }) {
            Ok(made) => {
                if workspace {
                    println!("created a workspace at {}", made.root.display());
                } else {
                    println!("created package `{}` at {}", made.name, made.root.display());
                }
                for f in &made.files {
                    println!("  {}", nearby(f).display());
                }
                if let Some(ws) = &made.joined {
                    println!(
                        "added `{}` to the members of {}",
                        made.name,
                        nearby(ws).display()
                    );
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Some(Cmd::Cut {
            file,
            runtime,
            aot: ahead,
            opt,
            no_run,
            emit,
            args,
        }) => {
            let opt = match opt {
                0 => meadow_compiler::OptLevel::O0,
                1 => meadow_compiler::OptLevel::O1,
                _ => meadow_compiler::OptLevel::O2,
            };
            let ran = match aot::Runtime::named(&runtime) {
                _ if emit.is_some() => {
                    meadow::cut::emitted(&file, emit.as_deref().unwrap_or_default()).map(|text| {
                        print!("{text}");
                        0
                    })
                }
                Some(aot::Runtime::Glade) if no_run && ahead => {
                    meadow::cut::build_for_glade(&file, opt).map(|_| 0)
                }
                Some(aot::Runtime::Glade) if no_run => {
                    meadow::cut::kept_for_glade(&file).map(|()| 0)
                }
                Some(aot::Runtime::Silo) if no_run => {
                    meadow::cut::build_for_silo(&file, opt).map(|_| 0)
                }
                Some(aot::Runtime::Glade) if ahead => meadow::cut::build_for_glade(&file, opt)
                    .and_then(|exe| {
                        std::process::Command::new(&exe)
                            .args(&args)
                            .status()
                            .map(|s| s.code().unwrap_or(1))
                            .map_err(|e| format!("{}: {e}", exe.display()))
                    }),
                Some(aot::Runtime::Glade) => {
                    // The process is `meadow`, not the program: what the
                    // program was given is what followed `--`.
                    meadow_compiler::core::args::set(args);
                    meadow::cut::run_on_glade(&file).map(|()| 0)
                }
                Some(aot::Runtime::Silo) => {
                    meadow::cut::build_for_silo(&file, opt).and_then(|exe| {
                        // A library is built, and there is nothing to run.
                        if matches!(
                            exe.extension().and_then(|e| e.to_str()),
                            Some("dll" | "so" | "dylib")
                        ) {
                            meadow::status::status("Built", exe.display().to_string());
                            return Ok(0);
                        }
                        std::process::Command::new(&exe)
                            .args(&args)
                            .status()
                            .map(|s| s.code().unwrap_or(1))
                            .map_err(|e| format!("{}: {e}", exe.display()))
                    })
                }
                None => Err(format!("`{runtime}` is not a runtime: `glade` or `silo`")),
            };
            match ran {
                Ok(0) => {}
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Cmd::Clean {
            path,
            profile,
            dry_run,
        }) => match meadow::clean::run(&path, profile.as_deref(), dry_run) {
            Ok(out) if out.removed.is_empty() => {
                status::status("Clean", "nothing to remove".to_string());
            }
            Ok(out) => {
                let what = if dry_run { "Would remove" } else { "Removed" };
                for dir in &out.removed {
                    status::status(
                        what,
                        format!("{} ({})", dir.display(), meadow::clean::bytes(out.bytes)),
                    );
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Some(Cmd::Update {
            only,
            path,
            dry_run,
        }) => {
            if let Err(e) = meadow::update::run(&meadow::update::Options {
                dir: path,
                only,
                dry_run,
            }) {
                status::error(e);
                std::process::exit(1);
            }
        }
        Some(Cmd::PrecompileStd { out, target }) => {
            if let Err(e) = meadow::stdlib::precompile(&out, target.as_deref()) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Some(Cmd::Toolchain { rest }) => {
            let what = rest.first().map(String::as_str).unwrap_or("update");
            eprintln!("error: `meadow` no longer manages the toolchain; `meadowup` does.");
            eprintln!();
            eprintln!("  meadowup {what}");
            eprintln!();
            eprintln!("`meadow` is the build system, as `cargo` is, and `meadowup` looks");
            eprintln!("after which version of it you have, as `rustup` does. If meadowup");
            eprintln!("is not installed yet, re-run the installer:");
            eprintln!();
            eprintln!(
                "  curl -fsSL https://raw.githubusercontent.com/meadow-lang/meadow/master/scripts/meadowup-init.sh | sh"
            );
            std::process::exit(1);
        }
    }
}

/// A path as a person would write it: without Windows' `\?\` prefix.
fn shown(path: &std::path::Path) -> String {
    meadow::dap::session::plain_path(&path.to_string_lossy())
}

/// `result`'s value, or its error said and the process ended.
fn exit_on_error<T>(result: Result<T, String>) -> T {
    result.unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    })
}

/// `path` as seen from the current directory, when it is inside it: `init`
/// finds a workspace by its full path, and says so more briefly.
fn nearby(path: &std::path::Path) -> PathBuf {
    let here = std::env::current_dir()
        .ok()
        .and_then(|d| std::fs::canonicalize(d).ok());
    let full = std::fs::canonicalize(path).ok();
    match (here, full) {
        (Some(here), Some(full)) => match full.strip_prefix(&here) {
            Ok(rest) if rest.as_os_str().is_empty() => PathBuf::from("."),
            Ok(rest) => rest.to_path_buf(),
            Err(_) => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// Discover, compile and link the package at `path`; with `engine`, also
/// evaluate its entry point. Exits non-zero if any diagnostic was produced or
/// evaluation failed.
fn build(
    path: &std::path::Path,
    engine: Option<Engine>,
    listing: Listing,
    gc_stats: bool,
    sampling: Option<(PathBuf, meadow_glade::sched::Sampling)>,
    emit: &[Emit],
    profile: Resolved,
    target: &TargetArgs,
    train: bool,
) {
    let profile = for_target(profile, target);
    let started = std::time::Instant::now();
    let out = pipeline::build(path, profile.options);

    for d in &out.diagnostics {
        status::diagnostic(d);
    }
    let name = out
        .package
        .as_ref()
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| shown(path));

    let Some(linked) = out.linked else {
        could_not_compile(&name, out.diagnostics.len().max(1));
        std::process::exit(1);
    };
    // A program with errors is not run, and nothing is written for it: what
    // it would do is not what was written. What was worked out is still worth
    // showing a build that asked for it.
    if !out.diagnostics.is_empty() {
        if engine.is_none() {
            listing.print(&linked);
        }
        could_not_compile(&name, out.diagnostics.len());
        std::process::exit(1);
    }
    finished(&profile, started);
    finish(
        linked,
        &out.package,
        out.fingerprint,
        engine,
        listing,
        gc_stats,
        sampling,
        emit,
        profile,
        target,
        train,
    );
}

/// How a run names what it runs: the package's `main`.
fn linked_entry(package: &Option<String>) -> String {
    match package {
        Some(name) => format!("{name}::main"),
        None => "main".to_string(),
    }
}

/// What a build prints on stdout besides the program's own output.
#[derive(Clone, Copy)]
struct Listing {
    /// Every top-level binding's type, in the packages being built.
    types: bool,
    /// Every node's type.
    annotations: bool,
}

impl Listing {
    fn print(self, linked: &meadow::linker::LinkedProgram) {
        if self.types || self.annotations {
            print!("{}", linked.dump());
        }
        if self.annotations {
            print!("{}", linked.annotations());
        }
    }
}

/// `    Finished `debug` profile [O1, glade jit] in 0.42s`
fn finished(profile: &Resolved, started: std::time::Instant) {
    status::status(
        "Finished",
        format!(
            "`{}` profile [{}, {}] in {}",
            profile.profile.name(),
            profile.opt().name(),
            profile.how(),
            status::elapsed(started.elapsed())
        ),
    );
}

/// `error: could not compile `app` due to 2 previous errors`
fn could_not_compile(name: &str, errors: usize) {
    status::error(format!(
        "could not compile `{name}` due to {errors} previous error{}",
        if errors == 1 { "" } else { "s" }
    ));
}

/// `meadow exec`: run the image at `path` on `backend`.
fn exec(path: &std::path::Path, backend: Backend, opt: OptLevel) {
    let image = read_image(path);
    let engine = match backend {
        Backend::Vm => Engine::Vm,
        Backend::Jit => Engine::Jit,
        Backend::Aot => {
            eprintln!(
                "error: an image runs on `vm` or `jit`; `meadow link` makes it an executable"
            );
            std::process::exit(1);
        }
    };
    let result = match runtime::native(&image, engine, opt) {
        Ok(jit) => runtime::run_image_with_stats(&image, jit.as_ref()).0,
        Err(e) => Err(e),
    };
    match result {
        // `main` gives nothing back: what it had to say, it printed.
        Ok(_) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// `meadow link`: the image at `path`, as an executable -- or, with `asm`, as
/// the text of the native code the executable would be linked from.
fn link(
    path: &std::path::Path,
    output: Option<PathBuf>,
    asm: bool,
    opt: OptLevel,
    target: &TargetArgs,
) {
    let image = read_image(path);
    let made = target.target().and_then(|target| {
        if asm {
            let out = output.unwrap_or_else(|| path.with_extension("s"));
            let text = listing::asm(&image, target.arch, opt);
            return artifacts::write_text(&out, &text).map(|()| ("asm", out));
        }
        let exe = output.unwrap_or_else(|| {
            path.with_extension(target.format.exe_suffix().trim_start_matches('.'))
        });
        aot::link_image(&image, opt, target, &Default::default(), &exe).map(|()| ("native", exe))
    });
    match made {
        Ok((what, file)) => eprintln!("{what}: {}", file.display()),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn read_image(path: &std::path::Path) -> meadow_bytecode::Program {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("error: could not read {}: {e}", path.display());
        std::process::exit(1);
    });
    meadow_bytecode::image::decode(&bytes).unwrap_or_else(|e| {
        eprintln!("error: {}: {e}", path.display());
        std::process::exit(1);
    })
}

/// [`build`] for several packages of a workspace, sharing what they have in
/// common. Nothing runs: `run` takes one package.
fn build_many(
    paths: &[PathBuf],
    listing: Listing,
    emit: &[Emit],
    profile: Resolved,
    target: &TargetArgs,
) {
    let profile = for_target(profile, target);
    let started = std::time::Instant::now();
    let paths: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
    let out = pipeline::build_each(&paths, profile.options);
    for d in &out.diagnostics {
        status::diagnostic(d);
    }
    if out.each.is_empty() {
        could_not_compile("the workspace", out.diagnostics.len().max(1));
        std::process::exit(1);
    }
    if !out.diagnostics.is_empty() {
        for built in &out.each {
            let linked = built.linked.as_ref().expect("each is linked");
            listing.print(linked);
        }
        could_not_compile("the workspace", out.diagnostics.len());
        std::process::exit(1);
    }
    finished(&profile, started);
    for built in out.each {
        let linked = built.linked.expect("each is linked");
        finish(
            linked,
            &built.package,
            built.fingerprint,
            None,
            listing,
            false,
            None,
            emit,
            profile,
            target,
            false,
        );
    }
    if !out.diagnostics.is_empty() {
        std::process::exit(1);
    }
}

/// `profile`, told the architecture an executable for another is compiled
/// for: that is the `arch` its `@cfg(…)` sees.
fn for_target(mut profile: Resolved, target: &TargetArgs) -> Resolved {
    if profile.native()
        && let Ok(t) = target.target()
    {
        profile.options.cfg.arch = match t.arch {
            meadow_glade::codegen::Arch::Aarch64 => "aarch64",
            meadow_glade::codegen::Arch::X86_64 => "x86_64",
        };
    }
    profile
}

/// Everything [`build`] does once a package is linked: print it, write its
/// image and executable -- or what `emit` asks for instead -- and, with
/// `engine`, run it.
fn finish(
    linked: meadow::linker::LinkedProgram,
    package: &Option<(PathBuf, meadow_compiler::intern::InternedString)>,
    fingerprint: Option<u64>,
    engine: Option<Engine>,
    listing: Listing,
    gc_stats: bool,
    sampling: Option<(PathBuf, meadow_glade::sched::Sampling)>,
    emit: &[Emit],
    profile: Resolved,
    target: &TargetArgs,
    train: bool,
) {
    listing.print(&linked);
    if !emit.is_empty() && package.is_none() {
        eprintln!(
            "error: `--emit` writes under a package's `target` directory, and a lone file has none; \
             `meadow dis` prints its bytecode, and `meadow dis --asm` its native code"
        );
        std::process::exit(1);
    }
    if emit.contains(&Emit::Expanded)
        && let Some((root, _)) = package
    {
        let produced = meadow_compiler::expand::record::take_produced();
        if !produced.is_empty() || emit == [Emit::Expanded] {
            match meadow::expanded::write(root, profile.profile, &produced) {
                Ok(report) => eprint!("{report}"),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        // Nothing else was asked for, and nothing else is made.
        if emit == [Emit::Expanded] {
            return;
        }
    }
    // A library has nothing to run. Its image would be the whole standard
    // library and everything else it could call, unpruned -- no entry point
    // to prune from -- and nothing reads it: what depends on the library links
    // against its compiled package, not its image. So building one is
    // compiling and checking it, unless `--emit` asks for more.
    if engine.is_none() && emit.is_empty() && linked.program.entry.is_none() {
        return;
    }
    let aot = profile.native() && engine != Some(Engine::Cek);
    // What is written: what was asked for, or else the image -- and for an
    // `aot` build, its executable.
    let wants = |kind: Emit| match kind {
        _ if !emit.is_empty() => emit.contains(&kind),
        Emit::Image => true,
        Emit::Exe => aot,
        Emit::Bytecode | Emit::Asm | Emit::Expanded => false,
    };

    // What runs: the entry point and what it reaches, unless the profile says
    // to keep everything.
    let program = profile.program(&linked.program);

    // What the VM runs is written under the package's `target` directory
    // whenever it is made: by `build`, and by `run` on the VM.
    let image = match engine {
        None | Some(Engine::Vm) | Some(Engine::Jit) => {
            match runtime::compile(&program, profile.opt()) {
                Ok(image) => {
                    if let Some((root, name)) = package {
                        let written = (|| {
                            if wants(Emit::Image) {
                                artifacts::write_image(root, profile.profile, name, &image)?;
                            }
                            if wants(Emit::Bytecode) {
                                let path =
                                    artifacts::bytecode_text_path(root, profile.profile, name);
                                artifacts::write_text(&path, &listing::bytecode(&image))?;
                                eprintln!("bytecode: {}", shown(&path));
                            }
                            if wants(Emit::Asm) {
                                let t = target.target()?;
                                let path = aot::write_asm(
                                    root,
                                    profile.profile,
                                    profile.opt(),
                                    name,
                                    &image,
                                    t,
                                )?;
                                eprintln!("asm: {}", shown(&path));
                            }
                            Ok::<(), String>(())
                        })();
                        if let Err(e) = written {
                            eprintln!("error: {e}");
                            std::process::exit(1);
                        }
                    }
                    Some(image)
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Engine::Cek) => None,
    };

    // Machine code, linked into an executable -- and, for `run`, run: what an
    // `aot` backend is, unless `--cek` asked for the CEK machine.
    let exe = match engine {
        None => wants(Emit::Exe),
        Some(_) => aot,
    };
    if let (true, Some(image)) = (exe, &image) {
        let exe = target.target().and_then(|target| {
            let (root, name) = package
                .as_ref()
                .ok_or("a native executable needs a package to put it in")?;
            // The native backend compiles AxCut itself, not the image.
            if target.runtime == aot::Runtime::Silo {
                return aot::build_native(
                    root,
                    profile.profile,
                    profile.opt(),
                    name,
                    &program,
                    target,
                    fingerprint,
                );
            }
            // What a training run saw the calls enter, if one was made of
            // this image: see `meadow::pgo`.
            let at = meadow::pgo::path(root, profile.profile.name());
            let calls = match meadow::pgo::read(&at, image)? {
                meadow::pgo::Found::Calls(calls) => {
                    status::status("Guided", format!("by {}", shown(&at)));
                    calls
                }
                meadow::pgo::Found::Stale => {
                    status::note(
                        "Unguided",
                        format!(
                            "{} is of an older build; `meadow run --train` makes a new one",
                            shown(&at)
                        ),
                    );
                    Default::default()
                }
                meadow::pgo::Found::None => Default::default(),
            };
            aot::build(
                root,
                profile.profile,
                profile.opt(),
                name,
                image,
                target,
                &calls,
            )
        });
        match exe {
            Ok(exe) if engine.is_none() => {
                status::note("Executable", shown(&exe));
            }
            Ok(exe) => {
                status::status("Running", format!("`{}`", shown(&exe)));
                let status = std::process::Command::new(&exe)
                    .args(meadow_compiler::core::args::get())
                    .status()
                    .unwrap_or_else(|e| {
                        eprintln!("error: could not run {}: {e}", exe.display());
                        std::process::exit(1);
                    });
                std::process::exit(status.code().unwrap_or(1));
            }
            // A release build of a lone file, or on a machine that cannot link
            // one: the JIT runs it, unless `aot` was asked for by name.
            Err(e) if profile.fallback().is_some() && emit.is_empty() => {
                if package.is_some() {
                    status::warning(format!("no native executable: {e}"));
                    if engine.is_some() {
                        status::note("Falling back", "to the JIT; `--aot` makes this an error");
                    }
                }
            }
            Err(e) => {
                status::error(e);
                std::process::exit(1);
            }
        }
    }

    if let Some(engine) = engine {
        let entry = linked_entry(&package.as_ref().map(|(_, n)| n.to_string()));
        status::status(
            "Running",
            format!(
                "`{entry}` on the {}",
                match engine {
                    Engine::Cek => "CEK machine",
                    Engine::Vm => "VM",
                    Engine::Jit => "JIT",
                }
            ),
        );
        // Sampling wants an image that carries debug info, which is what its
        // frames are named by, so it compiles its own -- the same instructions,
        // and more in the image beside them.
        // `--profile-to` says where; `profile = true` in the manifest asks for
        // one without saying, and gets it beside the build.
        let sampling = sampling.or_else(|| {
            profile.sample.then(|| {
                let to = match package {
                    Some((root, _)) => root
                        .join("target")
                        .join(profile.profile.name())
                        .join("profile.folded"),
                    None => std::path::PathBuf::from("profile.folded"),
                };
                (
                    to,
                    meadow_glade::sched::Sampling {
                        every: None,
                        hz: meadow_glade::profile::HZ,
                        depth: meadow_glade::profile::DEPTH,
                    },
                )
            })
        });
        if let Some((to, how)) = sampling {
            let result = match runtime::compile_for_profile(&program, profile.opt()) {
                Err(e) => Err(e),
                Ok(image) => match runtime::native(&image, engine, profile.opt()) {
                    Err(e) => Err(e),
                    Ok(jit) => {
                        let (result, profile, stats) =
                            runtime::run_image_sampled(&image, jit.as_ref(), how);
                        #[cfg(feature = "profile-alloc")]
                        eprint!(
                            "{}{}",
                            meadow::samples::instructions(&stats.ops),
                            meadow::samples::feedback(&stats.feedback, &image)
                        );
                        #[cfg(feature = "profile-alloc")]
                        if !stats.sites.is_empty() {
                            // Beside the samples: where the garbage came from.
                            let beside = to.with_extension("alloc");
                            let text = meadow::samples::allocation(&stats.sites, &image);
                            if std::fs::write(&beside, text).is_ok() {
                                status::status("Allocation", beside.display().to_string());
                            }
                        }
                        let _ = &stats;
                        match profile {
                            None => eprintln!("profile: nothing was sampled"),
                            Some(p) => {
                                eprintln!("{}", meadow::samples::summary(&p));
                                let folded = meadow::samples::folded(&p, &image);
                                match std::fs::write(&to, folded) {
                                    Ok(()) => status::status("Profile", to.display().to_string()),
                                    Err(e) => eprintln!("profile: {}: {e}", to.display()),
                                }
                            }
                        }
                        result
                    }
                },
            };
            match result {
                // `main` gives nothing back: what it had to say, it printed.
                Ok(_) => {}
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }

        let (result, stats) = match &image {
            Some(image) => match runtime::native(image, engine, profile.opt()) {
                Ok(jit) => {
                    let ran = runtime::run_image_with_stats(image, jit.as_ref());
                    if train {
                        train_from(jit.as_ref(), image, package.as_ref(), &profile);
                    }
                    ran
                }
                Err(e) => (Err(e), None),
            },
            None => runtime::run_with_stats(&program, engine, profile.opt()),
        };
        if gc_stats {
            match stats {
                Some(stats) => eprintln!("{stats}"),
                None => {
                    eprintln!("gc: the CEK machine reference-counts, so there is nothing to report")
                }
            }
        }
        match result {
            // `main` gives nothing back: what it had to say, it printed.
            Ok(_) => {}
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
}

/// After `meadow run --train`: write what the JIT saw each call site enter,
/// for an `aot` build of the same image. See `meadow::pgo`.
fn train_from(
    jit: Option<&meadow_glade::jit::Native>,
    image: &meadow_bytecode::Program,
    package: Option<&(PathBuf, meadow_compiler::intern::InternedString)>,
    profile: &Resolved,
) {
    let (Some(jit), Some((root, _))) = (jit, package) else {
        eprintln!("error: training needs a package to write its profile in, and the JIT");
        std::process::exit(1);
    };
    let calls = jit.calls();
    let to = meadow::pgo::path(root, profile.profile.name());
    match meadow::pgo::write(&to, image, &calls) {
        Ok(()) => status::status(
            "Trained",
            format!("{} call sites, in {}", calls.len(), shown(&to)),
        ),
        Err(e) => eprintln!("error: {e}"),
    }
}

/// Print the bytecode the VM would run — the back end's output, addresses and
/// all — for each package in `paths`; or, for `arch`, the native code an
/// `aot` build compiles it to.
fn disassemble(paths: &[PathBuf], profile: Resolved, arch: Option<meadow_glade::codegen::Arch>) {
    let refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
    let out = pipeline::build_each(&refs, profile.options);
    for d in &out.diagnostics {
        eprintln!("{}: {}", d.filename, d.msg);
    }
    if out.each.is_empty() {
        std::process::exit(1);
    }
    let several = out.each.len() > 1;
    for built in out.each {
        let linked = built.linked.expect("each is linked");
        if several && let Some((_, name)) = &built.package {
            println!("=== {name} ===");
        }
        match runtime::compile(&profile.program(&linked.program), profile.opt()) {
            Ok(image) => match arch {
                Some(arch) => print!("{}", listing::asm(&image, arch, profile.opt())),
                None => print!("{}", listing::bytecode(&image)),
            },
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
    if !out.diagnostics.is_empty() {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// `init` takes the current directory when told nothing, which is the way
    /// it will usually be run.
    #[test]
    fn init_defaults_to_here() {
        let parsed = Cli::try_parse_from(["meadow", "init"]).expect("bare `init`");
        match parsed.cmd {
            Some(Cmd::Init { path, name, .. }) => {
                assert_eq!(path, PathBuf::from("."));
                assert_eq!(name, None);
            }
            other => panic!("expected `init`, got {:?}", other.is_some()),
        }

        let parsed = Cli::try_parse_from(["meadow", "init", "pkg", "--name", "myPkg"]).unwrap();
        match parsed.cmd {
            Some(Cmd::Init { path, name, .. }) => {
                assert_eq!(path, PathBuf::from("pkg"));
                assert_eq!(name.as_deref(), Some("myPkg"));
            }
            _ => panic!("expected `init`"),
        }
    }

    /// The arguments a language client actually launches us with.
    ///
    /// `vscode-languageclient` appends `--stdio` to the command it was given
    /// whenever the transport is stdio, and other clients add
    /// `--clientProcessId=<pid>`. Rejecting either made `meadow lsp` exit with
    /// code 2 before reading a byte, and the editor reported that as
    /// `write EPIPE` — a message about its own failed write, naming neither the
    /// argument nor the exit code. Nothing about the language server itself was
    /// wrong, which is why it took a log file to find.
    #[test]
    fn lsp_accepts_what_an_editor_passes() {
        for args in [
            vec!["meadow", "lsp"],
            vec!["meadow", "lsp", "--stdio"],
            vec!["meadow", "lsp", "--clientProcessId=1234"],
            vec!["meadow", "lsp", "--stdio", "--clientProcessId=1234"],
        ] {
            let parsed = Cli::try_parse_from(&args);
            assert!(
                parsed.is_ok(),
                "`{}` should start the server, got {}",
                args.join(" "),
                parsed.err().map(|e| e.to_string()).unwrap_or_default()
            );
            assert!(matches!(parsed.unwrap().cmd, Some(Cmd::Lsp { .. })));
        }
    }

    #[test]
    fn an_argument_we_do_not_know_is_still_rejected() {
        // Tolerating the two above is deliberate, not a blanket `allow_hyphen_values`.
        assert!(Cli::try_parse_from(["meadow", "lsp", "--socket=9257"]).is_err());
    }
}
