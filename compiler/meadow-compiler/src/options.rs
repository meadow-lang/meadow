//! Compiler options — the individual switches a build turns on or off.
//!
//! Two axes, and they are independent. How hard the compiler works to make the
//! program fast ([`OptLevel`]), and how much it insists on before it will build
//! at all ([`Strictness`]). A driver bundles them into a named profile —
//! `meadow build --release` — but the compiler only ever sees the switches, and
//! either can be set on its own.
//!
//! They used to be one thing: `--release` meant "check `match` exhaustiveness",
//! which is a diagnostic and not an optimisation at all.

pub use meadow_core::OptLevel;
use meadow_intern::InternedString;

/// How much the compiler insists on before it will build.
///
/// Nothing here changes what a program *means* — only whether the compiler is
/// willing to hand it over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Strictness {
    /// The edit-run loop. A half-written `match` should still run, and fail at
    /// run time only if it is actually reached.
    #[default]
    Lenient,
    /// Shipping. A missing case is a bug you want to hear about first.
    Strict,
}

impl Strictness {
    pub fn parse(s: &str) -> Option<Strictness> {
        match s.trim() {
            "lenient" => Some(Strictness::Lenient),
            "strict" => Some(Strictness::Strict),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Strictness::Lenient => "lenient",
            Strictness::Strict => "strict",
        }
    }
}

/// Flags for one compilation unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    pub opt: OptLevel,
    pub strictness: Strictness,
    /// Keep where each call, branch and body was written, for a debugger.
    ///
    /// Not part of either profile: it is what `meadow dap` asks for, and it
    /// changes nothing a program does -- only what the compiler remembers.
    pub debug_info: bool,
    /// A top-level value that runs in place of `main`, and so may perform
    /// effects: the REPL's `it`, a debugger's entry, a test's `result`. It is
    /// evaluated and shown, where `main` is called. A `def` of any other name
    /// has to be pure -- see the type checker's `check_pure_def`.
    pub entry_name: Option<&'static str>,
    /// That value is an expression being run, as a line of the REPL is, and
    /// not a definition the program wrote: it does not see its own name
    /// anywhere, where a definition can call itself from inside a function.
    pub entry_is_expression: bool,
    /// What `@cfg(…)` is tested against -- see [`crate::cfg`].
    pub cfg: Cfg,
}

/// The facts a `@cfg(…)` condition can ask about: the platform the program is
/// built for, how it is built, and any flags the build turned on by name.
///
/// Every field is a plain value, so options stay `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cfg {
    /// `os = "…"`: `"windows"`, `"linux"` or `"macos"`.
    pub os: &'static str,
    /// `arch = "…"`: `"x86_64"` or `"aarch64"`.
    pub arch: &'static str,
    /// `profile = "…"`, and the bare `debug` or `release`.
    pub profile: &'static str,
    /// `backend = "…"`: Glade's `"vm"`, `"jit"` or `"aot"`, `"silo"`, or
    /// `"cek"`.
    pub backend: &'static str,
    /// The bare `test`: on while `meadow test` builds.
    pub test: bool,
    /// Flags a build turned on itself -- `fast`, `feature=gpu` -- written the
    /// way `--cfg` takes them and joined with commas. `None` when there are
    /// none.
    pub flags: Option<InternedString>,
}

impl Cfg {
    /// This machine, in the given profile and backend, with no flags.
    pub const fn host(profile: &'static str, backend: &'static str) -> Cfg {
        Cfg {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            profile,
            backend,
            test: false,
            flags: None,
        }
    }

    /// Just the platform: what `Std` is compiled against, so that one
    /// compile of it serves every profile, backend and flag.
    pub const fn platform(self) -> Cfg {
        Cfg::host("debug", "jit").on(self.os, self.arch)
    }

    /// The same, for a program built for `os` on `arch`.
    pub const fn on(mut self, os: &'static str, arch: &'static str) -> Cfg {
        self.os = os;
        self.arch = arch;
        self
    }

    /// `"unix"` or `"windows"`: `family = "…"`, and the bare `unix` or
    /// `windows`.
    pub fn family(&self) -> &'static str {
        if self.os == "windows" {
            "windows"
        } else {
            "unix"
        }
    }

    /// Whether the build turned on flag `flag`: `fast`, or `feature=gpu`.
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags
            .is_some_and(|fs| fs.split(',').any(|f| f.trim() == flag))
    }

    /// These flags as well: a comma-separated list, as `--cfg` and a
    /// manifest's `cfg = "…"` write them.
    pub fn with_flags(mut self, more: &str) -> Cfg {
        let mut all: Vec<String> = self
            .flags
            .map(|fs| fs.split(',').map(|f| f.trim().to_string()).collect())
            .unwrap_or_default();
        for f in more
            .split(',')
            .map(|f| f.split('=').map(str::trim).collect::<Vec<_>>().join("="))
        {
            if !f.is_empty() && !all.contains(&f) {
                all.push(f);
            }
        }
        all.sort();
        self.flags = (!all.is_empty()).then(|| InternedString::from(all.join(",")));
        self
    }
}

impl Default for Cfg {
    fn default() -> Cfg {
        Cfg::host("debug", "jit")
    }
}

impl Options {
    /// Report a non-exhaustive `match` as an error.
    ///
    /// Irrefutability of *binding* positions — function and lambda parameters,
    /// `def` / `let` destructuring — is checked regardless: those have no
    /// fallback arm, so a refutable pattern there is always an error.
    pub const fn check_exhaustive(self) -> bool {
        matches!(self.strictness, Strictness::Strict)
    }

    /// Fast edit-run loop: no exhaustiveness check, cheap passes only.
    pub const fn debug() -> Self {
        Options {
            opt: OptLevel::O1,
            strictness: Strictness::Lenient,
            debug_info: false,
            entry_name: None,
            entry_is_expression: false,
            cfg: Cfg::host("debug", "jit"),
        }
    }

    /// Shipping build: every `match` must cover its scrutinee, and the compiler
    /// does everything it knows how to.
    pub const fn release() -> Self {
        Options {
            opt: OptLevel::O2,
            strictness: Strictness::Strict,
            debug_info: false,
            entry_name: None,
            entry_is_expression: false,
            cfg: Cfg::host("release", "aot"),
        }
    }

    /// The same build, running the value `name` rather than calling `main`:
    /// see [`Options::entry_name`].
    pub const fn entry(self, name: &'static str) -> Self {
        Options {
            entry_name: Some(name),
            ..self
        }
    }
}
