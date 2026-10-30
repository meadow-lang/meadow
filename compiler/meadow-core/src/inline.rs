//! **Inlining a small top-level function at the places that call it.**
//!
//! # In the pipeline
//!
//! `meadow_seq::lower_program` runs this after `bools` and before `joins`, at
//! [`OptLevel::inlines`] -- a release build. A debug build keeps every call a
//! call, so that the debugger has frames to show and lines to stop at.
//! It was written and left out for a while, because turning it on broke two
//! differential tests against the CEK machine, and neither was this pass's
//! fault: they were shapes lowering had never been given before. A record
//! accessor inlined at a record literal is a `match` on the literal, and
//! lowering typed a literal's fields as nothing, so the field it selected had
//! no representation; and a jump to a join point whose argument has to be
//! evaluated first has to keep the join's environment alive meanwhile, which
//! lowering's free-variable expansion knew for `letrec` bindings and not for
//! join points. Both are fixed in `lower.rs`, where they belonged.
//!
//! A mention of a top-level name lowers to a *jump to its definition*, and a
//! jump is a transfer of control — so a call that is not in tail position has
//! to record where to come back to, which this machine does by pushing a frame
//! and returning through it. That is the right price for a function. It is
//! a ruinous one for
//!
//! ```text
//! @pub fun get a i = stGetArray a i
//! ```
//!
//! which is what `Std.St`'s array accessors are: one line each, wrapping a
//! primitive that cannot transfer control at all. Lowering emits a primitive
//! where it stands, so `stGetArray a i` written out costs one instruction — and
//! `St.get a i`, which means exactly that, cost a closure, an indirect call, a
//! jump and a return. A matrix multiply's inner loop is three of these, so it
//! paid for six continuations and six calls per element to do three loads and a
//! store.
//!
//! # What is inlined
//!
//! A definition is a candidate when it is
//!
//! * **a function**: some lambdas, then a body;
//! * **type-applied at the call site** if it is polymorphic, so that its body
//!   can be instantiated. Core is still typed here and a name's representation
//!   comes from its type all the way down to code generation, so a copy of a
//!   body has to carry the types that copy is *for* — see [`retype`]. This is
//!   not an optional refinement: eleven of the thirteen definitions a matrix
//!   multiply reaches are polymorphic, `Std.St`'s array accessors among them,
//!   because they are generic in the state type `runSt` invents. Refusing them
//!   left this pass with nothing to do. A specialized copy (see
//!   [`crate::specialize`]) is mentioned with the type arguments of the
//!   definition it was copied from, and is instantiated at the ones for the
//!   binders it kept -- see [`Body::instantiation`] and [`candidates`];
//! * **not recursive**, directly or through any chain of definitions — which is
//!   computed rather than guessed at, because a wrapper calling a wrapper is the
//!   ordinary case and refusing those would leave most of the library alone;
//! * **small**, under [`BUDGET`] nodes -- or, when it is monomorphic and the
//!   whole program mentions it exactly once, under [`ONCE_BUDGET`]: the one
//!   copy is its only one anywhere, so nothing is duplicated, and the call is
//!   gone. What that buys beyond the call is the callee's code in its
//!   caller's function, where it can see what the caller took apart:
//!   `balanceLeft (ins l k) x r` in a red-black insertion builds its nodes in
//!   the one `ins` matched, instead of the caller giving that node back and
//!   the callee asking for another.
//!
//! and the call is **saturated**: exactly as many arguments as parameters.
//! Anything else is left as it is.
//!
//! # Why it is safe to copy a body
//!
//! Core binds every name once, and the rest of the compiler relies on it:
//! [`crate::simplify`] substitutes without renaming for that reason, and
//! `meadow_seq` numbers its environment by it. A body copied to two call sites
//! would break it. So every copy is [`freshen`]ed first — every binder in it,
//! and every mention of one, given a number nothing else has.
//!
//! The arguments become `let`s rather than being substituted, which keeps them
//! evaluated once and in order whatever the body does with its parameters.
//! [`crate::simplify`] then removes the ones that cost nothing to copy, which
//! is most of them, so `St.get a i` really does end up as the primitive and
//! nothing else.

use crate::*;
use std::collections::{HashMap, HashSet};

/// Where this pass's own names start: clear of units, synthetic definitions,
/// specialized copies, [`crate::globals`] and [`crate::simplify`].
pub const INLINE_BASE: u32 = 0x7D00_0000;

/// The most nodes a definition's body may have and still be copied to its call
/// sites.
///
/// Small, on purpose. This is not a general inliner with a cost model: it is
/// here for the one-line wrapper, and a one-line wrapper is four or five nodes.
/// Raising it trades code size for calls saved, and nothing measured so far
/// asks for that.
pub const BUDGET: usize = 16;

/// The most nodes a definition the program mentions exactly once may have and
/// still be inlined at that one place. Larger than [`BUDGET`] because copying
/// it duplicates nothing -- but bounded, since the caller is compiled as one
/// function and grows by all of it.
pub const ONCE_BUDGET: usize = 256;

/// How many times inlining and simplification alternate. A wrapper around a
/// wrapper needs two, and a loop nested three deep -- see [`loops`] -- needs a
/// round per level and one more for the lambda each level inlines; the bound
/// is what stops mutual recursion between two small definitions from doubling
/// the program on every pass.
const ROUNDS: usize = 6;

/// The most nodes a recursive definition's body may have and still be
/// specialised at a call -- see [`loops`]. Larger than [`BUDGET`], since a
/// loop's body is written out once per call site rather than per call.
pub const LOOP_BUDGET: usize = 64;

/// Inline what is worth inlining, simplifying after each round so that the
/// `let`s a call becomes collapse before the next one looks at them.
pub fn program(p: &Program) -> Program {
    let mut out = p.clone();
    let mut fresh = Fresh(INLINE_BASE.max(simplify::max_var(p) + 1));
    for _ in 0..ROUNDS {
        let bodies = candidates(&out).unwrap_or_default();
        let loops = loops(&out);
        if bodies.is_empty() && loops.is_empty() {
            break;
        }
        let originals = originals(&out);
        let mut changed = false;
        for d in &mut out.defs {
            let next = term(&d.term, &bodies, &originals, &loops, &mut fresh);
            changed |= next != d.term;
            d.term = next;
        }
        if !changed {
            break;
        }
        out = simplify::program(&out);
    }
    out
}

/// A source of names nothing else uses.
pub(crate) struct Fresh(pub(crate) u32);

impl Fresh {
    pub(crate) fn var(&mut self) -> Var {
        let v = hir::VarId(self.0);
        self.0 += 1;
        v
    }
}

/// What a candidate definition is: the type binders a call has to supply, its
/// parameters, and what to do with them.
pub(crate) struct Body {
    pub(crate) binders: Vec<TyVar>,
    pub(crate) params: Vec<(Var, Ty)>,
    pub(crate) term: Term,
    /// For a specialized copy (see [`crate::specialize`]), the binders of the
    /// definition it was copied from: a call mentions the copy with the
    /// original's type arguments, all of them, and `binders` is the subset the
    /// copy did not fix, by id.
    pub(crate) original: Option<Vec<TyVar>>,
}

impl Body {
    /// The types this body's own binders take at a call that supplies `tys`,
    /// if that is exactly them -- or the original's, for a copy, from which the
    /// ones the copy still binds are picked out.
    fn instantiation(&self, tys: Vec<Ty>) -> Option<Vec<Ty>> {
        if tys.len() == self.binders.len() {
            return Some(tys);
        }
        let original = self.original.as_ref()?;
        if tys.len() != original.len() {
            return None;
        }
        let kept: Vec<Ty> = original
            .iter()
            .zip(tys)
            .filter(|(b, _)| self.binders.iter().any(|c| c.id == b.id))
            .map(|(_, t)| t)
            .collect();
        (kept.len() == self.binders.len()).then_some(kept)
    }
}

/// Whether `name` is a pattern synonym's matcher, `$m2P`: see the parser's
/// `synonym`.
fn is_matcher(name: &str) -> bool {
    let name = name.rsplit_once('.').map_or(name, |(_, last)| last);
    name.strip_prefix("$m")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

/// Every definition worth inlining, or `None` if there are none.
fn candidates(p: &Program) -> Option<HashMap<Var, Body>> {
    let recursive = recursive(p);
    let mentions = mentions(p);
    let binders_of: HashMap<Var, &Vec<TyVar>> =
        p.defs.iter().map(|d| (d.var, &d.poly.binders)).collect();
    let mut out = HashMap::new();
    for d in &p.defs {
        if recursive.contains(&d.var) {
            continue;
        }
        let Some(mut body) = function(&d.term) else {
            continue;
        };
        // A polymorphic definition's term is a `TyLam` binding exactly the
        // binders its type declares. One where they disagree is not something
        // to guess about: its body would be instantiated at the wrong types, or
        // not at all.
        if body.binders.len() != d.poly.binders.len() {
            continue;
        }
        // Only a monomorphic definition, and not a specialized copy: a big
        // polymorphic body is instantiated at its one call through every
        // local binding and lifted function inside it, which this pass has
        // only ever been trusted to do for the small ones.
        let once = mentions.get(&d.var) == Some(&1)
            && body.binders.is_empty()
            && !p.origins.contains_key(&d.var);
        // A pattern synonym's matcher is inlined wherever it is used, however
        // polymorphic: what it answers is only there to be taken apart where
        // it is matched, which it can only be once it is there.
        let budget = if once || is_matcher(&d.name) {
            ONCE_BUDGET
        } else {
            BUDGET
        };
        if body.params.is_empty() || size(&body.term, budget) > budget {
            continue;
        }
        // A specialized copy is inlined as the copy, not as what it was copied
        // from: its literals are made at their number types, and what it
        // calls are copies too, which is all a release build can call. What
        // a release copy no longer says -- every binder that is not a number
        // is `#Ref` in it -- lowering recovers from the patterns that match
        // on it, which were checked at the real types.
        body.original = p
            .origins
            .get(&d.var)
            .and_then(|o| binders_of.get(o))
            .map(|bs| (*bs).clone());
        out.insert(d.var, body);
    }
    (!out.is_empty()).then_some(out)
}

/// How many times each variable is mentioned, anywhere in the program.
fn mentions(p: &Program) -> HashMap<Var, usize> {
    let mut out: HashMap<Var, usize> = HashMap::new();
    for d in &p.defs {
        let mut note = |t: Term| {
            if let Term::Var(v) = &t {
                *out.entry(*v).or_default() += 1;
            }
            t
        };
        rewrite::term(&d.term, &mut note, &mut |p| p);
    }
    out
}

/// A definition's type binders, parameters and body, if it is a function.
///
/// A polymorphic definition is a `TyLam` around the lambdas, and its binders
/// are what a call's [`Term::TyApp`] supplies. More than one `TyLam` is not a
/// shape core makes, and is refused rather than guessed at.
fn function(t: &Term) -> Option<Body> {
    let mut binders = Vec::new();
    let mut params = Vec::new();
    let mut t = t;
    loop {
        match t {
            Term::Loc(_, inner) => t = inner,
            Term::TyLam(bs, body) if binders.is_empty() && params.is_empty() => {
                binders = bs.clone();
                t = body;
            }
            Term::TyLam(..) => return None,
            Term::Lam(v, ty, body) => {
                params.push((*v, ty.clone()));
                t = body;
            }
            _ => {
                return Some(Body {
                    binders,
                    params,
                    term: t.clone(),
                    original: None,
                });
            }
        }
    }
}

/// Definitions that can reach themselves through the definitions they mention.
///
/// Not just direct recursion: `a` calling `b` calling `a` would otherwise be
/// inlined into itself round after round, and the round limit would be all that
/// stopped it -- having already doubled the program twice.
fn recursive(p: &Program) -> HashSet<Var> {
    let own: HashSet<Var> = p.defs.iter().map(|d| d.var).collect();
    let calls: HashMap<Var, HashSet<Var>> = p
        .defs
        .iter()
        .map(|d| {
            let mut free = HashSet::new();
            free_vars_into(&d.term, &mut free);
            (d.var, free.intersection(&own).copied().collect())
        })
        .collect();
    // The transitive closure, a definition at a time: `reach[v]` is everything
    // `v` can get to. Programs here are thousands of definitions, so the square
    // is affordable and the simple thing is the right thing.
    let mut out = HashSet::new();
    for d in &p.defs {
        let mut seen = HashSet::new();
        let mut todo = vec![d.var];
        while let Some(v) = todo.pop() {
            for &w in calls.get(&v).into_iter().flatten() {
                if w == d.var {
                    out.insert(d.var);
                    todo.clear();
                    break;
                }
                if seen.insert(w) {
                    todo.push(w);
                }
            }
        }
    }
    out
}

/// How many nodes `t` has, giving up once past `cap`.
pub fn size(t: &Term, cap: usize) -> usize {
    let mut n = 0;
    let mut count = |t: Term| {
        n += 1;
        t
    };
    // Bottom-up and whole, because `rewrite::term` has no way to stop early.
    // `cap` is small and so are the terms this is asked about -- a definition
    // whose body is enormous is counted once and then never looked at again,
    // since `candidates` runs per round on a program that is not growing much.
    rewrite::term(t, &mut count, &mut |p| p);
    n.min(cap + 1)
}

// --- loops -----------------------------------------------------------------

/// A recursive definition worth specialising where it is called with a
/// lambda: `St.forRange lo hi (\i -> ...)`, `while`, a fold.
///
/// Such a definition is a loop whose body is a call to a function it was
/// given, and the call of it costs what the fifth round measured for `matmul`:
/// a closure call, a frame and a return per iteration, for a body that is a
/// few instructions. Specialised, the definition becomes a local `letrec` at
/// the call site with the lambda substituted for the parameter and dropped
/// from the recursion, and [`crate::simplify`] then applies the lambda where
/// it stands -- so the loop's body *is* the lambda's body, and the loop is a
/// jump.
///
/// What qualifies: a function that mentions itself only in saturated calls
/// to itself -- not through any other definition -- whose body is under
/// [`LOOP_BUDGET`] nodes; and a call is specialised at the parameters every
/// recursive call passes through **unchanged** (`invariant`) that the call
/// gives a lambda for. Anything else stays a call.
struct Loop {
    body: Body,
    invariant: Vec<bool>,
    /// The definition's polytype: the specialised copy's type is it, minus
    /// the parameters dropped.
    poly: Poly,
}

/// Every definition worth specialising as a loop.
fn loops(p: &Program) -> HashMap<Var, Loop> {
    let own: HashSet<Var> = p.defs.iter().map(|d| d.var).collect();
    let calls: HashMap<Var, HashSet<Var>> = p
        .defs
        .iter()
        .map(|d| {
            let mut free = HashSet::new();
            free_vars_into(&d.term, &mut free);
            (d.var, free.intersection(&own).copied().collect())
        })
        .collect();
    let binders_of: HashMap<Var, &Vec<TyVar>> =
        p.defs.iter().map(|d| (d.var, &d.poly.binders)).collect();
    let dbg = std::env::var("MEADOW_INLINE_DEBUG").is_ok();
    let mut out = HashMap::new();
    for d in &p.defs {
        let Some(mine) = calls.get(&d.var) else {
            continue;
        };
        if dbg && d.name.to_string().contains("forRange") {
            eprintln!(
                "loops: {} {:?} mentions {:?} origin {:?}",
                d.name,
                d.var,
                mine,
                p.origins.get(&d.var)
            );
        }
        if !mine.contains(&d.var) {
            continue;
        }
        // Recursive through itself alone: nothing else it calls calls it
        // back, however indirectly.
        let others: Vec<Var> = mine.iter().copied().filter(|w| *w != d.var).collect();
        if others.iter().any(|w| reaches(&calls, *w, d.var)) {
            continue;
        }
        let Some(mut body) = function(&d.term) else {
            continue;
        };
        if body.binders.len() != d.poly.binders.len()
            || body.params.is_empty()
            || size(&body.term, LOOP_BUDGET) > LOOP_BUDGET
        {
            continue;
        }
        let Some(invariant) = passes_through(&body.term, d.var, &body.params) else {
            if dbg {
                eprintln!(
                    "loops: {} mentions itself other than in a saturated call",
                    d.name
                );
            }
            continue;
        };
        if dbg {
            eprintln!("loops: {} invariant {:?}", d.name, invariant);
        }
        if !invariant.iter().any(|i| *i) {
            continue;
        }
        body.original = p
            .origins
            .get(&d.var)
            .and_then(|o| binders_of.get(o))
            .map(|bs| (*bs).clone());
        out.insert(
            d.var,
            Loop {
                body,
                invariant,
                poly: d.poly.clone(),
            },
        );
    }
    out
}

/// Can `from` reach `to` through `calls`, not counting `from` itself?
fn reaches(calls: &HashMap<Var, HashSet<Var>>, from: Var, to: Var) -> bool {
    let mut seen = HashSet::new();
    let mut todo = vec![from];
    while let Some(v) = todo.pop() {
        for &w in calls.get(&v).into_iter().flatten() {
            if w == to {
                return true;
            }
            if seen.insert(w) {
                todo.push(w);
            }
        }
    }
    false
}

/// Which of `params` every call of `d` in `body` passes through as itself --
/// or `None` if `d` is mentioned anywhere other than as the head of a call
/// with exactly as many arguments as parameters.
fn passes_through(body: &Term, d: Var, params: &[(Var, Ty)]) -> Option<Vec<bool>> {
    let mut invariant = vec![true; params.len()];
    let mut ok = true;
    let mut work: Vec<&Term> = vec![body];
    while let Some(t) = work.pop() {
        rewrite::visit(t, &mut |t| match t {
            Term::Var(v) if *v == d => {
                ok = false;
                false
            }
            Term::App(..) | Term::TyApp(..) if spine_head(t) == Some(d) => {
                let (_, args) = call_spine_of(t);
                if args.len() != params.len() {
                    ok = false;
                    return false;
                }
                for (k, a) in args.iter().enumerate() {
                    if !matches!(peel(a), Term::Var(v) if *v == params[k].0) {
                        invariant[k] = false;
                    }
                    work.push(a);
                }
                false
            }
            _ => true,
        });
        if !ok {
            return None;
        }
    }
    Some(invariant)
}

fn peel(t: &Term) -> &Term {
    match t {
        Term::Loc(_, inner) => peel(inner),
        t => t,
    }
}

/// The head of a call spine, and its arguments in order.
fn call_spine_of(t: &Term) -> (&Term, Vec<&Term>) {
    let mut args = Vec::new();
    let mut head = t;
    loop {
        match head {
            Term::Loc(_, inner) => head = inner,
            Term::App(f, a) => {
                args.push(&**a);
                head = f;
            }
            Term::TyApp(f, _) => head = f,
            _ => break,
        }
    }
    args.reverse();
    (head, args)
}

/// The definition a call spine calls, if it is one.
fn spine_head(t: &Term) -> Option<Var> {
    match call_spine_of(t).0 {
        Term::Var(v) => Some(*v),
        _ => None,
    }
}

/// `t` as a call to a loop specialised at these arguments: the loop, the
/// types it is called at, its arguments, and which of them are dropped into
/// the copy -- the invariant parameters given a lambda. `None` where there is
/// none of those, or the call is not of the shape [`call`] wants.
fn loop_call<'l>(
    t: &Term,
    loops: &'l HashMap<Var, Loop>,
) -> Option<(&'l Loop, Vec<Ty>, Vec<Term>, Vec<bool>)> {
    let (head, args) = call_spine_of(t);
    let Term::Var(v) = head else { return None };
    let l = loops.get(v)?;
    if args.len() != l.body.params.len() {
        return None;
    }
    let tys = match peel(t) {
        _ => {
            // The instantiation sits under the applications, as `call` reads it.
            let mut cur = t;
            let mut tys = Vec::new();
            loop {
                match cur {
                    Term::Loc(_, inner) => cur = inner,
                    Term::App(f, _) => cur = f,
                    Term::TyApp(_, at) => {
                        tys = at.clone();
                        break;
                    }
                    _ => break,
                }
            }
            tys
        }
    };
    let tys = l.body.instantiation(tys)?;
    let dropped: Vec<bool> = args
        .iter()
        .enumerate()
        .map(|(k, a)| l.invariant[k] && matches!(peel(a), Term::Lam(..)))
        .collect();
    // A copy needs a parameter left to be a function of: one with every
    // parameter dropped would be a recursive *value*, which is not a loop.
    if !dropped.iter().any(|d| *d) || dropped.iter().all(|d| *d) {
        return None;
    }
    Some((l, tys, args.into_iter().cloned().collect(), dropped))
}

/// The loop, specialised: its body at these types, with each dropped
/// parameter replaced by its lambda (a fresh copy per mention) and left out
/// of every recursive call, entered with the remaining arguments. Where every
/// recursive call is a tail call -- `forRange`, `while`, a fold -- that is a
/// **join point** jumping to itself, which lowering makes a block and a jump:
/// a loop, with no call in it. Otherwise it is a `letrec` of one binding.
fn enter_loop(
    l: &Loop,
    d: Var,
    tys: Vec<Ty>,
    args: Vec<Term>,
    dropped: &[bool],
    fresh: &mut Fresh,
) -> Option<Term> {
    // The copy's type: the definition's at these types, minus what is dropped.
    let full = if l.poly.binders.len() == tys.len() {
        l.poly.instantiate(&tys)
    } else {
        // A specialised copy called with its original's arguments: its own
        // type is already at them.
        l.poly.ty.clone()
    };
    let (ps, ret, eff) = uncurry(&full, args.len())?;
    let at = retype(&l.body, &tys);
    let copy = freshen(&at, fresh);
    let go = fresh.var();
    let keep = |k: usize| !dropped[k];
    let looping = tail_only(&copy.term, d);
    // Recursive calls: to `go`, without the dropped arguments -- a jump, for
    // a loop.
    let term = rewrite::term(
        &copy.term,
        &mut |t| {
            if !matches!(t, Term::App(..)) || spine_head(&t) != Some(d) {
                return t;
            }
            let (_, call_args) = call_spine_of(&t);
            if call_args.len() != dropped.len() {
                return t;
            }
            let kept: Vec<Term> = call_args
                .into_iter()
                .enumerate()
                .filter(|(k, _)| keep(*k))
                .map(|(_, a)| a.clone())
                .collect();
            if looping {
                return Term::Jump(go, kept, ret.clone());
            }
            kept.into_iter()
                .fold(Term::Var(go), |f, a| Term::App(Arc::new(f), Arc::new(a)))
        },
        &mut |p| p,
    );
    // The dropped parameters: each mention replaced by a copy of the lambda.
    let mut term = term;
    for (k, (v, _)) in copy.params.iter().enumerate() {
        if keep(k) {
            continue;
        }
        let lam = &args[k];
        term = rewrite::term(
            &term,
            &mut |t| match &t {
                Term::Var(x) if *x == *v => fresh_copy(lam, fresh),
                _ => t,
            },
            &mut |p| p,
        );
    }
    if looping {
        let params: Vec<(Var, Ty)> = copy
            .params
            .iter()
            .enumerate()
            .filter(|(k, _)| keep(*k))
            .map(|(_, p)| p.clone())
            .collect();
        let start: Vec<Term> = args
            .into_iter()
            .enumerate()
            .filter(|(k, _)| keep(*k))
            .map(|(_, a)| a)
            .collect();
        return Some(Term::Join {
            var: go,
            params,
            ty: ret.clone(),
            rhs: Arc::new(term),
            body: Arc::new(Term::Jump(go, start, ret)),
        });
    }
    let lam = copy
        .params
        .iter()
        .enumerate()
        .filter(|(k, _)| keep(*k))
        .rev()
        .fold(term, |body, (_, (v, ty))| {
            Term::Lam(*v, ty.clone(), Arc::new(body))
        });
    let kept_tys: Vec<Ty> = ps
        .iter()
        .enumerate()
        .filter(|(k, _)| keep(*k))
        .map(|(_, t)| t.clone())
        .collect();
    let poly = Poly::mono(curry(kept_tys, ret, eff));
    let call = args
        .into_iter()
        .enumerate()
        .filter(|(k, _)| keep(*k))
        .fold(Term::Var(go), |f, (_, a)| {
            Term::App(Arc::new(f), Arc::new(a))
        });
    Some(Term::LetRec(vec![(go, poly, lam)], Arc::new(call)))
}

/// Is every call of `d` in `t` a tail call: the last thing done, with
/// nothing waiting for what it answers? A call under a lambda or inside a
/// handler is not one, whatever its position there.
fn tail_only(t: &Term, d: Var) -> bool {
    fn go(t: &Term, d: Var, tail: bool) -> bool {
        match t {
            Term::Loc(_, x) => go(x, d, tail),
            Term::App(..) | Term::TyApp(..) if spine_head(t) == Some(d) => {
                let (_, args) = call_spine_of(t);
                tail && args.iter().all(|a| go(a, d, false))
            }
            Term::Var(v) => *v != d,
            Term::Lit(_) | Term::Error => true,
            Term::App(f, a) => go(f, d, false) && go(a, d, false),
            Term::TyApp(f, _) => go(f, d, false),
            Term::Lam(_, _, b) | Term::TyLam(_, b) => go(b, d, false),
            Term::Let(_, _, r, b) => go(r, d, false) && go(b, d, tail),
            Term::LetRec(bs, b) => bs.iter().all(|(_, _, t)| go(t, d, false)) && go(b, d, tail),
            Term::Join { rhs, body, .. } => go(rhs, d, tail) && go(body, d, tail),
            Term::Jump(_, xs, _)
            | Term::Tuple(xs)
            | Term::Array(xs, _)
            | Term::Prim(_, xs, _)
            | Term::Ctor(_, _, xs) => xs.iter().all(|x| go(x, d, false)),
            Term::If(c, a, b) => go(c, d, false) && go(a, d, tail) && go(b, d, tail),
            Term::Case(s, arms, _) => {
                go(s, d, false)
                    && arms.iter().all(|(_, g, b)| {
                        g.as_ref().is_none_or(|g| go(g, d, false)) && go(b, d, tail)
                    })
            }
            Term::Proj(x, _) | Term::Sel(x, _, _) | Term::Perform(_, _, x, _) => go(x, d, false),
            Term::Record(fs) => fs.iter().all(|(_, x)| go(x, d, false)),
            Term::Extend(a, _, b) => go(a, d, false) && go(b, d, false),
            Term::Handle {
                body, clauses, ret, ..
            } => {
                go(body, d, false)
                    && clauses.iter().all(|c| go(&c.body, d, false))
                    && ret.as_ref().is_none_or(|(_, _, b)| go(b, d, false))
            }
        }
    }
    go(t, d, true)
}

/// A function type taking `n` arguments, however its arrows are grouped:
/// the parameters in order, what it answers, and the effect of the call
/// that answers it.
fn uncurry(ty: &Ty, n: usize) -> Option<(Vec<Ty>, Ty, Ty)> {
    let mut params = Vec::new();
    let mut cur = ty.clone();
    loop {
        if params.len() == n {
            return Some((params, cur, Ty::RowEmpty));
        }
        let Ty::Fun(ps, ret, eff) = cur else {
            return None;
        };
        if params.len() + ps.len() > n {
            return None;
        }
        params.extend(ps);
        if params.len() == n {
            return Some((params, *ret, *eff));
        }
        cur = *ret;
    }
}

/// The function type taking `params` one at a time, with `eff` on the call
/// that answers `ret`, as core writes a curried definition's type.
fn curry(params: Vec<Ty>, ret: Ty, eff: Ty) -> Ty {
    let mut ty = ret;
    let last = params.len().saturating_sub(1);
    for (k, p) in params.into_iter().enumerate().rev() {
        let e = if k == last { eff.clone() } else { Ty::RowEmpty };
        ty = Ty::Fun(vec![p], Box::new(ty), Box::new(e));
    }
    ty
}

/// A copy of `t` binding names nothing else binds.
fn fresh_copy(t: &Term, fresh: &mut Fresh) -> Term {
    freshen(
        &Body {
            binders: Vec::new(),
            params: Vec::new(),
            term: t.clone(),
            original: None,
        },
        fresh,
    )
    .term
}

// --- the rewrite -----------------------------------------------------------

fn term(
    t: &Term,
    bodies: &HashMap<Var, Body>,
    originals: &HashMap<Var, Original>,
    loops: &HashMap<Var, Loop>,
    fresh: &mut Fresh,
) -> Term {
    rewrite::term(
        t,
        &mut |t| match call(&t, bodies) {
            Some((f, body, tys, written, args)) => {
                let entered = enter(body, tys, args, fresh);
                match originals.get(&f) {
                    Some(original) => original.arguments(entered, &written),
                    None => entered,
                }
            }
            None => match loop_call(&t, loops) {
                Some((l, tys, args, dropped)) => {
                    let d = spine_head(&t).expect("a loop call has a head");
                    enter_loop(l, d, tys, args, &dropped, fresh).unwrap_or(t)
                }
                None => t,
            },
        },
        &mut |p| p,
    )
}

/// `t` as a saturated call to something worth inlining: what it calls, what
/// to inline, the types it is called at, the types the call wrote down -- the
/// original's, for a specialized copy -- and its arguments in order.
#[allow(clippy::type_complexity)]
fn call<'b>(
    t: &Term,
    bodies: &'b HashMap<Var, Body>,
) -> Option<(Var, &'b Body, Vec<Ty>, Vec<Ty>, Vec<Term>)> {
    let mut args = Vec::new();
    let mut tys: Vec<Ty> = Vec::new();
    let mut head = t;
    loop {
        match head {
            Term::Loc(_, inner) => head = inner,
            Term::App(f, a) => {
                args.push((**a).clone());
                head = f;
            }
            // The instantiation core wrote down for a polymorphic name. It sits
            // under the applications, because it is part of the name.
            Term::TyApp(f, at) if tys.is_empty() => {
                tys = at.clone();
                head = f;
            }
            Term::Var(v) => {
                let body = bodies.get(v)?;
                // Saturated, and no more: a call with extra arguments applies
                // what the body answers, which is not this rewrite's business.
                // And instantiated exactly, or the body's types cannot be made
                // the ones this copy is for.
                if args.len() != body.params.len() {
                    return None;
                }
                let written = tys.clone();
                let tys = body.instantiation(tys)?;
                args.reverse();
                return Some((*v, body, tys, written, args));
            }
            _ => return None,
        }
    }
}

/// What a release copy was copied from takes: that definition's type
/// binders, and its parameters' types over them.
///
/// The copy's own parameters are `#Ref` wherever a binder that is not a
/// number was, which says how such an argument is held and nothing of what
/// it is. Lowering recovers what it needs from where the value is taken
/// apart -- but a function is not taken apart, it is applied: `(id f) x`,
/// with `id` inlined, is a `#Ref` applied to `x`, and what that answers has
/// no type at all. The call wrote down the types the original is at, and
/// those say.
struct Original {
    binders: Vec<TyVar>,
    params: Vec<Ty>,
}

impl Original {
    /// `entered`, a call to the copy inlined -- the `let`s of its parameters
    /// around its body -- with each parameter the copy has as `#Ref` given
    /// the type the original has it at, at the types `written`.
    fn arguments(&self, entered: Term, written: &[Ty]) -> Term {
        if written.len() != self.binders.len() {
            return entered;
        }
        let map: HashMap<u32, Ty> = self
            .binders
            .iter()
            .map(|b| b.id)
            .zip(written.iter().cloned())
            .collect();
        fn go(t: Term, params: &[Ty], map: &HashMap<u32, Ty>) -> Term {
            match (t, params.split_first()) {
                (Term::Let(v, poly, rhs, body), Some((original, rest))) => {
                    let poly = match &poly.ty {
                        Ty::Con(n, args) if &**n == specialize::REF && args.is_empty() => {
                            Poly::mono(subst_rigid(original, map))
                        }
                        _ => poly,
                    };
                    let body = go(Arc::unwrap_or_clone(body), rest, map);
                    Term::Let(v, poly, rhs, Arc::new(body))
                }
                (t, _) => t,
            }
        }
        go(entered, &self.params, &map)
    }
}

/// [`Original`] for each release copy whose original is still there to say.
fn originals(p: &Program) -> HashMap<Var, Original> {
    let defs: HashMap<Var, &Def> = p.defs.iter().map(|d| (d.var, d)).collect();
    p.origins
        .iter()
        .filter_map(|(copy, of)| {
            let body = function(&defs.get(of)?.term)?;
            Some((
                *copy,
                Original {
                    binders: body.binders,
                    params: body.params.into_iter().map(|(_, ty)| ty).collect(),
                },
            ))
        })
        .collect()
}

/// The body at these types, with its parameters bound to the arguments.
fn enter(body: &Body, tys: Vec<Ty>, args: Vec<Term>, fresh: &mut Fresh) -> Term {
    let at = retype(body, &tys);
    let copy = freshen(&at, fresh);

    copy.params
        .iter()
        .zip(args)
        .rev()
        .fold(copy.term, |acc, ((v, ty), arg)| {
            Term::Let(*v, Poly::mono(ty.clone()), Arc::new(arg), Arc::new(acc))
        })
}

/// `body` with its type binders replaced by `tys`, everywhere a type appears.
///
/// Core stays typed until lowering, and a name's representation comes from its
/// type -- so a copy of a generic body placed where `a` is `Float` has to say
/// `Float`, or code generation will read the words the wrong way. Every node
/// that carries a type is rewritten here, and so is every annotated pattern.
pub(crate) fn retype(body: &Body, tys: &[Ty]) -> Body {
    if body.binders.is_empty() {
        return Body {
            binders: Vec::new(),
            params: body.params.clone(),
            term: body.term.clone(),
            original: None,
        };
    }
    let map: HashMap<u32, Ty> = body
        .binders
        .iter()
        .map(|b| b.id)
        .zip(tys.iter().cloned())
        .collect();
    let at = |ty: &Ty| subst_rigid(ty, &map);
    let term = rewrite::term(
        &body.term,
        &mut |t| match t {
            // A literal typed by a binder is made at the type the binder takes.
            Term::Lit(l) => Term::Lit(specialize::lit_at(&l, &map)),
            Term::Lam(v, ty, b) => Term::Lam(v, at(&ty), b),
            Term::Let(v, poly, rhs, b) => Term::Let(v, at_poly(&poly, &map), rhs, b),
            Term::LetRec(binds, b) => Term::LetRec(
                binds
                    .into_iter()
                    .map(|(v, poly, t)| (v, at_poly(&poly, &map), t))
                    .collect(),
                b,
            ),
            Term::TyApp(f, args) => Term::TyApp(f, args.iter().map(&at).collect()),
            Term::Array(xs, ty) => Term::Array(xs, at(&ty)),
            Term::Sel(x, l, ty) => Term::Sel(x, l, at(&ty)),
            Term::Ctor(n, ty, xs) => Term::Ctor(n, at(&ty), xs),
            Term::Case(s, arms, ty) => Term::Case(s, arms, at(&ty)),
            Term::Prim(p, xs, ty) => Term::Prim(p, xs, at(&ty)),
            Term::Perform(e, o, x, ty) => Term::Perform(e, o, x, at(&ty)),
            Term::Jump(j, xs, ty) => Term::Jump(j, xs, at(&ty)),
            Term::Join {
                var,
                params,
                ty,
                rhs,
                body,
            } => Term::Join {
                var,
                params: params.into_iter().map(|(v, t)| (v, at(&t))).collect(),
                ty: at(&ty),
                rhs,
                body,
            },
            Term::Handle {
                body,
                clauses,
                ret,
                ty,
            } => Term::Handle {
                body,
                clauses: clauses
                    .into_iter()
                    .map(|c| HClause {
                        param_ty: at(&c.param_ty),
                        resume_ty: at(&c.resume_ty),
                        ..c
                    })
                    .collect(),
                ret: ret.map(|(v, t, b)| (v, at(&t), b)),
                ty: at(&ty),
            },
            t => t,
        },
        &mut |p| match p {
            Pat::Var(v, ty) => Pat::Var(v, at(&ty)),
            Pat::As(v, ty, inner) => Pat::As(v, at(&ty), inner),
            Pat::Lit(l) => Pat::Lit(specialize::lit_at(&l, &map)),
            p => p,
        },
    );
    Body {
        binders: Vec::new(),
        params: body.params.iter().map(|(v, ty)| (*v, at(ty))).collect(),
        term,
        original: None,
    }
}

/// A `let`'s polytype at the same substitution. Its own binders shadow, so
/// anything it quantifies is left alone.
fn at_poly(poly: &Poly, map: &HashMap<u32, Ty>) -> Poly {
    let mut map = map.clone();
    for b in &poly.binders {
        map.remove(&b.id);
    }
    Poly {
        binders: poly.binders.clone(),
        ty: subst_rigid(&poly.ty, &map),
    }
}

/// A copy of `body` in which every bound name is one nothing else uses.
///
/// Core binds every name once and the whole compiler below here relies on it.
/// Two copies of one body at two call sites would bind the same names twice, so
/// each copy is renamed as it is made.
pub(crate) fn freshen(body: &Body, fresh: &mut Fresh) -> Body {
    freshen_mapped(body, fresh).0
}

/// [`freshen`], and which new name each old one got: what a pass that keeps
/// `Program::origins` needs to record.
pub(crate) fn freshen_mapped(body: &Body, fresh: &mut Fresh) -> (Body, HashMap<Var, Var>) {
    let mut map: HashMap<Var, Var> = HashMap::new();
    for (v, _) in &body.params {
        map.insert(*v, fresh.var());
    }
    for v in bound(&body.term) {
        map.entry(v).or_insert_with(|| fresh.var());
    }
    let rename = |v: &Var| map.get(v).copied().unwrap_or(*v);
    let term = rewrite::term(
        &body.term,
        &mut |t| match t {
            Term::Var(v) => Term::Var(rename(&v)),
            Term::Lam(v, ty, b) => Term::Lam(rename(&v), ty, b),
            Term::Let(v, poly, rhs, b) => Term::Let(rename(&v), poly, rhs, b),
            Term::LetRec(binds, b) => Term::LetRec(
                binds
                    .into_iter()
                    .map(|(v, poly, t)| (rename(&v), poly, t))
                    .collect(),
                b,
            ),
            Term::Join {
                var,
                params,
                ty,
                rhs,
                body,
            } => Term::Join {
                var: rename(&var),
                params: params.into_iter().map(|(v, ty)| (rename(&v), ty)).collect(),
                ty,
                rhs,
                body,
            },
            Term::Jump(j, args, ty) => Term::Jump(rename(&j), args, ty),
            Term::Handle {
                body,
                clauses,
                ret,
                ty,
            } => Term::Handle {
                body,
                clauses: clauses
                    .into_iter()
                    .map(|c| HClause {
                        param: rename(&c.param),
                        resume: rename(&c.resume),
                        ..c
                    })
                    .collect(),
                ret: ret.map(|(v, t, b)| (rename(&v), t, b)),
                ty,
            },
            t => t,
        },
        &mut |p| match p {
            Pat::Var(v, ty) => Pat::Var(rename(&v), ty),
            Pat::As(v, ty, inner) => Pat::As(rename(&v), ty, inner),
            p => p,
        },
    );
    let params = body
        .params
        .iter()
        .map(|(v, ty)| (rename(v), ty.clone()))
        .collect();
    (
        Body {
            binders: Vec::new(),
            params,
            term,
            original: None,
        },
        map,
    )
}

/// Every name `t` binds anywhere inside it, patterns and handler clauses
/// included.
fn bound(t: &Term) -> HashSet<Var> {
    let out = std::cell::RefCell::new(HashSet::new());
    let mut note = |t: Term| {
        let mut out = out.borrow_mut();
        match &t {
            Term::Lam(v, _, _) | Term::Let(v, _, _, _) => {
                out.insert(*v);
            }
            Term::LetRec(binds, _) => out.extend(binds.iter().map(|(v, _, _)| *v)),
            Term::Join { var, params, .. } => {
                out.insert(*var);
                out.extend(params.iter().map(|(v, _)| *v));
            }
            Term::Handle { clauses, ret, .. } => {
                for c in clauses {
                    out.insert(c.param);
                    out.insert(c.resume);
                }
                if let Some((v, _, _)) = ret {
                    out.insert(*v);
                }
            }
            _ => {}
        }
        t
    };
    let mut in_pattern = |p: Pat| {
        if let Pat::Var(v, _) | Pat::As(v, _, _) = &p {
            out.borrow_mut().insert(*v);
        }
        p
    };
    rewrite::term(t, &mut note, &mut in_pattern);
    out.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use meadow_hir::VarId;

    fn v(n: u32) -> Var {
        VarId(n)
    }

    fn con(name: &str) -> Ty {
        Ty::Con(InternedString::from(name), Vec::new())
    }

    /// `fun f a b = a + b`, the shape of every one-line wrapper in `Std`.
    fn wrapper(f: Var, poly: Poly) -> Def {
        let (a, b) = (v(10), v(11));
        let body = Term::Prim(Prim::Add, vec![Term::Var(a), Term::Var(b)], con("Int"));
        Def {
            var: f,
            name: InternedString::from("f"),
            module: Default::default(),
            poly,
            term: Term::Lam(
                a,
                con("Int"),
                Arc::new(Term::Lam(b, con("Int"), Arc::new(body))),
            ),
        }
    }

    fn calling(f: Var, args: Vec<Term>) -> Def {
        Def {
            var: v(1),
            name: InternedString::from("main"),
            module: Default::default(),
            poly: Poly::mono(con("Int")),
            term: args
                .into_iter()
                .fold(Term::Var(f), |g, a| Term::App(Arc::new(g), Arc::new(a))),
        }
    }

    fn run(defs: Vec<Def>) -> Term {
        let p = Program {
            defs,
            ..Default::default()
        };
        program(&p).defs[0].term.clone()
    }

    /// The whole point: a saturated call to a small wrapper becomes what the
    /// wrapper does, with no call left.
    #[test]
    fn a_saturated_call_to_a_wrapper_becomes_its_body() {
        let f = v(2);
        let got = run(vec![
            calling(f, vec![Term::Lit(Lit::Int(1)), Term::Lit(Lit::Int(2))]),
            wrapper(f, Poly::mono(con("Int"))),
        ]);
        assert_eq!(
            got,
            Term::Prim(
                Prim::Add,
                vec![Term::Lit(Lit::Int(1)), Term::Lit(Lit::Int(2))],
                con("Int")
            ),
            "{got:?}"
        );
    }

    /// A call with too few arguments is a partial application, which is a
    /// closure and not this pass's business.
    #[test]
    fn an_unsaturated_call_is_left_alone() {
        let f = v(2);
        let before = calling(f, vec![Term::Lit(Lit::Int(1))]);
        let got = run(vec![before.clone(), wrapper(f, Poly::mono(con("Int")))]);
        assert_eq!(got, before.term);
    }

    /// A polymorphic wrapper is inlined at the types the call instantiates it
    /// with, and its body says so afterwards. Core stays typed until lowering
    /// and representations come from types, so this is the part that has to be
    /// right rather than merely fast.
    #[test]
    fn a_polymorphic_wrapper_is_inlined_at_its_types() {
        // `fun f (x : t) = #[x] : Array t`, at `t = Int`.
        let (f, x) = (v(2), v(10));
        let t = Ty::Var(7);
        let binders = vec![TyVar {
            id: 7,
            kind: VarKind::Type,
        }];
        let d = Def {
            var: f,
            name: InternedString::from("f"),
            module: Default::default(),
            poly: Poly {
                binders: binders.clone(),
                ty: t.clone(),
            },
            term: Term::TyLam(
                binders,
                Arc::new(Term::Lam(
                    x,
                    t.clone(),
                    Arc::new(Term::Array(vec![Term::Var(x)], t.clone())),
                )),
            ),
        };
        let main = Def {
            var: v(1),
            name: InternedString::from("main"),
            module: Default::default(),
            poly: Poly::mono(con("Int")),
            term: Term::App(
                Arc::new(Term::TyApp(Arc::new(Term::Var(f)), vec![con("Int")])),
                Arc::new(Term::Lit(Lit::Int(5))),
            ),
        };
        let got = run(vec![main, d]);
        assert_eq!(
            got,
            Term::Array(vec![Term::Lit(Lit::Int(5))], con("Int")),
            "the element type is the one the call asked for"
        );
    }

    /// A definition whose declared polytype and whose term disagree about how
    /// many types it takes is refused rather than instantiated at none.
    #[test]
    fn a_definition_whose_type_and_term_disagree_is_refused() {
        let f = v(2);
        let poly = Poly {
            binders: vec![TyVar {
                id: 7,
                kind: VarKind::Type,
            }],
            ty: con("Int"),
        };
        // `wrapper` builds a term with no `TyLam`, so this one claims a binder
        // it does not have.
        let before = calling(f, vec![Term::Lit(Lit::Int(1)), Term::Lit(Lit::Int(2))]);
        let got = run(vec![before.clone(), wrapper(f, poly)]);
        assert_eq!(got, before.term);
    }

    /// A definition that can reach itself is never copied into itself.
    #[test]
    fn a_recursive_definition_is_not_inlined() {
        let f = v(2);
        let mut d = wrapper(f, Poly::mono(con("Int")));
        d.term = Term::Lam(
            v(10),
            con("Int"),
            Arc::new(Term::Lam(
                v(11),
                con("Int"),
                Arc::new(Term::App(
                    Arc::new(Term::Var(f)),
                    Arc::new(Term::Var(v(10))),
                )),
            )),
        );
        let before = calling(f, vec![Term::Lit(Lit::Int(1)), Term::Lit(Lit::Int(2))]);
        let got = run(vec![before.clone(), d]);
        assert_eq!(got, before.term);
    }

    /// Two calls to one wrapper must not end up binding the same names: core
    /// binds every name once and everything below here relies on it.
    #[test]
    fn two_call_sites_do_not_share_names() {
        // `fun f x = let z = x + x in z * z` -- one parameter, and a binder
        // inside that gets copied with the body.
        let (f, x, z) = (v(2), v(20), v(21));
        let sum = Term::Prim(Prim::Add, vec![Term::Var(x), Term::Var(x)], con("Int"));
        let body = Term::Let(
            z,
            Poly::mono(con("Int")),
            Arc::new(sum),
            Arc::new(Term::Prim(
                Prim::Mul,
                vec![Term::Var(z), Term::Var(z)],
                con("Int"),
            )),
        );
        let d = Def {
            var: f,
            name: InternedString::from("f"),
            module: Default::default(),
            poly: Poly::mono(con("Int")),
            term: Term::Lam(x, con("Int"), Arc::new(body)),
        };
        let call = |k: i64| Term::App(Arc::new(Term::Var(f)), Arc::new(Term::Lit(Lit::Int(k))));
        let main = Def {
            var: v(1),
            name: InternedString::from("main"),
            module: Default::default(),
            poly: Poly::mono(con("Int")),
            term: Term::Tuple(vec![call(1), call(2)]),
        };
        let got = run(vec![main, d]);
        let mut seen = std::collections::HashSet::new();
        let mut note = |t: Term| {
            if let Term::Let(b, _, _, _) = &t {
                assert!(seen.insert(*b), "{b:?} is bound twice in {got:?}");
            }
            t
        };
        rewrite::term(&got, &mut note, &mut |p| p);
        assert_eq!(seen.len(), 2, "two copies, two names: {got:?}");
        assert!(!seen.contains(&z), "and neither is the original");
    }
}
