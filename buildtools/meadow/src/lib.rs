//! The Meadow **build system**: filesystem package discovery ([`package`],
//! [`workspace`]), the
//! per-unit compile [`pipeline`], the [`linker`], and the embedded standard
//! library ([`stdlib`]).
//!
//! It sits on top of the `meadow-compiler` workspace (front-end passes and the
//! AxCut back end) and the two machines that can run the result: `meadow-glade`,
//! the bytecode VM, and `meadow-eval`, the CEK machine. [`runtime`] picks
//! between them. The command line and REPL are [`cli`]; the `meadow` binary,
//! and `meadow-host`'s library for MeadowBoot, each only call it.

pub mod add;
pub mod aot;
pub mod artifacts;
pub mod cache;
pub mod channel;
pub mod clean;
pub mod cli;
pub mod complete;
pub mod cut;
pub mod dap;
pub mod editor;
pub mod expanded;
pub mod finder;
pub mod format;
pub mod git;
pub mod host;
pub mod incremental;
pub mod init;
pub mod ir;
pub mod linker;
pub mod listing;
pub mod lock;
pub mod new;
pub mod package;
pub mod pgo;
pub mod pipeline;
pub mod proc;
pub mod profile;
pub mod runtime;
pub mod samples;
pub mod semver;
pub mod status;
pub mod stdlib;
pub mod test;
pub mod tour;
pub mod update;
pub mod viewer;
pub mod workspace;

pub use meadow_compiler::{OptLevel, Options, Strictness};
pub use profile::{Backend, Profile, Resolved};
pub use runtime::Engine;
