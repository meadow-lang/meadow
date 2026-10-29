//! Compiling a package for an editor that is asking about one module of it.
//!
//! A package is one compilation unit: its modules are resolved and inferred
//! together, which is what lets them be mutually recursive. For an editor
//! that is the cost of the whole package on every change to one file, and
//! for a large package -- MeadowBoot, whose macros more than double what
//! inference reads -- that is tens of seconds a keystroke.
//!
//! So a compile can be *focused* on one module. Everything is still expanded
//! and resolved, which is cheap, and the binding groups are planned one at a
//! time, in the order inference meets them:
//!
//! - a group with a binding of the focused module is inferred;
//! - a group the focused module needs -- one its groups mention, however
//!   indirectly -- takes the types a previous compile gave it, if they still
//!   hold, and is inferred again if not;
//! - any other group takes whatever types were last found for it, held or
//!   not, or is left alone if there are none. Nothing inferred mentions it, so
//!   its types are only ever shown, never built on.
//!
//! Types still hold when nothing they were worked out from has changed: the
//! text of every module whose bindings the group mentions, its own included,
//! and everything the unit declares that is not a binding -- types, traits,
//! `impl`s, signatures, macro calls -- since a group's type can follow from a
//! constructor or a trait as well as from the bindings it calls. The
//! dependencies are not looked at here: whoever keeps a [`Memory`] clears it
//! when they change.
//!
//! Only the focused module comes out complete. The rest of the package has no
//! types in the table, no coverage checked and nothing lowered; its
//! diagnostics are not this compile's to give.

use crate::{
    ast, hir,
    infer::{Planned, Scheme, Type},
    intern::InternedString,
    scc::UnitGroup,
};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

/// What a compile for an editor is asked to answer, and what it may reuse.
pub struct Focus<'a> {
    /// The module whose bindings are inferred, by path; the others' are
    /// reused where they can be. `None`: every module, as an ordinary
    /// compile does -- which is also how `memory` is first filled.
    pub module: Option<Vec<InternedString>>,
    /// What earlier compiles found, and what this one adds to.
    pub memory: &'a mut Memory,
}

/// A module, by its path in the package.
type ModPath = Vec<InternedString>;

/// A top-level binding by where it is, what it is called, and which of the
/// module's bindings of that name it is: its `VarId` is minted afresh by
/// every compile, and moves whenever anything before it does.
type Name = (ModPath, InternedString, u32);

/// The types earlier compiles of one package found, by binding group.
///
/// Kept by the caller between compiles, and cleared by the caller when what
/// the package depends on changes.
#[derive(Default)]
pub struct Memory {
    groups: HashMap<Vec<Name>, Kept>,
    last: Tally,
}

/// What the last compile did with the unit's binding groups.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub inferred: usize,
    /// Given the types an earlier compile found for them.
    pub reused: usize,
    /// Left alone: not needed, and nothing kept for them.
    pub skipped: usize,
}

impl Memory {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn clear(&mut self) {
        self.groups.clear();
    }

    /// What the last compile that used this memory did.
    pub fn last(&self) -> Tally {
        self.last
    }
}

/// One group's types, and what they were worked out from.
struct Kept {
    /// Every module whose bindings went into them -- the group's own and
    /// those of every group it mentions, transitively -- with the hash of its
    /// text then.
    inputs: Vec<(ModPath, u64)>,
    /// [`Snapshot::declared`] then.
    declared: u64,
    schemes: Vec<(Name, Scheme)>,
}

/// What the unit's text was, taken before macros are expanded: a macro's
/// output is decided by its input, and the input is written here.
pub(crate) struct Snapshot {
    texts: HashMap<ModPath, u64>,
    /// Everything at the top of every module that is not a binding, as
    /// written. A change to a type or a trait can change the type of a
    /// binding that never calls anything that changed.
    declared: u64,
}

/// Take the snapshot of `modules`, as they stand once `@cfg` has been
/// decided and inline modules lifted out.
pub(crate) fn snapshot(modules: &[crate::AstModule]) -> Snapshot {
    let mut texts = HashMap::new();
    let mut declared = DefaultHasher::new();
    for m in modules {
        let text: &str = &m.source.content;
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        texts.insert(m.path.clone(), h.finish());
        m.path.hash(&mut declared);
        for d in &m.ast.value().decls {
            let base = match d.value() {
                ast::Decl::Attributed(_, inner) => inner.value(),
                other => other,
            };
            if matches!(base, ast::Decl::Bind(_)) {
                continue;
            }
            let (start, end) = (d.span.start as usize, d.span.end as usize);
            text.get(start..end).unwrap_or("").hash(&mut declared);
        }
    }
    Snapshot {
        texts,
        declared: declared.finish(),
    }
}

/// The plan for one compile's groups, and what it needs to remember what
/// came of them afterwards.
pub(crate) struct Plan {
    planned: Vec<Option<Planned>>,
    /// Each group's members by name, or `None` for a group that cannot be
    /// remembered: one with a name it shares with another member.
    names: Vec<Option<Vec<Name>>>,
    /// Each group's members' ids, in the order of `names`.
    vars: Vec<Vec<hir::VarId>>,
    /// The modules each group's types were worked out from, by index.
    inputs: Vec<Vec<usize>>,
    paths: Vec<ModPath>,
    /// How many groups [`Plan::take`] has handed out types for.
    reused: usize,
}

impl Plan {
    /// Decide what to do with each of `groups`.
    pub(crate) fn new(
        focus: &Focus<'_>,
        snapshot: &Snapshot,
        modules: &[&crate::TypedModule],
        groups: &[UnitGroup],
        names: &HashMap<hir::VarId, InternedString>,
    ) -> Plan {
        let paths: Vec<ModPath> = modules.iter().map(|m| m.path.clone()).collect();
        // Which of its module's bindings of its name each binding is, counted
        // in the order the module declares them: the same while its text is.
        let mut nth: HashMap<hir::VarId, u32> = HashMap::new();
        for m in modules {
            let mut seen: HashMap<InternedString, u32> = HashMap::new();
            for d in &m.hir.value().decls {
                if let hir::Decl::Bind(b) = d.value() {
                    for v in b.bound_vars() {
                        let name = names.get(&v).copied().unwrap_or_default();
                        let n = seen.entry(name).or_default();
                        nth.insert(v, *n);
                        *n += 1;
                    }
                }
            }
        }
        let mut group_names = Vec::with_capacity(groups.len());
        let mut group_vars: Vec<Vec<hir::VarId>> = Vec::with_capacity(groups.len());
        for g in groups {
            let mut members: Vec<(Name, hir::VarId)> = Vec::new();
            for &(m, i) in &g.members {
                if let hir::Decl::Bind(b) = modules[m].hir.value().decls[i].value() {
                    for v in b.bound_vars() {
                        let name = names.get(&v).copied().unwrap_or_default();
                        let n = nth.get(&v).copied().unwrap_or_default();
                        members.push(((paths[m].clone(), name, n), v));
                    }
                }
            }
            members.sort_by(|a, b| a.0.cmp(&b.0));
            let unique = members.windows(2).all(|w| w[0].0 != w[1].0);
            group_vars.push(members.iter().map(|(_, v)| *v).collect());
            group_names.push(unique.then(|| members.into_iter().map(|(n, _)| n).collect()));
        }

        // What each group was worked out from: earlier groups first, so one
        // pass is enough.
        let mut inputs: Vec<Vec<usize>> = Vec::with_capacity(groups.len());
        for g in groups {
            let mut mine: Vec<usize> = g.members.iter().map(|&(m, _)| m).collect();
            for &u in &g.uses {
                mine.extend_from_slice(&inputs[u]);
            }
            mine.sort_unstable();
            mine.dedup();
            inputs.push(mine);
        }

        let planned = match &focus.module {
            None => groups.iter().map(|_| Some(Planned::Infer)).collect(),
            Some(module) => {
                let here = |g: &UnitGroup| g.members.iter().any(|&(m, _)| paths[m] == *module);
                // Needed: the focused module's groups and everything they
                // mention. Later groups mention earlier ones, so from the end.
                let mut needed: Vec<bool> = groups.iter().map(here).collect();
                for g in (0..groups.len()).rev() {
                    if needed[g] {
                        for &u in &groups[g].uses {
                            needed[u] = true;
                        }
                    }
                }
                (0..groups.len())
                    .map(|g| {
                        if here(&groups[g]) {
                            return Some(Planned::Infer);
                        }
                        let kept = group_names[g]
                            .as_ref()
                            .and_then(|key| focus.memory.groups.get(key));
                        let holds = kept.is_some_and(|k| {
                            k.declared == snapshot.declared
                                && k.inputs
                                    .iter()
                                    .all(|(p, h)| snapshot.texts.get(p) == Some(h))
                        });
                        match kept.and_then(|k| known(k, &group_names[g], &group_vars[g])) {
                            Some(schemes) if holds || !needed[g] => Some(Planned::Known(schemes)),
                            _ if needed[g] => Some(Planned::Infer),
                            _ => Some(Planned::Skip),
                        }
                    })
                    .collect()
            }
        };
        Plan {
            planned,
            names: group_names,
            vars: group_vars,
            inputs,
            paths,
            reused: 0,
        }
    }

    /// What to do with group `g`; asked once for each.
    pub(crate) fn take(&mut self, g: usize) -> Planned {
        let p = self.planned[g].take().unwrap_or(Planned::Skip);
        if matches!(p, Planned::Known(_)) {
            self.reused += 1;
        }
        p
    }

    /// Keep what this compile inferred -- `inferred[g]` for each group --
    /// for the next one.
    pub(crate) fn remember(
        &self,
        memory: &mut Memory,
        snapshot: &Snapshot,
        inferred: &[bool],
        schemes: &HashMap<hir::VarId, Scheme>,
    ) {
        memory.last = Tally {
            inferred: inferred.iter().filter(|&&i| i).count(),
            reused: self.reused,
            skipped: inferred.len() - inferred.iter().filter(|&&i| i).count() - self.reused,
        };
        for g in 0..self.names.len() {
            if !inferred[g] {
                continue;
            }
            let Some(key) = &self.names[g] else {
                continue;
            };
            let found: Option<Vec<(Name, Scheme)>> = key
                .iter()
                .zip(&self.vars[g])
                .map(|(name, v)| {
                    schemes
                        .get(v)
                        .filter(|s| closed(s))
                        .map(|s| (name.clone(), s.clone()))
                })
                .collect();
            let Some(found) = found else {
                memory.groups.remove(key);
                continue;
            };
            let inputs = self.inputs[g]
                .iter()
                .filter_map(|&m| {
                    let p = &self.paths[m];
                    snapshot.texts.get(p).map(|h| (p.clone(), *h))
                })
                .collect();
            memory.groups.insert(
                key.clone(),
                Kept {
                    inputs,
                    declared: snapshot.declared,
                    schemes: found,
                },
            );
        }
    }
}

/// The schemes `kept` has for a group whose members are `names`, as this
/// compile numbers them.
fn known(
    kept: &Kept,
    names: &Option<Vec<Name>>,
    vars: &[hir::VarId],
) -> Option<Vec<(hir::VarId, Scheme)>> {
    let names = names.as_ref()?;
    if kept.schemes.len() != names.len() {
        return None;
    }
    names
        .iter()
        .zip(vars)
        .map(|(name, v)| {
            kept.schemes
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, s)| (*v, s.clone()))
        })
        .collect()
}

/// Whether `s` stands on its own: a binding that did not generalize can be
/// left holding a variable of the run that inferred it, which means nothing
/// in another.
fn closed(s: &Scheme) -> bool {
    fn no_vars(t: &Type) -> bool {
        match t {
            Type::Var(_) => false,
            Type::Bound(_) | Type::RowEmpty | Type::Error => true,
            Type::Con(_, args) | Type::Tuple(args) => args.iter().all(no_vars),
            Type::Fun(ps, r, e) => ps.iter().all(no_vars) && no_vars(r) && no_vars(e),
            Type::Record(r) => no_vars(r),
            Type::RowExtend(_, f, rest) => no_vars(f) && no_vars(rest),
        }
    }
    no_vars(&s.ty)
        && s.preds
            .iter()
            .all(|p| p.tys.iter().chain(&p.assocs).all(no_vars))
}
