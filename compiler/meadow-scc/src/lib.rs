//! **Dependency analysis of a compilation unit.**
//!
//! Type inference wants bindings in dependency order, and wants mutually
//! recursive ones handed to it together. This pass works that out at three scales,
//! both with Tarjan's algorithm over a "mentions" graph:
//!
//! * [`module_order`] sorts the *modules* of a unit, since they are discovered
//!   in alphabetical order, which has nothing to do with what depends on what.
//! * [`group_module`] sorts the *bindings* inside one module and records a
//!   [`hir::BindGroup`] per strongly connected component.
//! * [`unit_groups`] groups the bindings of the whole unit at once, which is
//!   the order inference goes in: modules that use one another both ways
//!   have no order of their own, and their bindings still do.
//!
//! Two things fall out of it.
//!
//! * **Inference stops guessing.** A reference to a binding that had not been
//!   inferred yet used to resolve to an unconstrained fresh variable, which
//!   silently discarded every constraint on it. Now a binding is only inferred
//!   once everything it refers to already has a type, and a cycle is inferred as
//!   a unit.
//! * **Evaluation stops caring about source order.** Top-level definitions are
//!   evaluated eagerly in the order they are lowered, so a `def` that used a
//!   function declared further down the file — or in an alphabetically later
//!   module — used to fail at run time.
//!
//! Free variables need no scope tracking: a `VarId` is unique across the whole
//! program, so a local binding can never collide with a top-level one and
//! intersecting "every variable mentioned" with "the top-level names" is exact.

use meadow_hir::{self as hir, BindGroup, VarId};
use std::collections::HashMap;

/// Dependency order for the modules of one compilation unit, as a permutation:
/// the module to place `k`th is `module_order(..)[k]`.
///
/// A unit's modules are resolved together — each in its own namespace, but
/// against one shared pool of ids — so a module may `use` a sibling declared
/// after it and two modules may be mutually recursive. Mutually recursive
/// modules keep their source order relative to one another: there is no better
/// answer, and source order is the predictable one.
pub fn module_order<'a>(
    modules: impl IntoIterator<Item = &'a hir::Module>,
    overloads: &hir::Overloads,
) -> Vec<usize> {
    let modules: Vec<&hir::Module> = modules.into_iter().collect();

    // Which module defines which name.
    let mut owner: HashMap<VarId, usize> = HashMap::new();
    for (m, module) in modules.iter().enumerate() {
        for decl in &module.decls {
            if let hir::Decl::Bind(b) = decl.value() {
                for v in b.bound_vars() {
                    owner.insert(v, m);
                }
            }
        }
    }

    // Edge module -> module. A module mentioning its own bindings says nothing
    // about ordering *between* modules, so self-edges are dropped.
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); modules.len()];
    for (m, module) in modules.iter().enumerate() {
        let mut seen = Vec::new();
        for decl in &module.decls {
            if let hir::Decl::Bind(b) = decl.value() {
                mentions(b, &owner, overloads, &mut seen);
            }
        }
        seen.retain(|&d| d != m);
        seen.sort_unstable();
        seen.dedup();
        edges[m] = seen;
    }

    scc(&edges).into_iter().flatten().collect()
}

/// Order `module`'s declarations by dependency and record its binding groups.
///
/// Non-binding declarations (`data`, `record`, `effect`, `use`, `mod`) keep
/// their relative order and come first; they declare types and bring names into
/// scope, neither of which is sequenced against the bindings.
pub fn group_module(module: &mut hir::Module, overloads: &hir::Overloads) {
    let binds: Vec<usize> = (0..module.decls.len())
        .filter(|&i| matches!(module.decls[i].value(), hir::Decl::Bind(_)))
        .collect();
    if binds.is_empty() {
        module.groups = Vec::new();
        return;
    }

    // Which binding defines which name, so a mention can be traced to a decl.
    let mut owner: HashMap<VarId, usize> = HashMap::new();
    for (slot, &i) in binds.iter().enumerate() {
        if let hir::Decl::Bind(b) = module.decls[i].value() {
            for v in b.bound_vars() {
                owner.insert(v, slot);
            }
        }
    }

    // Edge slot -> slot for "mentions". Self-edges are kept: they are what makes
    // a lone binding recursive.
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); binds.len()];
    for (slot, &i) in binds.iter().enumerate() {
        if let hir::Decl::Bind(b) = module.decls[i].value() {
            let mut seen = Vec::new();
            mentions(b, &owner, overloads, &mut seen);
            seen.sort_unstable();
            seen.dedup();
            edges[slot] = seen;
        }
    }

    let comps = scc(&edges);

    // Rebuild the declaration list: everything that is not a binding first, then
    // the bindings grouped and ordered by dependency.
    let mut decls: Vec<hir::LDecl> = Vec::with_capacity(module.decls.len());
    let mut taken: Vec<Option<hir::LDecl>> = module.decls.drain(..).map(Some).collect();
    for (i, slot) in taken.iter_mut().enumerate() {
        if !binds.contains(&i) {
            decls.push(slot.take().expect("each decl moved once"));
        }
    }

    let mut groups = Vec::with_capacity(comps.len());
    for comp in comps {
        let recursive = comp.len() > 1 || comp.first().is_some_and(|&s| edges[s].contains(&s));
        let members = comp
            .iter()
            .map(|&slot| {
                let decl = taken[binds[slot]].take().expect("each decl moved once");
                decls.push(decl);
                decls.len() - 1
            })
            .collect();
        groups.push(BindGroup { members, recursive });
    }

    module.decls = decls;
    module.groups = groups;
}

/// One strongly connected component of a whole unit's top-level bindings:
/// each member as `(module, declaration)`, indices into the modules as given
/// and into their declarations as [`group_module`] left them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitGroup {
    pub members: Vec<(usize, usize)>,
    pub recursive: bool,
    /// The groups this one mentions, by their place in the list: always
    /// earlier ones, since a group comes after everything it mentions. What
    /// an editor reusing the types of some groups needs, to know which
    /// others a group's type was worked out from.
    pub uses: Vec<usize>,
}

/// The binding groups of a whole unit, in dependency order across its modules.
///
/// [`group_module`]'s groups are right within a module, and [`module_order`]
/// puts one module after another; together they are right as long as modules
/// do not depend on one another both ways. When they do -- a module's tests
/// calling a helper in a sibling that uses the module -- one of them has to go
/// first, and a name it takes from the other has no type yet. Inference then
/// used it at one monomorphic type, latent effect and all, across every use:
/// a use inside a query made a pure parser "perform `Fetch`" everywhere else.
/// Grouping the unit's bindings as one graph never asks that: a binding comes
/// after everything it mentions, wherever that is, and a group spans modules
/// only where the bindings really are mutually recursive.
pub fn unit_groups(modules: &[&hir::Module], overloads: &hir::Overloads) -> Vec<UnitGroup> {
    let mut at: Vec<(usize, usize)> = Vec::new();
    let mut owner: HashMap<VarId, usize> = HashMap::new();
    for (m, module) in modules.iter().enumerate() {
        for (i, decl) in module.decls.iter().enumerate() {
            if let hir::Decl::Bind(b) = decl.value() {
                for v in b.bound_vars() {
                    owner.insert(v, at.len());
                }
                at.push((m, i));
            }
        }
    }
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); at.len()];
    for (slot, &(m, i)) in at.iter().enumerate() {
        if let hir::Decl::Bind(b) = modules[m].decls[i].value() {
            let mut seen = Vec::new();
            mentions(b, &owner, overloads, &mut seen);
            seen.sort_unstable();
            seen.dedup();
            edges[slot] = seen;
        }
    }
    let comps = scc(&edges);
    let mut group_of = vec![0; at.len()];
    for (g, comp) in comps.iter().enumerate() {
        for &s in comp {
            group_of[s] = g;
        }
    }
    comps
        .iter()
        .enumerate()
        .map(|(g, comp)| {
            let mut uses: Vec<usize> = comp
                .iter()
                .flat_map(|&s| edges[s].iter().map(|&t| group_of[t]))
                .filter(|&u| u != g)
                .collect();
            uses.sort_unstable();
            uses.dedup();
            UnitGroup {
                recursive: comp.len() > 1 || comp.first().is_some_and(|&s| edges[s].contains(&s)),
                members: comp.iter().map(|&s| at[s]).collect(),
                uses,
            }
        })
        .collect()
}

/// The slots of the top-level bindings a binding's body mentions.
fn mentions(
    bind: &hir::Bind,
    owner: &HashMap<VarId, usize>,
    ov: &hir::Overloads,
    out: &mut Vec<usize>,
) {
    match bind {
        hir::Bind::Fun(_, params, _, body) => {
            params.iter().for_each(|p| pat_mentions(p, owner, ov, out));
            expr_mentions(body, owner, ov, out)
        }
        hir::Bind::Pat(p, body) => {
            pat_mentions(p, owner, ov, out);
            expr_mentions(body, owner, ov, out)
        }
        hir::Bind::Error => {}
    }
}

/// What the views in a pattern mention.
fn pat_mentions(
    pat: &hir::LPat,
    owner: &HashMap<VarId, usize>,
    ov: &hir::Overloads,
    out: &mut Vec<usize>,
) {
    match pat.value() {
        hir::Pat::View(f, p) => {
            expr_mentions(f, owner, ov, out);
            pat_mentions(p, owner, ov, out);
        }
        hir::Pat::Ann(p, _) => pat_mentions(p, owner, ov, out),
        hir::Pat::As(_, p) => pat_mentions(p, owner, ov, out),
        hir::Pat::Tuple(ps) | hir::Pat::Array(ps) | hir::Pat::List(ps) | hir::Pat::Cons(_, ps) => {
            ps.iter().for_each(|p| pat_mentions(p, owner, ov, out))
        }
        hir::Pat::Record(fields, _) => fields
            .iter()
            .for_each(|(_, p)| pat_mentions(p, owner, ov, out)),
        hir::Pat::Wildcard
        | hir::Pat::Var(_)
        | hir::Pat::Lit(_)
        | hir::Pat::Unit
        | hir::Pat::Error => {}
    }
}

fn expr_mentions(
    expr: &hir::LExpr,
    owner: &HashMap<VarId, usize>,
    ov: &hir::Overloads,
    out: &mut Vec<usize>,
) {
    match expr.value() {
        hir::Expr::Var(id) => {
            if let Some(&slot) = owner.get(id.value()) {
                out.push(slot);
            }
            // An overloaded name depends on every candidate: inference has to
            // be able to see all of their types to choose between them.
            for c in ov.get(&id.id).into_iter().flatten() {
                if let hir::Alt::Value(v) = c.alt
                    && let Some(&slot) = owner.get(&v)
                {
                    out.push(slot);
                }
            }
        }
        hir::Expr::Lam(params, body) => {
            params.iter().for_each(|p| pat_mentions(p, owner, ov, out));
            expr_mentions(body, owner, ov, out)
        }
        hir::Expr::App(f, args) => {
            expr_mentions(f, owner, ov, out);
            args.iter().for_each(|a| expr_mentions(a, owner, ov, out));
        }
        hir::Expr::Let(binds, body) => {
            binds.iter().for_each(|b| mentions(b, owner, ov, out));
            expr_mentions(body, owner, ov, out);
        }
        hir::Expr::If(c, t, e) => {
            expr_mentions(c, owner, ov, out);
            expr_mentions(t, owner, ov, out);
            expr_mentions(e, owner, ov, out);
        }
        hir::Expr::Match(scrut, arms) => {
            expr_mentions(scrut, owner, ov, out);
            for (p, g, b) in arms {
                pat_mentions(p, owner, ov, out);
                if let Some(g) = g {
                    expr_mentions(g, owner, ov, out);
                }
                expr_mentions(b, owner, ov, out);
            }
        }
        hir::Expr::Tuple(xs)
        | hir::Expr::Array(xs)
        | hir::Expr::List(xs)
        | hir::Expr::Cons(_, xs) => xs.iter().for_each(|x| expr_mentions(x, owner, ov, out)),
        hir::Expr::Record(fields, base) => {
            fields
                .iter()
                .for_each(|(_, e)| expr_mentions(e, owner, ov, out));
            if let Some(b) = base {
                expr_mentions(b, owner, ov, out);
            }
        }
        hir::Expr::Field(o, _) => expr_mentions(o, owner, ov, out),
        hir::Expr::Update(base, fields) => {
            expr_mentions(base, owner, ov, out);
            fields
                .iter()
                .for_each(|(_, e)| expr_mentions(e, owner, ov, out));
        }
        hir::Expr::Handle(body, arms, ret) => {
            expr_mentions(body, owner, ov, out);
            for a in arms {
                pat_mentions(&a.param, owner, ov, out);
                expr_mentions(&a.body, owner, ov, out);
            }
            if let Some((p, b)) = ret {
                pat_mentions(p, owner, ov, out);
                expr_mentions(b, owner, ov, out);
            }
        }
        hir::Expr::Lit(_) | hir::Expr::Unit | hir::Expr::Error => {}
    }
}

// ===========================================================================
// Tarjan
// ===========================================================================

/// Strongly connected components of `edges`, in dependency order: a component
/// appears only after every component it points at.
///
/// That is exactly the order Tarjan's algorithm emits them in, since it closes a
/// component only once everything reachable from it is already closed.
fn scc(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut st = Tarjan {
        edges,
        index: vec![usize::MAX; edges.len()],
        low: vec![0; edges.len()],
        on_stack: vec![false; edges.len()],
        stack: Vec::new(),
        next: 0,
        out: Vec::new(),
    };
    // Rooted in source order, so independent bindings keep the order they were
    // written in.
    for v in 0..edges.len() {
        if st.index[v] == usize::MAX {
            st.visit(v);
        }
    }
    st.out
}

struct Tarjan<'a> {
    edges: &'a [Vec<usize>],
    index: Vec<usize>,
    low: Vec<usize>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
    next: usize,
    out: Vec<Vec<usize>>,
}

impl Tarjan<'_> {
    fn visit(&mut self, v: usize) {
        self.index[v] = self.next;
        self.low[v] = self.next;
        self.next += 1;
        self.stack.push(v);
        self.on_stack[v] = true;

        for i in 0..self.edges[v].len() {
            let w = self.edges[v][i];
            if self.index[w] == usize::MAX {
                self.visit(w);
                self.low[v] = self.low[v].min(self.low[w]);
            } else if self.on_stack[w] {
                self.low[v] = self.low[v].min(self.index[w]);
            }
        }

        if self.low[v] == self.index[v] {
            let mut comp = Vec::new();
            while let Some(w) = self.stack.pop() {
                self.on_stack[w] = false;
                comp.push(w);
                if w == v {
                    break;
                }
            }
            // Popped innermost-first; source order reads better in diagnostics
            // and keeps independent members where the author put them.
            comp.reverse();
            self.out.push(comp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scc;

    #[test]
    fn independent_nodes_keep_source_order() {
        // 0, 1, 2 with no edges at all.
        let comps = scc(&[vec![], vec![], vec![]]);
        assert_eq!(comps, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn a_dependency_is_emitted_first() {
        // 0 -> 1: `0` mentions `1`, so `1` must be typed first.
        let comps = scc(&[vec![1], vec![]]);
        assert_eq!(comps, vec![vec![1], vec![0]]);
    }

    #[test]
    fn a_cycle_becomes_one_component() {
        // 0 -> 1 -> 0, and 2 depends on the pair.
        let comps = scc(&[vec![1], vec![0], vec![0]]);
        assert_eq!(comps, vec![vec![0, 1], vec![2]]);
    }

    #[test]
    fn a_self_edge_is_its_own_component() {
        let comps = scc(&[vec![0]]);
        assert_eq!(comps, vec![vec![0]]);
    }

    #[test]
    fn a_chain_comes_out_deepest_first() {
        // 0 -> 1 -> 2
        let comps = scc(&[vec![1], vec![2], vec![]]);
        assert_eq!(comps, vec![vec![2], vec![1], vec![0]]);
    }

    #[test]
    fn separate_cycles_stay_separate() {
        // {0,1} and {2,3}, with 0 depending on 2.
        let comps = scc(&[vec![1, 2], vec![0], vec![3], vec![2]]);
        assert_eq!(comps, vec![vec![2, 3], vec![0, 1]]);
    }
}
