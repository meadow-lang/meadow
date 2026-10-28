//! **Dictionaries, specialized away.**
//!
//! A trait is compiled to a dictionary and a `where` to a parameter that takes
//! one (`meadow_infer::traits`), which is what lets a function over `Show a` be
//! compiled once, in its own package, before anyone has said what `a` is. It
//! is also slow where it does not need to be: `show 3` builds nothing and
//! decides nothing at run time, and yet it is a call to a function that takes
//! a record apart to find another function to call.
//!
//! So where a function that takes dictionaries is called with dictionaries
//! that are **known** -- an `impl`'s, which is the case wherever the types are
//! -- this pass makes a copy of the function *for those dictionaries and those
//! types*, and calls the copy instead. In the copy the dictionary is a name for
//! a constructor in plain sight, so selecting a method from it is selecting a
//! field of a known record: `show` at `Int` is `impl Show Int`'s `show`, a
//! direct call, which the inliner may then take away entirely. A copy mentions
//! other functions at dictionaries that are now known too, which asks for more
//! copies, until nothing new is asked for.
//!
//! What is left taking a dictionary is what has to: a function called at a type
//! its caller does not know either -- in which case the caller takes the
//! dictionary too, and *its* known callers are where the copying starts.
//!
//! # What is a known dictionary
//!
//! A top-level definition with no type parameters whose body is a dictionary
//! constructor: `impl Show Int`'s. An `impl` with a `where` of its own --
//! `impl Show [a;] where Show a` -- is a function from dictionaries to a
//! dictionary, so `Show [Int;]`'s is that function's copy at `Show Int`'s,
//! made here like any other, and then known in its turn. A required trait's
//! dictionary is a field of the requiring one's, and known when that is.
//!
//! # In the pipeline
//!
//! `meadow_seq::lower_program` runs this first, while core still has the exact
//! types a copy is made at, from [`OptLevel::O1`]: every engine that runs
//! bytecode -- the interpreter, the JIT and an executable -- runs the copies.
//! The CEK machine does not, which is what the differential tests compare
//! them against. A release build ([`OptLevel::O2`]) copies without limit but
//! one on the depth of a chain of copies, which only polymorphic recursion
//! reaches; a debug build stops sooner, and what it leaves alone is still
//! right, only slower.
//!
//! The generic original stays, as it does under [`crate::specialize`]: it is
//! what a caller that does not know its types still calls.
//!
//! # Primitives, back again
//!
//! `Std.Ops` defines the operators as methods -- `impl Add Int { fun (+) a b =
//! _primAdd a b }` -- so a method selected from a known dictionary is, for an
//! operator, a function whose whole body is a primitive of its parameters. A
//! call of one is that primitive of its arguments, rewritten here, so that `x +
//! y` at `Int` is the same `AddI` it was before `+` was a method. And an `impl`
//! for every type, `impl Eq a`, is a dictionary with type parameters but no
//! dictionary parameters: at known types it is copied like a function is, and
//! the copy is known.

use crate::inline::{Body, Fresh, freshen_mapped, retype};
use crate::*;
use std::collections::{HashMap, HashSet};

/// Where the names of copies, and of what they bind, start: clear of every
/// unit and of the test runner's definitions, and below [`crate::specialize`]'s.
pub const DICTIONARIES_BASE: u32 = 0x7400_0000;

/// How long a chain of copies -- a copy asking for a copy asking for a copy --
/// may get. Only polymorphic recursion under a `where` makes one that does not
/// end, by asking for itself at an ever larger type.
const DEPTH: usize = 24;

/// Copies a debug build makes of one function before leaving its other calls
/// generic. A release build has no such limit.
const DEBUG_COPIES: usize = 16;

/// Specialize `p` on the dictionaries its calls are known to pass.
pub fn program(p: &Program, opt: OptLevel) -> Program {
    // `MEADOW_KEEP_DICTIONARIES=1` leaves every call as it was written, for
    // telling a fault in this pass from one anywhere else -- and for measuring
    // what it is worth.
    if opt == OptLevel::O0 || std::env::var_os("MEADOW_KEEP_DICTIONARIES").is_some() {
        return p.clone();
    }
    let traits: HashSet<InternedString> = p
        .variants
        .iter()
        .filter(|(_, vs)| vs.len() == 1 && vs[0].name.ends_with(".#dict"))
        .map(|(name, _)| *name)
        .collect();
    if traits.is_empty() {
        return p.clone();
    }
    let mut s = Spec {
        traits,
        functions: HashMap::new(),
        known: HashMap::new(),
        generic: HashMap::new(),
        wrappers: HashMap::new(),
        labels: p.ctor_fields.clone(),
        copies: HashMap::new(),
        open_instances: HashMap::new(),
        per_function: HashMap::new(),
        made: Vec::new(),
        origins: HashMap::new(),
        fresh: Fresh(DICTIONARIES_BASE.max(simplify::max_var(p) + 1)),
        limit: (opt != OptLevel::O2).then_some(DEBUG_COPIES),
        depth: 0,
    };
    for d in &p.defs {
        if let Some(f) = s.function(d) {
            s.functions.insert(d.var, f);
        } else if let Some(found) = s.dictionary(&d.term) {
            if d.poly.binders.is_empty() {
                s.known.insert(d.var, found);
            }
        } else if let Term::TyLam(_, inner) = d.term.peel()
            && s.dictionary(inner).is_some()
        {
            s.generic.insert(d.var, d.clone());
        }
        if let Some(w) = wrapper(d) {
            s.wrappers.insert(d.var, w);
        }
    }
    if s.functions.is_empty() {
        return p.clone();
    }
    let mut defs: Vec<Def> = p
        .defs
        .iter()
        .map(|d| Def {
            var: d.var,
            name: d.name,
            module: d.module,
            poly: d.poly.clone(),
            term: s.term(&d.term),
        })
        .collect();
    defs.append(&mut s.made);
    let mut origins = p.origins.clone();
    origins.extend(s.origins);
    Program {
        defs,
        entry: p.entry,
        ctor_fields: p.ctor_fields.clone(),
        variants: p.variants.clone(),
        origins,
        migrations: p.migrations.clone(),
    }
}

/// A definition that takes dictionaries before anything else.
struct Function {
    name: InternedString,
    module: InternedString,
    poly: Poly,
    /// Its type binders, its dictionary parameters, and the rest of it.
    body: Body,
    /// Which of its dictionary parameters its body *is* a field of, and which
    /// field: a trait's method is nothing else, and a call to one at a known
    /// dictionary is the field itself rather than a copy that selects it.
    selects: Option<(usize, InternedString)>,
}

struct Spec {
    /// The names of the dictionary types: the traits.
    traits: HashSet<InternedString>,
    functions: HashMap<Var, Function>,
    /// The known dictionaries: each one's constructor, and its fields as they
    /// were written.
    known: HashMap<Var, (InternedString, Vec<Term>)>,
    /// The dictionaries with type parameters and nothing else: `impl Eq a`'s.
    generic: HashMap<Var, Def>,
    /// The definitions that are a primitive of their parameters, and nothing
    /// else: the operators' methods.
    wrappers: HashMap<Var, Body>,
    /// A dictionary constructor's fields' labels, in order.
    labels: HashMap<InternedString, Vec<InternedString>>,
    /// The copy made of a function at some types and dictionaries.
    copies: HashMap<(Var, String), Var>,
    /// The copies of a parameterized `impl` that are still generic: at
    /// their caller's own type variables, which each is over.
    open_instances: HashMap<Var, Vec<TyVar>>,
    per_function: HashMap<Var, usize>,
    made: Vec<Def>,
    origins: HashMap<Var, Var>,
    fresh: Fresh,
    limit: Option<usize>,
    depth: usize,
}

/// A call's head and its arguments, innermost first, through any positions.
fn spine(t: &Term) -> (&Term, Vec<&Arc<Term>>) {
    let mut args = Vec::new();
    let mut cur = t.peel();
    while let Term::App(f, a) = cur {
        args.push(a);
        cur = f.peel();
    }
    args.reverse();
    (cur, args)
}

/// `d`, if it is an operator's primitive of its parameters, in order, and
/// nothing else: `fun (+) a b = _primAdd a b`, which is `\a b -> Prim(Add, [a,
/// b])`. Only an operator's: another primitive -- `spawn`'s, say -- may be one
/// that later passes expect to find where lowering put it.
fn wrapper(d: &Def) -> Option<Body> {
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
    let Term::Prim(p, args, _) = t else {
        return None;
    };
    if !p.is_operator() && !matches!(p, Prim::Display | Prim::Show) {
        return None;
    }
    let exact = !params.is_empty()
        && args.len() == params.len()
        && args
            .iter()
            .zip(&params)
            .all(|(a, (p, _))| matches!(a.peel(), Term::Var(v) if v == p));
    exact.then(|| Body {
        binders,
        params,
        term: t.clone(),
        original: None,
    })
}

/// Does a type mention a variable? A copy is made at types that do not: its
/// caller's own type parameters are not known until *it* is copied.
///
/// Effects do not count. Nothing at run time depends on one, an effect
/// variable nothing ever pinned down is the ordinary case, and a copy is as
/// good at it as at any row it might have been.
fn open(ty: &Ty) -> bool {
    match ty {
        InferType::Var(_) | InferType::Bound(_) => true,
        InferType::RowEmpty | InferType::Error => false,
        InferType::Con(_, args) | InferType::Tuple(args) => args.iter().any(open),
        InferType::Fun(params, ret, _) => params.iter().any(open) || open(ret),
        InferType::Record(row) => open(row),
        InferType::RowExtend(_, field, rest) => open(field) || open(rest),
    }
}

impl Spec {
    /// A copy of `body` under names of its own, each noted as a copy of the
    /// variable it was -- or of what that was a copy of -- so that a debugger
    /// shows a copy's locals by the names they were written with.
    fn freshen(&mut self, body: &Body) -> Body {
        let (copy, map) = freshen_mapped(body, &mut self.fresh);
        for (old, new) in map {
            let root = self.origins.get(&old).copied().unwrap_or(old);
            self.origins.insert(new, root);
        }
        copy
    }

    fn is_dictionary_type(&self, ty: &Ty) -> bool {
        matches!(ty, InferType::Con(name, _) if self.traits.contains(name))
    }

    /// `d` as a function of dictionaries, if it is one.
    fn function(&self, d: &Def) -> Option<Function> {
        let mut t = d.term.peel();
        let mut binders = Vec::new();
        if let Term::TyLam(bs, inner) = t {
            binders = bs.clone();
            t = inner.peel();
        }
        let mut params = Vec::new();
        while let Term::Lam(v, ty, inner) = t {
            if !self.is_dictionary_type(ty) {
                break;
            }
            params.push((*v, ty.clone()));
            t = inner.peel();
        }
        if params.is_empty() {
            return None;
        }
        let selects = match t {
            Term::Sel(of, label, _) => match of.peel() {
                Term::Var(v) => params.iter().position(|(p, _)| p == v).map(|i| (i, *label)),
                _ => None,
            },
            _ => None,
        };
        Some(Function {
            name: d.name,
            module: d.module,
            poly: d.poly.clone(),
            body: Body {
                binders,
                params,
                term: t.clone(),
                original: None,
            },
            selects,
        })
    }

    /// The dictionary `t` constructs, if that is what it does.
    fn dictionary(&self, t: &Term) -> Option<(InternedString, Vec<Term>)> {
        match t.peel() {
            Term::Ctor(name, _, fields) if name.ends_with(".#dict") => {
                Some((*name, fields.clone()))
            }
            _ => None,
        }
    }

    /// The known dictionary `t` names: a known one, or the copy of an `impl`
    /// for every type at the types it is applied to.
    fn known_dictionary(&mut self, t: &Term) -> Option<Var> {
        match t.peel() {
            Term::Var(v) if self.known.contains_key(v) => Some(*v),
            Term::TyApp(g, tys) => match g.peel() {
                // A copy that is generic in its caller's variables, named at
                // them: as it is written wherever it is given.
                Term::Var(v) if self.open_instances.contains_key(v) => Some(*v),
                Term::Var(v) if self.generic.contains_key(v) => {
                    if !tys.iter().any(open) {
                        return self.instance(*v, tys, &[]);
                    }
                    // At the caller's own type variables -- `impl Rows
                    // (Builder s)` inside a function generic in `s`, which
                    // is every function run under a `runSt`. A release build
                    // copies at them, generic in them, as it copies a
                    // function ([`Spec::copy`]); a debug build waits.
                    if self.limit.is_some() {
                        return None;
                    }
                    let mut vars = Vec::new();
                    if !tys.iter().all(|t| type_vars(t, &mut vars)) {
                        return None;
                    }
                    self.instance(*v, tys, &vars)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// The copy of the generic dictionary `k` at `tys`, made if it has not
    /// been, and known from then on.
    fn instance(&mut self, k: Var, tys: &[Ty], vars: &[u32]) -> Option<Var> {
        let key = (k, format!("{tys:?}"));
        if let Some(v) = self.copies.get(&key) {
            return Some(*v);
        }
        if self.depth >= DEPTH {
            return None;
        }
        let d = self.generic.get(&k)?.clone();
        let Term::TyLam(binders, inner) = d.term.peel() else {
            return None;
        };
        if binders.len() != tys.len() {
            return None;
        }
        let at = retype(
            &Body {
                binders: binders.clone(),
                params: Vec::new(),
                term: (**inner).clone(),
                original: None,
            },
            tys,
        );
        let copy = self.freshen(&at);
        let found = self.dictionary(&copy.term)?;
        let map: HashMap<u32, Ty> = d
            .poly
            .binders
            .iter()
            .map(|b| b.id)
            .zip(tys.iter().cloned())
            .collect();
        let ty = subst_rigid(&d.poly.ty, &map);
        let var = self.fresh.var();
        self.copies.insert(key, var);
        self.origins.insert(var, k);
        self.known.insert(var, found);
        self.depth += 1;
        let term = self.term(&copy.term);
        self.depth -= 1;
        // Generic in the caller's variables it is at, if it is at any.
        let binders: Vec<TyVar> = vars
            .iter()
            .map(|v| TyVar {
                id: *v,
                kind: VarKind::Type,
            })
            .collect();
        let (poly, term) = if binders.is_empty() {
            (Poly::mono(ty), term)
        } else {
            self.open_instances.insert(var, binders.clone());
            (
                Poly {
                    binders: binders.clone(),
                    ty,
                },
                Term::TyLam(binders, Arc::new(term)),
            )
        };
        self.made.push(Def {
            var,
            name: d.name,
            module: d.module,
            poly,
            term,
        });
        Some(var)
    }

    /// A call of a primitive's wrapper with all its arguments, as the
    /// primitive of them.
    fn primitive(&self, t: &Term) -> Option<Term> {
        let (head, args) = spine(t);
        let (w, tys): (Var, &[Ty]) = match head {
            Term::Var(v) => (*v, &[]),
            Term::TyApp(g, tys) => match g.peel() {
                Term::Var(v) => (*v, tys.as_slice()),
                _ => return None,
            },
            _ => return None,
        };
        let body = self.wrappers.get(&w)?;
        if args.len() != body.params.len() || tys.len() != body.binders.len() {
            return None;
        }
        let Term::Prim(p, _, ty) = retype(body, tys).term else {
            return None;
        };
        Some(Term::Prim(
            p,
            args.iter().map(|a| (***a).clone()).collect(),
            ty,
        ))
    }

    /// Field `label` of the known dictionary `k`, ready to stand where the
    /// selection stood: a copy of its own, with what it mentions specialized.
    fn field(&mut self, k: Var, label: InternedString) -> Option<Term> {
        let (ctor, fields) = self.known.get(&k)?;
        let at = self.labels.get(ctor)?.iter().position(|l| *l == label)?;
        let field = fields.get(at)?.clone();
        if self.depth >= DEPTH {
            return None;
        }
        let copy = self.freshen(&Body {
            binders: Vec::new(),
            params: Vec::new(),
            term: field,
            original: None,
        });
        self.depth += 1;
        let term = self.term(&copy.term);
        self.depth -= 1;
        Some(term)
    }

    /// `t`, inside out: an argument that builds a dictionary has been made a
    /// name for one by the time the call it is an argument of is looked at.
    fn term(&mut self, t: &Term) -> Term {
        rewrite::term(t, &mut |node| self.here(node), &mut |p| p)
    }

    fn here(&mut self, t: Term) -> Term {
        match t.peel() {
            Term::Sel(of, label, _) => {
                if let Some(k) = self.known_dictionary(of)
                    && let Some(field) = self.field(k, *label)
                {
                    return field;
                }
                t
            }
            Term::App(..) => {
                let t = self.call(&t).unwrap_or(t);
                self.primitive(&t).unwrap_or(t)
            }
            _ => t,
        }
    }

    /// A call of a function of dictionaries at known ones, as a call of its
    /// copy for them.
    fn call(&mut self, t: &Term) -> Option<Term> {
        let (head, args) = spine(t);
        let (f, tys): (Var, Vec<Ty>) = match head {
            Term::Var(v) => (*v, Vec::new()),
            Term::TyApp(g, tys) => match g.peel() {
                Term::Var(v) => (*v, tys.clone()),
                _ => return None,
            },
            _ => return None,
        };
        let (count, kinds, selects) = {
            let function = self.functions.get(&f)?;
            let kinds: Vec<VarKind> = function.body.binders.iter().map(|b| b.kind).collect();
            (function.body.params.len(), kinds, function.selects)
        };
        // The caller's own type variables among the types: a release build
        // copies at them too -- see [`Spec::copy`] -- and a debug build waits
        // for the caller to be copied. Each keeps the kind it stands in for: a
        // record's row, or a number's, is not any type.
        let mut vars = Vec::new();
        let mut var_kinds: HashMap<u32, VarKind> = HashMap::new();
        for (t, k) in tys.iter().zip(&kinds) {
            if matches!(k, VarKind::Effect) {
                continue;
            }
            if let InferType::Var(v) = t {
                var_kinds.entry(*v).or_insert(*k);
            }
            if open(t) && !type_vars(t, &mut vars) {
                return None;
            }
        }
        let vars: Vec<(u32, VarKind)> = vars
            .into_iter()
            .map(|v| (v, var_kinds.get(&v).copied().unwrap_or(VarKind::Type)))
            .collect();
        if args.len() < count
            || tys.len() != kinds.len()
            || (self.limit.is_some() && !vars.is_empty())
        {
            return None;
        }
        let mut dicts: Vec<Var> = Vec::with_capacity(count);
        for a in &args[..count] {
            dicts.push(self.known_dictionary(a)?);
        }
        let head = match selects {
            Some((i, label)) => self.field(dicts[i], label)?,
            None => {
                let copy = Term::Var(self.copy(f, &tys, &dicts, &vars)?);
                if vars.is_empty() {
                    copy
                } else {
                    Term::TyApp(
                        Arc::new(copy),
                        vars.iter().map(|(v, _)| InferType::Var(*v)).collect(),
                    )
                }
            }
        };
        Some(
            args[count..]
                .iter()
                .fold(head, |f, a| Term::App(Arc::new(f), (*a).clone())),
        )
    }

    /// The copy of `f` at `tys` and `dicts`, made if it has not been.
    ///
    /// `vars` are the caller's own type variables among `tys`, each with its
    /// kind, and the copy is generic in them: its dictionaries are what is
    /// known, and the types it is at are whatever the caller's turn out to be. A function called
    /// inside a `runSt` -- whose state type no caller ever knows, because
    /// `runSt` asks for a body that works at every one -- is where it
    /// matters: `==` at a known type, through a helper generic in the state,
    /// was a dictionary looked up at run time at every call, never copied at
    /// all. The representation passes after this copy the copy in their turn.
    fn copy(&mut self, f: Var, tys: &[Ty], dicts: &[Var], vars: &[(u32, VarKind)]) -> Option<Var> {
        let key = (f, format!("{tys:?}{dicts:?}"));
        if let Some(v) = self.copies.get(&key) {
            return Some(*v);
        }
        let made = self.per_function.entry(f).or_default();
        if self.depth >= DEPTH || self.limit.is_some_and(|l| *made >= l) {
            return None;
        }
        *made += 1;
        let var = self.fresh.var();
        self.copies.insert(key, var);
        self.origins.insert(var, f);

        let function = &self.functions[&f];
        let (name, module) = (function.name, function.module);
        // An effect argument may be the caller's own row variable, which the
        // copy -- a definition of its own -- does not bind. Effects are erased
        // below core and not checked in it, so the copy takes such a row as
        // empty, and is closed.
        //
        // A record's row is not: what else a record holds decides how it is
        // laid out, so a copy that took `{ items | r }` as `{ items }` read
        // the record it was given as one of a single field -- a vector where
        // there was a record, and a match on what it held that fitted no arm.
        // The copy is generic in the caller's row instead, as in any other of
        // its type variables.
        let tys: Vec<Ty> = tys
            .iter()
            .zip(&function.body.binders)
            .map(|(t, b)| match b.kind {
                VarKind::Effect if open(t) => InferType::RowEmpty,
                _ => t.clone(),
            })
            .collect();
        let tys = &tys[..];
        let at = retype(&function.body, tys);
        // The type it has left: its own, at these types, past the dictionaries.
        let map: HashMap<u32, Ty> = function
            .poly
            .binders
            .iter()
            .map(|b| b.id)
            .zip(tys.iter().cloned())
            .collect();
        let mut ty = subst_rigid(&function.poly.ty, &map);
        for _ in 0..dicts.len() {
            ty = match ty {
                InferType::Fun(_, ret, _) => *ret,
                other => other,
            };
        }
        let copy = self.freshen(&at);
        // Each dictionary parameter is the known dictionary it was given.
        let given: HashMap<Var, Var> = copy
            .params
            .iter()
            .map(|(v, _)| *v)
            .zip(dicts.iter().copied())
            .collect();
        let body = rewrite::term(
            &copy.term,
            &mut |t| match t {
                // A dictionary that is generic in the caller's variables is
                // named at them.
                Term::Var(v) => match given.get(&v) {
                    Some(d) => match self.open_instances.get(d) {
                        Some(binders) => Term::TyApp(
                            Arc::new(Term::Var(*d)),
                            binders.iter().map(|b| InferType::Var(b.id)).collect(),
                        ),
                        None => Term::Var(*d),
                    },
                    None => Term::Var(v),
                },
                t => t,
            },
            &mut |p| p,
        );

        // A copy that is itself a dictionary is known from here on, before
        // its fields are looked into: a default method mentions the very
        // dictionary it is a field of.
        if let Some(found) = self.dictionary(&body) {
            self.known.insert(var, found);
        }
        self.depth += 1;
        let term = self.term(&body);
        self.depth -= 1;
        let binders: Vec<TyVar> = vars
            .iter()
            .map(|(v, kind)| TyVar {
                id: *v,
                kind: *kind,
            })
            .collect();
        let (poly, term) = if binders.is_empty() {
            (Poly::mono(ty), term)
        } else {
            (
                Poly {
                    binders: binders.clone(),
                    ty,
                },
                Term::TyLam(binders, Arc::new(term)),
            )
        };
        self.made.push(Def {
            var,
            name,
            module,
            poly,
            term,
        });
        Some(var)
    }
}

/// The type variables `t` mentions, into `out` once each -- or `false`, if it
/// mentions one a copy could not simply be generic in: a row's, or a bound
/// one.
fn type_vars(t: &Ty, out: &mut Vec<u32>) -> bool {
    match t {
        InferType::Var(v) => {
            if !out.contains(v) {
                out.push(*v);
            }
            true
        }
        InferType::Bound(_) => false,
        InferType::RowEmpty | InferType::Error => true,
        InferType::Con(_, args) | InferType::Tuple(args) => args.iter().all(|a| type_vars(a, out)),
        InferType::Fun(params, ret, _) => {
            params.iter().all(|p| type_vars(p, out)) && type_vars(ret, out)
        }
        InferType::Record(_) | InferType::RowExtend(..) => !open(t),
    }
}
