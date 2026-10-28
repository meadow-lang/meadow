//! **Versions of types and effects**: what lets a running program take a new
//! build of itself, with values of a type the new build changed still alive
//! (`docs/RUNTIME-MERGE.md`, "Hot loading").
//!
//! A value is made to its type's definition, and code that takes it apart
//! reads it by that definition. So when a build changes a type, the type is
//! a new one: its *version* is a hash of its definition -- its constructors,
//! their fields' types, and the versions of the types those mention, so that
//! `Scene`, holding a `List Shape`, is new when `Shape` is. A definition's
//! type is written with the versions of the types in it ([`signature`]), and a
//! definition is one with its old version only where its type is the same:
//! old code holding old values keeps calling the code made for them, new code
//! holding new values calls the new, and a value of a changed type never
//! reaches code written for the other.
//!
//! An effect has a version too. A handler is found by its operation's name,
//! not by a call whose type says anything, so an effect whose operations
//! carry a changed type would let an old handler take a new value. Its
//! version is the versions of the types its handlers and its `perform`s
//! carry: an effect that mentions a type mentions it at every handler and
//! every `perform` of it, whatever it is instantiated at, so when that type
//! changes, so does the effect, and an old handler is no longer what a new
//! `perform` finds. What an effect's parameters are instantiated at is in the
//! effect rows of the functions that perform it, which [`signature`] writes.
//!
//! Turned off -- a build that will never be loaded into a running program --
//! nothing here is worked out, and nothing is written with a version.

use crate::*;
use std::collections::BTreeSet;

/// Each type's version and each effect's, by name: what a build that may be
/// loaded into a running program writes its types with.
#[derive(Debug, Default, Clone)]
pub struct Versions {
    pub types: HashMap<InternedString, String>,
    pub effects: HashMap<InternedString, String>,
}

impl Versions {
    /// The versions of `p`'s types and effects.
    pub fn of(p: &Program) -> Versions {
        let types = type_versions(&p.variants);
        let effects = effect_versions(p, &types);
        Versions { types, effects }
    }
}

/// A 64-bit FNV-1a hash of `parts`, in hex: a version.
fn hash<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain([0]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// `t` written exactly: every type by its full name, a field over the
/// type's parameters as its number.
fn exact(t: &Ty, out: &mut String) {
    match t {
        InferType::Var(_) => out.push('?'),
        InferType::Bound(i) => {
            out.push('?');
            out.push_str(&i.to_string());
        }
        InferType::Con(n, args) => {
            out.push_str(n);
            if !args.is_empty() {
                out.push('(');
                for a in args {
                    exact(a, out);
                    out.push(',');
                }
                out.push(')');
            }
        }
        InferType::Fun(args, ret, eff) => {
            out.push_str("fn(");
            for a in args {
                exact(a, out);
                out.push(',');
            }
            out.push_str(")->");
            exact(ret, out);
            out.push('!');
            exact(eff, out);
        }
        InferType::Tuple(items) => {
            out.push('(');
            for a in items {
                exact(a, out);
                out.push(',');
            }
            out.push(')');
        }
        InferType::Record(row) => {
            out.push_str("rec");
            exact(row, out);
        }
        InferType::RowEmpty => out.push_str("{}"),
        InferType::RowExtend(label, field, rest) => {
            out.push('{');
            out.push_str(label);
            out.push(':');
            exact(field, out);
            out.push('|');
            exact(rest, out);
            out.push('}');
        }
        InferType::Error => out.push_str("!error"),
    }
}

/// The types `t` names.
fn named(t: &Ty, out: &mut BTreeSet<InternedString>) {
    match t {
        InferType::Con(n, args) => {
            out.insert(*n);
            args.iter().for_each(|a| named(a, out));
        }
        InferType::Fun(args, ret, eff) => {
            args.iter().for_each(|a| named(a, out));
            named(ret, out);
            named(eff, out);
        }
        InferType::Tuple(items) => items.iter().for_each(|a| named(a, out)),
        InferType::Record(row) => named(row, out),
        InferType::RowExtend(_, field, rest) => {
            named(field, out);
            named(rest, out);
        }
        InferType::Var(_) | InferType::Bound(_) | InferType::RowEmpty | InferType::Error => {}
    }
}

/// Each data or record type's version: a hash of its own definition and of
/// every one it reaches through its fields, so that a change to any of them
/// is a change to it -- recursion and all.
fn type_versions(variants: &meadow_infer::VariantEnv) -> HashMap<InternedString, String> {
    let mut defs: HashMap<InternedString, String> = HashMap::new();
    let mut mentions: HashMap<InternedString, BTreeSet<InternedString>> = HashMap::new();
    for (ty, vs) in variants {
        let ty = *ty;
        let mut def = String::new();
        let mut names = BTreeSet::new();
        for v in vs {
            def.push_str(&v.name);
            def.push('(');
            for f in &v.fields {
                exact(f, &mut def);
                def.push(',');
                named(f, &mut names);
            }
            def.push(')');
            for l in v.labels.iter().flatten() {
                def.push_str(l);
                def.push(' ');
            }
            def.push(';');
        }
        defs.insert(ty, def);
        mentions.insert(ty, names);
    }
    let mut out = HashMap::new();
    for ty in defs.keys() {
        // Everything it reaches, itself among them, in one order.
        let mut reach = BTreeSet::new();
        let mut todo = vec![*ty];
        while let Some(t) = todo.pop() {
            if defs.contains_key(&t) && reach.insert(t) {
                todo.extend(mentions[&t].iter().copied());
            }
        }
        let parts: Vec<&str> = reach
            .iter()
            .flat_map(|t| [&**t, defs[t].as_str()])
            .collect();
        out.insert(*ty, hash(parts));
    }
    out
}

/// Each effect's version: the versions of the types its handlers and its
/// `perform`s carry, or none, for an effect that carries none of them --
/// the world's, and most others.
fn effect_versions(
    p: &Program,
    types: &HashMap<InternedString, String>,
) -> HashMap<InternedString, String> {
    let mut carried: HashMap<InternedString, BTreeSet<InternedString>> = HashMap::new();
    let mut note = |effect: InternedString, t: &Ty| {
        let mut names = BTreeSet::new();
        named(t, &mut names);
        let set = carried.entry(effect).or_default();
        set.extend(names.into_iter().filter(|n| types.contains_key(n)));
    };
    for d in &p.defs {
        crate::rewrite::visit(&d.term, &mut |t| {
            match t {
                Term::Perform(effect, _, _, ty) => note(*effect, ty),
                Term::Handle { clauses, .. } => {
                    for c in clauses {
                        note(c.effect, &c.param_ty);
                        note(c.effect, &c.resume_ty);
                    }
                }
                _ => {}
            }
            true
        });
    }
    carried
        .into_iter()
        .filter(|(_, ts)| !ts.is_empty())
        .map(|(e, ts)| {
            let parts: Vec<&str> = ts.iter().flat_map(|t| [&**t, types[t].as_str()]).collect();
            (e, hash(parts))
        })
        .collect()
}
