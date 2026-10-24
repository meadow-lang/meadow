//! **Core: a typed lambda calculus that every front-end feature lowers to.**
//!
//! Inference runs on the HIR; once a program type-checks we translate it here.
//! Core is deliberately tiny — single-argument lambdas and applications,
//! explicit recursion, primitives already resolved, pattern matching reduced to
//! `Case` plus tuple and record projection.
//!
//! # Why it is typed
//!
//! It is **System F**, near enough: every binder carries its type, every
//! generalized binding is a [`Term::TyLam`], and every mention of a
//! polymorphic name is a [`Term::TyApp`] carrying the types it was
//! instantiated at. So each term has exactly one type, computable in one
//! bottom-up pass with nothing to infer — which is what makes [`lint`] cheap
//! and total, and what makes an optimisation pass *checkable*.
//!
//! That is the whole reason for the types. An inlining that substitutes the
//! wrong type, a specialization that drops a type argument, a `case` rebuilt
//! with an arm of the wrong type — each is a miscompile that runs, and runs
//! wrong. GHC's answer is a typed core and `-dcore-lint`; this is the same
//! answer, and the lint runs after every pass in a debug build and in the
//! whole test suite.
//!
//! Two conventions are worth knowing before reading the types:
//!
//! * A core type is an [`meadow_infer::Type`] in which **`Var(n)` is a rigid
//!   type variable** — bound by an enclosing `TyLam`, never solved by
//!   anything. `Bound` does not appear; a core polytype ([`Poly`]) writes its
//!   binders down instead of numbering them.
//! * [`unknown`] is the type of something the compiler could not type, which
//!   only happens in a unit that already has errors. The lint accepts it
//!   anywhere.
//!
//! # What is downstream
//!
//! Nothing. [`erase`] drops the type abstractions on the way out, and AxCut,
//! the bytecode machine and the CEK evaluator are all untyped: none of them
//! can ask a question a type would answer.

pub mod bools;
pub mod defaults;
pub mod dictionaries;
pub mod erase;
pub mod globals;
pub mod inline;
pub mod joins;
pub mod lift;
pub mod lint;
pub mod lower;
pub mod prune;
pub mod rewrite;
pub mod simplify;
pub mod specialize;
pub mod trmc;
pub mod versions;
pub use lower::Lowerer;
pub mod desc;
pub use meadow_rt::{
    args, compact, console, ffi, hash, log, net, num, stm, terminal, text, thread,
};

use meadow_hir as hir;
/// How a canonical name is written, for the runtimes' messages.
pub use meadow_hir::{ctor_spelling, spelling};
use meadow_infer::{Generalized, Scheme, Type as InferType, TypeTable, VarKind, VariantEnv};
use meadow_intern::InternedString;
use std::collections::HashMap;
use std::sync::Arc;

// The values and primitives the runtimes share, and what they agree on about
// them: see `meadow-rt`.
pub use meadow_rt::{Lit, Loc, OptLevel, Prim, fmt_float, roles};

pub type Var = hir::VarId;

/// A type, as core writes them.
///
/// The same representation inference uses, with one reinterpretation that is
/// the whole of core's type discipline: **`Type::Var(n)` is a rigid type
/// variable**, with an id unique across the compilation unit, bound by an
/// enclosing [`Term::TyLam`] or by a [`Def`]'s [`Poly`]. It is not a
/// unification variable — nothing in core solves anything — and
/// `Type::Bound` never appears here at all, since a core polytype writes its
/// binders down rather than numbering them.
pub type Ty = InferType;

/// A rigid type variable: its id, and what sort of thing it ranges over
/// (an ordinary type, a record row, an effect row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TyVar {
    pub id: u32,
    pub kind: VarKind,
}

/// A polytype — `forall (a : k) …. t` — as a definition or a `let` binder
/// carries it. Monomorphic when `binders` is empty, which is the common case.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Poly {
    pub binders: Vec<TyVar>,
    pub ty: Ty,
}

impl Poly {
    pub fn mono(ty: Ty) -> Poly {
        Poly {
            binders: Vec::new(),
            ty,
        }
    }

    pub fn is_mono(&self) -> bool {
        self.binders.is_empty()
    }

    /// The type this polytype takes at `args`, one per binder.
    ///
    /// Substituting rather than unifying: core says what the arguments are, so
    /// instantiation is a walk.
    pub fn instantiate(&self, args: &[Ty]) -> Ty {
        if self.binders.is_empty() {
            return self.ty.clone();
        }
        let map: HashMap<u32, Ty> = self
            .binders
            .iter()
            .zip(args)
            .map(|(b, a)| (b.id, a.clone()))
            .collect();
        subst_rigid(&self.ty, &map)
    }
}

/// The type of something the compiler could not type: a node inference never
/// reached, or a type argument that could not be recovered. Both only happen
/// in a unit that already has errors, and [`crate::lint`] accepts it anywhere
/// rather than piling a second complaint on the first.
pub fn unknown() -> Ty {
    InferType::Con(InternedString::from("?"), Vec::new())
}

/// Is this the placeholder [`unknown`] type?
pub fn is_unknown(ty: &Ty) -> bool {
    matches!(ty, InferType::Con(n, args) if args.is_empty() && &**n == "?")
}

/// Does the placeholder [`unknown`] type appear anywhere in this type?
///
/// Like [`meadow_infer::Type::references_error`], and asked in the same
/// places, because the two mean the same thing: the unit has an error in it
/// already. They are separate because they come from different places -- the
/// checker writes `Error` where it found the fault, and lowering writes `?`
/// where it gave up carrying a type it could not recover. Either can reach a
/// mention whose type arguments then cannot be worked out, and neither is
/// worth a second complaint.
pub fn mentions_unknown(ty: &Ty) -> bool {
    if is_unknown(ty) {
        return true;
    }
    match ty {
        InferType::Var(_) | InferType::Bound(_) | InferType::RowEmpty | InferType::Error => false,
        InferType::Con(_, args) | InferType::Tuple(args) => args.iter().any(mentions_unknown),
        InferType::Fun(ps, r, e) => {
            ps.iter().any(mentions_unknown) || mentions_unknown(r) || mentions_unknown(e)
        }
        InferType::Record(r) => mentions_unknown(r),
        InferType::RowExtend(_, f, rest) => mentions_unknown(f) || mentions_unknown(rest),
    }
}

/// Replace a scheme's numbered quantifiers by types — the bridge from an
/// inference `Scheme` to a core [`Poly`], whose binders are named.
pub fn subst_bound(ty: &Ty, map: &HashMap<u32, Ty>) -> Ty {
    match ty {
        InferType::Bound(i) => map.get(i).cloned().unwrap_or_else(|| ty.clone()),
        InferType::Var(_) | InferType::RowEmpty | InferType::Error => ty.clone(),
        InferType::Con(n, args) => {
            InferType::Con(*n, args.iter().map(|a| subst_bound(a, map)).collect())
        }
        InferType::Fun(args, ret, eff) => InferType::Fun(
            args.iter().map(|a| subst_bound(a, map)).collect(),
            Box::new(subst_bound(ret, map)),
            Box::new(subst_bound(eff, map)),
        ),
        InferType::Tuple(items) => {
            InferType::Tuple(items.iter().map(|a| subst_bound(a, map)).collect())
        }
        InferType::Record(row) => InferType::Record(Box::new(subst_bound(row, map))),
        InferType::RowExtend(label, field, rest) => InferType::RowExtend(
            *label,
            Box::new(subst_bound(field, map)),
            Box::new(subst_bound(rest, map)),
        ),
    }
}

/// Replace rigid type variables by types.
pub fn subst_rigid(ty: &Ty, map: &HashMap<u32, Ty>) -> Ty {
    match ty {
        InferType::Var(v) => map.get(v).cloned().unwrap_or_else(|| ty.clone()),
        InferType::Bound(_) | InferType::RowEmpty | InferType::Error => ty.clone(),
        InferType::Con(n, args) => {
            InferType::Con(*n, args.iter().map(|a| subst_rigid(a, map)).collect())
        }
        InferType::Fun(args, ret, eff) => InferType::Fun(
            args.iter().map(|a| subst_rigid(a, map)).collect(),
            Box::new(subst_rigid(ret, map)),
            Box::new(subst_rigid(eff, map)),
        ),
        InferType::Tuple(items) => {
            InferType::Tuple(items.iter().map(|a| subst_rigid(a, map)).collect())
        }
        InferType::Record(row) => InferType::Record(Box::new(subst_rigid(row, map))),
        InferType::RowExtend(label, field, rest) => InferType::RowExtend(
            *label,
            Box::new(subst_rigid(field, map)),
            Box::new(subst_rigid(rest, map)),
        ),
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Pat {
    Wild,
    /// A bound variable and the type it is bound at. Annotated like every other
    /// binder: the checker types an arm's body in an environment built from
    /// this, rather than working the types out from the scrutinee itself.
    Var(Var, Ty),
    As(Var, Ty, Box<Pat>),
    Lit(Lit),
    Tuple(Vec<Pat>),
    /// `#[p, …]` — matches a builtin `Array` of exactly this length.
    Array(Vec<Pat>),
    Ctor(InternedString, Vec<Pat>),
    Record(Vec<(InternedString, Pat)>),
}

/// Core terms. Recursive positions are `Arc<Term>` (not `Box`) so the CEK
/// interpreter (the `meadow-eval` crate) can share subterms freely — a captured
/// continuation is just a slice of `Arc`-holding stack frames.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Term {
    Var(Var),
    Lit(Lit),
    /// `\(x : T) -> e`
    Lam(Var, Ty, Arc<Term>),
    /// `/\(a : k) … -> e` — the type abstraction a generalized binding gets.
    /// Its binders are what the annotations inside `e` refer to.
    TyLam(Vec<TyVar>, Arc<Term>),
    App(Arc<Term>, Arc<Term>),
    /// `e [T, …]` — the instantiation of a polymorphic binding, written out.
    /// Every mention of a polymorphic name is wrapped in one of these.
    TyApp(Arc<Term>, Vec<Ty>),
    Let(Var, Poly, Arc<Term>, Arc<Term>),
    LetRec(Vec<(Var, Poly, Term)>, Arc<Term>),
    If(Arc<Term>, Arc<Term>, Arc<Term>),
    Tuple(Vec<Term>),
    Proj(Arc<Term>, usize),
    /// `#[e, …]` — a builtin `Array` literal. The *only* built-in collection:
    /// `List` and `Vector` are ordinary `Std` data types and lower to `Ctor`.
    /// The type is the element type, which an empty literal could not otherwise
    /// supply.
    Array(Vec<Term>, Ty),
    Record(Vec<(InternedString, Term)>),
    Sel(Arc<Term>, InternedString, Ty),
    Extend(Arc<Term>, InternedString, Arc<Term>),
    /// A saturated constructor application, with the type it builds — from
    /// which the checker instantiates the constructor's field types.
    Ctor(InternedString, Ty, Vec<Term>),
    /// `case scrut of …` and the type every arm has to produce. An arm is its
    /// pattern, a guard -- a `Bool`, in the scope of the pattern's variables --
    /// that must also hold for it to be taken, and its body. An arm whose guard
    /// is false is passed over for the next, as if its pattern had not matched.
    Case(Arc<Term>, Vec<(Pat, Option<Term>, Term)>, Ty),
    Prim(Prim, Vec<Term>, Ty),
    /// `perform Effect.op arg` — an algebraic-effect operation call, and the
    /// type it resumes with.
    Perform(InternedString, InternedString, Arc<Term>, Ty),
    /// `handle body with { … }` — an effect handler.
    Handle {
        body: Arc<Term>,
        clauses: Vec<HClause>,
        /// `return x -> e` — defaults to the identity when absent.
        ret: Option<(Var, Ty, Arc<Term>)>,
        /// What the whole `handle` produces.
        ty: Ty,
    },
    /// `join j (x : T)… = rhs in body` — a binding that is only ever *jumped*
    /// to, never called.
    ///
    /// A join point is a `let` with a promise: every mention of `j` in `body`
    /// is a saturated [`Term::Jump`] in tail position, so `j` never escapes,
    /// never needs a closure, and never needs its free variables captured —
    /// they are still in scope where it is entered. It is a name for a
    /// continuation that several places share.
    ///
    /// That is what makes it worth having. Transformations that push a context
    /// inwards -- case-of-case above all -- otherwise have to choose between
    /// copying the context into every branch, which can square the size of a
    /// program, and building a closure for it, which allocates. A join point
    /// is the third answer: name it once, jump to it from each branch, and let
    /// the back end make it a label. `meadow_seq` does exactly that, and its
    /// IR has had labels all along.
    ///
    /// The promise is not checked by the type system here. It is *established*
    /// by [`crate::joins`], which only makes a join point where it holds, and
    /// preserved by construction because nothing else creates one.
    Join {
        var: Var,
        params: Vec<(Var, Ty)>,
        /// What entering it produces, which is what `body` produces.
        ty: Ty,
        rhs: Arc<Term>,
        body: Arc<Term>,
    },
    /// `jump j a…` — entering a join point. Always saturated, always in tail
    /// position. The type is what it produces, which is the `Join`'s.
    Jump(Var, Vec<Term>, Ty),
    /// A term that failed to compile. Well-typed at any type, on purpose: it
    /// only exists in a unit that already has errors, and the checker has
    /// nothing useful to say about it.
    Error,
    /// `e`, which was written at `loc` -- present only when the unit was
    /// compiled for a debugger, and meaning exactly what `e` means.
    ///
    /// Only placed where a person would want to stop: a call, a `perform`, a
    /// function's body, the branches of an `if` and the arms of a `match`.
    /// Never around a literal or a condition, which the back end inspects to
    /// fold a constant or fuse a comparison into its branch: a debug build
    /// should run the program the ordinary build runs.
    Loc(Loc, Arc<Term>),
}

impl Term {
    /// The term under any [`Term::Loc`]s.
    pub fn peel(&self) -> &Term {
        let mut t = self;
        while let Term::Loc(_, inner) = t {
            t = inner;
        }
        t
    }
}

impl Term {
    // --- untyped constructors -------------------------------------------
    //
    // For terms built where no type is available or wanted: a test, or code
    // the driver invents after type checking is over (the entry point the
    // test runner wraps around a `@test` function, say). They annotate with
    // [`unknown`], which [`crate::lint`] accepts anywhere.

    pub fn lam(v: Var, body: Term) -> Term {
        Term::Lam(v, unknown(), Arc::new(body))
    }

    pub fn let_(v: Var, rhs: Term, body: Term) -> Term {
        Term::Let(v, Poly::mono(unknown()), Arc::new(rhs), Arc::new(body))
    }

    pub fn letrec(binds: Vec<(Var, Term)>, body: Term) -> Term {
        Term::LetRec(
            binds
                .into_iter()
                .map(|(v, t)| (v, Poly::mono(unknown()), t))
                .collect(),
            Arc::new(body),
        )
    }

    pub fn prim(op: Prim, args: Vec<Term>) -> Term {
        Term::Prim(op, args, unknown())
    }

    pub fn ctor(name: impl Into<InternedString>, args: Vec<Term>) -> Term {
        Term::Ctor(name.into(), unknown(), args)
    }

    pub fn case(scrut: Term, arms: Vec<(Pat, Term)>) -> Term {
        let arms = arms.into_iter().map(|(p, b)| (p, None, b)).collect();
        Term::Case(Arc::new(scrut), arms, unknown())
    }

    pub fn array(items: Vec<Term>) -> Term {
        Term::Array(items, unknown())
    }

    pub fn sel(rec: Term, label: impl Into<InternedString>) -> Term {
        Term::Sel(Arc::new(rec), label.into(), unknown())
    }

    pub fn perform(
        effect: impl Into<InternedString>,
        op: impl Into<InternedString>,
        arg: Term,
    ) -> Term {
        Term::Perform(effect.into(), op.into(), Arc::new(arg), unknown())
    }
}

impl Pat {
    pub fn var(v: Var) -> Pat {
        Pat::Var(v, unknown())
    }

    pub fn as_(v: Var, sub: Pat) -> Pat {
        Pat::As(v, unknown(), Box::new(sub))
    }
}

impl Program {
    /// The type of calling definition `f` with `()` -- what a test runner's
    /// stand-in for an entry point has, since that is all it does.
    pub fn result_of_calling(&self, f: Var) -> Poly {
        let ty = self
            .defs
            .iter()
            .find(|d| d.var == f)
            .and_then(|d| match &d.poly.ty {
                InferType::Fun(_, ret, _) => Some((**ret).clone()),
                _ => None,
            });
        Poly::mono(ty.unwrap_or_else(unknown))
    }
}

impl Def {
    /// A definition with no type worth stating — see [`Term::lam`] and friends.
    pub fn untyped(var: Var, name: impl Into<InternedString>, term: Term) -> Def {
        Def {
            var,
            name: name.into(),
            module: InternedString::default(),
            poly: Poly::mono(unknown()),
            term,
        }
    }
}

/// One operation clause of a handler: `op param resume -> body`. `resume` is bound
/// to the (one-shot, deep) continuation.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HClause {
    pub effect: InternedString,
    pub op: InternedString,
    pub param: Var,
    /// The operation's argument type.
    pub param_ty: Ty,
    pub resume: Var,
    /// `resume`'s type: `a -> r` from the operation's result to the handler's.
    pub resume_ty: Ty,
    pub body: Term,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Def {
    pub var: Var,
    pub name: InternedString,
    /// The module it was written in, package first: `Std.Collections.Vector`.
    /// A definition a pass makes out of another -- a specialized copy, a
    /// lifted lambda, a worker -- is of the module that one is. Empty for one
    /// nobody wrote: a test's entry, a REPL line's.
    ///
    /// What the native back end divides a program by (`meadow_llvm`): a
    /// module's code is a file of its own, which is the same file while the
    /// module is.
    #[serde(default)]
    pub module: InternedString,
    /// The definition's type. A polymorphic one's `term` is a [`Term::TyLam`]
    /// binding exactly these binders.
    pub poly: Poly,
    pub term: Term,
}

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub defs: Vec<Def>,
    pub entry: Option<Var>,
    /// Named-field order for each data/record constructor, so `.field` selection
    /// works on `Value::Ctor` at runtime.
    pub ctor_fields: HashMap<InternedString, Vec<InternedString>>,
    /// Every data and record type's constructors and their field types, by type
    /// name: what a backend needs to know what is in a field it did not name.
    pub variants: meadow_infer::VariantEnv,
    /// Variables a pass made as copies of others, and which: what a debugger
    /// needs to call a specialized copy's variables by their names.
    pub origins: HashMap<Var, Var>,
}

impl Program {
    /// A stable, human-readable rendering of the whole program.
    ///
    /// [`Var`]s (`hir::VarId`) come from a process-wide counter, so their numeric
    /// values are non-deterministic across runs — this printer renumbers them
    /// `v0, v1, …` in first-occurrence order, which makes it safe to snapshot.
    pub fn pretty(&self) -> String {
        let mut p = Printer::default();
        let mut out = String::new();
        for d in &self.defs {
            let v = p.var(d.var);
            out.push_str(&format!("{v} : {}\n", p.poly(&d.poly)));
            out.push_str(&format!("{v} = {}\n", p.term(&d.term)));
        }
        if let Some(e) = self.entry {
            out.push_str(&format!("entry: {}\n", p.var(e)));
        }
        out
    }
}

/// `p` as text that is the same in every build of a program: its variables
/// named in the order they appear, as [`Program::pretty`] names them, rather
/// than by ids that come from the inference arena -- and each type and effect
/// in it with its version, from `versions`, where it has one: so that a type
/// a build changed makes a different signature of everything that mentions
/// it (see [`versions`]).
pub fn signature(p: &Poly, versions: &versions::Versions) -> String {
    Printer {
        versions: Some(versions.clone().into()),
        ..Printer::default()
    }
    .poly(p)
}

/// What constructor `v`'s fields are, written as [`signature`] writes a
/// type: `(Int, String)`, or with their labels, `(x : Int, y : Int)`.
pub fn variant_signature(v: &meadow_infer::VariantSig) -> String {
    let mut p = Printer::default();
    let fields: Vec<String> = match &v.labels {
        Some(ls) => ls
            .iter()
            .zip(&v.fields)
            .map(|(l, t)| format!("{l} : {}", p.ty(t)))
            .collect(),
        None => v.fields.iter().map(|t| p.ty(t)).collect(),
    };
    format!("({})", fields.join(", "))
}

#[derive(Default)]
struct Printer {
    names: HashMap<Var, String>,
    next: u32,
    /// Rigid type variables, named in first-occurrence order for the same
    /// reason values are: their ids come from the inference arena and mean
    /// nothing to a reader.
    tyvars: HashMap<u32, String>,
    next_ty: u32,
    /// What [`signature`] writes types and effects with.
    versions: Option<std::sync::Arc<versions::Versions>>,
}

impl Printer {
    /// Type `n`, with its version where it has one.
    fn con(&self, n: &InternedString) -> String {
        let shown = hir::spelling(n).to_string();
        match self.versions.as_ref().and_then(|v| v.types.get(n)) {
            Some(v) => format!("{shown}#{v}"),
            None => shown,
        }
    }

    /// An effect row's label `l`, with its effect's version where it has one.
    fn effect(&self, l: &InternedString) -> String {
        match self.versions.as_ref().and_then(|v| v.effects.get(l)) {
            Some(v) => format!("{l}#{v}"),
            None => l.to_string(),
        }
    }

    /// A rigid type variable's printed name: `a`, `b`, … then `a1`, `b1`.
    fn tyvar(&mut self, id: u32) -> String {
        if let Some(s) = self.tyvars.get(&id) {
            return s.clone();
        }
        let n = self.next_ty;
        self.next_ty += 1;
        let letter = (b'a' + (n % 26) as u8) as char;
        let s = if n < 26 {
            letter.to_string()
        } else {
            format!("{letter}{}", n / 26)
        };
        self.tyvars.insert(id, s.clone());
        s
    }

    fn ty(&mut self, t: &Ty) -> String {
        match t {
            InferType::Var(v) => self.tyvar(*v),
            InferType::Bound(i) => format!("?{i}"),
            InferType::RowEmpty => "{}".to_string(),
            InferType::Error => "{error}".to_string(),
            InferType::Con(n, args) if args.is_empty() => self.con(n),
            InferType::Con(n, args) => {
                let parts: Vec<String> = args.iter().map(|a| self.ty(a)).collect();
                format!("({} {})", self.con(n), parts.join(" "))
            }
            InferType::Fun(args, ret, eff) => {
                let parts: Vec<String> = args.iter().map(|a| self.ty(a)).collect();
                let e = match &**eff {
                    InferType::RowEmpty => String::new(),
                    other @ InferType::RowExtend(..) => format!(" ! {}", self.row(other, true)),
                    other => format!(" ! {}", self.ty(other)),
                };
                format!("({} -> {}{e})", parts.join(" "), self.ty(ret))
            }
            InferType::Tuple(items) => {
                let parts: Vec<String> = items.iter().map(|a| self.ty(a)).collect();
                format!("({})", parts.join(", "))
            }
            InferType::Record(row) => self.row(row, false),
            InferType::RowExtend(..) => self.row(t, false),
        }
    }

    /// A row, record or effect: `{ x : Int, y : Int }`, `{ Console, Mut | e }`.
    ///
    /// An effect's payload is the empty tuple when the effect takes no
    /// parameters, and writing `Console : ()` for that would be noise.
    fn row(&mut self, t: &Ty, effects: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut cur = t;
        loop {
            match cur {
                InferType::RowExtend(label, field, rest) => {
                    let label = if effects {
                        self.effect(label)
                    } else {
                        label.to_string()
                    };
                    parts.push(match &**field {
                        InferType::Tuple(xs) if xs.is_empty() => label,
                        other => format!("{label} : {}", self.ty(other)),
                    });
                    cur = rest;
                }
                InferType::RowEmpty => {
                    return format!("{{{}}}", parts.join(", "));
                }
                tail => {
                    let tail = self.ty(tail);
                    return if parts.is_empty() {
                        format!("{{| {tail}}}")
                    } else {
                        format!("{{{} | {tail}}}", parts.join(", "))
                    };
                }
            }
        }
    }

    fn poly(&mut self, p: &Poly) -> String {
        if p.binders.is_empty() {
            return self.ty(&p.ty);
        }
        let names: Vec<String> = p.binders.iter().map(|b| self.tyvar(b.id)).collect();
        format!("forall {}. {}", names.join(" "), self.ty(&p.ty))
    }

    fn tys(&mut self, ts: &[Ty]) -> String {
        ts.iter()
            .map(|t| format!("@{}", self.ty(t)))
            .collect::<Vec<_>>()
            .join(" ")
    }
    fn var(&mut self, v: Var) -> String {
        if let Some(s) = self.names.get(&v) {
            return s.clone();
        }
        let s = format!("v{}", self.next);
        self.next += 1;
        self.names.insert(v, s.clone());
        s
    }

    fn lit(l: &Lit) -> String {
        match l {
            Lit::Int(i) => i.to_string(),
            Lit::BigInt(i) => i.to_string(),
            Lit::Float(x) => fmt_float(*x),
            Lit::Word(w, b) => format!("{}{}", w.value(*b), w.name()),
            Lit::Float32(x) => format!("{}f32", num::fmt_float32(*x)),
            Lit::AnyInt(i, _) => format!("{i}?"),
            Lit::AnyFloat(x, _) => format!("{}?", fmt_float(*x)),
            Lit::Str(s) => format!("{:?}", &**s), // the string contents, quoted
            Lit::Sym(s) => format!("#{s}"),
            Lit::Char(c) => format!("{c:?}"),
            Lit::Bool(b) => b.to_string(),
            Lit::Unit => "()".to_string(),
        }
    }

    fn term(&mut self, t: &Term) -> String {
        match t {
            Term::Var(v) => self.var(*v),
            Term::Lit(l) => Self::lit(l),
            Term::Lam(v, t, b) => {
                let v = self.var(*v);
                let t = self.ty(t);
                format!("(\\({v} : {t}). {})", self.term(b))
            }
            Term::Join {
                var,
                params,
                rhs,
                body,
                ..
            } => {
                let j = self.var(*var);
                let ps: Vec<String> = params
                    .iter()
                    .map(|(v, t)| format!("({} : {})", self.var(*v), self.ty(t)))
                    .collect();
                let rhs = self.term(rhs);
                format!("(join {j} {} = {rhs} in {})", ps.join(" "), self.term(body))
            }
            Term::Jump(j, args, _) => {
                let j = self.var(*j);
                let args: Vec<String> = args.iter().map(|a| self.term(a)).collect();
                format!("(jump {j} {})", args.join(" "))
            }
            Term::TyLam(binders, b) => {
                let names: Vec<String> = binders.iter().map(|x| self.tyvar(x.id)).collect();
                format!("(/\\{}. {})", names.join(" "), self.term(b))
            }
            Term::TyApp(f, args) => {
                format!("({} {})", self.term(f), self.tys(args))
            }
            Term::App(f, a) => format!("({} {})", self.term(f), self.term(a)),
            Term::Let(v, p, r, b) => {
                let v = self.var(*v);
                let p = self.poly(p);
                format!("(let {v} : {p} = {} in {})", self.term(r), self.term(b))
            }
            Term::LetRec(binds, b) => {
                let parts: Vec<String> = binds
                    .iter()
                    .map(|(v, p, t)| {
                        let v = self.var(*v);
                        let p = self.poly(p);
                        format!("{v} : {p} = {}", self.term(t))
                    })
                    .collect();
                format!("(letrec {} in {})", parts.join("; "), self.term(b))
            }
            Term::If(c, t, e) => {
                format!("(if {} {} {})", self.term(c), self.term(t), self.term(e))
            }
            Term::Tuple(items) => format!("(tup {})", self.terms(items)),
            Term::Proj(t, i) => format!("({}.{i})", self.term(t)),
            Term::Array(items, t) => {
                let t = self.ty(t);
                format!("#[{} : {t}]", self.terms(items))
            }
            Term::Record(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(l, t)| format!("{l} = {}", self.term(t)))
                    .collect();
                format!("{{ {} }}", parts.join(", "))
            }
            Term::Sel(t, l, _) => format!("({}.{l})", self.term(t)),
            Term::Extend(t, l, v) => {
                format!("({} with {l} = {})", self.term(t), self.term(v))
            }
            Term::Ctor(n, t, args) => {
                let t = self.ty(t);
                format!("({} {} : {t})", hir::ctor_spelling(n), self.terms(args))
            }
            Term::Case(s, arms, _) => {
                let parts: Vec<String> = arms
                    .iter()
                    .map(|(p, g, b)| match g {
                        Some(g) => {
                            format!("{} if {} -> {}", self.pat(p), self.term(g), self.term(b))
                        }
                        None => format!("{} -> {}", self.pat(p), self.term(b)),
                    })
                    .collect();
                format!("(case {} of {})", self.term(s), parts.join("; "))
            }
            Term::Prim(op, args, _) => format!("({op:?} {})", self.terms(args)),
            Term::Perform(eff, op, arg, _) => {
                format!("(perform {eff}.{op} {})", self.term(arg))
            }
            Term::Handle {
                body, clauses, ret, ..
            } => {
                let mut parts: Vec<String> = clauses
                    .iter()
                    .map(|c| {
                        let p = self.var(c.param);
                        let k = self.var(c.resume);
                        let effect = hir::spelling(&c.effect.to_string()).to_string();
                        format!("{effect}.{} {p} {k} -> {}", c.op, self.term(&c.body))
                    })
                    .collect();
                if let Some((v, _, b)) = ret {
                    let v = self.var(*v);
                    parts.push(format!("return {v} -> {}", self.term(b)));
                }
                format!(
                    "(handle {} with {{ {} }})",
                    self.term(body),
                    parts.join("; ")
                )
            }
            // Transparent: a dump of a debug build reads like any other.
            Term::Loc(_, inner) => self.term(inner),
            Term::Error => "<error>".to_string(),
        }
    }

    fn terms(&mut self, ts: &[Term]) -> String {
        ts.iter()
            .map(|t| self.term(t))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn pat(&mut self, p: &Pat) -> String {
        match p {
            Pat::Wild => "_".to_string(),
            Pat::Var(v, _) => self.var(*v),
            Pat::As(v, _, sub) => {
                let v = self.var(*v);
                format!("{v}@{}", self.pat(sub))
            }
            Pat::Lit(l) => Self::lit(l),
            Pat::Tuple(ps) => format!("(tup {})", self.pats(ps)),
            Pat::Array(ps) => format!("#[{}]", self.pats(ps)),
            Pat::Ctor(n, ps) => format!("({} {})", hir::ctor_spelling(n), self.pats(ps)),
            Pat::Record(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(l, p)| format!("{l} = {}", self.pat(p)))
                    .collect();
                format!("{{ {} }}", parts.join(", "))
            }
        }
    }

    fn pats(&mut self, ps: &[Pat]) -> String {
        ps.iter().map(|p| self.pat(p)).collect::<Vec<_>>().join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prim_name_roundtrip() {
        for name in [
            "_primAdd",
            "_primSub",
            "_primMul",
            "_primDiv",
            "_primMod",
            "_primPow",
            "_primEq",
            "_primNe",
            "_primLt",
            "_primGt",
            "_primLe",
            "_primGe",
            "neg",
            "display",
            "_primAddF",
            "_primSubF",
            "_primMulF",
            "_primDivF",
            "_primLtF",
            "_primGtF",
            "_primLeF",
            "_primGeF",
            "_primShl",
            "toFloat",
            "floor",
        ] {
            assert!(Prim::from_name(name).is_some(), "{name} should be a prim");
        }
        assert_eq!(Prim::from_name("map"), None);
        assert_eq!(
            Prim::from_name("toUInt8"),
            Some(Prim::ToWord(num::Width::U8))
        );
        assert_eq!(Prim::from_name("toInt64"), Some(Prim::ToInt));
        assert_eq!(Prim::from_name("toString"), None);
        assert_eq!(Prim::from_name("+~"), None, "BigInt shares `+` now");
    }

    #[test]
    fn every_primitive_has_a_code_that_comes_back() {
        for c in 0..u16::MAX {
            match Prim::from_code(c) {
                Some(p) => assert_eq!(p.code(), c, "{p:?}"),
                None => {
                    assert!(Prim::from_code(c + 1).is_none(), "codes are dense");
                    break;
                }
            }
        }
    }

    #[test]
    fn prim_arity() {
        assert_eq!(Prim::Add.arity(), 2);
        assert_eq!(Prim::Neg.arity(), 1);
        assert_eq!(Prim::Display.arity(), 1);
        assert_eq!(Prim::Eq.arity(), 2);
    }

    #[test]
    fn pretty_renumbers_variables_and_shows_types() {
        let mut vars = hir::VarIdGen::starting_at(0);
        let a = vars.fresh();
        let b = vars.fresh();
        // `forall t. t -> t`, with `t` a rigid variable — as `id` lowers.
        let t = InferType::Var(7);
        let poly = Poly {
            binders: vec![TyVar {
                id: 7,
                kind: VarKind::Type,
            }],
            ty: InferType::Fun(
                vec![t.clone()],
                Box::new(t.clone()),
                Box::new(InferType::RowEmpty),
            ),
        };
        let prog = Program {
            defs: vec![Def {
                var: a,
                name: "id".into(),
                module: Default::default(),
                poly: poly.clone(),
                term: Term::TyLam(
                    poly.binders.clone(),
                    Arc::new(Term::Lam(b, t, Arc::new(Term::Var(b)))),
                ),
            }],
            entry: Some(a),
            ..Default::default()
        };
        // `a` is seen first (as the def name) -> v0; `b` -> v1; the one rigid
        // type variable -> a.
        assert_eq!(
            prog.pretty(),
            "v0 : forall a. (a -> a)\nv0 = (/\\a. (\\(v1 : a). v1))\nentry: v0\n"
        );
    }
}

// ===========================================================================
// Free variables
// ===========================================================================

/// The names bound around a point of a term, innermost last: a stack, to
/// unwind, and how many times each name is on it, to ask of in constant time.
/// A term nested thousands deep -- a program of a few thousand `let`s -- has
/// thousands of names bound around its innermost parts, and asking a list of
/// them at every variable made finding a term's free variables quadratic in
/// its depth.
#[derive(Default)]
struct Bound {
    stack: Vec<Var>,
    counts: std::collections::HashMap<Var, u32>,
}

impl Bound {
    fn contains(&self, v: &Var) -> bool {
        self.counts.get(v).is_some_and(|n| *n > 0)
    }

    fn len(&self) -> usize {
        self.stack.len()
    }

    fn push(&mut self, v: Var) {
        self.stack.push(v);
        *self.counts.entry(v).or_insert(0) += 1;
    }

    fn pop(&mut self) {
        if let Some(v) = self.stack.pop()
            && let Some(n) = self.counts.get_mut(&v)
        {
            *n -= 1;
        }
    }

    fn truncate(&mut self, len: usize) {
        while self.stack.len() > len {
            self.pop();
        }
    }

    fn extend(&mut self, vs: impl IntoIterator<Item = Var>) {
        for v in vs {
            self.push(v);
        }
    }
}

/// Adds the free variables of `t` to `out`.
pub fn free_vars_into(t: &Term, out: &mut std::collections::HashSet<Var>) {
    fn go(t: &Term, bound: &mut Bound, out: &mut std::collections::HashSet<Var>) {
        match t {
            Term::TyLam(_, b) | Term::TyApp(b, _) | Term::Loc(_, b) => go(b, bound, out),
            Term::Var(v) => {
                if !bound.contains(v) {
                    out.insert(*v);
                }
            }
            Term::Lit(_) | Term::Error => {}
            Term::Join {
                var,
                params,
                rhs,
                body,
                ..
            } => {
                let depth = bound.len();
                bound.extend(params.iter().map(|(v, _)| *v));
                go(rhs, bound, out);
                bound.truncate(depth);
                bound.push(*var);
                go(body, bound, out);
                bound.truncate(depth);
            }
            Term::Jump(j, args, _) => {
                if !bound.contains(j) {
                    out.insert(*j);
                }
                for a in args {
                    go(a, bound, out);
                }
            }
            Term::Lam(p, _, b) => {
                bound.push(*p);
                go(b, bound, out);
                bound.pop();
            }
            Term::App(f, a) => {
                go(f, bound, out);
                go(a, bound, out);
            }
            Term::Let(x, _, r, b) => {
                go(r, bound, out);
                bound.push(*x);
                go(b, bound, out);
                bound.pop();
            }
            Term::LetRec(binds, body) => {
                for (v, _, _) in binds {
                    bound.push(*v);
                }
                for (_, _, t) in binds {
                    go(t, bound, out);
                }
                go(body, bound, out);
                for _ in binds {
                    bound.pop();
                }
            }
            Term::If(a, b, c) => {
                go(a, bound, out);
                go(b, bound, out);
                go(c, bound, out);
            }
            Term::Tuple(xs) | Term::Array(xs, _) => {
                for x in xs {
                    go(x, bound, out);
                }
            }
            Term::Ctor(_, _, xs) | Term::Prim(_, xs, _) => {
                for x in xs {
                    go(x, bound, out);
                }
            }
            Term::Proj(t, _) | Term::Sel(t, _, _) => go(t, bound, out),
            Term::Extend(t, _, u) => {
                go(t, bound, out);
                go(u, bound, out);
            }
            Term::Record(fs) => {
                for (_, t) in fs {
                    go(t, bound, out);
                }
            }
            Term::Perform(_, _, a, _) => go(a, bound, out),
            Term::Case(s, arms, _) => {
                go(s, bound, out);
                for (p, g, t) in arms {
                    let before = bound.len();
                    let mut vs = Vec::new();
                    pat_vars(p, &mut vs);
                    bound.extend(vs);
                    if let Some(g) = g {
                        go(g, bound, out);
                    }
                    go(t, bound, out);
                    bound.truncate(before);
                }
            }
            Term::Handle {
                body, clauses, ret, ..
            } => {
                go(body, bound, out);
                for c in clauses {
                    bound.push(c.param);
                    bound.push(c.resume);
                    go(&c.body, bound, out);
                    bound.pop();
                    bound.pop();
                }
                if let Some((v, _, t)) = ret {
                    bound.push(*v);
                    go(t, bound, out);
                    bound.pop();
                }
            }
        }
    }
    go(t, &mut Bound::default(), out);
}

/// The variables a pattern binds.
pub fn pat_vars(p: &Pat, out: &mut Vec<Var>) {
    match p {
        Pat::Wild | Pat::Lit(_) => {}
        Pat::Var(v, _) => out.push(*v),
        Pat::As(v, _, sub) => {
            out.push(*v);
            pat_vars(sub, out);
        }
        Pat::Tuple(ps) | Pat::Array(ps) | Pat::Ctor(_, ps) => {
            for p in ps {
                pat_vars(p, out);
            }
        }
        Pat::Record(fs) => {
            for (_, p) in fs {
                pat_vars(p, out);
            }
        }
    }
}
