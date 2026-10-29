//! # Meadow — compiler front end (facade)
//!
//! The Meadow source tree is four independent Cargo workspaces:
//!
//! * **`compiler/`** — this workspace. One crate per pass, wired together here so
//!   downstream code can `use meadow_compiler::{lexer, parser, infer, …}`.
//! * **`eval/`** — the `meadow-eval` crate: a CEK machine that runs a
//!   [`core::Program`]. Depends only on `meadow-core` + `meadow-intern`.
//! * **`buildtools/`** — everything you point *at* Meadow source: `meadow`, the
//!   build system (package discovery, pipeline, linker, embedded stdlib) plus
//!   the CLI and REPL, and `meadow-fmt`, the formatter behind `meadow fmt`.
//! * **`installer/`** — `meadowup`, which installs and updates the toolchain.
//!
//! ## The pipeline
//!
//! ```text
//!   &str
//!    │  lexer::tokenize            logos-based; skips whitespace/comments
//!    ▼
//!   Vec<LToken>
//!    │  parser::parse              chumsky; pratt-parsed operators
//!    ▼
//!   ast::Module                   surface syntax, names still strings
//!    │  rename::Resolver          ast → hir
//!    ▼
//!   hir::Module                   nodes stamped with hir::NodeId; names → hir::VarId
//!    │  infer::Infer              Algorithm J
//!    ▼
//!   infer::TypeTable             NodeId → Type, plus a Scheme per binding
//!    │  core::Lowerer            hir → core (System F: every binder typed,
//!    ▼                           every instantiation written out)
//!   Vec<core::Def>
//!    │  core::lint::check        the types check (debug builds and tests)
//!    ▼
//!   Vec<core::Def>
//!    │  meadow::linker::Linker   concatenate packages, find `main`   (in the `meadow` crate)
//!    ▼
//!   core::Program
//!    │  core::erase              types off; everything below is untyped
//!    ▼
//!   core::Program
//!    │  meadow_eval::run         CEK abstract machine                (in the `eval` crate)
//!    ▼
//!   meadow_eval::Value
//! ```
//!
//! ## Identifiers
//!
//! [`hir::VarId`]s are globally unique (a process-wide atomic counter), so ids
//! minted by separately compiled packages / REPL lines never collide and the
//! linker can key on them. [`hir::NodeId`]s are dense per resolver, so type
//! inference can use a `Vec`-backed side table.

pub use meadow_ast as ast;
pub use meadow_core as core;
pub use meadow_diagnostics as diagnostics;
pub use meadow_exhaust as exhaust;
pub use meadow_hir as hir;
pub use meadow_infer as infer;
pub use meadow_intern as intern;
pub use meadow_lexer as lexer;
pub use meadow_parser as parser;
pub use meadow_rename as rename;
pub use meadow_scc as scc;
pub use meadow_source as source;
pub use meadow_span as span;

pub mod cfg;
pub mod expand;
pub mod focus;
mod options;
pub use options::{Cfg, OptLevel, Options, Strictness};

mod unit;
pub use unit::{
    AstModule, CompiledPackage, Dep, Export, Resolved, TestSite, TypedModule, compile_str,
    compile_str_with, compile_unit, compile_unit_above, compile_unit_as, compile_unit_focused,
    compile_unit_in_package, compile_unit_with_procs, resolve_module,
};
