//! **Pruning: only the definitions a program can reach.**
//!
//! A program is its own definitions and every one of its dependencies', so a
//! one-line program that prints a string arrives with the whole standard
//! library: a thousand definitions, of which it uses three. Lowering and code
//! generation are whole-program passes, and do all thousand.
//!
//! [`prune`] keeps the definitions the entry point reaches -- through the
//! variables their terms mention, transitively -- in the order they were in,
//! and drops the rest. What it keeps runs exactly as it did: a definition only
//! ever reaches another by naming it, and the tables that are not definitions
//! (constructors' fields, data types' variants) stay whole.
//!
//! [`Deps`] is the part worth keeping between programs that share their
//! dependencies, as every line of a REPL does: each definition's references,
//! found once, so that pruning the next program only walks the edges.

use crate::{Def, Program, Var, free_vars_into};
use std::collections::{HashMap, HashSet};

/// Each definition's references to the other definitions it was given with.
#[derive(Debug, Clone, Default)]
pub struct Deps {
    of: HashMap<Var, Vec<Var>>,
}

impl Deps {
    /// The references of `defs`, which may mention each other and anything
    /// added before.
    pub fn new(defs: &[Def]) -> Deps {
        let mut deps = Deps::default();
        deps.add(defs);
        deps
    }

    /// Learn `defs`' references too. A reference to a definition added later
    /// still counts: the edges are resolved when reaching, not here.
    pub fn add(&mut self, defs: &[Def]) {
        for d in defs {
            let mut free = HashSet::new();
            free_vars_into(&d.term, &mut free);
            let mut refs: Vec<Var> = free.into_iter().collect();
            refs.sort_unstable_by_key(|v| v.0);
            self.of.insert(d.var, refs);
        }
    }

    /// Every definition reachable from `roots`, roots included, among those
    /// added.
    pub fn reach(&self, roots: impl IntoIterator<Item = Var>) -> HashSet<Var> {
        reach_all(&[self], roots)
    }
}

/// [`Deps::reach`] across several: a program whose definitions were learnt in
/// parts -- a REPL's prefix, and the line just typed -- without merging them.
pub fn reach_all(layers: &[&Deps], roots: impl IntoIterator<Item = Var>) -> HashSet<Var> {
    let mut seen = HashSet::new();
    let mut work: Vec<Var> = roots.into_iter().collect();
    while let Some(v) = work.pop() {
        let Some(refs) = layers.iter().find_map(|l| l.of.get(&v)) else {
            // Not a definition: a local, or a name from elsewhere.
            continue;
        };
        if seen.insert(v) {
            work.extend(refs.iter().copied().filter(|r| !seen.contains(r)));
        }
    }
    seen
}

/// `program` with only the definitions its entry point reaches. Without an
/// entry point there is nothing to reach from, and it is returned whole.
pub fn prune(program: &Program) -> Program {
    match program.entry {
        Some(entry) => prune_from(program, [entry]),
        None => program.clone(),
    }
}

/// `program` with only the definitions `roots` reach.
///
/// What the roots are is what whoever runs the result can name: an
/// executable's entry point, and nothing else; a library's public definitions,
/// every one, since what a caller will reach is not known when it is compiled.
pub fn prune_from(program: &Program, roots: impl IntoIterator<Item = Var>) -> Program {
    // A `@migrate` function too, which nothing calls but a reload.
    let migrations = program.migrations.iter().map(|(v, _)| *v);
    let keep = Deps::new(&program.defs).reach(roots.into_iter().chain(migrations));
    Program {
        defs: program
            .defs
            .iter()
            .filter(|d| keep.contains(&d.var))
            .cloned()
            .collect(),
        entry: program.entry,
        ctor_fields: program.ctor_fields.clone(),
        variants: program.variants.clone(),
        origins: program.origins.clone(),
        migrations: program.migrations.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Lit, Poly, Term};
    use meadow_intern::InternedString;
    use std::sync::Arc;

    fn def(n: u32, term: Term) -> Def {
        Def {
            var: crate::hir::VarId(n),
            name: InternedString::from(format!("d{n}")),
            module: Default::default(),
            poly: Poly::mono(crate::unknown()),
            term,
        }
    }

    fn var(n: u32) -> Term {
        Term::Var(crate::hir::VarId(n))
    }

    #[test]
    fn only_what_the_entry_reaches_is_kept_in_order() {
        // d0 -> d2 -> d3; d1 is unused; d3 refers back to d0.
        let program = Program {
            defs: vec![
                def(
                    0,
                    Term::App(Arc::new(var(2)), Arc::new(Term::Lit(Lit::Unit))),
                ),
                def(1, Term::Lit(Lit::Unit)),
                def(2, var(3)),
                def(3, var(0)),
            ],
            entry: Some(crate::hir::VarId(2)),
            ..Program::default()
        };
        let pruned = prune(&program);
        let kept: Vec<u32> = pruned.defs.iter().map(|d| d.var.0).collect();
        assert_eq!(kept, vec![0, 2, 3]);
        assert_eq!(pruned.entry, program.entry);
    }

    #[test]
    fn a_local_that_shadows_nothing_is_not_a_definition() {
        let deps = Deps::new(&[def(
            0,
            Term::Lam(crate::hir::VarId(9), crate::unknown(), Arc::new(var(9))),
        )]);
        assert_eq!(deps.reach([crate::hir::VarId(0)]).len(), 1);
    }
}
