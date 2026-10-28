//! **Type erasure: typed core to untyped core.**
//!
//! Everything downstream of core — AxCut, the bytecode machine, the CEK
//! evaluator — is untyped, and deliberately so: none of them can ask a
//! question a type would answer. This pass is the boundary. It drops the two
//! nodes that exist only for the type system, [`Term::TyLam`] and
//! [`Term::TyApp`], and leaves everything else alone (the annotations on
//! binders cost a backend nothing to ignore).
//!
//! Why a pass rather than two extra match arms in each backend: a `TyLam`
//! sits between a definition and its parameters, and a `TyApp` between a call
//! and the name it calls. A backend that skipped them *locally* would still
//! fail to see a polymorphic function's arity, or that a call is a known one —
//! so every polymorphic function in the program would quietly lose the fast
//! path it had before core was typed. Erasing first means no backend has to
//! remember.

use crate::*;

/// Erase a whole program.
pub fn program(p: &Program) -> Program {
    Program {
        defs: p
            .defs
            .iter()
            .map(|d| Def {
                var: d.var,
                name: d.name,
                module: d.module,
                poly: d.poly.clone(),
                term: term(&d.term),
            })
            .collect(),
        entry: p.entry,
        ctor_fields: p.ctor_fields.clone(),
        variants: p.variants.clone(),
        origins: p.origins.clone(),
        migrations: p.migrations.clone(),
    }
}

/// Erase one term.
pub fn term(t: &Term) -> Term {
    match t {
        // The two that go.
        Term::TyLam(_, body) => term(body),
        Term::TyApp(f, _) => term(f),

        Term::Loc(l, inner) => Term::Loc(*l, Arc::new(term(inner))),
        Term::Var(_) | Term::Lit(_) | Term::Error => t.clone(),
        Term::Lam(v, ty, body) => Term::Lam(*v, ty.clone(), Arc::new(term(body))),
        Term::App(f, a) => Term::App(Arc::new(term(f)), Arc::new(term(a))),
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
            rhs: Arc::new(term(rhs)),
            body: Arc::new(term(body)),
        },
        Term::Jump(j, args, ty) => Term::Jump(*j, args.iter().map(term).collect(), ty.clone()),
        Term::Let(v, p, rhs, body) => {
            Term::Let(*v, p.clone(), Arc::new(term(rhs)), Arc::new(term(body)))
        }
        Term::LetRec(binds, body) => Term::LetRec(
            binds
                .iter()
                .map(|(v, p, t)| (*v, p.clone(), term(t)))
                .collect(),
            Arc::new(term(body)),
        ),
        Term::If(c, a, b) => Term::If(Arc::new(term(c)), Arc::new(term(a)), Arc::new(term(b))),
        Term::Tuple(xs) => Term::Tuple(xs.iter().map(term).collect()),
        Term::Proj(x, i) => Term::Proj(Arc::new(term(x)), *i),
        Term::Array(xs, ty) => Term::Array(xs.iter().map(term).collect(), ty.clone()),
        Term::Record(fs) => Term::Record(fs.iter().map(|(l, x)| (*l, term(x))).collect()),
        Term::Sel(x, l, ty) => Term::Sel(Arc::new(term(x)), *l, ty.clone()),
        Term::Extend(x, l, v) => Term::Extend(Arc::new(term(x)), *l, Arc::new(term(v))),
        Term::Ctor(n, ty, xs) => Term::Ctor(*n, ty.clone(), xs.iter().map(term).collect()),
        Term::Case(s, arms, ty) => Term::Case(
            Arc::new(term(s)),
            arms.iter()
                .map(|(p, g, b)| (p.clone(), g.as_ref().map(term), term(b)))
                .collect(),
            ty.clone(),
        ),
        Term::Prim(op, xs, ty) => Term::Prim(*op, xs.iter().map(term).collect(), ty.clone()),
        Term::Perform(e, op, a, ty) => Term::Perform(*e, *op, Arc::new(term(a)), ty.clone()),
        Term::Handle {
            body,
            clauses,
            ret,
            ty,
        } => Term::Handle {
            body: Arc::new(term(body)),
            clauses: clauses
                .iter()
                .map(|c| HClause {
                    effect: c.effect,
                    op: c.op,
                    param: c.param,
                    param_ty: c.param_ty.clone(),
                    resume: c.resume,
                    resume_ty: c.resume_ty.clone(),
                    body: term(&c.body),
                })
                .collect(),
            ret: ret
                .as_ref()
                .map(|(v, t, b)| (*v, t.clone(), Arc::new(term(b)))),
            ty: ty.clone(),
        },
    }
}
