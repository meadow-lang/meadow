//! **Tail recursion modulo cons.**
//!
//! ```text
//! fun map f xs = match xs with
//!   | [;] -> [;]
//!   | x :: rest -> f x :: map f rest
//! ```
//!
//! The recursive call is not a tail call: the cons is still to be built when it
//! answers, so every element costs a frame on the way down and a return on the
//! way back up. The frame stack is chunked and grows without limit, so this is
//! never an overflow -- but it is a frame per element, live until the end.
//!
//! What the cons needs from the call is only its tail field. So build the cons
//! *first*, with a placeholder where the tail goes, and have the recursive call
//! write its answer into that field instead of returning it. That call is then
//! the last thing done: a tail call, which is a jump. The list is built front
//! to back in constant stack, the way a loop would build it.
//!
//! For each top-level function with such a call this pass makes a second one,
//! in **destination-passing style**: `map_dps dst i f xs`, which writes what
//! `map f xs` would have answered into field `i` of `dst` and answers `()`.
//! Its body is `map`'s, with every tail position changed:
//!
//! * a constructor with the recursive call in a field -- `C a (map f rest)` --
//!   builds `C a hole`, writes it into `dst`, and continues with
//!   `map_dps cell 1 f rest`, a tail call;
//! * a tail call to `map` itself continues with `map_dps dst i …`;
//! * anything else is written into `dst`.
//!
//! And `map` itself changes only at those constructors, which build the cell,
//! hand it to `map_dps`, and answer it.
//!
//! # The placeholder
//!
//! Field `i` holds, until it is written, a value of the field's own type, so
//! that the cell is a well-typed value of its type at every moment, with the
//! representation its type says, and nothing downstream has to know what a
//! hole is. In the twin that value is `dst`, which is at hand and costs
//! nothing. The first cell, which the function itself builds, has no `dst` to
//! borrow, and takes a **nullary constructor of the type** -- `Nil` for a
//! list, `Leaf` for a tree; a type with none is left alone.
//!
//! # Why the write is safe
//!
//! Constructors are immutable everywhere else, and nothing may tell this one
//! was not. It cannot: the cell is made by the step that writes it into its
//! parent and passes it on, and until the outermost call answers, the chain of
//! cells is reachable only from that call's frame. Resumptions are one-shot, so
//! no continuation re-enters a half-built chain. The write itself is
//! [`Prim::SetField`], which on the bytecode machine is the collector's
//! barriered field store: a cell promoted mid-recursion is remembered like any
//! old object that comes to point at a young one.
//!
//! # Order of evaluation
//!
//! The recursive call's arguments are evaluated after the constructor's other
//! fields rather than in their place. That is only allowed when every field
//! after the recursive one is already a value -- nothing to run, so nothing to
//! reorder. It is the last field in every case that matters.
//!
//! # In the pipeline
//!
//! From [`OptLevel::O1`], after `simplify`, so that nothing afterwards copies a
//! cell or looks into it: `simplify` would read a placeholder as the field's
//! value. The CEK machine never sees it -- it runs core as lowering wrote it.

use crate::inline::{Body, Fresh, freshen_mapped};
use crate::*;

/// Rewrite every top-level function that has a recursive call under a
/// constructor in tail position.
pub fn program(p: &Program, opt: OptLevel) -> Program {
    if opt == OptLevel::O0 || std::env::var_os("MEADOW_NO_TRMC").is_some() {
        return p.clone();
    }
    let mut fresh = Fresh(simplify::max_var(p) + 1);
    let mut origins = p.origins.clone();
    let mut defs = Vec::with_capacity(p.defs.len());
    let mut made = Vec::new();
    for d in &p.defs {
        match Candidate::of(d, &p.variants) {
            Some(c) if c.worthwhile() => {
                let (entry, dps) = c.rewrite(&mut fresh, &mut origins);
                defs.push(entry);
                made.push(dps);
            }
            _ => defs.push(d.clone()),
        }
    }
    defs.extend(made);
    Program {
        defs,
        entry: p.entry,
        ctor_fields: p.ctor_fields.clone(),
        variants: p.variants.clone(),
        origins,
        migrations: p.migrations.clone(),
    }
}

/// A top-level function, taken apart: what TRMC needs to know about it.
struct Candidate<'d> {
    def: &'d Def,
    binders: Vec<TyVar>,
    params: Vec<(Var, Ty)>,
    body: &'d Term,
    /// What it answers, which is also the type of every cell it builds.
    result: Ty,
    /// The first cell's placeholder: a nullary constructor of `result`.
    hole: Term,
}

impl<'d> Candidate<'d> {
    fn of(d: &'d Def, variants: &meadow_infer::VariantEnv) -> Option<Candidate<'d>> {
        let mut t = d.term.peel();
        let mut binders = Vec::new();
        if let Term::TyLam(bs, inner) = t {
            binders = bs.clone();
            t = inner.peel();
        }
        let mut params = Vec::new();
        while let Term::Lam(v, ty, inner) = t {
            params.push((*v, ty.clone()));
            t = inner.peel();
        }
        if params.is_empty() {
            return None;
        }
        let mut result = &d.poly.ty;
        for _ in 0..params.len() {
            result = match result {
                InferType::Fun(ps, ret, _) if ps.len() == 1 => ret,
                _ => return None,
            };
        }
        let InferType::Con(ty, _) = result else {
            return None;
        };
        let nullary = variants.get(ty)?.iter().find(|v| v.fields.is_empty())?;
        Some(Candidate {
            def: d,
            binders,
            params,
            body: t,
            result: result.clone(),
            hole: Term::Ctor(nullary.name, result.clone(), Vec::new()),
        })
    }

    /// Is there a constructor with a recursive call in it, in tail position?
    fn worthwhile(&self) -> bool {
        self.any_site(self.body)
    }

    fn any_site(&self, t: &Term) -> bool {
        match t {
            Term::Loc(_, inner) => self.any_site(inner),
            Term::Let(_, _, _, body) | Term::LetRec(_, body) => self.any_site(body),
            Term::If(_, a, b) => self.any_site(a) || self.any_site(b),
            Term::Case(_, arms, _) => arms.iter().any(|(_, _, a)| self.any_site(a)),
            Term::Join { rhs, body, .. } => self.any_site(rhs) || self.any_site(body),
            t => self.site(t).is_some(),
        }
    }

    /// The arguments of `t`, if it is a saturated call of this function at
    /// its own type parameters.
    fn self_call<'t>(&self, t: &'t Term) -> Option<Vec<&'t Term>> {
        let mut args = Vec::new();
        let mut cur = t.peel();
        while let Term::App(f, a) = cur {
            args.push(&**a);
            cur = f.peel();
        }
        args.reverse();
        let head = match cur {
            Term::TyApp(g, tys) if self.at_own_types(tys) => g.peel(),
            t if self.binders.is_empty() => t,
            _ => return None,
        };
        (matches!(head, Term::Var(v) if *v == self.def.var) && args.len() == self.params.len())
            .then_some(args)
    }

    fn at_own_types(&self, tys: &[Ty]) -> bool {
        tys.len() == self.binders.len()
            && tys
                .iter()
                .zip(&self.binders)
                .all(|(t, b)| matches!(t, InferType::Var(id) if *id == b.id))
    }

    /// A constructor with a recursive call in field `k` and nothing but values
    /// after it: its name, type and fields, `k`, and the call's arguments.
    fn site<'t>(&self, t: &'t Term) -> Option<Site<'t>> {
        let Term::Ctor(name, ty, fields) = t.peel() else {
            return None;
        };
        let k = fields.iter().rposition(|f| self.self_call(f).is_some())?;
        if !fields[k + 1..].iter().all(is_value) {
            return None;
        }
        Some(Site {
            name: *name,
            ty,
            fields,
            k,
            args: self.self_call(&fields[k])?,
        })
    }

    /// The function itself, changed at its sites, and its destination-passing
    /// twin.
    fn rewrite(&self, fresh: &mut Fresh, origins: &mut HashMap<Var, Var>) -> (Def, Def) {
        let dps = fresh.var();
        let entry_body = self.entry(self.body, dps, fresh);
        let entry = Def {
            var: self.def.var,
            name: self.def.name,
            module: self.def.module,
            poly: self.def.poly.clone(),
            term: self.wrap(&self.params, entry_body),
        };

        let dst = fresh.var();
        let at = fresh.var();
        let body = self.dps(self.body, dps, dst, at, fresh);
        // Its own names for everything its body binds: core binds each once.
        let (copy, renamed) = freshen_mapped(
            &Body {
                binders: Vec::new(),
                params: self.params.clone(),
                term: body,
                original: None,
            },
            fresh,
        );
        for (old, new) in renamed {
            let root = origins.get(&old).copied().unwrap_or(old);
            origins.insert(new, root);
        }
        let mut params = vec![(dst, self.result.clone()), (at, InferType::con("Int"))];
        params.extend(copy.params);
        let twin = Def {
            var: dps,
            name: self.def.name,
            module: self.def.module,
            poly: Poly {
                binders: self.binders.clone(),
                ty: self.dps_type(),
            },
            term: self.wrap(&params, copy.term),
        };
        (entry, twin)
    }

    /// `/\binders. \params. body`.
    fn wrap(&self, params: &[(Var, Ty)], body: Term) -> Term {
        let lams = params.iter().rev().fold(body, |acc, (v, ty)| {
            Term::Lam(*v, ty.clone(), Arc::new(acc))
        });
        if self.binders.is_empty() {
            lams
        } else {
            Term::TyLam(self.binders.clone(), Arc::new(lams))
        }
    }

    /// `result -> Int -> <the function's own arrows> -> ()`.
    fn dps_type(&self) -> Ty {
        fn unit_after(ty: &Ty, n: usize) -> Ty {
            match ty {
                InferType::Fun(ps, ret, eff) if n > 0 => {
                    InferType::Fun(ps.clone(), Box::new(unit_after(ret, n - 1)), eff.clone())
                }
                _ => InferType::con("Unit"),
            }
        }
        let own = unit_after(&self.def.poly.ty, self.params.len());
        let arrow =
            |p: Ty, r: Ty| InferType::Fun(vec![p], Box::new(r), Box::new(InferType::RowEmpty));
        arrow(self.result.clone(), arrow(InferType::con("Int"), own))
    }

    /// `dps [binders] dst at args…`.
    fn call(&self, dps: Var, dst: Term, at: Term, args: &[&Term]) -> Term {
        let head = if self.binders.is_empty() {
            Term::Var(dps)
        } else {
            Term::TyApp(
                Arc::new(Term::Var(dps)),
                self.binders.iter().map(|b| InferType::Var(b.id)).collect(),
            )
        };
        std::iter::once(dst)
            .chain(std::iter::once(at))
            .chain(args.iter().map(|a| (*a).clone()))
            .fold(head, |f, a| Term::App(Arc::new(f), Arc::new(a)))
    }

    /// The site's constructor, with `hole` where the call was.
    fn cell(&self, site: &Site, hole: Term) -> Term {
        let mut fields = site.fields.to_vec();
        fields[site.k] = hole;
        Term::Ctor(site.name, site.ty.clone(), fields)
    }

    /// The function's own body: at a site, build the cell, have the twin fill
    /// it, and answer it.
    fn entry(&self, t: &Term, dps: Var, fresh: &mut Fresh) -> Term {
        match t {
            Term::Loc(l, inner) => Term::Loc(l.clone(), Arc::new(self.entry(inner, dps, fresh))),
            Term::Let(x, p, rhs, body) => Term::Let(
                *x,
                p.clone(),
                rhs.clone(),
                Arc::new(self.entry(body, dps, fresh)),
            ),
            Term::LetRec(binds, body) => {
                Term::LetRec(binds.clone(), Arc::new(self.entry(body, dps, fresh)))
            }
            Term::If(c, a, b) => Term::If(
                c.clone(),
                Arc::new(self.entry(a, dps, fresh)),
                Arc::new(self.entry(b, dps, fresh)),
            ),
            Term::Case(s, arms, ty) => Term::Case(
                s.clone(),
                arms.iter()
                    .map(|(p, g, a)| (p.clone(), g.clone(), self.entry(a, dps, fresh)))
                    .collect(),
                ty.clone(),
            ),
            Term::Join {
                var,
                params,
                ty,
                rhs,
                body,
            } => Term::Join {
                var: *var,
                params: params.clone(),
                ty: ty.clone(),
                rhs: Arc::new(self.entry(rhs, dps, fresh)),
                body: Arc::new(self.entry(body, dps, fresh)),
            },
            t => match self.site(t) {
                Some(site) => {
                    let cell = fresh.var();
                    let done = fresh.var();
                    let fill = self.call(
                        dps,
                        Term::Var(cell),
                        Term::Lit(Lit::Int(site.k as i64)),
                        &site.args,
                    );
                    Term::Let(
                        cell,
                        Poly::mono(self.result.clone()),
                        Arc::new(self.cell(&site, self.hole.clone())),
                        Arc::new(Term::Let(
                            done,
                            Poly::mono(InferType::con("Unit")),
                            Arc::new(fill),
                            Arc::new(Term::Var(cell)),
                        )),
                    )
                }
                None => t.clone(),
            },
        }
    }

    /// The twin's body: every tail position writes into field `at` of `dst`.
    fn dps(&self, t: &Term, dps: Var, dst: Var, at: Var, fresh: &mut Fresh) -> Term {
        let unit = || InferType::con("Unit");
        let write = |value: Term| {
            Term::Prim(
                Prim::SetField,
                vec![Term::Var(dst), Term::Var(at), value],
                unit(),
            )
        };
        match t {
            Term::Loc(l, inner) => {
                Term::Loc(l.clone(), Arc::new(self.dps(inner, dps, dst, at, fresh)))
            }
            Term::Let(x, p, rhs, body) => Term::Let(
                *x,
                p.clone(),
                rhs.clone(),
                Arc::new(self.dps(body, dps, dst, at, fresh)),
            ),
            Term::LetRec(binds, body) => {
                Term::LetRec(binds.clone(), Arc::new(self.dps(body, dps, dst, at, fresh)))
            }
            Term::If(c, a, b) => Term::If(
                c.clone(),
                Arc::new(self.dps(a, dps, dst, at, fresh)),
                Arc::new(self.dps(b, dps, dst, at, fresh)),
            ),
            Term::Case(s, arms, _) => Term::Case(
                s.clone(),
                arms.iter()
                    .map(|(p, g, a)| (p.clone(), g.clone(), self.dps(a, dps, dst, at, fresh)))
                    .collect(),
                unit(),
            ),
            Term::Join {
                var,
                params,
                rhs,
                body,
                ..
            } => Term::Join {
                var: *var,
                params: params.clone(),
                ty: unit(),
                rhs: Arc::new(self.dps(rhs, dps, dst, at, fresh)),
                body: Arc::new(self.dps(body, dps, dst, at, fresh)),
            },
            // Every jump in tail position is to a join point of this body,
            // whose answer is now `()` too.
            Term::Jump(j, args, _) => Term::Jump(*j, args.clone(), unit()),
            t => {
                if let Some(site) = self.site(t) {
                    let cell = fresh.var();
                    let done = fresh.var();
                    let next = self.call(
                        dps,
                        Term::Var(cell),
                        Term::Lit(Lit::Int(site.k as i64)),
                        &site.args,
                    );
                    // `dst` as the placeholder: a value of the type, already
                    // made, so the cell costs one allocation rather than two.
                    return Term::Let(
                        cell,
                        Poly::mono(self.result.clone()),
                        Arc::new(self.cell(&site, Term::Var(dst))),
                        Arc::new(Term::Let(
                            done,
                            Poly::mono(unit()),
                            Arc::new(write(Term::Var(cell))),
                            Arc::new(next),
                        )),
                    );
                }
                if let Some(args) = self.self_call(t) {
                    return self.call(dps, Term::Var(dst), Term::Var(at), &args);
                }
                let answer = fresh.var();
                Term::Let(
                    answer,
                    Poly::mono(self.result.clone()),
                    Arc::new(t.clone()),
                    Arc::new(write(Term::Var(answer))),
                )
            }
        }
    }
}

/// A constructor in tail position with a recursive call in field `k`.
struct Site<'t> {
    name: InternedString,
    ty: &'t Ty,
    fields: &'t [Term],
    k: usize,
    args: Vec<&'t Term>,
}

/// Already computed: nothing to run, so nothing whose order could change.
fn is_value(t: &Term) -> bool {
    match t.peel() {
        Term::Lit(_) | Term::Var(_) | Term::Lam(..) => true,
        Term::Ctor(_, _, args) | Term::Tuple(args) => args.iter().all(is_value),
        _ => false,
    }
}
