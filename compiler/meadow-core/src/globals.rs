//! **Top-level definitions, evaluated once.**
//!
//! A mention of a top-level name lowers to a jump to its definition, so without
//! this pass `def table = build 1000000` builds the table again at every use.
//! That is only a matter of speed, because a top-level `def` other than `main`
//! cannot perform effects -- the type checker refuses one that does -- and
//! evaluating a pure expression twice gives the same value twice.
//!
//! So each definition that is not a function gets a cache. Its body becomes
//!
//! ```text
//! if cached i then cached-value i
//! else let v = <body> in let _ = cache i v in v
//! ```
//!
//! with the three [`Prim::GlobalReady`], [`Prim::GlobalGet`] and
//! [`Prim::GlobalSet`], where `i` is the definition's position. The first use
//! computes and keeps the value; every later one reads it back. A definition
//! nothing uses is never evaluated, and one that fails is not cached, so it
//! fails the same way wherever it is used.
//!
//! A function needs none of this: its value is the closure, which is as cheap to
//! make as to look up, and calling it is supposed to run it every time. Nor does
//! a literal, or the entry point, which the runtime evaluates exactly once. A
//! definition generic over a type cannot have it: its value is made from the
//! descriptors of the types it is used at, which differ from use to use.
//!
//! The cache belongs to the machine running the code. On the bytecode VM that is
//! one green thread, and each keeps its own -- a thread never reads another's
//! heap -- so a definition is computed at most once per thread that uses it.
//! Runs on erased core, just before lowering.

use crate::*;

/// Where the pass's own names start: clear of units, of synthetic definitions,
/// and of specialized copies.
pub const GLOBALS_BASE: u32 = 0x7C00_0000;

/// Replace every mention of a top-level definition whose value is a literal
/// with the literal itself.
///
/// A mention of a global is a *jump* to its definition (see
/// `meadow_seq::lower`), and a jump is a transfer of control. That is fine for
/// a table built once, and ruinous for a bound written as
/// `def limit : Int = 100` and then read in a loop condition, because lowering
/// only fuses a comparison into the branch that tests it when the comparison
/// cannot transfer control. So this
///
/// ```text
///   def limit : Int = 100
///   fun loop (i : Int) acc = if i >= limit then acc else loop (i + 1) (acc + i)
/// ```
///
/// compiled to two heap-allocated continuations and an unfused compare *per
/// iteration*, where the same loop over a parameter compiled to a single
/// `bri`. Inlining the literal makes the two identical.
///
/// Only literals, and constructors applied to nothing -- `Empty`, `None` at a
/// type, which specializing a generic `def empty : [a] = Empty` makes one of
/// for every type it is used at: they cost nothing to evaluate, cannot fail,
/// and cannot observe anything, so a mention is worth exactly as much as the
/// definition. Cached instead, every `[]` a parser wrote in a loop was a
/// question to the runtime and a jump back.
/// A generic definition is left alone -- its value is made from the descriptors
/// of the types it is used at.
///
/// The definitions stay; `prune` drops the ones nothing mentions any more, and
/// the ones a package exports have to remain.
pub fn inline_literals(p: &Program) -> Program {
    let mut lits: HashMap<Var, Term> = HashMap::new();
    for d in &p.defs {
        if Some(d.var) == p.entry || is_generic(&d.term) {
            continue;
        }
        if let Some(l) = literal(&d.term) {
            lits.insert(d.var, l);
        }
    }
    if lits.is_empty() {
        return p.clone();
    }
    let put = &mut |t: Term| match &t {
        Term::Var(v) => lits.get(v).cloned().unwrap_or(t),
        // A mention of a binding is written `e [T, …]`, and a literal has
        // nothing to instantiate -- nor has a constructor already at its type.
        Term::TyApp(inner, _) if matches!(&**inner, Term::Lit(_)) || is_constant(inner) => {
            (**inner).clone()
        }
        _ => t,
    };
    let mut out = p.clone();
    for d in &mut out.defs {
        d.term = crate::rewrite::term(&d.term, put, &mut |q| q);
    }
    out
}

/// The literal a definition's body is, or the constructor applied to nothing,
/// under any number of source positions.
fn literal(t: &Term) -> Option<Term> {
    match t {
        Term::Loc(_, inner) => literal(inner),
        Term::Lit(_) => Some(t.clone()),
        _ if is_constant(t) => Some(t.clone()),
        _ => None,
    }
}

/// A constructor applied to nothing: a word, like a literal.
fn is_constant(t: &Term) -> bool {
    matches!(t, Term::Ctor(_, _, args) if args.is_empty())
}

pub fn program(p: &Program) -> Program {
    let mut next = GLOBALS_BASE;
    let mut fresh = || {
        let v = hir::VarId(next);
        next += 1;
        v
    };
    let defs = p
        .defs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            // The entry point runs once as the program; a literal costs nothing
            // to evaluate again.
            if Some(d.var) == p.entry
                || is_function(&d.term)
                || is_literal(&d.term)
                || is_generic(&d.term)
            {
                return d.clone();
            }
            let index = || Term::Lit(Lit::Int(i as i64));
            let value = fresh();
            let ignored = fresh();
            // Typed like everything else: below core, what a name holds is read
            // from its type.
            let ty = || d.poly.ty.clone();
            let con = |n: &str| InferType::Con(InternedString::from(n), Vec::new());
            let compute = Term::Let(
                value,
                Poly::mono(ty()),
                Arc::new(d.term.clone()),
                Arc::new(Term::Let(
                    ignored,
                    Poly::mono(con("Unit")),
                    Arc::new(Term::Prim(
                        Prim::GlobalSet,
                        vec![index(), Term::Var(value)],
                        con("Unit"),
                    )),
                    Arc::new(Term::Var(value)),
                )),
            );
            let term = Term::If(
                Arc::new(Term::Prim(Prim::GlobalReady, vec![index()], con("Bool"))),
                Arc::new(Term::Prim(Prim::GlobalGet, vec![index()], ty())),
                Arc::new(compute),
            );
            Def {
                var: d.var,
                name: d.name,
                module: d.module,
                poly: d.poly.clone(),
                term,
            }
        })
        .collect();
    Program {
        defs,
        entry: p.entry,
        ctor_fields: p.ctor_fields.clone(),
        variants: p.variants.clone(),
        origins: p.origins.clone(),
        migrations: p.migrations.clone(),
    }
}

/// Is this definition generic over a type? Then what it evaluates to depends
/// on the descriptors it is instantiated with (`crate::desc`), so one cached
/// value would answer every instantiation with the first one's. It is
/// evaluated at each use instead, which purity makes unobservable.
fn is_generic(t: &Term) -> bool {
    match t {
        Term::Loc(_, inner) => is_generic(inner),
        Term::TyLam(vs, _) => vs.iter().any(|v| v.kind == meadow_infer::VarKind::Type),
        _ => false,
    }
}

fn is_literal(t: &Term) -> bool {
    match t {
        Term::Loc(_, inner) => is_literal(inner),
        Term::Lit(_) => true,
        _ => is_constant(t),
    }
}

/// Is this definition a function -- a value that is only a closure?
pub fn is_function(t: &Term) -> bool {
    match t {
        Term::Loc(_, inner) | Term::TyLam(_, inner) => is_function(inner),
        Term::Lam(..) => true,
        _ => false,
    }
}
