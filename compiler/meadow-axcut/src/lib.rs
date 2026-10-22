//! **AxCut** — a machine-level sequent-calculus IR.
//!
//! From Schuster, Müller, Ostermann and Brachthäuser, *Compiling Classical
//! Sequent Calculus to Stock Hardware: The Duality of Compilation* (OOPSLA
//! 2025). The grammar below follows the artifact's `Syntax.idr`; the abstract
//! machine in [`crate::machine`] follows its `Semantics.idr`.
//!
//! # What makes it a machine language
//!
//! A textbook sequent calculus (λμμ̃ — the one in "Grokking the Sequent
//! Calculus") has producers, consumers, and a *cut* `⟨p | c⟩` that runs one
//! against the other. A cut is a redex. Cut elimination is not a pass that runs
//! before the program does — it *is* the reduction relation, and a program with
//! no cuts left in it is a program that has already finished.
//!
//! So AxCut does not remove cuts. It **restricts** them. There is no `Cut`
//! constructor because a cut is never a node of its own here: every one of the
//! seven statements below *is* a cut, with the rule it cuts against fused into
//! it. The paper's normal form is reached by pushing cuts until a variable — an
//! axiom — stands on one side, which is what the name says: `Ax` for the axiom
//! rule, `Cut` for the cut that meets it.
//!
//! Which rule a cut meets is also what decides memory, and that pairing is the
//! paper's central trick:
//!
//! ```text
//!   let, new         a cut against an *activation* rule    acquires memory
//!   switch, invoke   a cut against an *axiom* (deactivation)  releases it
//! ```
//!
//! `substitute` is the structural rules written down. In the paper, duplicating
//! a name shares the memory behind it and dropping one erases it — a pair of
//! reference-count operations — so a well-typed AxCut program manages its own
//! memory and needs no collector at all. **Meadow takes neither half of that**:
//! it collects instead of counting, so `switch` and `invoke` free nothing and
//! `substitute` counts nothing. See [Linearity](#linearity).
//!
//! What remains either way is seven statements and no expressions whatsoever.
//!
//! The consequence that matters, and the reason for the whole exercise:
//!
//! > **`Jump` and `Invoke` take no arguments.**
//!
//! Arguments are not passed. They are already *in the environment*, in the right
//! order, because a [`Statement::Substitute`] put them there. The environment is
//! an ordered list — a register file — and `Substitute` is the only thing that
//! rearranges it. Code generation for a call is therefore a permutation of
//! registers followed by a branch, and register allocation stops being a search
//! problem: the IR already says where everything goes.
//!
//! # The seven statements
//!
//! ```text
//!   substitute [x, y] in {(a, b) => s}   rebuild the environment as [x, y]
//!   jump L                               tail-jump; environment unchanged
//!   let z = K#2(x, y); s                 build a data value
//!   switch x { K#2 => s0, else => s1 }   branch on a tag, binding the fields
//!   new f {…methods…} capturing [x]; s   build codata: a closure, or a handler
//!   invoke f#0                           call a method
//!   extern add(x, y) { {(r) => s} }      a primitive; its blocks are its
//!                                        continuations
//! ```
//!
//! Like the paper's, this AxCut has **no effect handlers**. Effekt compiles
//! effects away before reaching it, and so does Meadow now: `handle` and
//! `perform` lower to evidence passing -- objects, data and jumps -- and an
//! operation no handler answers becomes an [`Extern::Native`]. See
//! `lower`'s module docs.
//!
//! Data (`let` / `switch`) and codata (`new` / `invoke`) are exact duals, which
//! is the "duality" of the title. A closure is codata with one method. A
//! *continuation* is also codata with one method — which is why returning from a
//! function is `invoke ret#0` and needs no separate mechanism, and why an effect
//! handler's clauses and resumptions are ordinary objects rather than special
//! forms.
//!
//! # Linearity
//!
//! The paper's environment is linear: a statement consumes exactly the names it
//! mentions, and `Substitute` is what duplicates, drops and reorders. This crate
//! **does not enforce that** — see [`Statement::Substitute`]. Names are checked
//! for scope, not for use count.
//!
//! That is one deviation with two consequences, and it is worth being plain
//! about which is which. The paper spends its linearity on *memory*: because
//! every name is used once, a cut against an axiom knows the object it leaves
//! behind is dead and can release it there and then. Meadow does not want that
//! half — it has a generational collector, green threads with a heap each, and
//! [`Statement::Switch`] deliberately keeps its scrutinee so that
//! `match o with | Just x -> o` works. The half it does want is *register
//! allocation*: with use counts on the IR the allocator could read its answer
//! off `Substitute` instead of computing it. So linearity here would be an
//! optimization, not the memory discipline it is in the paper.

use meadow_intern::InternedString;
use meadow_rt::Prim;
pub use meadow_rt::roles::Role;

/// A name in the environment.
///
/// Not a variable in the usual sense: it is a *position*, and the environment is
/// a register file. A front end may number them as it numbers its own
/// variables -- Meadow's are [`VarId`]s already, which keeps its lowering from
/// renaming everything it touches.
pub type Name = VarId;

mod names;
pub use names::{SYNTHETIC_BASE, VarId};

/// Which constructor of a data type, or which method of a codata object.
pub type Tag = u32;

/// How the value a name holds is represented at run time.
///
/// What a collector has to know about a register -- whether it holds a pointer
/// -- and what native code has to know to read one. Every name a program binds
/// has one, in [`Program::reps`]: from the type core gave the value, or, for the
/// names lowering invents, from what they are. `Var` is the one that is not
/// known when the program is compiled: a value whose type is a type variable,
/// whose representation is whatever the variable is instantiated to -- which
/// the variable's *descriptor* says, at run time (`meadow_core::desc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Rep {
    /// A heap object: data, a closure or continuation, an array, a record, a
    /// `BigInt`, a `Ref` -- anything the collector follows.
    Ref,
    /// A 64-bit `Int`.
    Int,
    /// A 64-bit `Float`.
    Float,
    /// Any other immediate: `()`, a `Bool`, a `Char`, a sized integer, a
    /// `Float32` -- which one, as its descriptor.
    Bits(meadow_rt::desc::Desc),
    /// A `String`.
    Str,
    /// Whatever the descriptor in the name `VarId(n)` says. Wherever a name
    /// of this representation is in an environment that can collect, so is
    /// that descriptor. [`NO_DESC`] when no abstraction binds the variable.
    Var(u32),
    /// A type the compiler could not work out -- only in a unit with errors.
    Unknown,
}

/// [`Rep::Var`] of a type variable no enclosing abstraction binds: nothing
/// describes it, and no value of it is expected at run time.
pub const NO_DESC: u32 = u32::MAX;

impl Rep {
    /// The descriptor of a value of this representation, when it is known
    /// without looking at one.
    pub fn desc(self) -> Option<meadow_rt::desc::Desc> {
        use meadow_rt::desc;
        match self {
            Rep::Ref => Some(desc::REF),
            Rep::Int => Some(desc::INT),
            Rep::Float => Some(desc::FLOAT),
            Rep::Str => Some(desc::STR),
            Rep::Bits(d) => Some(d),
            Rep::Var(_) | Rep::Unknown => None,
        }
    }
}

/// A top-level block, reachable by [`Statement::Jump`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Label(pub u32);

/// A block: parameter names, and a body that runs with those names bound.
///
/// The parameters are the shape the environment must have on entry. A caller
/// arranges it with [`Statement::Substitute`] and then jumps.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct Block {
    pub params: Vec<Name>,
    pub body: Statement,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Statement {
    /// `substitute [x, y, …] in block` — rebuild the environment from the named
    /// values, in that order, and run the block.
    ///
    /// The whole of calling convention, argument passing, closure conversion and
    /// register allocation lives here. A `jump` carries no arguments precisely
    /// because a `substitute` has already put them where the target expects
    /// them, so lowering this is emitting moves and nothing else.
    ///
    /// In the paper this is where the linear environment is duplicated, dropped
    /// and permuted, and the discipline is what makes the moves minimal. Here it
    /// is just a permutation with repeats allowed.
    Substitute(Vec<Name>, Box<Block>),

    /// `jump L` — transfer control, environment unchanged.
    Jump(Label),

    /// `let x = tag(fields); rest` — build a data value and bind it.
    ///
    /// The paper's `let` *consumes* its fields — they are the front of the
    /// linear environment and the new value replaces them. This one reads them
    /// and leaves them, for the reason set out on [`Statement::Switch`]:
    /// without a duplication pass, consuming would break `let p = Pair x y`
    /// followed by any further use of `x`. [`Statement::Invoke`] is the only
    /// statement here that consumes what it names.
    Let {
        name: Name,
        /// Which constructor. The name is kept alongside for readable output and
        /// for talking to a runtime that identifies constructors by name.
        tag: Tag,
        ctor: InternedString,
        fields: Vec<Name>,
        rest: Box<Statement>,
    },

    /// `switch x { K#2 => …, else => … }` — branch on a constructor tag. The
    /// matching arm's parameters bind that constructor's fields, followed by the
    /// environment as it stands — `x` included.
    ///
    /// Two departures from the paper, both deliberate.
    ///
    /// Its `switch` is a dense list, one block per constructor, because its arms
    /// come from a typed constructor table. `core` carries no such grouping —
    /// constructor names are global and a `match` need not be exhaustive — so
    /// arms are keyed by tag and there is always a default, which binds the
    /// environment unchanged since it knows no fields.
    ///
    /// Its `switch` also *consumes* the scrutinee, because its environment is
    /// linear and a value still wanted would have been duplicated first. This
    /// one keeps it, for the same reason [`Statement::Let`] and
    /// [`Statement::New`] keep what they build from: `match o with | Just x -> o`
    /// is ordinary Meadow, and without the duplication pass that would make
    /// consuming safe, the honest thing is to leave the environment alone.
    /// [`Statement::Invoke`] is the exception and has to be — see its note.
    Switch {
        scrutinee: Name,
        arms: Vec<(Tag, Block)>,
        default: Box<Block>,
    },

    /// `new f { …methods… } capturing [x, …]; rest` — build codata.
    ///
    /// A closure, a continuation and an effect handler are all this. The methods
    /// run with the captured environment restored, plus whatever the caller
    /// arranged.
    ///
    /// Like [`Statement::Let`], and unlike the paper, this reads its captures
    /// rather than consuming them.
    New {
        name: Name,
        captures: Vec<Name>,
        methods: Vec<Block>,
        rest: Box<Statement>,
    },

    /// `invoke f#tag` — enter a method of a codata object. The method's
    /// environment is `f`'s captures followed by what was left behind `f`.
    ///
    /// Takes no arguments, for the same reason `jump` does not.
    ///
    /// This is the one statement that *consumes*, and it must: a method's
    /// parameters are `captures ++ arguments`, so `f` cannot still be sitting
    /// among them. It is also the one place consuming is safe without a
    /// duplication pass, because every `invoke` this crate emits is immediately
    /// preceded by a `substitute` that built the environment for it.
    Invoke(Name, Tag),

    /// `extern p(args) { blocks }` — a primitive.
    ///
    /// The blocks are its continuations, which is how branching primitives fit
    /// without a separate `if`: a branch takes two blocks, arithmetic takes one
    /// that binds the result. The paper's `Extern` carries a string, and
    /// `lit_5` is how a literal is written — there is no literal producer,
    /// because a producer would be an expression and there are none.
    Extern {
        op: Extern,
        args: Vec<Name>,
        blocks: Vec<Block>,
    },

    /// Not in the paper. Reaching one is a runtime error, and it exists so that
    /// an untranslatable term is loud rather than silently missing.
    Error(#[serde(deserialize_with = "leaked")] &'static str),

    /// Not in the paper either: `s`, which a person wrote at `loc`. It does
    /// nothing. Code generation reads it to say which source position the
    /// instructions for `s` came from -- see `meadow_core::Term::Loc`, which
    /// is where it comes from and why it is only there in a debug build.
    Mark(meadow_rt::Loc, Box<Statement>),
}

/// What an [`Statement::Extern`] does.
///
/// The paper's is a bare string interpreted by the backend (`"add"`, `"ifz"`,
/// `"lit_5"`). Naming the three kinds keeps the lowering from building strings
/// only to parse them again.
/// Where a label is in a record whose type is a closed row: its place among
/// the labels in the order a record keeps them -- the interned names' -- and
/// how many labels there are. A back end that lays a record out in that order
/// reads the field there, rather than searching the labels at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Place {
    pub at: u32,
    pub of: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Extern {
    /// Produce a constant. One continuation, which binds it.
    Lit(meadow_rt::Lit),
    /// A `meadow` primitive. One continuation, which binds the result.
    Prim(Prim),
    /// A binary primitive whose right operand is a literal — `n - 1`, `x == 0`.
    /// One argument, naming the left operand; one continuation.
    ///
    /// Folding the literal in is worth a variant of its own because of what it
    /// removes, which is not only the instruction that loaded it: the literal
    /// never gets a name, so it never joins the environment, never takes a
    /// register, and is never carried through the moves at the next jump. A loop
    /// counting down by one used to hoist `1` into the environment on every
    /// iteration.
    PrimK(Prim, meadow_rt::Lit),
    /// Branch: two continuations, taken on false and true respectively.
    ///
    /// `if` is not a statement of its own — a branching primitive with two
    /// continuation blocks is all it ever was.
    Branch,
    /// Branch on a binary primitive: `if x < y`, in one statement rather than a
    /// comparison whose result is immediately tested and then thrown away.
    ///
    /// Two arguments, two continuations.
    BranchPrim(Prim),
    /// The same with a literal right operand: `if n == 0`, which is the shape
    /// every counting loop and every literal pattern ends up in.
    ///
    /// One argument, two continuations.
    BranchPrimK(Prim, meadow_rt::Lit),

    /// Build a record. The labels name the arguments, in the same order.
    Record(Vec<InternedString>),
    /// `r.label`, and where it is, if the record's type says.
    Select(InternedString, Option<Place>),
    /// `{ r | label = v }` — one argument the record, one the value — and
    /// where the label is, if the record's type says it has it already.
    Extend(InternedString, Option<Place>),
    /// Build the builtin `Array` from its arguments.
    Array,

    /// An effect operation no handler in the program answers, for the runtime
    /// to: `Console.writeOutput`, `Fs.readFile`, `Test.fail`. One argument; one
    /// continuation, which binds the result.
    Native(InternedString, InternedString),

    /// Field `i` of a data value, tuple or array.
    ///
    /// The paper would use `switch` for this, and for a `match` arm so does the
    /// lowering. But `core`'s tuple projection carries only an index, and its
    /// arity is not recoverable from the term — a `switch` arm has to name every
    /// field, so it cannot be written. This can, and it does not care.
    Field(usize),
}

/// A labelled block in the program table.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct Def {
    pub label: Label,
    pub name: InternedString,
    /// The module its source is in, package first -- or empty, where a front
    /// end does not say. A back end that compiles a program in parts divides
    /// it by these.
    pub module: InternedString,
    pub block: Block,
}

/// A block C may call, by the name it calls it: what a library built of the
/// program exports. The block takes `params` and then the continuation it
/// answers through, and answers `result`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Export {
    pub name: String,
    pub label: Label,
    pub params: Vec<Rep>,
    pub result: Rep,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct Program {
    /// What C may call, where the program is built as a library.
    pub exports: Vec<Export>,
    pub defs: Vec<Def>,
    /// Where execution starts.
    pub entry: Option<Label>,
    /// Constructor name -> tag, so `switch` arms and `let` agree on numbering.
    pub tags: std::collections::HashMap<InternedString, Tag>,
    /// Every name that is a function's own *return continuation* -- the `k` a
    /// definition, a lambda or a handler clause answers with.
    ///
    /// The machine has no stack pointer to unwind from, and this is what lets
    /// a debugger draw a call stack anyway: code running in a function has its function's return
    /// continuation in its environment, and the continuation that holds
    /// captures the caller's. A name is fresh wherever it is bound, so a set of
    /// them is unambiguous.
    pub returns: std::collections::HashSet<Name>,
    /// The other continuations: the ones a function makes for itself, to
    /// receive the value of a call it is part-way through. One of these
    /// capturing another is still the same activation -- `f (g x)` waits for
    /// `g` and then for `f` -- where one capturing a [`Program::returns`] name
    /// is the caller's.
    pub continuations: std::collections::HashSet<Name>,
    /// The continuations a function makes for its own non-tail calls, by the
    /// name each is bound to at its `new`: the ones that are **frames**. A
    /// frame is entered exactly once, by the call it was made for returning
    /// through it, and everything pushed after it is dead by then -- so the
    /// back end puts it on the thread's frame stack instead of the heap. Every
    /// other `new` -- a closure, a handler clause, a resumption -- is an object
    /// that may outlive the code that made it.
    pub frames: std::collections::HashSet<Name>,
    /// Named-field order per constructor, carried from core. A `record` value
    /// is constructor data rather than an anonymous record, and `.field` on one
    /// is resolved against this at run time, as the CEK machine does.
    pub ctor_fields: std::collections::HashMap<InternedString, Vec<InternedString>>,
    /// The constructor playing each [`Role`] -- what a runtime builds and
    /// recognizes by itself, an optional value or a list, asked for by role
    /// rather than by a name only one language uses. See `meadow_rt::roles`.
    pub roles: std::collections::HashMap<Role, InternedString>,
    /// What every name holds -- see [`Rep`].
    pub reps: std::collections::HashMap<Name, Rep>,
    /// Names that are copies of a variable the program was written with, and
    /// which -- see `meadow_core::Program::origins`.
    pub origins: std::collections::HashMap<Name, Name>,
    /// What a definition's block answers its continuation with, for the blocks
    /// a runtime starts: each definition's, and a generic entry point's. A
    /// runtime that has only a word back has to be told what it is.
    pub results: std::collections::HashMap<Label, Rep>,
    /// For a name holding a thread, a `Task a`: how `a` is represented, which
    /// is what the thread answers with.
    pub threads: std::collections::HashMap<Name, Rep>,
    /// The blocks whose code can never have its activation captured: a
    /// definition's, or a direct entry point's, that performs nothing,
    /// handles nothing and calls nothing that might -- which is what taking
    /// no evidence says. A continuation made in one runs once, before the
    /// block's caller goes on, and so may borrow what the block borrowed: see
    /// `meadow_llvm::linear`.
    pub pure: std::collections::HashSet<Label>,
}

impl Program {
    /// The constructor that plays `role` in this program, if it declares one.
    pub fn role(&self, role: Role) -> Option<InternedString> {
        self.roles.get(&role).copied()
    }

    pub fn block(&self, label: Label) -> Option<&Block> {
        self.defs
            .iter()
            .find(|d| d.label == label)
            .map(|d| &d.block)
    }
}

pub mod describe;

/// AxCut with every share, erase and reuse written down: what an engine that
/// counts references runs (`docs/RUNTIME-MERGE.md`, the memory pass).
pub mod linear;

pub mod machine;

/// A program as a file, for an engine that runs in a process of its own:
/// Silo's interpreter (`silo/vm`).
pub mod image;

mod print;
pub use print::Listing;

impl Extern {
    /// Does this `extern` choose between two continuations rather than produce
    /// a value?
    ///
    /// The three branching forms differ only in how they get their boolean, and
    /// every consumer wants to know which group an operation is in before it
    /// cares which member.
    pub fn is_branch(&self) -> bool {
        matches!(
            self,
            Extern::Branch | Extern::BranchPrim(_) | Extern::BranchPrimK(_, _)
        )
    }
}

/// Can `p`'s operands be given in either order?
///
/// Only used to fold a literal *left* operand into the right-hand slot the
/// folded forms provide — `2 * x` should compile the way `x * 2` does. Listed
/// rather than derived, and deliberately short: a wrong entry here is a program
/// that runs and computes something else.
pub fn commutes(p: Prim) -> bool {
    use Prim::*;
    matches!(
        p,
        Add | Mul | Eq | Ne | AddF | MulF | BitAnd | BitOr | BitXor
    )
}

/// The names `s` still needs from the environment it starts in.
///
/// # What this is for, and why nothing uses it yet
///
/// `fib` retires two `move` instructions in every call, a quarter of
/// everything it runs, and they are all one shape: a continuation captures the
/// names the frame will not need again, and `meadow_codegen` gives the next
/// value the lowest register *no name is sitting in* -- so the dead names hold
/// the low registers, the live values are pushed above them, and the
/// `substitute` before the call has to move them back down.
///
/// Knowing which names are dead is this function. Acting on it is a pass that
/// is not written, because two other things assume an environment never
/// shrinks:
///
/// * `meadow_codegen` enters a block by zipping its parameters against the
///   registers of the environment, so a shorter environment is a block entered
///   with the wrong arity. Leaving the dead name in and reusing its register is
///   worse: the GC map at the next safepoint would still say a reference lives
///   there, over a register holding something else.
/// * [`crate::describe`] splices a descriptor into an environment *by
///   position*, and asserts that a continuation's parameters end with exactly
///   the environment it continues. Narrowing an environment in `lower` trips
///   that assert.
///
/// So the shape of the work is a **late pass over the whole program**, after
/// `describe`, which narrows every block's parameters to what its body reads
/// and fixes up each entry point to match -- a `substitute`'s selection, a
/// `new`'s captures, a `switch` arm, an `extern`'s continuation, and every
/// `jump` to a label, which share one parameter list and must agree.
///
/// # What it counts
///
/// Not the names it *mentions*: a block's parameter list names the whole
/// environment, because that is what an AxCut block takes, so mentioning
/// proves nothing. This is the smaller question a register allocator wants --
/// which of the values in registers now will be read again.
///
/// Three rules carry the weight:
///
/// * A [`Statement::New`]'s **methods are not counted**. They run later, in an
///   activation of their own, from the captures -- which *are* counted, because
///   copying them out is a use here and now. This is the whole point: a
///   continuation captures the names its frame will not need again, and once it
///   has, their registers are free.
/// * A [`Statement::Switch`]'s arms and a [`Statement::Extern`]'s blocks **are**
///   counted. They are continuations within the same activation, running on the
///   same registers, so what they read is read here.
/// * A [`Statement::Jump`] needs nothing of its own. The environment it carries
///   is the one a [`Statement::Substitute`] just built, and that selection is
///   where those names were used.
pub fn still_used(s: &Statement) -> Option<std::collections::HashSet<Name>> {
    let mut out = std::collections::HashSet::new();
    gather_uses(s, &mut out).then_some(out)
}

/// Collects into `out`; `false` means the statement hands its whole
/// environment on and nothing may be dropped.
fn gather_uses(s: &Statement, out: &mut std::collections::HashSet<Name>) -> bool {
    match s {
        // A `substitute` is the complete interface to what follows it: the
        // block it enters gets exactly this selection and nothing else, so
        // there is no need to look inside, and looking inside would be wrong
        // for a block that gives its parameters other names.
        Statement::Substitute(sel, _) => {
            out.extend(sel.iter().copied());
            true
        }
        // These two hand the environment on whole -- a `jump` to the block it
        // names, an `invoke` to the method as its arguments -- so nothing in it
        // can be called dead. Both are reached through a `substitute` that has
        // already said what they need, which is where the narrowing happens.
        Statement::Jump(_) | Statement::Invoke(..) => false,
        Statement::Let { fields, rest, .. } => {
            out.extend(fields.iter().copied());
            gather_uses(rest, out)
        }
        Statement::New { captures, rest, .. } => {
            out.extend(captures.iter().copied());
            gather_uses(rest, out)
        }
        Statement::Switch {
            scrutinee,
            arms,
            default,
        } => {
            out.insert(*scrutinee);
            let mut ok = gather_uses(&default.body, out);
            for (_, block) in arms {
                ok &= gather_uses(&block.body, out);
            }
            ok
        }
        Statement::Extern { args, blocks, .. } => {
            out.extend(args.iter().copied());
            let mut ok = true;
            for block in blocks {
                ok &= gather_uses(&block.body, out);
            }
            ok
        }
        Statement::Mark(_, inner) => gather_uses(inner, out),
        Statement::Error(_) => true,
    }
}

#[cfg(test)]
mod still_used_tests {
    use super::*;
    use crate::VarId;

    fn n(i: u32) -> Name {
        VarId(i)
    }

    fn block(params: Vec<Name>, body: Statement) -> Block {
        Block { params, body }
    }

    /// A continuation's captures are a use *here*; what its methods do with
    /// them later is not, because they have been copied into the object. This
    /// is the rule the whole analysis exists for: once a frame has captured the
    /// names it will not need again, their registers are free.
    #[test]
    fn capturing_a_name_is_the_last_use_of_it() {
        // new k2 { (a, b) => invoke a#0 } capturing [a, b];
        //   substitute [k2] in jump L
        let onwards = Statement::Substitute(
            vec![n(10)],
            Box::new(block(vec![n(10)], Statement::Jump(Label(0)))),
        );
        let s = Statement::New {
            name: n(10),
            captures: vec![n(1), n(2)],
            methods: vec![block(vec![n(1), n(2)], Statement::Invoke(n(1), 0))],
            rest: Box::new(onwards.clone()),
        };
        let used = still_used(&s).expect("narrowable");
        assert!(used.contains(&n(1)), "captured, so read here");
        assert!(used.contains(&n(2)));
        assert!(used.contains(&n(10)), "and the object is handed on");

        // After the capture the frame wants only the object. What the method
        // does with `a` and `b` later is the method's business; they are in it.
        let after = still_used(&onwards).expect("narrowable");
        assert!(
            !after.contains(&n(1)),
            "the method's use is not this frame's"
        );
        assert!(!after.contains(&n(2)));
    }

    /// A bare `jump` or `invoke` hands the environment on whole -- one to the
    /// block it names, the other to the method as its arguments -- so nothing
    /// in it can be called dead.
    #[test]
    fn handing_the_environment_on_whole_narrows_nothing() {
        assert!(still_used(&Statement::Jump(Label(1))).is_none());
        assert!(still_used(&Statement::Invoke(n(4), 0)).is_none());
        // And it carries: a statement that ends in one narrows nothing either.
        let ends_in_invoke = Statement::Let {
            name: n(9),
            tag: 0,
            ctor: meadow_intern::InternedString::from("K"),
            fields: vec![n(1)],
            rest: Box::new(Statement::Invoke(n(9), 0)),
        };
        assert!(still_used(&ends_in_invoke).is_none());
    }

    /// A `switch` arm and an `extern`'s continuation run on the same registers
    /// in the same activation, so what they read is read here.
    #[test]
    fn an_arm_reads_in_the_frame_it_is_in() {
        let go = |to: Name| {
            Statement::Substitute(
                vec![to],
                Box::new(block(vec![to], Statement::Jump(Label(0)))),
            )
        };
        let s = Statement::Switch {
            scrutinee: n(1),
            arms: vec![(0, block(vec![n(5)], go(n(2))))],
            default: Box::new(block(vec![], go(n(3)))),
        };
        let used = still_used(&s).expect("narrowable");
        for name in [1, 2, 3] {
            assert!(used.contains(&n(name)), "{name} is read here");
        }
    }

    /// A `jump` carries the environment a `substitute` just chose, and that
    /// choice is where those names were used. Counting the jump as well would
    /// make every name live for ever.
    #[test]
    fn a_jump_needs_nothing_the_substitute_did_not_name() {
        let jump = Statement::Substitute(
            vec![n(7), n(8)],
            Box::new(block(vec![n(7), n(8)], Statement::Jump(Label(3)))),
        );
        let used = still_used(&jump).expect("narrowable");
        assert_eq!(used.len(), 2, "exactly what the selection named");
        assert!(used.contains(&n(7)) && used.contains(&n(8)));
        assert!(
            still_used(&Statement::Jump(Label(3))).is_none(),
            "a bare jump carries the environment whole"
        );
    }

    /// The shape that motivated this: a continuation is built, an argument is
    /// computed, and the frame jumps. After the capture, neither the old
    /// continuation nor the value it captured is needed -- which is two
    /// registers, and on `fib` two `move` instructions per call.
    #[test]
    fn a_call_stops_needing_what_its_continuation_took() {
        // new kk capturing [n, k, ev]; extern sub(n) { (m, ...) =>
        //   substitute [m, kk, ev] in jump L }
        let jump = Statement::Substitute(
            vec![n(20), n(10), n(3)],
            Box::new(block(vec![n(20), n(10), n(3)], Statement::Jump(Label(0)))),
        );
        let after_capture = Statement::Extern {
            op: Extern::Prim(meadow_rt::Prim::Sub),
            args: vec![n(1)],
            blocks: vec![block(vec![n(20), n(1), n(2), n(3)], jump)],
        };
        let s = Statement::New {
            name: n(10),
            captures: vec![n(1), n(2), n(3)],
            methods: vec![block(
                vec![n(1), n(2), n(3), n(21)],
                Statement::Invoke(n(2), 0),
            )],
            rest: Box::new(after_capture.clone()),
        };

        // At the `new`, `k` (2) has been captured and is not read again.
        let at_new = still_used(&s).expect("narrowable");
        assert!(
            at_new.contains(&n(1)),
            "n is still the subtraction's operand"
        );
        assert!(at_new.contains(&n(2)), "k is captured, which is a read");

        // After it, `k` is gone -- its register is free.
        let rest = still_used(&after_capture).expect("narrowable");
        assert!(!rest.contains(&n(2)), "k is dead once captured");
        assert!(rest.contains(&n(1)), "n is not, yet");

        // And after the subtraction, `n` is gone too.
        let Statement::Extern { blocks, .. } = &after_capture else {
            unreachable!()
        };
        let at_jump = still_used(&blocks[0].body).expect("narrowable");
        assert!(!at_jump.contains(&n(1)), "n is dead after the subtraction");
        assert!(!at_jump.contains(&n(2)));
        assert_eq!(at_jump.len(), 3, "only what the jump carries");
    }
}

/// A `&'static str` read back from a program's file: leaked, since the
/// message lives as long as the program does: a `Statement::Error`'s, and a
/// `linear::L::Error`'s.
pub(crate) fn leaked<'de, D: serde::Deserializer<'de>>(d: D) -> Result<&'static str, D::Error> {
    let s: String = serde::Deserialize::deserialize(d)?;
    Ok(Box::leak(s.into_boxed_str()))
}
