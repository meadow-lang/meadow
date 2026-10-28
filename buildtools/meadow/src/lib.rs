//! The Meadow **build system**: filesystem package discovery ([`package`],
//! [`workspace`]), the
//! per-unit compile [`pipeline`], the [`linker`], and the embedded standard
//! library ([`stdlib`]).
//!
//! It sits on top of the `meadow-compiler` workspace (front-end passes and the
//! AxCut back end) and the machines that can run the result: the runtime's
//! VM (`silo/vm`), an executable compiled by LLVM ([`aot`]), and
//! `meadow-eval`, the CEK machine. [`runtime`] picks between them. The
//! `meadow` binary — CLI + REPL — is a thin shell over this crate.

pub mod add;
pub mod aot;
pub mod artifacts;
pub mod channel;
pub mod clean;
pub mod complete;
pub mod cut;
pub mod dap;
pub mod editor;
pub mod expanded;
pub mod finder;
pub mod format;
pub mod git;
pub mod incremental;
pub mod init;
pub mod linker;
pub mod listing;
pub mod lock;
pub mod package;
pub mod pipeline;
pub mod proc;
pub mod profile;
pub mod runtime;
pub mod semver;
pub mod served;
pub mod status;
pub mod stdlib;
pub mod test;
pub mod tour;
pub mod update;
pub mod watch;
pub mod workspace;

pub use meadow_compiler::{OptLevel, Options, Strictness};
pub use profile::{Backend, Profile, Resolved};
pub use runtime::Engine;
