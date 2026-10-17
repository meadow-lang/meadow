//! # The Meadow runtime system
//!
//! A register bytecode VM with a generational garbage collector. It loads a
//! [`meadow_bytecode::Program`] and runs it, and that is the whole of its
//! interface to the rest of the compiler.
//!
//! ## What it does not know
//!
//! Anything about how the bytecode was produced. Meadow's back end lowers to a
//! sequent-calculus IR (`meadow_seq`, AxCut) and compiles that to registers
//! (`meadow_codegen`), and neither of them is visible from here. The VM sees a
//! flat instruction array, a flat register file, and a heap.
//!
//! One consequence is worth stating because it looks like an omission: **there
//! is no `call` and no `ret`.** In the IR above, returning from a function is
//! entering the continuation it was given, and a closure, a continuation and a
//! handler are all the same kind of object. So [`Op::Invoke`] is the entire
//! calling convention — rebuild the registers, jump. The continuation of a call
//! that is not a tail call is a frame on a chunked, per-thread frame stack the
//! heap owns, popped when the function returns through it; recursion grows
//! that, not the machine's stack.
//!
//! [`Op::Invoke`]: meadow_bytecode::Op::Invoke
//!
//! ## The CEK machine is the specification
//!
//! `meadow_eval` stays. It is small enough to read and to believe, it defines
//! what a Meadow program *means*, and this crate is checked against it: the same
//! program run through both must produce the same value. Where they disagree,
//! the CEK is right by definition and this is a bug. `meadow run --cek` is that
//! same switch, available to anyone who suspects one.
//!
//! Two layers of tests hold the line. `tests/differential.rs` is hand-written
//! programs, one construct at a time; `meadow`'s `tests/vm.rs` puts the whole
//! standard library through the pipeline and requires every one of its `@test`
//! functions to give the CEK's answer.
//!
//! ## The pipeline
//!
//! ```text
//!   core::Program
//!    │  meadow_seq::lower_program     expressions -> AxCut statements
//!    ▼
//!   seq::Program                      seven statements, no expressions
//!    │  meadow_codegen::compile       environments -> registers
//!    ▼
//!   bytecode::Program                 fixed 8-byte instructions
//!    │  meadow_glade::run
//!    ▼
//!   a value
//! ```
//!
//! ## Memory
//!
//! [`heap`] is one per green thread, and built for short pauses: a copying
//! nursery, where allocation is a bounds check and a bump and a collection costs
//! what survives; an Immix old generation ([`old`]) where survivors stay put;
//! marking of that old generation on other OS threads while the program runs
//! ([`mark`]); and moving survivors out of its sparsest blocks a little per
//! pause, to give memory back (`evacuate`). Pauses are measured, pause by pause, in [`pauses`]. Nothing is
//! reference counted and no runtime value has a destructor, which removed two
//! problems the CEK machine had to work around by hand — dropping a long list
//! used to recurse once per element, and a cycle through a mutable cell could
//! never be freed at all.
//!
//! The one allocation this crate does not manage is strings, which are interned
//! for the life of the process. The CEK does the same.

pub mod abi;
pub mod aot;
pub mod codegen;
mod evacuate;
pub mod heap;
pub mod host;
pub mod jit;
pub mod journal;
pub mod mark;
mod native;
pub mod object;
pub mod old;
pub mod pauses;
mod prims;
pub mod profile;
pub mod region;
pub mod sched;
mod show;
pub mod stm;
mod text;
pub mod value;
pub mod vm;

pub use heap::{Heap, Kind};
pub use host::{Answer, Host, Owned};
pub use journal::{Journal, Undo};
pub use native::Build;
pub use value::{Addr, Value};
pub use vm::{Error, Io, Vm, run};
