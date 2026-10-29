//! Build **profiles** — the driver-level names for a bundle of compiler options.
//!
//! A profile is what a user selects (`meadow build --release`); it expands to a
//! [`meadow_compiler::Options`] before anything reaches the compiler. Keeping the
//! bundle here means the compiler itself only ever sees individual switches, and
//! new ones (optimization levels, debug info) can join a profile without the CLI
//! growing a flag each.
//!
//! Three layers, each overriding the one before it:
//!
//! 1. the profile's built-in meaning — [`Profile::options`];
//! 2. the package's `[profile.<name>]` section, if it has a manifest -- or its
//!    workspace root's, if it is in a workspace;
//! 3. flags on the command line.
//!
//! [`Resolved`] is what that produces. The built-in layer is what makes a
//! manifest optional, and the manifest layer is what lets a package say
//! `opt-level = 2` in debug builds without every `meadow run` needing a flag.

use crate::package::{Manifest, ProfileConfig};
use meadow_compiler::{OptLevel, Options, Strictness};
use std::path::Path;

/// How a built program is run.
///
/// Not a compiler option: the program is the same bytecode whichever runs it.
/// Chosen by a profile, a package's `backend = ...` in `[profile.<name>]`, or
/// `--backend` -- in that order of authority, as everything in a profile is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// The bytecode interpreter.
    Vm,
    /// The interpreter, compiling the blocks that run often to machine code as
    /// it goes -- see `meadow_glade::jit`.
    #[default]
    Jit,
    /// Machine code compiled ahead of time, linked into an executable -- see
    /// [`crate::aot`].
    Aot,
}

impl Backend {
    pub fn parse(s: &str) -> Option<Backend> {
        match s {
            "vm" => Some(Backend::Vm),
            "jit" => Some(Backend::Jit),
            "aot" | "native" => Some(Backend::Aot),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Backend::Vm => "vm",
            Backend::Jit => "jit",
            Backend::Aot => "aot",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// The edit-run loop: compile fast, check less.
    #[default]
    Debug,
    /// Shipping: optimize, and every `match` must be exhaustive.
    Release,
}

impl Profile {
    /// What this profile means before a manifest or a flag has had its say.
    pub const fn options(self) -> Options {
        match self {
            Profile::Debug => Options::debug(),
            Profile::Release => Options::release(),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Profile::Debug => "debug",
            Profile::Release => "release",
        }
    }

    /// How this profile runs a program before a manifest or a flag says
    /// otherwise: debug on the interpreter and the JIT, which start at once;
    /// release as an executable, compiled ahead of time.
    pub const fn backend(self) -> Backend {
        match self {
            Profile::Debug => Backend::Jit,
            Profile::Release => Backend::Aot,
        }
    }
}

/// A profile plus the overrides that apply to it.
///
/// Carrying the profile alongside the options keeps the *name* available for
/// messages and for build directories, which the expanded options no longer
/// say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Resolved {
    pub profile: Profile,
    pub options: Options,
    pub backend: Backend,
    /// Whether a flag or the manifest named `backend`, rather than the profile
    /// supplying it. A backend nobody asked for gives way when it cannot be
    /// had -- see [`Resolved::fallback`].
    pub backend_named: bool,
    /// Compile only the definitions the entry point reaches, rather than every
    /// definition of the package and its dependencies -- `Std` included, which
    /// a small program otherwise carries whole (see `meadow_core::prune`). On
    /// in every profile; `prune = false` in a `[profile.<name>]`, or
    /// `--no-prune`, turns it off. `meadow test` and the debugger never prune:
    /// they start from more places than one.
    pub prune: bool,
    /// `profile = true` in the manifest's `[profile.<name>]`: sample the run
    /// and write folded stacks beside the build. `--profile-to` says where
    /// instead, and asking for one on the command line does not need this.
    pub sample: bool,
    /// The runtime system -- Glade, unless the command line or the manifest
    /// said Silo. `backend` is Glade's, and means nothing with Silo, which is
    /// always compiled ahead of time: see [`Resolved::native`].
    pub runtime: crate::aot::Runtime,
    /// `threads` from the manifest: `-j`'s default.
    pub threads: Option<usize>,
    /// `nursery` from the manifest, in bytes: `--nursery`'s default.
    pub nursery: Option<u64>,
    /// `cycles` from the manifest: how Silo finds cycles.
    pub cycles: Option<meadow_llvm::Cycles>,
    /// `leaks = true` from the manifest: `--leaks`'s default.
    pub leaks: bool,
    /// `target` from the manifest: `--target`'s default.
    pub target: Option<meadow_compiler::intern::InternedString>,
}

impl Resolved {
    /// Just the built-in meaning of `profile`.
    pub const fn new(profile: Profile) -> Resolved {
        let mut options = profile.options();
        options.cfg.profile = profile.name();
        options.cfg.backend = profile.backend().name();
        Resolved {
            profile,
            options,
            backend: profile.backend(),
            backend_named: false,
            prune: true,
            sample: false,
            runtime: crate::aot::Runtime::Glade,
            threads: None,
            nursery: None,
            cycles: None,
            leaks: false,
            target: None,
        }
    }

    /// Layer the package manifest at `path` over the profile, then `flags` over
    /// that.
    ///
    /// `path` is whatever the user named on the command line — a package
    /// directory or a single `.mw` file. A package with no manifest, or one
    /// that says nothing about this profile, simply keeps the built-in meaning.
    ///
    /// In a workspace the manifest is the workspace root's, whichever member
    /// `path` names: every member builds with the same profiles.
    pub fn resolve(profile: Profile, path: &Path, flags: ProfileConfig) -> Resolved {
        let from_manifest = crate::workspace::Workspace::find(path)
            .ok()
            .flatten()
            .map(|ws| ws.manifest)
            .or_else(|| Manifest::find(path))
            .map(|m| m.profile(profile.name()))
            .unwrap_or_default();
        let runtime = flags.runtime.or(from_manifest.runtime).unwrap_or_default();
        // A backend is Glade's to choose. Silo has none: it is compiled ahead
        // of time by what it is.
        if runtime == crate::aot::Runtime::Silo && from_manifest.backend.is_some() {
            crate::status::warning(format!(
                "`[profile.{}]` picks Silo, which is always compiled ahead of time; \
                 its `backend` is Glade's, and is ignored",
                profile.name()
            ));
        }
        let named = flags.backend.or(from_manifest.backend);
        let backend = named.unwrap_or(profile.backend());
        let mut options = flags.apply(from_manifest.apply(profile.options()));
        // What `@cfg(profile = …)` and `@cfg(backend = …)` see: this build's.
        options.cfg.profile = profile.name();
        options.cfg.backend = match runtime {
            crate::aot::Runtime::Silo => "silo",
            crate::aot::Runtime::Glade => backend.name(),
        };
        Resolved {
            profile,
            options,
            backend,
            backend_named: named.is_some(),
            prune: flags.prune.or(from_manifest.prune).unwrap_or(true),
            sample: from_manifest.profile.unwrap_or(false),
            runtime,
            threads: flags.threads.or(from_manifest.threads),
            nursery: from_manifest.nursery,
            cycles: from_manifest.cycles,
            leaks: flags.leaks.or(from_manifest.leaks).unwrap_or(false),
            target: flags.target.or(from_manifest.target),
        }
    }

    /// What runs the program when `backend` cannot -- `aot` without a package
    /// to put an executable in, or without a linker or runtime library to make
    /// one: the JIT, if the backend was only the profile's default, and
    /// nothing, if someone asked for it.
    pub fn fallback(self) -> Option<Resolved> {
        let glade_aot = self.runtime == crate::aot::Runtime::Glade && self.backend == Backend::Aot;
        (glade_aot && !self.backend_named).then_some(Resolved {
            backend: Backend::Jit,
            ..self
        })
    }

    /// Whether the program is compiled ahead of time into an executable: on
    /// Silo always, and on Glade when its backend is `aot`.
    pub fn native(self) -> bool {
        self.runtime == crate::aot::Runtime::Silo || self.backend == Backend::Aot
    }

    /// What runs the program, as a build reports it: the runtime, and on
    /// Glade the backend -- `glade vm`, `glade jit`, `glade aot` -- or `silo`,
    /// which has one way to run a program.
    pub const fn how(self) -> &'static str {
        match (self.runtime, self.backend) {
            (crate::aot::Runtime::Silo, _) => "silo",
            (crate::aot::Runtime::Glade, Backend::Vm) => "glade vm",
            (crate::aot::Runtime::Glade, Backend::Jit) => "glade jit",
            (crate::aot::Runtime::Glade, Backend::Aot) => "glade aot",
        }
    }

    pub const fn opt(self) -> OptLevel {
        self.options.opt
    }

    pub const fn strictness(self) -> Strictness {
        self.options.strictness
    }

    /// `program`, pruned to what its entry point reaches if this profile
    /// prunes, and as it is if not.
    pub fn program<'a>(
        self,
        program: &'a meadow_compiler::core::Program,
    ) -> std::borrow::Cow<'a, meadow_compiler::core::Program> {
        if self.prune {
            std::borrow::Cow::Owned(meadow_compiler::core::prune::prune(program))
        } else {
            std::borrow::Cow::Borrowed(program)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flag_beats_the_manifest_beats_the_profile() {
        let base = Profile::Debug.options();
        assert_eq!(base.opt, OptLevel::O1);

        let manifest = ProfileConfig {
            opt: Some(OptLevel::O2),
            ..ProfileConfig::default()
        };
        assert_eq!(manifest.apply(base).opt, OptLevel::O2);
        // Untouched by a section that says nothing about it.
        assert_eq!(manifest.apply(base).strictness, base.strictness);

        let flag = ProfileConfig {
            opt: Some(OptLevel::O0),
            ..ProfileConfig::default()
        };
        assert_eq!(flag.apply(manifest.apply(base)).opt, OptLevel::O0);
    }
}

/// The smallest and the largest a nursery may be asked to grow to, in bytes:
/// below the first a program collects more than it runs, and past the second
/// a nursery collection is no longer a short pause.
pub const NURSERY_MIN: u64 = 256 << 10;
pub const NURSERY_MAX: u64 = 1 << 30;

/// A nursery size as `--nursery` and `nursery = "…"` write it -- bytes, or
/// with a unit: `512K`, `8M`, `1G`, and `KiB`, `MiB`, `GiB` or `KB`, `MB`,
/// `GB` for the same, all powers of 1024 -- or what is wrong with it. One
/// outside [`NURSERY_MIN`] to [`NURSERY_MAX`] is refused.
pub fn nursery_size(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let digits = t.len() - t.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let (number, unit) = t.split_at(digits);
    let shift = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        _ => {
            return Err(format!(
                "`{text}` is not a size: write `8M`, `512K`, `1G`, or bytes"
            ));
        }
    };
    let bytes = number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_shl(shift).filter(|b| b >> shift == n))
        .ok_or_else(|| format!("`{text}` is not a size: write `8M`, `512K`, `1G`, or bytes"))?;
    if !(NURSERY_MIN..=NURSERY_MAX).contains(&bytes) {
        return Err(format!(
            "a nursery of `{text}` is outside 256K to 1G, which is what it may be"
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod nursery_tests {
    use super::*;

    #[test]
    fn a_size_is_bytes_or_has_a_unit_and_is_within_the_range() {
        assert_eq!(nursery_size("8M"), Ok(8 << 20));
        assert_eq!(nursery_size("8MiB"), Ok(8 << 20));
        assert_eq!(nursery_size(" 512 kb "), Ok(512 << 10));
        assert_eq!(nursery_size("1G"), Ok(1 << 30));
        assert_eq!(nursery_size("2097152"), Ok(2 << 20));
        assert!(nursery_size("64K").is_err(), "too small");
        assert!(nursery_size("2G").is_err(), "too large");
        assert!(nursery_size("lots").is_err());
        assert!(nursery_size("8X").is_err());
        assert!(nursery_size("").is_err());
    }
}
