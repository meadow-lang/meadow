//! **Lambda lifting**: local functions become top-level definitions.
//!
//! ```text
//!   fun sumTo n =                      fun sumTo n = sumTo.go n 0 0
//!     let rec go i acc =         ==>
//!       if i >= n then acc             fun sumTo.go n i acc =
//!       else go (i + 1) (acc + i)        if i >= n then acc
//!     in go 0 0                          else sumTo.go n (i + 1) (acc + i)
//! ```
//!
//! A local function is a closure: an object built where it is bound, holding
//! what it captured, and called through its one method. A loop written as one
//! pays for that on every turn -- a call through an object, and in the native
//! backend no chance for LLVM to see the loop at all. A top-level function is a
//! label with a direct entry point, and one calling itself in tail position is
//! a jump. Written the second way, the loop above ran 30,000 times faster
//! native; so the compiler writes it the second way for you.
//!
//! What a function captured becomes its first parameters, and every mention of
//! it becomes the new definition applied to them. A saturated call then lowers
//! to a jump to the definition's direct entry, with the captured values passed
//! in registers like any other arguments.
//!
//! **Types.** The new definition is abstracted over every type variable in
//! scope where the function was bound, then over the function's own, and a
//! mention instantiates the first lot at themselves. Type variable ids stay
//! what they were: a definition's binders are its own, as `specialize`'s
//! copies already rely on.
//!
//! **What is left alone.** A group that captures something polymorphic -- a
//! generic local value, which cannot be passed as an argument -- or a join
//! point stays where it is, as does anything at `-O0`, where a debugger wants
//! the program as it was written. The captured parameters are fresh names, so
//! each binding site stays unique; [`Program::origins`] says what each is a copy
//! of.

use crate::inline::Fresh;
use crate::*;
use meadow_infer::VarKind;
use std::collections::{HashMap, HashSet};

pub fn program(p: &Program, opt: OptLevel) -> Program {
    if opt == OptLevel::O0 || std::env::var_os("MEADOW_NO_LIFT").is_some() {
        return p.clone();
    }
    let mut lifter = Lifter {
        fresh: Fresh(simplify::max_var(p) + 1),
        globals: p.defs.iter().map(|d| d.var).collect(),
        made: Vec::new(),
        origins: p.origins.clone(),
        types: HashMap::new(),
        joins: HashSet::new(),
        tvs: Vec::new(),
        lifted: HashMap::new(),
        renames: Vec::new(),
        outer: InternedString::from(""),
        module: InternedString::default(),
        count: 0,
        representations: opt.specializes(),
    };
    let mut defs = Vec::with_capacity(p.defs.len());
    for d in &p.defs {
        lifter.outer = d.name;
        lifter.module = d.module;
        lifter.count = 0;
        lifter.tvs.clear();
        let term = lifter.term(&d.term);
        defs.push(Def { term, ..d.clone() });
    }
    defs.extend(lifter.made);
    Program {
        defs,
        entry: p.entry,
        ctor_fields: p.ctor_fields.clone(),
        variants: p.variants.clone(),
        origins: lifter.origins,
    }
}

/// Whether `d` is a local function this pass made a definition of: named as
/// it names one, `<outer>Local<n>`, and a copy of something
/// ([`Program::origins`]), as each it makes is. A person may name a
/// definition so, but not also make it a copy. What reads this keeps such a
/// definition with the one it was written in: see `meadow_axcut`'s
/// `Program::types`.
pub fn is_lifted(p: &Program, d: &Def) -> bool {
    let name = d.name.to_string();
    p.origins.contains_key(&d.var)
        && name.rsplit_once("Local").is_some_and(|(outer, n)| {
            !outer.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
        })
}

/// A local function made a definition: what a mention of it becomes.
#[derive(Clone)]
struct Lifted {
    def: Var,
    /// The type variables in scope where it was bound, which the definition
    /// abstracts over first.
    scope: Vec<TyVar>,
    /// Its own type binders, after those.
    own: Vec<TyVar>,
    /// What it captured, by the names they have where it was bound.
    captured: Vec<Var>,
}

struct Lifter {
    fresh: Fresh,
    globals: HashSet<Var>,
    made: Vec<Def>,
    origins: HashMap<Var, Var>,
    /// Every local binder met so far and its type. Names are unique per
    /// binding site, so nothing has to be taken out on leaving a scope.
    types: HashMap<Var, Poly>,
    joins: HashSet<Var>,
    /// The type variables bound around the term being rewritten.
    tvs: Vec<TyVar>,
    lifted: HashMap<Var, Lifted>,
    /// Inside a lifted function, what its captured names are called there:
    /// innermost last.
    renames: Vec<HashMap<Var, Var>>,
    /// The definition being rewritten, to name what is lifted out of it.
    outer: InternedString,
    /// And the module it is of, which what is lifted out of it is of too.
    module: InternedString,
    count: usize,
    /// Whether `specialize` copied generic bindings per representation, whose
    /// mentions carry their original's type arguments -- see [`Lifter::lift`].
    representations: bool,
}

impl Lifter {
    fn name(&self, v: Var) -> Var {
        for layer in self.renames.iter().rev() {
            if let Some(n) = layer.get(&v) {
                return *n;
            }
        }
        v
    }

    /// A mention of a lifted function: the definition, instantiated and
    /// applied to what it captured.
    fn mention(&self, v: Var, tys: Option<&[Ty]>) -> Term {
        let l = &self.lifted[&v];
        let mut args: Vec<Ty> = l.scope.iter().map(|b| InferType::Var(b.id)).collect();
        match tys {
            Some(tys) => args.extend(tys.iter().cloned()),
            // A bare mention of a generic function is one inside its own
            // group, at its own binders.
            None => args.extend(l.own.iter().map(|b| InferType::Var(b.id))),
        }
        let mut t = Term::Var(l.def);
        if !args.is_empty() {
            t = Term::TyApp(Arc::new(t), args);
        }
        for c in &l.captured {
            t = Term::App(Arc::new(t), Arc::new(Term::Var(self.name(*c))));
        }
        t
    }

    /// A binding's right-hand side, if it is a function: its own type
    /// binders, and the lambda under them.
    fn function<'t>(poly: &Poly, rhs: &'t Term) -> Option<(Vec<TyVar>, &'t Term)> {
        match rhs.peel() {
            Term::TyLam(bs, inner) if *bs == poly.binders => match inner.peel() {
                Term::Lam(..) => Some((bs.clone(), inner)),
                _ => None,
            },
            Term::Lam(..) if poly.binders.is_empty() => Some((Vec::new(), rhs)),
            _ => None,
        }
    }

    /// What a group of functions captures, as the names they have where it is
    /// bound -- or `None` if it cannot be lifted.
    fn captures(&self, group: &[Var], rhss: &[&Term]) -> Option<Vec<Var>> {
        let mut free = HashSet::new();
        for t in rhss {
            free_vars_into(t, &mut free);
        }
        // In the order they are first mentioned, not by number: a package
        // read back from the incremental cache is the same program under other
        // numbers, and has to compile to the same code.
        let mut ordered: Vec<Var> = Vec::new();
        for t in rhss {
            rewrite::visit(t, &mut |x| {
                if let Term::Var(v) | Term::Jump(v, ..) = x
                    && free.contains(v)
                    && !ordered.contains(v)
                {
                    ordered.push(*v);
                }
                true
            });
        }
        let mut out: Vec<Var> = Vec::new();
        let add = |v: Var, out: &mut Vec<Var>| {
            if !out.contains(&v) {
                out.push(v);
            }
        };
        for v in ordered {
            if group.contains(&v) || self.globals.contains(&v) {
                continue;
            }
            if let Some(l) = self.lifted.get(&v) {
                for c in &l.captured {
                    add(*c, &mut out);
                }
                continue;
            }
            if self.joins.contains(&v) {
                return None;
            }
            match self.types.get(&v) {
                Some(p) if p.is_mono() => add(v, &mut out),
                _ => return None,
            }
        }
        Some(out)
    }

    /// Lift a group of functions, if it can be: `None` leaves it be.
    ///
    /// `body` is where the group is in scope besides its own right-hand sides.
    /// A copy `specialize` made at a representation is mentioned with its
    /// original's type arguments rather than its own -- see `specialize` --
    /// which a lifted definition could not say, so a group mentioned that way
    /// stays where it is.
    fn lift(&mut self, binds: &[(Var, &Poly, &Term)], body: &Term) -> Option<()> {
        let mut shapes = Vec::new();
        for (_, poly, rhs) in binds {
            shapes.push(Self::function(poly, rhs)?);
        }
        // Copies made per representation sit beside the generic binding they
        // were made from, and are typed through it: it stays, and so do they.
        let generic = |own: &Vec<TyVar>| {
            own.iter()
                .any(|b| !matches!(b.kind, VarKind::Effect | VarKind::Row))
        };
        if self.representations && shapes.iter().any(|(own, _)| generic(own)) {
            return None;
        }
        let arity: HashMap<Var, usize> = binds
            .iter()
            .zip(&shapes)
            .map(|((v, _, _), (own, _))| (*v, own.len()))
            .collect();
        let mut mismatched = false;
        for t in binds
            .iter()
            .map(|(_, _, t)| *t)
            .chain(std::iter::once(body))
        {
            rewrite::visit(t, &mut |x| {
                if let Term::TyApp(f, tys) = x
                    && let Term::Var(v) = f.peel()
                    && arity.get(v).is_some_and(|n| *n != tys.len())
                {
                    mismatched = true;
                }
                !mismatched
            });
        }
        if mismatched {
            return None;
        }
        let group: Vec<Var> = binds.iter().map(|(v, _, _)| *v).collect();
        let lambdas: Vec<&Term> = shapes.iter().map(|(_, l)| *l).collect();
        let captured = self.captures(&group, &lambdas)?;
        let scope = self.tvs.clone();
        for ((v, _, _), (own, _)) in binds.iter().zip(&shapes) {
            let def = self.fresh.var();
            self.lifted.insert(
                *v,
                Lifted {
                    def,
                    scope: scope.clone(),
                    own: own.clone(),
                    captured: captured.clone(),
                },
            );
        }
        for ((v, poly, _), (own, lambda)) in binds.iter().zip(&shapes) {
            let mut layer = HashMap::new();
            let mut params = Vec::new();
            for c in &captured {
                let fresh = self.fresh.var();
                let origin = self.origins.get(c).copied().unwrap_or(*c);
                self.origins.insert(fresh, origin);
                layer.insert(*c, fresh);
                params.push((fresh, self.types[c].ty.clone()));
            }
            self.renames.push(layer);
            let depth = self.tvs.len();
            self.tvs.extend(own.iter().copied());
            let mut term = self.term(lambda);
            self.tvs.truncate(depth);
            self.renames.pop();

            let mut ty = poly.ty.clone();
            for (p, pty) in params.iter().rev() {
                term = Term::Lam(*p, pty.clone(), Arc::new(term));
                ty = InferType::Fun(vec![pty.clone()], Box::new(ty), Box::new(unknown()));
            }
            let mut binders = scope.clone();
            binders.extend(own.iter().copied());
            if !binders.is_empty() {
                term = Term::TyLam(binders.clone(), Arc::new(term));
            }
            self.count += 1;
            let def = self.lifted[v].def;
            self.origins.insert(def, *v);
            self.made.push(Def {
                var: def,
                name: InternedString::from(format!("{}Local{}", self.outer, self.count)),
                module: self.module,
                poly: Poly { binders, ty },
                term,
            });
        }
        Some(())
    }

    fn term(&mut self, t: &Term) -> Term {
        let go = |s: &mut Lifter, x: &Term| Arc::new(s.term(x));
        match t {
            Term::Var(v) => {
                if self.lifted.contains_key(v) {
                    self.mention(*v, None)
                } else {
                    Term::Var(self.name(*v))
                }
            }
            Term::TyApp(f, tys) => match f.peel() {
                Term::Var(v) if self.lifted.contains_key(v) => self.mention(*v, Some(tys)),
                _ => Term::TyApp(go(self, f), tys.clone()),
            },
            Term::Lit(_) | Term::Error => t.clone(),
            Term::Loc(l, inner) => Term::Loc(*l, go(self, inner)),
            Term::Lam(v, ty, body) => {
                self.types.insert(*v, Poly::mono(ty.clone()));
                Term::Lam(*v, ty.clone(), go(self, body))
            }
            Term::TyLam(bs, body) => {
                let depth = self.tvs.len();
                self.tvs.extend(bs.iter().copied());
                let body = go(self, body);
                self.tvs.truncate(depth);
                Term::TyLam(bs.clone(), body)
            }
            Term::App(a, b) => {
                let a = go(self, a);
                Term::App(a, go(self, b))
            }
            Term::Let(v, poly, rhs, body) => {
                self.types.insert(*v, poly.clone());
                if self.lift(&[(*v, poly, rhs)], body).is_some() {
                    return self.term(body);
                }
                let rhs = self.generic(poly, rhs);
                Term::Let(*v, poly.clone(), rhs, go(self, body))
            }
            Term::LetRec(binds, body) => {
                for (v, poly, _) in binds {
                    self.types.insert(*v, poly.clone());
                }
                let group: Vec<(Var, &Poly, &Term)> =
                    binds.iter().map(|(v, p, t)| (*v, p, t)).collect();
                if self.lift(&group, body).is_some() {
                    return self.term(body);
                }
                let binds = binds
                    .iter()
                    .map(|(v, poly, rhs)| (*v, poly.clone(), (*self.generic(poly, rhs)).clone()))
                    .collect();
                Term::LetRec(binds, go(self, body))
            }
            Term::Join {
                var,
                params,
                ty,
                rhs,
                body,
            } => {
                for (v, pty) in params {
                    self.types.insert(*v, Poly::mono(pty.clone()));
                }
                self.joins.insert(*var);
                let rhs = go(self, rhs);
                Term::Join {
                    var: *var,
                    params: params.clone(),
                    ty: ty.clone(),
                    rhs,
                    body: go(self, body),
                }
            }
            Term::Jump(j, args, ty) => Term::Jump(
                self.name(*j),
                args.iter().map(|a| self.term(a)).collect(),
                ty.clone(),
            ),
            Term::If(c, a, b) => {
                let c = go(self, c);
                let a = go(self, a);
                Term::If(c, a, go(self, b))
            }
            Term::Tuple(xs) => Term::Tuple(xs.iter().map(|x| self.term(x)).collect()),
            Term::Proj(x, i) => Term::Proj(go(self, x), *i),
            Term::Array(xs, ty) => {
                Term::Array(xs.iter().map(|x| self.term(x)).collect(), ty.clone())
            }
            Term::Record(fs) => Term::Record(fs.iter().map(|(n, x)| (*n, self.term(x))).collect()),
            Term::Sel(x, n, ty) => Term::Sel(go(self, x), *n, ty.clone()),
            Term::Extend(x, n, y) => {
                let x = go(self, x);
                Term::Extend(x, *n, go(self, y))
            }
            Term::Ctor(n, ty, xs) => {
                Term::Ctor(*n, ty.clone(), xs.iter().map(|x| self.term(x)).collect())
            }
            Term::Prim(p, xs, ty) => {
                Term::Prim(*p, xs.iter().map(|x| self.term(x)).collect(), ty.clone())
            }
            Term::Perform(e, op, a, ty) => Term::Perform(*e, *op, go(self, a), ty.clone()),
            Term::Case(s, arms, ty) => {
                let s = go(self, s);
                let arms = arms
                    .iter()
                    .map(|(p, g, b)| {
                        self.pattern(p);
                        let g = g.as_ref().map(|g| self.term(g));
                        (p.clone(), g, self.term(b))
                    })
                    .collect();
                Term::Case(s, arms, ty.clone())
            }
            Term::Handle {
                body,
                clauses,
                ret,
                ty,
            } => {
                let body = go(self, body);
                let clauses = clauses
                    .iter()
                    .map(|c| {
                        self.types.insert(c.param, Poly::mono(c.param_ty.clone()));
                        self.types.insert(c.resume, Poly::mono(c.resume_ty.clone()));
                        HClause {
                            body: self.term(&c.body),
                            ..c.clone()
                        }
                    })
                    .collect();
                let ret = ret.as_ref().map(|(v, vty, t)| {
                    self.types.insert(*v, Poly::mono(vty.clone()));
                    (*v, vty.clone(), go(self, t))
                });
                Term::Handle {
                    body,
                    clauses,
                    ret,
                    ty: ty.clone(),
                }
            }
        }
    }

    /// A right-hand side left where it is, with its own type binders in scope.
    fn generic(&mut self, poly: &Poly, rhs: &Term) -> Arc<Term> {
        let depth = self.tvs.len();
        if matches!(rhs.peel(), Term::TyLam(..)) {
            // The `TyLam` case pushes them itself.
        } else {
            self.tvs.extend(poly.binders.iter().copied());
        }
        let out = Arc::new(self.term(rhs));
        self.tvs.truncate(depth);
        out
    }

    fn pattern(&mut self, p: &Pat) {
        match p {
            Pat::Wild | Pat::Lit(_) => {}
            Pat::Var(v, ty) => {
                self.types.insert(*v, Poly::mono(ty.clone()));
            }
            Pat::As(v, ty, sub) => {
                self.types.insert(*v, Poly::mono(ty.clone()));
                self.pattern(sub);
            }
            Pat::Tuple(ps) | Pat::Array(ps) | Pat::Ctor(_, ps) => {
                for p in ps {
                    self.pattern(p);
                }
            }
            Pat::Record(fs) => {
                for (_, p) in fs {
                    self.pattern(p);
                }
            }
        }
    }
}
