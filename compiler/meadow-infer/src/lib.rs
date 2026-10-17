//! Hindley–Milner type inference, **Algorithm J**.
//!
//! * Meta variables live in an [`Arena`] of union-find slots. `unify` links slots
//!   with path-compressing [`Arena::prune`]; there is no substitution map.
//! * Generalization is **ranked** (a la OCaml): every unbound meta var carries the
//!   `level` it was created at; entering the rhs of a `let`/`fun` bumps the level,
//!   and on the way out any var still deeper than the current level is quantified.
//!   This replaces the "occurs in the environment" scan.
//! * Records use **row polymorphism**: a record type wraps a *row*, which is a
//!   chain of `RowExtend(label, field, rest)` ending in `RowEmpty` (closed) or an
//!   unbound row meta var (open). `unify` rewrites rows to line up labels.
//!
//! Inference never bails: type errors are collected as [`Diagnostic`]s and a fresh
//! var is used to keep going, so one program yields all its errors and a maximally
//! annotated tree.
//!
//! Output is a [`TypeTable`] mapping every [`hir::NodeId`] to its (zonked) [`Type`],
//! plus a [`Scheme`] per top-level binding.

use meadow_diagnostics::Diagnostic;
use meadow_hir::{self as hir, NodeId, PRIMS, VarId};
use meadow_intern::InternedString;
use meadow_span::Span;
use std::collections::HashMap;
use std::fmt;

// ===========================================================================
// Types
// ===========================================================================

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Type {
    /// Meta variable: an index into [`Arena::slots`].
    Var(u32),
    /// Quantified variable: only ever appears inside a [`Scheme`]; `u32` indexes
    /// [`Scheme::quant`]. Keeping schemes in terms of `Bound` (not arena indices)
    /// makes them independent of the arena that produced them, so a dependency
    /// package's schemes can be dropped straight into a fresh inference run.
    Bound(u32),
    /// Type constructor applied to arguments: `Int`, `Bool`, `String`, `Unit`,
    /// `List a`, …
    Con(InternedString, Vec<Type>),
    /// Function type `arg -> ret ! effect`. Currying keeps the arg list length 1;
    /// the third component is the arrow's **latent effect** — a *row* (`RowEmpty`
    /// for a pure arrow, `RowExtend` / a row var otherwise). See the module docs.
    Fun(Vec<Type>, Box<Type>, Box<Type>),
    Tuple(Vec<Type>),
    /// A record over a row (the boxed type is `RowEmpty` / `RowExtend` / a row var).
    Record(Box<Type>),
    /// Empty row — a closed record `{}` *and* the pure effect.
    RowEmpty,
    /// `label(field) | rest`. For a record row the `field` is the field's type; for
    /// an effect row it is `Tuple([..effect type args..])`.
    RowExtend(InternedString, Box<Type>, Box<Type>),
    /// The type of something already reported as wrong: an undefined name, an
    /// unknown constructor or type. It unifies with anything, and a mismatch
    /// that involves it is not reported, which is what stops one mistake from
    /// turning into a cascade. The same idea as rustc's `{type error}`.
    Error,
}

impl Type {
    pub fn is_fun(&self) -> bool {
        matches!(self, Type::Fun(..))
    }

    /// Whether the error type appears anywhere in this (zonked) type.
    pub fn references_error(&self) -> bool {
        match self {
            Type::Error => true,
            Type::Var(_) | Type::Bound(_) | Type::RowEmpty => false,
            Type::Con(_, args) | Type::Tuple(args) => args.iter().any(Type::references_error),
            Type::Fun(ps, r, e) => {
                ps.iter().any(Type::references_error)
                    || r.references_error()
                    || e.references_error()
            }
            Type::Record(r) => r.references_error(),
            Type::RowExtend(_, f, rest) => f.references_error() || rest.references_error(),
        }
    }

    pub fn con(name: &str) -> Type {
        Type::Con(InternedString::from(name), vec![])
    }
    pub fn int() -> Type {
        Type::con("Int")
    }
    pub fn bigint() -> Type {
        Type::con("BigInt")
    }
    pub fn float() -> Type {
        Type::con("Float")
    }
    pub fn bool() -> Type {
        Type::con("Bool")
    }
    pub fn string() -> Type {
        Type::con("String")
    }
    pub fn char() -> Type {
        Type::con("Char")
    }
    pub fn unit() -> Type {
        Type::con("Unit")
    }
    pub fn list(elem: Type) -> Type {
        Type::Con(InternedString::from("List"), vec![elem])
    }
    /// The builtin `Array a` — a fixed-size, `Rc`-shared contiguous buffer.
    pub fn array(elem: Type) -> Type {
        Type::Con(InternedString::from("Array"), vec![elem])
    }
    /// `Vector a` — the `Std.Collections.Vector` RRB vector that `[…]` builds.
    pub fn vector(elem: Type) -> Type {
        Type::Con(InternedString::from("Vector"), vec![elem])
    }
    /// `Ref a` — the one mutable cell. Reading and writing one carries the `Mut`
    /// effect, so a function that mutates says so in its type.
    pub fn reference(inner: Type) -> Type {
        Type::Con(InternedString::from("Ref"), vec![inner])
    }
    /// `StRef s a` — a cell that belongs to one `runSt`, whose state `s` is.
    pub fn st_ref(state: Type, inner: Type) -> Type {
        Type::Con(InternedString::from("StRef"), vec![state, inner])
    }
    /// `Task a` — a running green thread, and the result it will have.
    pub fn task(result: Type) -> Type {
        Type::Con(InternedString::from("Task"), vec![result])
    }
    /// `TVar a` — a transactional variable.
    pub fn tvar(elem: Type) -> Type {
        Type::Con(InternedString::from("TVar"), vec![elem])
    }
    /// `Channel a` — values passed between green threads.
    pub fn channel(elem: Type) -> Type {
        Type::Con(InternedString::from("Channel"), vec![elem])
    }
    /// `Compact a` — a value copied into a compact region.
    pub fn compact(elem: Type) -> Type {
        Type::Con(InternedString::from("Compact"), vec![elem])
    }
    /// `StArray s a` — a mutable array that belongs to one `runSt`.
    pub fn st_array(state: Type, elem: Type) -> Type {
        Type::Con(InternedString::from("StArray"), vec![state, elem])
    }
    /// A curried **pure** function type: `func([a, b], r)` is `a -> b -> r`.
    pub fn func(args: Vec<Type>, ret: Type) -> Type {
        Type::func_eff(args, ret, Type::RowEmpty)
    }

    /// A curried function type whose *innermost* arrow carries `eff`; the outer
    /// arrows (from extra curried params) are pure.
    pub fn func_eff(args: Vec<Type>, ret: Type, eff: Type) -> Type {
        let mut it = args.into_iter().rev();
        let inner = match it.next() {
            Some(last) => Type::Fun(vec![last], Box::new(ret), Box::new(eff)),
            None => return ret, // nullary "function" — just the result
        };
        it.fold(inner, |acc, a| {
            Type::Fun(vec![a], Box::new(acc), Box::new(Type::RowEmpty))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum VarKind {
    Type,
    /// A record row variable.
    Row,
    /// An effect row variable — structurally a row, tracked separately so error
    /// messages and pretty-printing can tell effects from records.
    Effect,
    /// A variable standing for some **integer** type: `Int` (`Int64`),
    /// `BigInt`, `Int8` … `Int32` or `UInt8` … `UInt64`. What an integer literal
    /// and the integer operators have. Unifies with those types, a plain type
    /// variable (which becomes one of these), or another `Num`.
    ///
    /// A `fun` generalizes over it -- `fun add a b = a + b` is
    /// `forall n. n -> n -> n` -- and a `def` or `let` does not, so `def x = 5`
    /// is one number rather than a family of them. One left over defaults to
    /// `Int` (see `Arena::default_num_vars`). Printed `n`, `n1`, ….
    Num,
    /// The same for **float** types, `Float` (`Float64`) and `Float32`: what a
    /// float literal and `+.` and friends have. Defaults to `Float`. Printed
    /// `f`, `f1`, ….
    Frac,
}

/// A polytype: `quant` lists the kind of each quantified variable, and `ty` refers
/// to them as `Type::Bound(i)`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Scheme {
    pub quant: Vec<VarKind>,
    /// The traits the quantified variables have to implement: `Show a =>`.
    /// A value of such a type takes a dictionary for each, in this order,
    /// before anything else.
    #[serde(default)]
    pub preds: Vec<Pred>,
    pub ty: Type,
    /// For each quantified row variable that must lack some labels -- by its
    /// index in `quant` -- which: `fun ext r = { x = 1 | r }` is generic in
    /// `r` only over rows without an `x`, and an instance keeps to that.
    #[serde(default)]
    pub lacks: Vec<(u32, Vec<InternedString>)>,
}

/// `tys` implement the trait `tr` -- one type, or one per parameter of a trait
/// of several -- whose associated types are `assocs` there, in the trait's
/// order. In a [`Scheme`] the types are over its quantifiers.
///
/// An associated type is carried as a type of its own beside the one it is
/// of, rather than as an application `Elem f` to reduce: a function over
/// `Container f` is quantified over `Elem f` too, as one more variable, and
/// choosing an `impl` for `f` is what settles it. So nothing past inference
/// ever meets an associated type -- only ordinary variables and the types
/// that replaced them.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pred {
    pub tr: InternedString,
    pub tys: Vec<Type>,
    pub assocs: Vec<Type>,
}

/// An operation of an `effect` declaration, with its argument and result types
/// over the effect's parameters (`Bound(0..params)`).
#[derive(Debug, Clone)]
struct EffOp {
    name: InternedString,
    arg: Type,
    ret: Type,
}

#[derive(Debug, Clone)]
struct EffectInfo {
    params: usize,
    ops: Vec<EffOp>,
}

impl Scheme {
    pub fn mono(ty: Type) -> Scheme {
        Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![],
            ty,
        }
    }
}

// ===========================================================================
// The meta-variable arena / union-find
// ===========================================================================

#[derive(Debug, Clone)]
enum Slot {
    /// `lacks` is for a record row variable: labels the row it comes to stand
    /// for may not have. A record extended with `x` over a row `r` is only a
    /// record with one `x` if `r` has none, so extension puts `x` here, and
    /// binding the variable to a row with an `x` is an error. Kept in the slot
    /// so a snapshot's rollback takes it back with everything else.
    Unbound {
        level: u32,
        kind: VarKind,
        lacks: Vec<InternedString>,
    },
    Bound(Type),
}

#[derive(Debug, Clone)]
enum UnifyError {
    Mismatch(Type, Type),
    /// A `runSt`'s state type would reach a variable from outside that
    /// `runSt` -- see [`Arena::fresh_skolem`].
    Escape,
    /// A type that is not in a number class where one is needed: `true` is for
    /// the integer class, `false` for the float class.
    NotANumber(bool, Type),
    Occurs(Type, Type),
    Arity(usize, usize),
    /// A required label is absent from a closed row.
    MissingLabel(InternedString),
    /// A record would have a label twice: extended with a field it already has.
    Duplicate(InternedString),
}

#[derive(Debug, Clone)]
pub struct Arena {
    slots: Vec<Slot>,
    level: u32,
    /// The previous contents of every slot written since the oldest open
    /// [`Snapshot`], so a trial unification can be taken back. Empty, and not
    /// written to, while no snapshot is open.
    trail: Vec<(u32, Slot)>,
    snapshots: usize,
    /// Every rigid type made so far, by name, with the level it was made at.
    skolems: HashMap<InternedString, u32>,
}

/// A point to roll the arena back to -- see [`Arena::snapshot`].
struct Snapshot {
    slots: usize,
    trail: usize,
}

impl Arena {
    fn new() -> Self {
        Arena {
            slots: Vec::new(),
            level: 0,
            trail: Vec::new(),
            snapshots: 0,
            skolems: HashMap::new(),
        }
    }

    /// A rigid type made fresh for one `runSt`: the `s` in its
    /// `forall s. () -> a ! { St s | e }`.
    ///
    /// It is a type constructor no program can name, so it unifies with itself
    /// and nothing else, and it is made one level in from the `runSt`. That
    /// level is the whole escape check: binding any variable from further out
    /// to a type that mentions it is an error ([`Arena::occurs_adjust`]), and
    /// everything the `runSt` hands back or shares with its surroundings --
    /// its result, its leftover effects, a cell from outside it writes to --
    /// is such a variable.
    fn fresh_skolem(&mut self) -> Type {
        let name = InternedString::from(format!("s#{}", self.skolems.len() + 1));
        self.skolems.insert(name, self.level);
        Type::Con(name, Vec::new())
    }

    fn is_skolem(&self, name: InternedString) -> bool {
        self.skolems.contains_key(&name)
    }

    /// `row` without the `St s` labels at its front whose `s` was made deeper
    /// than `level` -- the `runSt`s a variable at `level` is older than.
    fn without_inner_state(&mut self, row: Type, level: u32) -> Type {
        let row = self.prune(row);
        if let Type::RowExtend(label, field, rest) = &row
            && &**label == "St"
            && let Type::Tuple(args) = self.zonk(field)
            && let [Type::Con(name, targs)] = args.as_slice()
            && targs.is_empty()
            && self.skolems.get(name).is_some_and(|&l| l > level)
        {
            return self.without_inner_state((**rest).clone(), level);
        }
        row
    }

    /// Every slot write goes through here, so a snapshot can undo it.
    fn set_slot(&mut self, id: u32, slot: Slot) {
        if self.snapshots > 0 {
            let old = std::mem::replace(&mut self.slots[id as usize], slot);
            self.trail.push((id, old));
        } else {
            self.slots[id as usize] = slot;
        }
    }

    /// The unbound variables in `ty`.
    fn free_vars(&mut self, ty: &Type, out: &mut Vec<u32>) {
        let ty = self.prune(ty.clone());
        match ty {
            Type::Var(id) => {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
            Type::Bound(_) | Type::RowEmpty | Type::Error => {}
            Type::Con(_, args) | Type::Tuple(args) => {
                args.iter().for_each(|a| self.free_vars(a, out))
            }
            Type::Fun(args, ret, eff) => {
                args.iter().for_each(|a| self.free_vars(a, out));
                self.free_vars(&ret, out);
                self.free_vars(&eff, out);
            }
            Type::Record(row) => self.free_vars(&row, out),
            Type::RowExtend(_, field, rest) => {
                self.free_vars(&field, out);
                self.free_vars(&rest, out);
            }
        }
    }

    /// [`Arena::hold_back`], for the `Num` and `Frac` variables in `ty` only.
    fn hold_back_classes(&mut self, ty: &Type, level: u32) {
        let mut vars = Vec::new();
        self.free_vars(ty, &mut vars);
        for id in vars {
            if matches!(self.slot_kind(id), VarKind::Num | VarKind::Frac) {
                self.hold_back(&Type::Var(id), level);
            }
        }
    }

    /// Keep every variable still free in `ty` from generalizing past `level`.
    fn hold_back(&mut self, ty: &Type, level: u32) {
        let ty = self.prune(ty.clone());
        match ty {
            Type::Var(id) => {
                if let Slot::Unbound {
                    level: l,
                    kind,
                    lacks,
                } = self.slots[id as usize].clone()
                    && l > level
                {
                    self.set_slot(id, Slot::Unbound { level, kind, lacks });
                }
            }
            Type::Bound(_) | Type::RowEmpty | Type::Error => {}
            Type::Con(_, args) | Type::Tuple(args) => {
                args.iter().for_each(|a| self.hold_back(a, level))
            }
            Type::Fun(args, ret, eff) => {
                args.iter().for_each(|a| self.hold_back(a, level));
                self.hold_back(&ret, level);
                self.hold_back(&eff, level);
            }
            Type::Record(row) => self.hold_back(&row, level),
            Type::RowExtend(_, field, rest) => {
                self.hold_back(&field, level);
                self.hold_back(&rest, level);
            }
        }
    }

    /// Start recording, for [`Arena::rollback`]. Unifying against each of an
    /// overloaded name's candidates in turn is the one thing that needs it.
    fn snapshot(&mut self) -> Snapshot {
        self.snapshots += 1;
        Snapshot {
            slots: self.slots.len(),
            trail: self.trail.len(),
        }
    }

    /// Put every slot back as it was at `snap`, and drop the variables made since.
    fn rollback(&mut self, snap: Snapshot) {
        while self.trail.len() > snap.trail {
            let (id, old) = self.trail.pop().expect("trail is longer than the snapshot");
            if (id as usize) < self.slots.len() {
                self.slots[id as usize] = old;
            }
        }
        self.slots.truncate(snap.slots);
        self.snapshots -= 1;
        if self.snapshots == 0 {
            self.trail.clear();
        }
    }

    fn fresh_in(&mut self, kind: VarKind) -> Type {
        let id = self.slots.len() as u32;
        self.slots.push(Slot::Unbound {
            level: self.level,
            lacks: Vec::new(),
            kind,
        });
        Type::Var(id)
    }
    fn fresh(&mut self) -> Type {
        self.fresh_in(VarKind::Type)
    }
    fn fresh_row(&mut self) -> Type {
        self.fresh_in(VarKind::Row)
    }
    fn fresh_effect(&mut self) -> Type {
        self.fresh_in(VarKind::Effect)
    }
    fn fresh_num(&mut self) -> Type {
        self.fresh_in(VarKind::Num)
    }
    fn fresh_of(&mut self, kind: VarKind) -> Type {
        self.fresh_in(kind)
    }

    /// A type variable at the outermost level, so nothing can generalize over it.
    /// The placeholder a forward reference gets — see `Expr::Var`.
    fn fresh_global(&mut self) -> Type {
        let id = self.slots.len() as u32;
        self.slots.push(Slot::Unbound {
            level: 0,
            kind: VarKind::Type,
            lacks: Vec::new(),
        });
        Type::Var(id)
    }

    fn enter_level(&mut self) {
        self.level += 1;
    }
    fn exit_level(&mut self) {
        self.level -= 1;
    }

    fn slot_level(&self, id: u32) -> u32 {
        match &self.slots[id as usize] {
            Slot::Unbound { level, .. } => *level,
            Slot::Bound(_) => u32::MAX,
        }
    }
    fn slot_kind(&self, id: u32) -> VarKind {
        match &self.slots[id as usize] {
            Slot::Unbound { kind, .. } => *kind,
            Slot::Bound(_) => VarKind::Type,
        }
    }

    /// The labels an unbound row variable may not come to have.
    fn lacks_of(&self, id: u32) -> Vec<InternedString> {
        match &self.slots[id as usize] {
            Slot::Unbound { lacks, .. } => lacks.clone(),
            Slot::Bound(_) => Vec::new(),
        }
    }

    /// Require that `row` has none of `labels`: an error for each it already
    /// has, and for its unknown rest, a promise it never will -- carried by
    /// the row variable it ends in, and checked when that is bound.
    fn require_lacks(&mut self, row: Type, labels: &[InternedString]) -> Result<(), UnifyError> {
        if labels.is_empty() {
            return Ok(());
        }
        match self.prune(row) {
            Type::RowExtend(l, _, rest) => {
                if labels.contains(&l) {
                    return Err(UnifyError::Duplicate(l));
                }
                self.require_lacks(*rest, labels)
            }
            Type::Var(id) => {
                if let Slot::Unbound { level, kind, lacks } = self.slots[id as usize].clone() {
                    let mut more = lacks.clone();
                    for l in labels {
                        if !more.contains(l) {
                            more.push(*l);
                        }
                    }
                    if more.len() != lacks.len() {
                        self.set_slot(
                            id,
                            Slot::Unbound {
                                level,
                                kind,
                                lacks: more,
                            },
                        );
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// A fresh variable for each of `scheme`'s quantifiers, each row variable
    /// promising again the labels it lacked when the scheme was made.
    fn fresh_quantifiers(&mut self, scheme: &Scheme) -> Vec<Type> {
        let fresh: Vec<Type> = scheme.quant.iter().map(|k| self.fresh_of(*k)).collect();
        for (i, labels) in &scheme.lacks {
            if let Some(v) = fresh.get(*i as usize) {
                // Fresh, so nothing can be in the way: this only records.
                let _ = self.require_lacks(v.clone(), labels);
            }
        }
        fresh
    }

    /// Union-find `find` with path compression.
    fn prune(&mut self, ty: Type) -> Type {
        match ty {
            Type::Var(id) => {
                let resolved = match self.slots[id as usize].clone() {
                    Slot::Bound(inner) => inner,
                    Slot::Unbound { .. } => return Type::Var(id),
                };
                let p = self.prune(resolved);
                self.set_slot(id, Slot::Bound(p.clone()));
                p
            }
            other => other,
        }
    }

    /// Fully resolve a type against the current union-find state.
    pub fn zonk(&mut self, ty: &Type) -> Type {
        let ty = self.prune(ty.clone());
        match ty {
            Type::Var(_) | Type::Bound(_) | Type::RowEmpty | Type::Error => ty,
            Type::Con(name, args) => Type::Con(name, args.iter().map(|a| self.zonk(a)).collect()),
            Type::Fun(args, ret, eff) => Type::Fun(
                args.iter().map(|a| self.zonk(a)).collect(),
                Box::new(self.zonk(&ret)),
                Box::new(self.zonk(&eff)),
            ),
            Type::Tuple(items) => Type::Tuple(items.iter().map(|a| self.zonk(a)).collect()),
            Type::Record(row) => Type::Record(Box::new(self.zonk(&row))),
            Type::RowExtend(label, field, rest) => Type::RowExtend(
                label,
                Box::new(self.zonk(&field)),
                Box::new(self.zonk(&rest)),
            ),
        }
    }

    // --- unification -----------------------------------------------------------

    fn unify(&mut self, a: Type, b: Type) -> Result<(), UnifyError> {
        let a = self.prune(a);
        let b = self.prune(b);
        match (a, b) {
            (Type::Var(i), Type::Var(j)) if i == j => Ok(()),
            (Type::Var(i), Type::Var(j)) => {
                // When a `Num` var meets a plain type var, keep the `Num` (bind
                // the type var to it) so a literal threaded through a polymorphic
                // function stays numeric and later defaults, instead of the plain
                // var winning and freezing the result as `∀a. a`.
                match (self.slot_kind(i), self.slot_kind(j)) {
                    (VarKind::Num | VarKind::Frac, VarKind::Type) => self.bind_var(j, Type::Var(i)),
                    (VarKind::Num, VarKind::Frac) | (VarKind::Frac, VarKind::Num) => {
                        Err(UnifyError::NotANumber(true, Type::float()))
                    }
                    _ => self.bind_var(i, Type::Var(j)),
                }
            }
            // A type variable meeting the error type is poisoned along with
            // it, so later uses of the same variable stay quiet too. Only a
            // plain one: a row or a numeric literal's variable has a shape the
            // error type cannot stand for, and is simply left alone.
            (Type::Var(i), Type::Error) | (Type::Error, Type::Var(i)) => {
                if self.slot_kind(i) == VarKind::Type {
                    self.bind_var(i, Type::Error)
                } else {
                    Ok(())
                }
            }
            (Type::Var(i), t) | (t, Type::Var(i)) => self.bind_var(i, t),
            (Type::Error, _) | (_, Type::Error) => Ok(()),

            (Type::Con(n1, a1), Type::Con(n2, a2)) if n1 == n2 && a1.len() == a2.len() => {
                for (x, y) in a1.into_iter().zip(a2) {
                    self.unify(x, y)?;
                }
                Ok(())
            }
            (Type::Fun(a1, r1, e1), Type::Fun(a2, r2, e2)) => {
                if a1.len() != a2.len() {
                    return Err(UnifyError::Arity(a1.len(), a2.len()));
                }
                for (x, y) in a1.into_iter().zip(a2) {
                    self.unify(x, y)?;
                }
                self.unify(*r1, *r2)?;
                // latent effects are rows — falls into the row arm below
                self.unify(*e1, *e2)
            }
            (Type::Tuple(a1), Type::Tuple(a2)) if a1.len() == a2.len() => {
                for (x, y) in a1.into_iter().zip(a2) {
                    self.unify(x, y)?;
                }
                Ok(())
            }
            (Type::Record(r1), Type::Record(r2)) => self.unify(*r1, *r2),

            (Type::RowEmpty, Type::RowEmpty) => Ok(()),
            (Type::RowEmpty, Type::RowExtend(l2, ..)) => Err(UnifyError::MissingLabel(l2)),
            (Type::RowExtend(l1, t1, rest1), row2 @ (Type::RowExtend(..) | Type::RowEmpty)) => {
                let (t2, rest2) = self.rewrite_row(row2, l1, &t1)?;
                self.unify(*t1, t2)?;
                self.unify(*rest1, rest2)
            }

            (a, b) => Err(UnifyError::Mismatch(a, b)),
        }
    }

    fn bind_var(&mut self, id: u32, ty: Type) -> Result<(), UnifyError> {
        self.occurs_adjust(id, &ty)?;
        // A row variable promised to lack some labels keeps that promise in
        // whatever it becomes -- or, if that is another variable, hands it on.
        let lacks = self.lacks_of(id);
        self.require_lacks(ty.clone(), &lacks)?;
        match self.slot_kind(id) {
            VarKind::Num if !Self::is_integer_type(&ty) => {
                return Err(UnifyError::NotANumber(true, ty));
            }
            VarKind::Frac if !Self::is_float_type(&ty) => {
                return Err(UnifyError::NotANumber(false, ty));
            }
            _ => {}
        }
        self.set_slot(id, Slot::Bound(ty));
        Ok(())
    }

    /// Can a `Num` var stand for this type? An integer type, or a variable (a
    /// plain one is bound to the `Num` rather than the other way round).
    pub fn is_integer_type(ty: &Type) -> bool {
        match ty {
            Type::Var(_) | Type::Error => true,
            Type::Con(n, args) if args.is_empty() => {
                matches!(&**n, "Int" | "BigInt") || meadow_num_width(n)
            }
            _ => false,
        }
    }

    /// Can a `Frac` var stand for this type? `Float`, `Float32`, or a variable.
    pub fn is_float_type(ty: &Type) -> bool {
        match ty {
            Type::Var(_) | Type::Error => true,
            Type::Con(n, args) if args.is_empty() => matches!(&**n, "Float" | "Float32"),
            _ => false,
        }
    }

    /// Bind every still-unbound `Num` var to `Int` and `Frac` var to `Float`: a
    /// number nothing ever pinned to a type. Run once, at the end of inference.
    ///
    /// `Int`, not `BigInt`. It was `BigInt` -- the type that cannot overflow --
    /// on the grounds that a program that never says what its numbers are
    /// should at least get the right answer. The cost was that every program
    /// that never says what its numbers are ran on heap-allocated integers:
    /// `def work = [30, 30, 30]` was a list of `BigInt`s, and a loop over it
    /// was an allocation and a primitive call per step where a machine word
    /// would have done. A number that needs to be exact past 2^63 says so with
    /// `toBigInt` or an annotation, the same way one that needs to be eight
    /// bits does.
    fn default_num_vars(&mut self, quantified: &HashSet<u32>) {
        for (id, slot) in self.slots.iter_mut().enumerate() {
            if quantified.contains(&(id as u32)) {
                continue;
            }
            match slot {
                Slot::Unbound {
                    kind: VarKind::Num, ..
                } => {
                    *slot = Slot::Bound(Type::int());
                }
                Slot::Unbound {
                    kind: VarKind::Frac,
                    ..
                } => {
                    *slot = Slot::Bound(Type::float());
                }
                _ => {}
            }
        }
    }

    /// Occurs check + level adjustment in one walk. Any unbound var reachable from
    /// `ty` that is deeper than `id`'s level is lifted to `id`'s level, so it stays
    /// generalizable no further out than `id` is.
    fn occurs_adjust(&mut self, id: u32, ty: &Type) -> Result<(), UnifyError> {
        let ty = self.prune(ty.clone());
        match ty {
            Type::Var(j) => {
                if j == id {
                    return Err(UnifyError::Occurs(Type::Var(id), Type::Var(j)));
                }
                let min = self.slot_level(id).min(self.slot_level(j));
                if let Slot::Unbound { level, kind, lacks } = self.slots[j as usize].clone()
                    && level != min
                {
                    self.set_slot(
                        j,
                        Slot::Unbound {
                            level: min,
                            kind,
                            lacks,
                        },
                    );
                }
                Ok(())
            }
            Type::Bound(_) | Type::RowEmpty | Type::Error => Ok(()),
            Type::Con(name, args) => {
                if let Some(&level) = self.skolems.get(&name)
                    && level > self.slot_level(id)
                {
                    return Err(UnifyError::Escape);
                }
                for a in &args {
                    self.occurs_adjust(id, a)?;
                }
                Ok(())
            }
            Type::Tuple(args) => {
                for a in &args {
                    self.occurs_adjust(id, a)?;
                }
                Ok(())
            }
            Type::Fun(args, ret, eff) => {
                for a in &args {
                    self.occurs_adjust(id, a)?;
                }
                self.occurs_adjust(id, &ret)?;
                self.occurs_adjust(id, &eff)
            }
            Type::Record(row) => self.occurs_adjust(id, &row),
            Type::RowExtend(_, field, rest) => {
                self.occurs_adjust(id, &field)?;
                self.occurs_adjust(id, &rest)
            }
        }
    }

    /// Given a row, produce `(field_ty, rest)` such that `row ≡ {label: field_ty | rest}`.
    /// If the row ends in an unbound row var and lacks the label, the var is
    /// extended in place (standard rewrite-row; no lacks-predicates, so distinct
    /// labels are assumed).
    ///
    /// `want` is the field the label is being matched with. Labels are found by
    /// name, first match first, except that two rigid state types that differ
    /// are different effects: `{ St s1, St s2 | e }` is a row a `runSt` nested in
    /// another can have, and the inner one's `St s2` has to find its own label
    /// rather than fail against the outer one's.
    fn rewrite_row(
        &mut self,
        row: Type,
        label: InternedString,
        want: &Type,
    ) -> Result<(Type, Type), UnifyError> {
        let row = self.prune(row);
        match row {
            Type::RowExtend(l, field, rest)
                if l == label && !self.rigidly_distinct(&field, want) =>
            {
                Ok((*field, *rest))
            }
            Type::RowExtend(l, field, rest) => {
                let (found, rest2) = self.rewrite_row(*rest, label, want)?;
                Ok((found, Type::RowExtend(l, field, Box::new(rest2))))
            }
            Type::Var(id) => {
                // At the row variable's own level, not the current one: the
                // label is being added to a row that may belong further out,
                // and what it carries must not be generalized -- or escape --
                // more freely than the row itself.
                // A row that was promised not to have `label` cannot grow it.
                let lacks = self.lacks_of(id);
                if lacks.contains(&label) {
                    return Err(UnifyError::Duplicate(label));
                }
                let at = self.slot_level(id);
                let saved = std::mem::replace(&mut self.level, at);
                let field = self.fresh();
                let new_rest = self.fresh_row();
                self.level = saved;
                // What is left of the row keeps the promise.
                self.require_lacks(new_rest.clone(), &lacks)?;
                let ext =
                    Type::RowExtend(label, Box::new(field.clone()), Box::new(new_rest.clone()));
                self.set_slot(id, Slot::Bound(ext));
                Ok((field, new_rest))
            }
            Type::RowEmpty => Err(UnifyError::MissingLabel(label)),
            Type::Error => Ok((Type::Error, Type::Error)),
            other => Err(UnifyError::Mismatch(other, Type::RowEmpty)),
        }
    }

    /// Two effect fields that name different rigid state types, like the
    /// `(s#1)` and `(s#2)` of two `St` labels.
    fn rigidly_distinct(&mut self, a: &Type, b: &Type) -> bool {
        let a = self.zonk(a);
        let b = self.zonk(b);
        let rigid = |t: &Type| match t {
            Type::Tuple(args) => match args.as_slice() {
                [Type::Con(name, targs)] if targs.is_empty() => Some(*name),
                _ => None,
            },
            _ => None,
        };
        match (rigid(&a), rigid(&b)) {
            (Some(x), Some(y)) => x != y && self.is_skolem(x) && self.is_skolem(y),
            _ => false,
        }
    }

    // --- generalize / instantiate -------------------------------------------

    /// `classes`: whether `Num` and `Frac` variables may be quantified too --
    /// yes for a `fun`, no for a `def` or `let` (see [`VarKind::Num`]).
    fn quantify(
        &self,
        ty: &Type,
        map: &mut HashMap<u32, u32>,
        kinds: &mut Vec<VarKind>,
        classes: bool,
    ) -> Type {
        self.quantify_from(ty, Some(self.level), map, kinds, classes)
    }

    /// [`Arena::quantify`], over every variable deeper than `level` -- or over
    /// every variable at all, for `None`.
    fn quantify_from(
        &self,
        ty: &Type,
        level: Option<u32>,
        map: &mut HashMap<u32, u32>,
        kinds: &mut Vec<VarKind>,
        classes: bool,
    ) -> Type {
        match ty {
            Type::Var(id) => {
                let class = matches!(self.slot_kind(*id), VarKind::Num | VarKind::Frac);
                if (!class || classes) && level.is_none_or(|l| self.slot_level(*id) > l) {
                    let idx = *map.entry(*id).or_insert_with(|| {
                        kinds.push(self.slot_kind(*id));
                        (kinds.len() - 1) as u32
                    });
                    Type::Bound(idx)
                } else {
                    Type::Var(*id)
                }
            }
            Type::Bound(i) => Type::Bound(*i),
            Type::RowEmpty => Type::RowEmpty,
            Type::Error => Type::Error,
            Type::Con(name, args) => Type::Con(
                *name,
                args.iter()
                    .map(|a| self.quantify_from(a, level, map, kinds, classes))
                    .collect(),
            ),
            Type::Fun(args, ret, eff) => Type::Fun(
                args.iter()
                    .map(|a| self.quantify_from(a, level, map, kinds, classes))
                    .collect(),
                Box::new(self.quantify_from(ret, level, map, kinds, classes)),
                Box::new(self.quantify_from(eff, level, map, kinds, classes)),
            ),
            Type::Tuple(items) => Type::Tuple(
                items
                    .iter()
                    .map(|a| self.quantify_from(a, level, map, kinds, classes))
                    .collect(),
            ),
            Type::Record(row) => Type::Record(Box::new(
                self.quantify_from(row, level, map, kinds, classes),
            )),
            Type::RowExtend(label, field, rest) => Type::RowExtend(
                *label,
                Box::new(self.quantify_from(field, level, map, kinds, classes)),
                Box::new(self.quantify_from(rest, level, map, kinds, classes)),
            ),
        }
    }

    fn subst_bound(ty: &Type, fresh: &[Type]) -> Type {
        match ty {
            Type::Bound(i) => fresh[*i as usize].clone(),
            Type::Var(id) => Type::Var(*id),
            Type::RowEmpty => Type::RowEmpty,
            Type::Error => Type::Error,
            Type::Con(name, args) => Type::Con(
                *name,
                args.iter().map(|a| Self::subst_bound(a, fresh)).collect(),
            ),
            Type::Fun(args, ret, eff) => Type::Fun(
                args.iter().map(|a| Self::subst_bound(a, fresh)).collect(),
                Box::new(Self::subst_bound(ret, fresh)),
                Box::new(Self::subst_bound(eff, fresh)),
            ),
            Type::Tuple(items) => {
                Type::Tuple(items.iter().map(|a| Self::subst_bound(a, fresh)).collect())
            }
            Type::Record(row) => Type::Record(Box::new(Self::subst_bound(row, fresh))),
            Type::RowExtend(label, field, rest) => Type::RowExtend(
                *label,
                Box::new(Self::subst_bound(field, fresh)),
                Box::new(Self::subst_bound(rest, fresh)),
            ),
        }
    }
}

// ===========================================================================
// Side table: NodeId -> Type
// ===========================================================================

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TypeTable {
    types: Vec<Option<Type>>,
    /// The kind of every variable still free in these types that is not a
    /// plain type variable -- so a renderer, which never sees the arena, names
    /// it the way a scheme would: an integer variable `n` rather than a plain
    /// `a` no one could write `+` on, an effect `e`, a record row `r`.
    classes: HashMap<u32, VarKind>,
}

impl TypeTable {
    pub fn new(node_count: usize) -> Self {
        TypeTable {
            types: vec![None; node_count],
            classes: HashMap::new(),
        }
    }

    fn set(&mut self, id: NodeId, ty: Type) {
        let i = id.0 as usize;
        if i >= self.types.len() {
            self.types.resize(i + 1, None);
        }
        self.types[i] = Some(ty);
    }

    pub fn get(&self, id: NodeId) -> Option<&Type> {
        self.types.get(id.0 as usize).and_then(|t| t.as_ref())
    }

    /// Fold another table's annotations into this one (used when a package is
    /// assembled from several separately-compiled modules). Node ids are only
    /// meaningful per module afterwards — this feeds the diagnostic dump.
    pub fn absorb(&mut self, other: TypeTable) {
        self.types.extend(other.types);
        self.classes.extend(other.classes);
    }

    fn zonk_all(&mut self, arena: &mut Arena) {
        for slot in &mut self.types {
            if let Some(ty) = slot {
                *ty = arena.zonk(ty);
            }
        }
        for (id, slot) in arena.slots.iter().enumerate() {
            if let Slot::Unbound { kind, .. } = slot
                && *kind != VarKind::Type
            {
                self.classes.insert(id as u32, *kind);
            }
        }
    }

    /// `(NodeId, rendered type)` for every annotated node, using one shared naming
    /// of free variables so the same var reads the same across the whole tree.
    pub fn rendered(&self) -> Vec<(NodeId, String)> {
        let mut namer = Namer::default();
        let mut out = Vec::new();
        for (i, slot) in self.types.iter().enumerate() {
            if let Some(ty) = slot {
                let mut s = String::new();
                let _ = write_type(
                    &mut s,
                    ty,
                    &mut namer,
                    Prec::Top,
                    &std::collections::HashSet::new(),
                );
                out.push((NodeId(i as u32), s));
            }
        }
        out
    }
}

// ===========================================================================
// The inference pass
// ===========================================================================

/// What one inference run hands back.
pub struct InferResult {
    pub table: TypeTable,
    /// Scheme for each top-level binding produced by this run, keyed by its `VarId`.
    pub schemes: HashMap<VarId, Scheme>,
    /// Every binding this run generalized -- top-level *and* local -- with the
    /// arena variables it quantified over.
    ///
    /// Lowering needs both halves. The scheme says what the binding's type is;
    /// the variables say which of the type annotations scattered through its
    /// body are the *same* variable, which is what a `TyLam` binds and a
    /// `TyApp` supplies. Inference is the only pass that knows this, and it
    /// knows it only while generalizing.
    pub generalized: HashMap<VarId, Generalized>,
    /// Every data / record type's constructors, for the exhaustiveness checker.
    pub variants: VariantEnv,
    /// The candidate chosen for each overloaded name -- see [`hir::Overloads`]
    /// and [`hir::apply_resolutions`].
    pub resolutions: HashMap<NodeId, hir::Alt>,
    /// The dictionaries each mention of a name with a `where` is applied to,
    /// one per trait its type asks for, in the scheme's order -- and, for an
    /// `impl`'s dictionary (by the node of its name), the dictionaries of the
    /// traits its trait requires.
    pub evidence: HashMap<NodeId, Vec<Evidence>>,
    /// Every trait known here, as lowering needs it.
    pub traits: TraitEnv,
    pub errors: Vec<Diagnostic>,
}

/// The state of [`Infer::search`].
struct Search {
    /// Every assignment found so far: a candidate index per group member.
    solutions: Vec<Vec<usize>>,
    /// Candidates left to try before giving up.
    budget: usize,
    truncated: bool,
}

/// An overloaded name inference has not chosen a meaning for yet.
#[derive(Clone)]
struct Pending {
    /// The node the name was written at -- the key into [`hir::Overloads`].
    node: NodeId,
    candidates: Vec<hir::Candidate>,
    /// The type the context wants the name to have; what a candidate must
    /// unify with to be chosen.
    ty: Type,
    /// The level the name was mentioned at, which a chosen candidate is
    /// instantiated at -- as it would have been had it been written alone.
    level: u32,
    span: Span,
    filename: String,
    /// Nodes to give the error type if no candidate is ever chosen.
    poison: Vec<NodeId>,
}

/// A generalized binding, as lowering needs to see it.
#[derive(Debug, Clone)]
pub struct Generalized {
    pub scheme: Scheme,
    /// The arena variable behind each quantifier, in quantifier order. A
    /// monomorphic binding has none.
    pub vars: Vec<u32>,
}

/// `type name -> its constructors`, covering this unit *and* its dependencies.
pub type VariantEnv = HashMap<InternedString, Vec<VariantSig>>;

/// One constructor of a data / record type.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VariantSig {
    pub name: InternedString,
    /// Field types, written over the type's parameters as `Type::Bound(i)` —
    /// instantiate with [`subst_bound`] against the scrutinee's type arguments.
    pub fields: Vec<Type>,
    /// Field labels, for a `record` or a named `data` variant.
    pub labels: Option<Vec<InternedString>>,
}

/// Replace each `Type::Bound(i)` in `ty` with `args[i]`.
pub fn subst_bound(ty: &Type, args: &[Type]) -> Type {
    Arena::subst_bound(ty, args)
}

mod traits;
pub use traits::{Evidence, TraitEnv, TraitShape};

pub struct Infer {
    filename: String,
    /// The definitions that are run rather than defined -- `main`, and the
    /// REPL's or a debugger's stand-in for it: the top-level `def`s allowed to
    /// perform effects. See [`Infer::check_pure_def`].
    entries: Vec<VarId>,
    arena: Arena,
    /// `VarId` -> polytype. Populated from the prelude, dependency packages, and
    /// as we walk. `VarId`s are globally unique, so this never needs scoping.
    env: HashMap<VarId, Scheme>,
    table: TypeTable,
    /// `VarId`s bound at module top level by this run (its exports).
    exports: Vec<VarId>,
    /// Every binding generalized (or deliberately not) by this run — see
    /// [`InferResult::generalized`].
    generalized: HashMap<VarId, Generalized>,
    /// Data / record constructor schemes, e.g. `Leaf : ∀a. Vector a -> Node a`.
    ctors: HashMap<InternedString, Scheme>,
    /// The same information grouped by *type*, which is what an exhaustiveness
    /// check needs: given a scrutinee type, what are all its constructors?
    variants: VariantEnv,
    /// `tyname -> field -> accessor scheme` (`Person -> name -> ∀. Person -> String`).
    record_fields: HashMap<InternedString, HashMap<InternedString, Scheme>>,
    /// Declared effects and their operation signatures (for `handle` checking).
    effects: HashMap<InternedString, EffectInfo>,
    /// The effect row of the code region currently being inferred — an open row
    /// var that accumulates every effect performed in the current function body.
    /// Saved/restored around lambda bodies and `let` right-hand sides.
    cur_effect: Type,
    /// The meta variable standing for each type variable a pattern annotation
    /// introduced -- see [`Infer::annotation`]. Never scoped, because the
    /// resolver already scoped the `VarId`s it is keyed by.
    ann_tyvars: HashMap<VarId, Type>,
    /// Every `type` alias known, this unit's and its dependencies'.
    aliases: Aliases,
    /// The standalone signature of each top-level binding that has one, with
    /// the traits its `where` asks for.
    sigs: HashMap<VarId, (hir::LTypeExpr, Vec<hir::Bound>)>,
    /// Traits, `impl`s, and what is still wanted of them -- see [`traits`].
    tr: traits::State,
    /// Bindings whose signature did not even unify with them: reported once,
    /// so not checked for generality as well.
    sig_failed: HashSet<VarId>,
    /// Names the resolver found several meanings for -- see [`Infer::defer`].
    overloads: hir::Overloads,
    pending: Vec<Pending>,
    /// `(argument's effect, what the parameter allows, where)`: a function
    /// passed as an argument may do less than the parameter's type permits.
    /// Solved by [`Infer::solve_subsumptions`] -- see [`Infer::loosen_argument`].
    subsumptions: Vec<(Type, Type, Span)>,
    /// The variables that stand for a trait's associated effect at types
    /// whose `impl` is not found yet: what a call of a method of it performs.
    assoc_effect_vars: HashSet<u32>,
    /// Joining what was left to wait: an associated effect that is still a
    /// variable now is tied to its region after all.
    settling_joins: bool,
    resolutions: HashMap<NodeId, hir::Alt>,
    errors: Vec<Diagnostic>,
    /// The `runSt` primitive's `VarId` in this unit -- see [`Infer::infer_run_st`].
    run_st: Option<VarId>,
    /// The levels of the `runSt`s being inferred, innermost last. While one
    /// is open, an older effect variable is not tied to a region that is still
    /// only a variable: see [`Infer::join_effect_into`].
    st_levels: Vec<u32>,
}

impl Infer {
    /// Name the file diagnostics from here on point into — the driver moves
    /// this along as it walks the unit's modules.
    /// Which bindings are run rather than defined -- see [`Infer::check_pure_def`].
    pub fn set_entries(&mut self, entries: Vec<VarId>) {
        self.entries = entries;
    }

    /// The program's entry point, `main`, has to be `() -> () ! Eff`: it is
    /// given nothing, gives nothing back, and performs at most what the runtime
    /// answers -- [`THREAD_EFFECTS`], which `Std.Eff` names -- since once it
    /// runs there is nothing above it to answer anything else.
    ///
    /// Held to that after it is generalized, as a use of it would be: what it
    /// was inferred to be only has to fit -- at most, not exactly: a `main`
    /// that prints nothing, or nothing at all, is a `main` -- and one that
    /// fits is `() -> () ! Eff` from then on, whatever less its body performs.
    pub fn check_main(&mut self, main: VarId, span: Span) {
        let Some(scheme) = self.generalized.get(&main).map(|g| g.scheme.clone()) else {
            return;
        };
        let got = self.instantiate(&scheme);
        let effect = match self.arena.zonk(&got) {
            Type::Fun(params, ret, eff)
                if params.len() == 1
                    && self.arena.unify(params[0].clone(), Type::unit()).is_ok()
                    && self.arena.unify((*ret).clone(), Type::unit()).is_ok() =>
            {
                Some(self.arena.zonk(&eff))
            }
            _ => None,
        };
        let (msg, label) = match effect {
            None => (
                format!("`main` is `{scheme}`, and has to be `() -> () ! Eff`"),
                "the program starts here: `fun main () = ...`".to_string(),
            ),
            Some(eff) => {
                // What it performs that nobody would answer.
                let unanswered: Vec<String> = row_parts(&eff)
                    .0
                    .into_iter()
                    .filter(|(l, _)| !THREAD_EFFECTS.contains(&&**l))
                    .map(|(l, _)| format!("`{}`", hir::spelling(&l)))
                    .collect();
                if unanswered.is_empty() {
                    // It fits, so `Eff` is what it is: `main` is not generic
                    // in what else it might perform -- nothing runs it at
                    // another row -- and whatever `Eff` names may be
                    // performed where it is called from.
                    // The row it was inferred at is closed to the same: what
                    // an editor reads of `main` and of what is in it is each
                    // node's type, and those are written with the variable
                    // the row ended in, which is `Eff`'s other effects now.
                    if let Some(g) = self.generalized.get(&main).cloned()
                        && let Type::Fun(_, _, row) = &g.scheme.ty
                    {
                        let (present, tail) = row_parts(row);
                        if let Some(Type::Bound(i)) = tail
                            && let Some(var) = g.vars.get(i as usize)
                        {
                            let others = THREAD_EFFECTS
                                .iter()
                                .rev()
                                .filter(|e| !present.iter().any(|(l, _)| &**l == **e))
                                .fold(Type::RowEmpty, |rest, e| {
                                    Type::RowExtend(
                                        InternedString::from(*e),
                                        Box::new(Type::Tuple(vec![])),
                                        Box::new(rest),
                                    )
                                });
                            let _ = self.arena.unify(Type::Var(*var), others);
                        }
                    }
                    let fixed = Scheme::mono(Type::Fun(
                        vec![Type::unit()],
                        Box::new(Type::unit()),
                        Box::new(thread_body_row()),
                    ));
                    if let Some(g) = self.generalized.get_mut(&main) {
                        g.scheme = fixed.clone();
                        g.vars = Vec::new();
                    }
                    // And what the unit says of it to whoever asks: a package
                    // that names it, `--types`.
                    if self.env.contains_key(&main) {
                        self.env.insert(main, fixed);
                    }
                    return;
                }
                let list = unanswered.join(", ");
                (
                    format!("`main` performs {list}, which nothing would handle"),
                    format!("handle {list} inside `main`; it may perform only `Eff`"),
                )
            }
        };
        self.errors.push(Diagnostic {
            msg,
            filename: self.filename.clone(),
            label: (label, span),
            extra_labels: vec![],
        });
    }

    /// A top-level `def` has to be pure to evaluate.
    ///
    /// Its right-hand side is evaluated once, the first time anything needs it,
    /// and the value is kept. That is only the same program as evaluating it
    /// at every use -- or all at load, in order -- when evaluating it does
    /// nothing a program can see. So effects belong inside functions, where they
    /// happen each time the function is called: a `def` may be a function that
    /// prints, but not a value whose making prints. `main` is the exception,
    /// because running it once is running the program.
    fn check_pure_def(&mut self, bound: &[VarId], rhs_eff: &Type, span: Span) {
        if bound.iter().any(|v| self.entries.contains(v)) {
            return;
        }
        let mut labels = Vec::new();
        let mut row = self.arena.zonk(rhs_eff);
        while let Type::RowExtend(l, _, rest) = row {
            if !labels.contains(&l) {
                labels.push(l);
            }
            row = *rest;
        }
        if labels.is_empty() {
            return;
        }
        let list = labels
            .iter()
            .map(|l| format!("`{}`", hir::spelling(l)))
            .collect::<Vec<_>>()
            .join(", ");
        self.errors.push(Diagnostic {
            msg: format!(
                "a top-level `def` cannot perform effects, and this one performs {list}; \
                 perform them inside a function instead -- `fun name () = ...` -- or in `main`"
            ),
            filename: self.filename.clone(),
            label: (format!("performs {list}"), span),
            extra_labels: vec![],
        });
    }

    pub fn set_filename(&mut self, filename: impl Into<String>) {
        self.filename = filename.into();
    }

    pub fn new(filename: impl Into<String>, node_count: usize) -> Self {
        Infer {
            filename: filename.into(),
            entries: Vec::new(),
            arena: Arena::new(),
            env: HashMap::new(),
            generalized: HashMap::new(),
            table: TypeTable::new(node_count),
            exports: Vec::new(),
            ctors: HashMap::new(),
            variants: HashMap::new(),
            ann_tyvars: HashMap::new(),
            aliases: HashMap::new(),
            sigs: HashMap::new(),
            tr: traits::State::default(),
            sig_failed: HashSet::new(),
            overloads: HashMap::new(),
            pending: Vec::new(),
            subsumptions: Vec::new(),
            assoc_effect_vars: HashSet::new(),
            settling_joins: false,
            resolutions: HashMap::new(),
            record_fields: HashMap::new(),
            effects: HashMap::new(),
            cur_effect: Type::RowEmpty,
            errors: Vec::new(),
            run_st: None,
            st_levels: Vec::new(),
        }
    }

    /// Record that the current region performs effect `name` with type args `args`
    /// (used by operation calls in later phases).
    #[allow(dead_code)]
    fn emit_effect(&mut self, span: Span, name: InternedString, args: Vec<Type>) {
        let tail = self.arena.fresh_effect();
        let want = Type::RowExtend(name, Box::new(Type::Tuple(args)), Box::new(tail));
        self.unify_at(span, self.cur_effect.clone(), want);
    }

    /// Fold a called function's latent effect `phi` into the current region.
    /// A pure arrow (`phi` = `RowEmpty`) imposes nothing; an open effect row ties
    /// its tail to `cur_effect`, which is how effect polymorphism propagates
    /// (`map`'s effect ends up equal to its function argument's).
    fn join_effect(&mut self, span: Span, phi: Type) {
        if matches!(self.arena.zonk(&phi), Type::RowEmpty) {
            return;
        }
        let region = self.cur_effect.clone();
        self.join_effect_into(span, region, phi);
    }

    /// A named function's type as a use of it sees it: the effect of each
    /// arrow it answers with left open at its end.
    ///
    /// `: Int` declares a body that performs nothing, and `! { Console }` one
    /// that performs only that -- what the function *does*. Where it is used,
    /// that is a bound, not everything the place allows: a pure function can
    /// be passed where a callback may print, and must not close the effect of
    /// the call it is passed to. So a closed row is opened here, as Koka does
    /// on instantiation. Only the arrows it returns: its parameters' effects
    /// say what it allows of its callers, and those stay as written.
    fn open_arrows(&mut self, ty: Type) -> Type {
        match self.arena.zonk(&ty) {
            Type::Fun(ps, ret, eff) => {
                let eff = match self.arena.zonk(&eff) {
                    Type::RowEmpty => self.arena.fresh_effect(),
                    closed => self.open_row_end(closed),
                };
                let ret = self.open_arrows(*ret);
                Type::Fun(ps, Box::new(ret), Box::new(eff))
            }
            _ => ty,
        }
    }

    /// `row` with a fresh variable for its end, if it is a closed row of at
    /// least one effect; otherwise `row`.
    fn open_row_end(&mut self, row: Type) -> Type {
        fn rebuild(row: Type, tail: &Type) -> Type {
            match row {
                Type::RowExtend(label, field, rest) => {
                    Type::RowExtend(label, field, Box::new(rebuild(*rest, tail)))
                }
                _ => tail.clone(),
            }
        }
        let zonked = self.arena.zonk(&row);
        let mut end = &zonked;
        while let Type::RowExtend(_, _, rest) = end {
            end = rest;
        }
        match (&zonked, end) {
            (Type::RowExtend(..), Type::RowEmpty) => {
                let tail = self.arena.fresh_effect();
                rebuild(zonked, &tail)
            }
            _ => row,
        }
    }

    /// [`Infer::join_effect`], into a region other than the current one.
    ///
    /// Joining is unification, which asks for *equal* rows where the truth is
    /// only that `phi` is part of `region`. That difference matters inside a
    /// `runSt`, whose region starts with its own `St s`: a function parameter's
    /// effect variable, made outside, cannot be equal to a row naming `s` --
    /// that is exactly an escape -- but it does not need to be. When `phi` is a
    /// bare variable older than the `runSt`s at the front of the region, it is
    /// tied to what follows them instead, which is everything it could stand for.
    ///
    /// Inside such a `runSt`, a region may still be a bare variable -- a
    /// `let`'s, before anything in it has performed `St s`. Tying an older
    /// variable to it then would make the two one, and the `St s` the region
    /// gains later would land in the older one, which is an escape. So the
    /// join waits, as a subsumption, until the region has taken shape; the
    /// `runSt` settles what is left before it closes.
    /// Whether `phi` is, for now, a variable standing for a trait's
    /// associated effect: the one remembered, or one it was unified with --
    /// an application's own effect is a fresh variable tied to its callee's.
    fn is_assoc_effect(&mut self, phi: &Type) -> bool {
        // Alone, or at the end of a row that names effects before it: `{ St
        // s | Reading r }`, what a function that writes a cell and reads
        // rows performs.
        let whole = self.arena.zonk(phi);
        let mut end = &whole;
        while let Type::RowExtend(_, _, rest) = end {
            end = rest;
        }
        let Type::Var(id) = *end else {
            return false;
        };
        // What an `impl` has settled is a variable no longer, and is dropped.
        let mut still = HashSet::new();
        let mut found = false;
        for v in std::mem::take(&mut self.assoc_effect_vars) {
            if let Type::Var(now) = self.arena.zonk(&Type::Var(v)) {
                found |= now == id;
                still.insert(v);
            }
        }
        self.assoc_effect_vars = still;
        found
    }

    fn join_effect_into(&mut self, span: Span, region: Type, phi: Type) {
        // A trait's associated effect, of types whose `impl` has not been
        // found: what it is, that `impl` will say. Unified with the region
        // now it would *be* the region -- everything else performed here --
        // and the `impl` would then be refused for saying less. So the join
        // waits until the binding's wanted are answered.
        if !self.settling_joins && self.is_assoc_effect(&phi) {
            self.subsumptions.push((phi, region, span));
            return;
        }
        // One that is still a variable when its binding is done is generic
        // there: the function performs it, whatever it is, *and* what else
        // its region names. So it is the region's end -- `{ St s | Reading r
        // }` -- and not the whole of it, which would have every `impl` say
        // it performs the `St s` too.
        if self.settling_joins && self.is_assoc_effect(&phi) {
            // What it names before its end is joined as any row is, with an
            // end of its own; then the end.
            let mut named = Vec::new();
            let mut end = self.arena.zonk(&phi);
            while let Type::RowExtend(l, f, rest) = end {
                named.push((l, f));
                end = *rest;
            }
            if !named.is_empty() {
                let open = self.arena.fresh_effect();
                let row = named
                    .into_iter()
                    .rev()
                    .fold(open, |rest, (l, f)| Type::RowExtend(l, f, Box::new(rest)));
                self.join_effect_into(span, region.clone(), row);
            }
            let whole = self.arena.zonk(&region);
            let mut last = &whole;
            while let Type::RowExtend(_, _, rest) = last {
                last = rest;
            }
            if matches!(last, Type::Var(_)) {
                let last = last.clone();
                self.unify_at(span, last, end);
                return;
            }
            self.unify_at(span, region, end);
            return;
        }
        // An effect named twice is performed once: `{ St s, St s }`, what a
        // function that writes a state and reads rows of that same state
        // comes to once the rows' `impl` is known.
        let phi = {
            let mut seen: Vec<(InternedString, Type)> = Vec::new();
            let mut end = self.arena.zonk(&phi);
            while let Type::RowExtend(l, f, rest) = end {
                let f = self.arena.zonk(&f);
                if !seen.iter().any(|(m, g)| *m == l && *g == f) {
                    seen.push((l, f));
                }
                end = *rest;
            }
            seen.into_iter().rev().fold(end, |rest, (l, f)| {
                Type::RowExtend(l, Box::new(f), Box::new(rest))
            })
        };
        // Performing nothing asks nothing of the region: what an associated
        // effect turned out to be, where its `impl` is pure.
        if matches!(self.arena.zonk(&phi), Type::RowEmpty) {
            return;
        }
        // A closed row -- `a -> b ! { Random }`, say, from an annotation --
        // lists what a call performs, not all the region may: the region
        // holds it and possibly more, so it is joined with its end left open.
        let phi = self.open_row_end(phi);
        if self.joins_within(&region, &phi, span) {
            return;
        }
        let target = match self.arena.zonk(&phi) {
            Type::Var(id) => {
                let level = self.arena.slot_level(id);
                let inside_newer_run_st = self.st_levels.iter().any(|&l| l > level);
                if inside_newer_run_st
                    && let Type::Var(rid) = self.arena.zonk(&region)
                    && rid != id
                    && self.arena.slot_level(rid) >= self.st_levels.last().copied().unwrap_or(0)
                {
                    self.subsumptions.push((phi, region, span));
                    return;
                }
                self.arena.without_inner_state(region, level)
            }
            _ => region,
        };
        self.unify_at(span, target, phi);
    }

    /// Whether `phi` is already part of `region` because the two end in the
    /// same row variable -- `e` into `{ Thread, Mut | e }`, what is left of a
    /// handled `{ Ask | e }` in a function that performs `{ Thread, Mut | e }`
    /// -- with each effect it names in `region` too, whose arguments are then
    /// unified. Unifying the rows instead would ask for `e = { Thread, Mut |
    /// e }`, which no type is: the occurs check fails, and says so as
    /// `infinite type: a occurs in a`.
    fn joins_within(&mut self, region: &Type, phi: &Type, span: Span) -> bool {
        fn split(row: Type) -> (Vec<(InternedString, Type)>, Type) {
            let mut labels = Vec::new();
            let mut end = row;
            while let Type::RowExtend(l, f, rest) = end {
                labels.push((l, *f));
                end = *rest;
            }
            (labels, end)
        }
        let (want, end) = split(self.arena.zonk(phi));
        let (mut have, region_end) = split(self.arena.zonk(region));
        if !matches!(end, Type::Var(_)) || end != region_end {
            return false;
        }
        let mut pairs = Vec::new();
        for (label, field) in want {
            // Labels are matched first come first served, as unification
            // matches them.
            match have.iter().position(|(l, _)| *l == label) {
                Some(i) => pairs.push((field, have.remove(i).1)),
                None => return false,
            }
        }
        for (a, b) in pairs {
            self.unify_at(span, a, b);
        }
        true
    }

    /// Whether a join into `region` is still waiting, from `mark` on.
    fn has_pending_join(&mut self, mark: usize, region: &Type) -> bool {
        let Type::Var(rid) = self.arena.zonk(region) else {
            return false;
        };
        let pending: Vec<Type> = self.subsumptions[mark..]
            .iter()
            .map(|(_, r, _)| r.clone())
            .collect();
        pending
            .iter()
            .any(|r| matches!(self.arena.zonk(r), Type::Var(id) if id == rid))
    }

    /// Seed the environment with the primitive operators. `prims` must be the
    /// `(name, VarId)` pairs the resolver handed out for [`PRIMS`], in order.
    pub fn load_prelude(&mut self, prims: &[(InternedString, VarId)]) {
        debug_assert_eq!(prims.len(), PRIMS.len(), "prelude binding count mismatch");
        for (name, id) in prims {
            if let Some(scheme) = prim_scheme(name) {
                self.env.insert(*id, open_effects(scheme));
            }
            if &**name == "runSt" {
                self.run_st = Some(*id);
            }
        }
    }

    /// Bring a dependency package's exported schemes into scope.
    pub fn load_deps(&mut self, deps: &[(VarId, Scheme)]) {
        for (id, scheme) in deps {
            self.env.insert(*id, scheme.clone());
        }
    }

    /// Infer a module, one binding group at a time.
    ///
    /// `meadow-scc` has already put `decls` in dependency order and filled in
    /// [`hir::Module::groups`], so by the time a group is reached every binding it
    /// refers to is generalized and a mention of it instantiates properly. Within
    /// a recursive group the members stay monomorphic until all of them are
    /// solved — the usual binding-group discipline.
    pub fn infer_module(&mut self, module: &hir::LModule) {
        let module = module.value();
        self.declarations(module);
        if module.groups.is_empty() {
            // Ungrouped HIR (the SCC pass didn't run): source order is all we have.
            for decl in &module.decls {
                if let hir::Decl::Bind(bind) = decl.value() {
                    self.infer_bind(bind, true);
                }
            }
            return;
        }
        for group in &module.groups {
            self.infer_group(&module.decls, group);
        }
    }

    /// A module's declarations that are not bindings: a declaration is not
    /// itself a value -- the node type is only there so the table has no holes
    /// -- and a signature is kept for the binding it is of.
    fn declarations(&mut self, module: &hir::Module) {
        for decl in &module.decls {
            self.table.set(decl.id, Type::unit());
            if let hir::Decl::Sig(name, t, bounds) = decl.value() {
                self.sigs.insert(*name.value(), (t.clone(), bounds.clone()));
            }
        }
    }

    /// Infer a whole unit: each module's declarations, then its bindings group
    /// by group in `groups`' order -- each group a strongly connected
    /// component of the unit's bindings, members as `(module, declaration)`,
    /// in dependency order across modules (`meadow_scc::unit_groups`). Each
    /// binding is inferred with its own module's `filenames` entry, so a
    /// message says where, whichever module its group started in.
    pub fn infer_unit(
        &mut self,
        modules: &[&hir::LModule],
        filenames: &[String],
        groups: &[(Vec<(usize, usize)>, bool)],
    ) {
        for (m, file) in modules.iter().zip(filenames) {
            self.set_filename(file.clone());
            self.declarations(m.value());
        }
        for (members, recursive) in groups {
            let binds: Vec<(&hir::Bind, Span, String)> = members
                .iter()
                .filter_map(|&(m, i)| {
                    let decl = &modules[m].value().decls[i];
                    match decl.value() {
                        hir::Decl::Bind(bind) => Some((bind, decl.span, filenames[m].clone())),
                        _ => None,
                    }
                })
                .collect();
            self.infer_binds(&binds, *recursive);
        }
    }

    /// Infer one strongly connected component of the top-level bindings.
    fn infer_group(&mut self, decls: &[hir::LDecl], group: &hir::BindGroup) {
        let file = self.filename.clone();
        let binds: Vec<(&hir::Bind, Span, String)> = group
            .members
            .iter()
            .filter_map(|&i| match decls[i].value() {
                hir::Decl::Bind(bind) => Some((bind, decls[i].span, file.clone())),
                _ => None,
            })
            .collect();
        self.infer_binds(&binds, group.recursive);
    }

    /// Infer a group of bindings -- each with the file it was written in --
    /// that mention one another if `recursive`.
    fn infer_binds(&mut self, binds: &[(&hir::Bind, Span, String)], recursive: bool) {
        // A lone non-recursive binding is just a binding.
        if !recursive && binds.len() == 1 {
            self.set_filename(binds[0].2.clone());
            self.infer_bind(binds[0].0, true);
            return;
        }

        let started = self.begin_binding();
        self.arena.enter_level();
        let mark = self.subsumptions.len();

        // Seed every name in the group before inferring any body, so a mention of
        // a sibling resolves to a variable the sibling's own inference constrains
        // rather than to an unrelated fresh one.
        let mut seeds: Vec<Vec<(VarId, Type)>> = Vec::with_capacity(binds.len());
        for (bind, span, _) in binds {
            // A function is seeded as the function it is -- a pure arrow for
            // each parameter but the last, whose effect is its body's -- not as
            // a bare variable. Otherwise a sibling applying it to fewer
            // arguments than it takes, `map (f k) xs`, would guess that
            // application might perform something and join that guess to its
            // own effect -- and when `f` turned out to be pure until its last
            // argument, the guess would close the caller's effect along with it.
            let arity = match bind {
                hir::Bind::Fun(_, params, _, _) => params.len(),
                _ => 0,
            };
            let seeded: Vec<(VarId, Type)> = bind
                .bound_vars()
                .into_iter()
                .map(|vid| {
                    let ty = if arity > 0 {
                        let params = (0..arity).map(|_| self.arena.fresh()).collect();
                        let ret = self.arena.fresh();
                        let eff = self.arena.fresh_effect();
                        Type::func_eff(params, ret, eff)
                    } else {
                        self.arena.fresh()
                    };
                    self.bind_mono(vid, &ty, *span);
                    self.seed_sig(vid, &ty);
                    (vid, ty)
                })
                .collect();
            seeds.push(seeded);
        }

        // Every member of the group has to be pure for any of them to generalize:
        // the value restriction, applied to the group as a whole.
        let mut pure = true;
        self.tr.group = seeds.iter().flatten().map(|(v, _)| *v).collect();
        for ((bind, _, file), seed) in binds.iter().zip(&seeds) {
            self.set_filename(file.clone());
            self.tr.member = seed.first().map(|(v, _)| *v);
            pure &= self.infer_group_member(bind, seed);
        }
        self.tr.member = None;

        // A top-level group settles its own names: its type is final once it
        // generalizes, and an overload left open would leave it half-inferred.
        self.solve_overloads(true);
        // And what its arguments perform, as a lone binding's is: left pending,
        // a closure given to `atomically` inside a recursive function was never
        // held to the `{ Stm }` it is allowed.
        self.solve_subsumptions(mark);
        self.solve_wanted();
        self.arena.exit_level();

        // Only functions take dictionaries: a `def` that did would be made
        // again at every use, where it is promised to be made once.
        let functions = binds
            .iter()
            .all(|(b, _, _)| matches!(b, hir::Bind::Fun(..)));
        let members: Vec<(VarId, Type)> = seeds.iter().flatten().cloned().collect();
        let mut preds = self.close_binding(started, &members, pure && functions);

        for ((bind, _, file), seed) in binds.iter().zip(&seeds) {
            self.set_filename(file.clone());
            let classes = matches!(bind, hir::Bind::Fun(..));
            for (vid, ty) in seed {
                let scheme = if pure {
                    let mine = preds.remove(vid).unwrap_or_default();
                    self.generalize_named(Some(*vid), ty, classes, &mine)
                } else {
                    let s = Scheme::mono(self.arena.zonk(ty));
                    self.record_mono(*vid, &s);
                    s
                };
                self.check_sig(*vid, &scheme);
                self.env.insert(*vid, scheme);
                self.exports.push(*vid);
            }
        }
    }

    /// One member of a recursive group: infer its body and tie the result to the
    /// seed the rest of the group is seeing. Levels and generalization are the
    /// group's business, not this function's. Returns whether it was pure.
    fn infer_group_member(&mut self, bind: &hir::Bind, seed: &[(VarId, Type)]) -> bool {
        match bind {
            hir::Bind::Fun(name, params, declared, body) => {
                // The parameters are matched when the function is called: a
                // view in one does what the body does.
                let body_eff = self.arena.fresh_effect();
                let saved = std::mem::replace(&mut self.cur_effect, body_eff.clone());
                let mut bound = Vec::new();
                let param_tys: Vec<Type> = params
                    .iter()
                    .map(|p| self.infer_pat(p, &mut bound))
                    .collect();
                let (ret, declared_eff) = self.declared_result(declared);
                if let Some(eff) = declared_eff {
                    self.unify_at(body.span, body_eff.clone(), eff);
                }

                let fn_ty = Type::func_eff(param_tys, ret.clone(), body_eff);
                if let Some((_, seed_ty)) = seed.first() {
                    self.unify_at(name.span, seed_ty.clone(), fn_ty.clone());
                }
                self.table.set(name.id, fn_ty.clone());

                let body_ty = self.infer_expr(body);
                self.unify_at(body.span, ret, body_ty);
                self.cur_effect = saved;
                self.table.set(name.id, self.arena.zonk(&fn_ty));
                // Defining a function performs no effects.
                true
            }
            hir::Bind::Pat(pat, expr) => {
                let rhs_eff = self.arena.fresh_effect();
                let saved = std::mem::replace(&mut self.cur_effect, rhs_eff.clone());
                let mark = self.subsumptions.len();
                let rhs = self.infer_expr(expr);
                let mut bound = Vec::new();
                let pty = self.infer_pat(pat, &mut bound);
                self.unify_at(pat.span, pty, rhs);
                self.cur_effect = saved;

                // `infer_pat` gave each name a fresh variable of its own; the group
                // has been looking at the seed instead.
                let vars: Vec<VarId> = bound.iter().map(|(v, _)| *v).collect();
                for (vid, vty) in bound {
                    if let Some((_, seed_ty)) = seed.iter().find(|(v, _)| *v == vid) {
                        self.unify_at(pat.span, seed_ty.clone(), vty);
                    }
                }
                // Before the purity test, as for a lone `def`.
                self.solve_subsumptions(mark);
                self.check_pure_def(&vars, &rhs_eff, expr.span);
                matches!(self.arena.zonk(&rhs_eff), Type::RowEmpty | Type::Var(_))
            }
            hir::Bind::Error => true,
        }
    }

    pub fn finish(mut self) -> InferResult {
        // The whole unit has been seen: whatever is still ambiguous stays so.
        self.solve_overloads(true);
        // A variable some `fun` quantified over is that function's parameter,
        // not a number waiting for a type: it stays a variable.
        let quantified: HashSet<u32> = self
            .generalized
            .values()
            .flat_map(|g| g.vars.iter().copied())
            .collect();
        self.arena.default_num_vars(&quantified);
        // A number nothing pinned down has its type now, and with it its
        // `impl`. Whatever is wanted of a type still unknown stays unknown.
        let evidence = self.finish_wanted();
        self.table.zonk_all(&mut self.arena);
        let mut schemes = HashMap::new();
        for id in self.exports.clone() {
            if let Some(scheme) = self.env.get(&id).cloned() {
                schemes.insert(id, self.normalize_scheme(&scheme));
            }
        }
        // Re-zonk: a variable can be solved after the binding that generalized
        // over it was recorded, and core's annotations have to agree with the
        // table's.
        let recorded: Vec<(VarId, Generalized)> = self
            .generalized
            .iter()
            .map(|(v, g)| (*v, g.clone()))
            .collect();
        let generalized = recorded
            .iter()
            .map(|(v, g)| {
                (
                    *v,
                    Generalized {
                        scheme: self.normalize_scheme(&g.scheme),
                        vars: g.vars.clone(),
                    },
                )
            })
            .collect();
        InferResult {
            table: self.table,
            schemes,
            generalized,
            variants: self.variants,
            resolutions: self.resolutions,
            evidence,
            traits: self.tr.env(),
            errors: self.errors,
        }
    }

    /// Re-zonk a scheme's body so exported schemes never leak arena indices from a
    /// var that got bound after generalization (can't happen for a closed
    /// top-level binding, but keeps exports self-contained regardless).
    fn normalize_scheme(&mut self, scheme: &Scheme) -> Scheme {
        Scheme {
            quant: scheme.quant.clone(),
            preds: scheme
                .preds
                .iter()
                .map(|p| Pred {
                    tr: p.tr,
                    tys: p.tys.iter().map(|t| self.arena.zonk(t)).collect(),
                    assocs: p.assocs.iter().map(|a| self.arena.zonk(a)).collect(),
                })
                .collect(),
            ty: self.arena.zonk(&scheme.ty),
            lacks: scheme.lacks.clone(),
        }
    }

    // --- bindings ----------------------------------------------------------

    /// Bind `vid` to `ty` monomorphically, first tying `ty` to whatever
    /// placeholder an earlier forward reference left behind for the name (see the
    /// `Expr::Var` arm). For everything else — every local, and every top-level
    /// binding the sort managed to order — there is no prior entry and this is a
    /// plain insert.
    fn bind_mono(&mut self, vid: VarId, ty: &Type, span: Span) {
        if let Some(prev) = self.env.get(&vid).cloned()
            && prev.quant.is_empty()
        {
            self.unify_at(span, prev.ty, ty.clone());
        }
        self.env.insert(vid, Scheme::mono(ty.clone()));
    }

    fn infer_bind(&mut self, bind: &hir::Bind, toplevel: bool) {
        match bind {
            hir::Bind::Fun(name, params, declared, body) => {
                let vid = *name.value();
                let started = toplevel.then(|| self.begin_binding());
                if toplevel {
                    self.tr.group = [vid].into_iter().collect();
                    self.tr.member = Some(vid);
                }
                self.arena.enter_level();

                // The body runs in its own effect region; that region ends up on the
                // function's (innermost) arrow. Defining the function is itself pure,
                // so the outer `cur_effect` is untouched. `: R ! e` says what the
                // region is. The parameters are matched in it too, when the
                // function is called: a view in one does what the body does.
                let body_eff = self.arena.fresh_effect();
                let saved = std::mem::replace(&mut self.cur_effect, body_eff.clone());
                // Parameters are patterns (`fun f a (x, y) = …`); inferring each
                // binds the variables it introduces.
                let mut bound = Vec::new();
                let param_tys: Vec<Type> = params
                    .iter()
                    .map(|p| self.infer_pat(p, &mut bound))
                    .collect();
                let (ret, declared_eff) = self.declared_result(declared);
                if let Some(eff) = declared_eff {
                    self.unify_at(body.span, body_eff.clone(), eff);
                }
                let body_eff_of_point_free = body_eff.clone();

                // curried: `fun f a b = e` is `a -> b -> typeof(e) ! <body effect>`
                let fn_ty = Type::func_eff(param_tys, ret.clone(), body_eff);
                // Bind the name monomorphically first so the body can recurse.
                self.bind_mono(vid, &fn_ty, name.span);
                self.seed_sig(vid, &fn_ty);
                self.table.set(name.id, fn_ty.clone());

                let mark = self.subsumptions.len();
                let body_ty = self.infer_expr(body);
                self.unify_at(body.span, ret, body_ty);
                self.cur_effect = saved;
                // A local function may leave a name to the body around it; a
                // top-level one has nothing around it to wait for.
                self.solve_overloads(toplevel);
                self.solve_subsumptions(mark);
                self.solve_wanted();
                self.arena.exit_level();
                // A point-free `fun f = e` runs its body wherever it is used,
                // which a use cannot be seen to do: like a `def`'s, its body
                // may not perform effects.
                if params.is_empty() {
                    let waiting = self.has_pending_join(mark, &body_eff_of_point_free);
                    let pure = !waiting
                        && match self.arena.zonk(&body_eff_of_point_free) {
                            Type::RowEmpty => true,
                            Type::Var(id) => self.arena.slot_level(id) > self.arena.level,
                            _ => false,
                        };
                    if !pure {
                        self.check_pure_def(&[vid], &body_eff_of_point_free, body.span);
                    }
                }

                let preds = match started {
                    // A top-level function takes a dictionary for each trait
                    // its type variables turned out to need.
                    Some(started) => {
                        self.tr.member = None;
                        self.close_binding(started, &[(vid, fn_ty.clone())], true)
                            .remove(&vid)
                            .unwrap_or_default()
                    }
                    // A local one does not: what it needs of a type it cannot
                    // say is left to the function around it, whose variable
                    // that then is.
                    None => {
                        self.hold_back_wanted();
                        Vec::new()
                    }
                };
                let scheme = self.generalize_named(Some(vid), &fn_ty, true, &preds);
                self.check_sig(vid, &scheme);
                self.table.set(name.id, self.arena.zonk(&fn_ty));
                self.env.insert(vid, scheme);
                if toplevel {
                    self.exports.push(vid);
                }
            }
            hir::Bind::Pat(pat, expr) => {
                // Infer rhs *and* the pattern at the raised level, so unifying them
                // doesn't drag the rhs's fresh vars down out of generalization.
                self.arena.enter_level();
                let rhs_eff = self.arena.fresh_effect();
                let saved = std::mem::replace(&mut self.cur_effect, rhs_eff.clone());
                let mark = self.subsumptions.len();
                let rhs = self.infer_expr(expr);
                let mut bound = Vec::new();
                let pty = self.infer_pat(pat, &mut bound);
                self.unify_at(pat.span, pty, rhs);
                for (vid, vty) in &bound {
                    self.seed_sig(*vid, vty);
                }
                self.cur_effect = saved;
                self.solve_overloads(toplevel);
                // Before the purity test below, which reads `rhs_eff`: an
                // argument's effect still waiting to be tied in could make an
                // effectful right-hand side look pure.
                self.solve_subsumptions(mark);
                self.solve_wanted();
                self.arena.exit_level();
                // A value is made once, so it takes no dictionary: a type it
                // needs a trait of is not its to generalize.
                self.hold_back_wanted();

                // The value restriction, replaced: generalize a `let`/`def` binding
                // only when its right-hand side is pure. `def r = ref []` is
                // effectful ⇒ `r` stays monomorphic (and can't be misused
                // polymorphically); `def id = \x -> x` is pure ⇒ generalized.
                //
                // "Pure" is a closed empty row, or a row *variable* that nothing
                // outside this binding can still constrain -- one whose level is
                // above the current one, which is what an untouched fresh variable
                // looks like. A variable is not pure in itself: in `let _ = f x`
                // the rhs's row is unified with `f`'s latent effect, a variable
                // that lives at the parameter's level and means "whatever `f`
                // does". Counting that as pure dropped the effect from the
                // enclosing function's type -- so a `Log`-performing call could
                // pass for a pure one -- and generalized `let r = f ()` even when
                // `f` turned out to allocate a `Ref`.
                let waiting = self.has_pending_join(mark, &rhs_eff);
                let pure = !waiting
                    && match self.arena.zonk(&rhs_eff) {
                        Type::RowEmpty => true,
                        Type::Var(id) => self.arena.slot_level(id) > self.arena.level,
                        _ => false,
                    };
                // A pure right-hand side's row is a variable nothing outside can
                // constrain, and nothing inside will again: it is empty, and is
                // said to be. Left open, it stays unsolved in whatever was
                // instantiated inside -- `let r = f x` recorded `f` at an effect
                // no binder anywhere binds. Unless the bound type mentions it,
                // where it is the binding's own to generalize.
                if pure && let Type::Var(id) = self.arena.zonk(&rhs_eff) {
                    let mut free = Vec::new();
                    for (_, vty) in &bound {
                        self.arena.free_vars(vty, &mut free);
                    }
                    if !free.contains(&id) {
                        self.unify_at(pat.span, rhs_eff.clone(), Type::RowEmpty);
                    }
                }
                if !pure && !toplevel {
                    // let the enclosing region see the rhs's effects
                    let region = self.cur_effect.clone();
                    self.join_effect_into(pat.span, region, rhs_eff);
                } else if !pure {
                    let vars: Vec<VarId> = bound.iter().map(|(v, _)| *v).collect();
                    self.check_pure_def(&vars, &rhs_eff, expr.span);
                }

                for (vid, vty) in bound {
                    let scheme = if pure {
                        self.generalize_named(Some(vid), &vty, false, &[])
                    } else {
                        // Not generalized, so not the `let`'s own either: its
                        // variables belong to the level around it. Left one
                        // level in, they would look like they were made inside
                        // anything that level is reused for -- a `runSt` among
                        // them, which could then smuggle its state out through
                        // this binding (`setRef outer [cell;]`).
                        let level = self.arena.level;
                        self.arena.hold_back(&vty, level);
                        let s = Scheme::mono(self.arena.zonk(&vty));
                        self.record_mono(vid, &s);
                        s
                    };
                    self.check_sig(vid, &scheme);
                    self.env.insert(vid, scheme);
                    if toplevel {
                        self.exports.push(vid);
                    }
                }
            }
            hir::Bind::Error => {}
        }
    }

    // --- expressions -----------------------------------------------------------

    fn infer_expr(&mut self, expr: &hir::LExpr) -> Type {
        let ty = self.infer_expr_inner(expr);
        self.table.set(expr.id, ty.clone());
        ty
    }

    fn infer_expr_inner(&mut self, expr: &hir::LExpr) -> Type {
        match expr.value() {
            hir::Expr::Lit(hir::Lit::Int(_)) => self.arena.fresh_num(),
            hir::Expr::Lit(hir::Lit::Float(_)) => self.arena.fresh_in(VarKind::Frac),
            hir::Expr::Lit(hir::Lit::String(_)) => Type::string(),
            hir::Expr::Lit(hir::Lit::Char(_)) => Type::char(),
            hir::Expr::Unit => Type::unit(),

            hir::Expr::Var(ident) if self.overloads.contains_key(&ident.id) => {
                let ty = self.defer(ident.id, ident.span, vec![expr.id, ident.id]);
                self.table.set(ident.id, ty.clone());
                ty
            }
            hir::Expr::Var(ident) => {
                let ty = match self.env.get(&*ident.value()).cloned() {
                    Some(scheme) => {
                        self.mention_in_group(ident.id, *ident.value());
                        let ty = self.instantiate_at(ident.id, ident.span, &scheme);
                        self.open_arrows(ty)
                    }
                    None => {
                        // Either an unresolved name (the resolver has already said
                        // so) or a top-level binding in a dependency cycle that
                        // `meadow-scc` could not break — mutually recursive
                        // modules, in practice. Give the name one shared
                        // monomorphic placeholder rather than an unrelated fresh
                        // variable per mention: that is what makes the constraints
                        // from the uses meet the eventual definition instead of
                        // being silently discarded. The outermost level keeps any
                        // enclosing binding from generalizing over it, which would
                        // be a promise of polymorphism the definition never made.
                        let ty = self.arena.fresh_global();
                        self.env.insert(*ident.value(), Scheme::mono(ty.clone()));
                        ty
                    }
                };
                self.table.set(ident.id, ty.clone());
                ty
            }

            hir::Expr::Lam(params, body) => {
                // Multi-parameter lambdas curry: `\a b -> e` is `\a -> \b -> e`.
                // The body has its own effect region, which lands on the arrow;
                // building a closure is pure, so the ambient effect is untouched.
                let body_eff = self.arena.fresh_effect();
                let saved = std::mem::replace(&mut self.cur_effect, body_eff.clone());
                let mut bound = Vec::new();
                let ptys: Vec<Type> = params
                    .iter()
                    .map(|p| self.infer_pat(p, &mut bound))
                    .collect();
                let bty = self.infer_expr(body);
                self.cur_effect = saved;
                Type::func_eff(ptys, bty, body_eff)
            }

            hir::Expr::App(func, args) => {
                // n-ary application is a fold of single-argument applications. Only
                // the final (saturating) call actually runs the function body, so
                // only its latent effect joins the current region; the intermediate
                // arrows of a curried call just build closures and stay pure — which
                // is also how every function type here is constructed (`func_eff`).
                let run_st = match func.value() {
                    hir::Expr::Var(ident) => {
                        Some(*ident.value()) == self.run_st && !args.is_empty()
                    }
                    _ => false,
                };
                let (mut fty, rest) = if run_st {
                    let result = self.infer_run_st(expr.span, func, &args[0]);
                    (result, &args[1..])
                } else {
                    (self.infer_expr(func), &args[..])
                };
                let args = rest;
                let last = args.len().saturating_sub(1);
                for (i, arg) in args.iter().enumerate() {
                    let aty = self.infer_expr(arg);
                    let ret = self.arena.fresh();
                    let phi = self.arena.fresh_effect();
                    match self.arena.zonk(&fty) {
                        // A callee whose parameter is known to be a function:
                        // the argument need only do less than it allows.
                        Type::Fun(params, fret, feff)
                            if params.len() == 1 && params[0].is_fun() =>
                        {
                            let loose = self.loosen_parameter(arg.span, params[0].clone());
                            self.unify_at(arg.span, aty, loose);
                            self.unify_at(expr.span, *fret, ret.clone());
                            self.unify_at(expr.span, *feff, phi.clone());
                        }
                        fty => self.unify_at(
                            expr.span,
                            fty,
                            Type::Fun(vec![aty], Box::new(ret.clone()), Box::new(phi.clone())),
                        ),
                    }
                    if i == last {
                        // A callee from an enclosing level -- a function
                        // parameter, called inside a `let` or a `runSt` -- does
                        // what its caller passed, which is only *part* of this
                        // region. Tie it in once the region is known, as an
                        // argument's effect is, so that a `St s` the region
                        // gains later is not taken for part of the callback.
                        let outer = match self.arena.zonk(&phi) {
                            Type::Var(id) => self.arena.slot_level(id) < self.arena.level,
                            _ => false,
                        };
                        if outer {
                            let region = self.cur_effect.clone();
                            self.subsumptions.push((phi, region, expr.span));
                        } else {
                            self.join_effect(expr.span, phi);
                        }
                    } else {
                        self.unify_at(expr.span, phi, Type::RowEmpty);
                    }
                    fty = ret;
                }
                fty
            }

            hir::Expr::Let(binds, body) => {
                for b in binds {
                    self.infer_bind(b, false);
                }
                self.infer_expr(body)
            }

            hir::Expr::If(cond, then_branch, else_branch) => {
                let ct = self.infer_expr(cond);
                self.unify_at(cond.span, ct, Type::bool());
                let tt = self.infer_expr(then_branch);
                let et = self.infer_expr(else_branch);
                self.unify_at(expr.span, tt.clone(), et);
                tt
            }

            hir::Expr::Match(scrut, arms) => {
                let st = self.infer_expr(scrut);
                let result = self.arena.fresh();
                for (pat, guard, arm) in arms {
                    let mut bound = Vec::new();
                    let pt = self.infer_pat(pat, &mut bound);
                    self.unify_at(pat.span, pt, st.clone());
                    if let Some(g) = guard {
                        let gt = self.infer_expr(g);
                        self.unify_at(g.span, gt, Type::bool());
                    }
                    let at = self.infer_expr(arm);
                    self.unify_at(arm.span, at, result.clone());
                }
                result
            }

            hir::Expr::Tuple(items) => {
                Type::Tuple(items.iter().map(|e| self.infer_expr(e)).collect())
            }

            hir::Expr::Array(items) => {
                let elem = self.arena.fresh();
                for e in items {
                    let t = self.infer_expr(e);
                    self.unify_at(e.span, t, elem.clone());
                }
                Type::array(elem)
            }

            hir::Expr::List(items) => {
                let elem = self.arena.fresh();
                for e in items {
                    let t = self.infer_expr(e);
                    self.unify_at(e.span, t, elem.clone());
                }
                Type::list(elem)
            }

            hir::Expr::Cons(label, args) => {
                let name = *label.value();
                let known = if self.overloads.contains_key(&label.id) {
                    Some(self.defer(label.id, label.span, vec![expr.id, label.id]))
                } else {
                    self.ctor_type(name)
                };
                match known {
                    Some(mut cty) => {
                        self.table.set(label.id, cty.clone());
                        // apply the constructor to its arguments, one at a time
                        // (constructors are pure — the fresh effect var unifies away)
                        for arg in args {
                            let aty = self.infer_expr(arg);
                            let ret = self.arena.fresh();
                            let eff = self.arena.fresh_effect();
                            self.unify_at(
                                arg.span,
                                cty,
                                Type::Fun(vec![aty], Box::new(ret.clone()), Box::new(eff)),
                            );
                            cty = ret;
                        }
                        cty
                    }
                    None => {
                        // Unknown constructor, which the resolver has reported.
                        for a in args {
                            self.infer_expr(a);
                        }
                        Type::Error
                    }
                }
            }

            hir::Expr::Record(fields, base) => {
                let labels = self.distinct_labels(fields);
                let mut row = match base {
                    Some(b) => {
                        let bt = self.infer_expr(b);
                        let r = self.arena.fresh_row();
                        self.unify_at(b.span, bt, Type::Record(Box::new(r.clone())));
                        // `{ x = 1 | r }` adds `x`, so `r` may not have one:
                        // a record never holds a label twice.
                        if let Err(err) = self.arena.require_lacks(r.clone(), &labels) {
                            let diag = self.unify_diagnostic(expr.span, err);
                            self.errors.push(diag);
                        }
                        r
                    }
                    None => Type::RowEmpty,
                };
                for (label, val) in fields.iter().rev() {
                    let vt = self.infer_expr(val);
                    self.table.set(label.id, vt.clone());
                    row = Type::RowExtend(*label.value(), Box::new(vt), Box::new(row));
                }
                Type::Record(Box::new(row))
            }

            hir::Expr::Update(base, fields) => self.infer_update(base, fields),

            hir::Expr::Field(obj, label) => {
                let ot = self.infer_expr(obj);
                let fname = *label.value();
                let pruned = self.arena.zonk(&ot);

                // Nominal record / data type with this field: use its accessor type.
                let nominal = if let Type::Con(tyname, _) = &pruned {
                    self.record_fields
                        .get(tyname)
                        .and_then(|m| m.get(&fname))
                        .cloned()
                } else {
                    None
                };

                let result = match nominal {
                    Some(scheme) => {
                        let accessor = self.instantiate(&scheme);
                        let res = self.arena.fresh();
                        let eff = self.arena.fresh_effect();
                        self.unify_at(
                            expr.span,
                            accessor,
                            Type::Fun(vec![pruned], Box::new(res.clone()), Box::new(eff)),
                        );
                        res
                    }
                    None => {
                        // Structural: `obj : { fname : field | rest }`.
                        let field = self.arena.fresh();
                        let rest = self.arena.fresh_row();
                        let want = Type::Record(Box::new(Type::RowExtend(
                            fname,
                            Box::new(field.clone()),
                            Box::new(rest),
                        )));
                        self.unify_at(expr.span, ot, want);
                        field
                    }
                };
                self.table.set(label.id, result.clone());
                result
            }

            hir::Expr::Handle(body, arms, ret) => {
                // The handled expression runs in its own effect region.
                let body_eff = self.arena.fresh_effect();
                let saved = std::mem::replace(&mut self.cur_effect, body_eff.clone());
                let body_ty = self.infer_expr(body);
                self.cur_effect = saved;

                let result = self.arena.fresh();

                // Every effect the clauses name, in order of first mention, each
                // with fresh type arguments. One handler may answer operations of
                // several effects.
                let mut handled: Vec<(InternedString, EffectInfo, Vec<Type>)> = Vec::new();
                // Which effect each clause answers, decided once.
                let arm_effects: Vec<Option<InternedString>> =
                    arms.iter().map(|arm| self.arm_effect(arm)).collect();
                for name in arm_effects.iter().copied() {
                    let Some(name) = name else {
                        continue;
                    };
                    if handled.iter().any(|(n, ..)| *n == name) {
                        continue;
                    }
                    if let Some(info) = self.effects.get(&name).cloned() {
                        let params = (0..info.params).map(|_| self.arena.fresh()).collect();
                        handled.push((name, info, params));
                    }
                }

                // The body performs the handled effects and whatever else, `ρ`.
                //
                // What reaches the enclosing region is `ρ` plus every handled
                // effect with an operation that has no clause here: at run time
                // such an operation goes straight past this handler to the next
                // one out, so the effect is still performed as far as anyone
                // outside can tell. Dropping it -- or, before, dropping the whole
                // body's row when there was no clause at all -- let an
                // `unhandled effect` through a type that claimed to be pure.
                let rho = self.arena.fresh_effect();
                let row = |effects: &mut dyn Iterator<
                    Item = &(InternedString, EffectInfo, Vec<Type>),
                >| {
                    let effects: Vec<_> = effects.collect();
                    effects
                        .into_iter()
                        .rev()
                        .fold(rho.clone(), |tail, (name, _, params)| {
                            Type::RowExtend(
                                *name,
                                Box::new(Type::Tuple(params.clone())),
                                Box::new(tail),
                            )
                        })
                };
                let inside = row(&mut handled.iter());
                let answered = |effect: InternedString, op: InternedString| {
                    arms.iter()
                        .zip(&arm_effects)
                        .any(|(a, e)| a.op == op && *e == Some(effect))
                };
                let outside = row(&mut handled
                    .iter()
                    .filter(|(name, info, _)| info.ops.iter().any(|o| !answered(*name, o.name))));
                self.unify_at(expr.span, body_eff, inside);
                self.join_effect(expr.span, outside.clone());

                for (arm, effect) in arms.iter().zip(&arm_effects) {
                    let op = handled
                        .iter()
                        .filter(|(name, ..)| Some(*name) == *effect)
                        .find_map(|(_, info, params)| {
                            info.ops
                                .iter()
                                .find(|o| o.name == arm.op)
                                .map(|o| (o, params))
                        });
                    let (arg_ty, ret_ty) = match op {
                        Some((o, params)) => (
                            Arena::subst_bound(&o.arg, params),
                            Arena::subst_bound(&o.ret, params),
                        ),
                        // An unknown operation, which the resolver has reported.
                        None => (self.arena.fresh(), self.arena.fresh()),
                    };
                    let mut bound = Vec::new();
                    let pty = self.infer_pat(&arm.param, &mut bound);
                    self.unify_at(arm.param.span, pty, arg_ty);
                    // resume : op-result -> handler-result ! outside   (deep: the
                    // rest of the body runs under this same handler again)
                    let k_ty = Type::Fun(
                        vec![ret_ty],
                        Box::new(result.clone()),
                        Box::new(outside.clone()),
                    );
                    // Recorded, so that lowering can give the variable its type.
                    self.table.set(arm.resume.id, k_ty.clone());
                    self.env.insert(*arm.resume.value(), Scheme::mono(k_ty));
                    let at = self.infer_expr(&arm.body);
                    self.unify_at(arm.body.span, at, result.clone());
                }

                match ret {
                    Some((pat, rbody)) => {
                        let mut bound = Vec::new();
                        let pty = self.infer_pat(pat, &mut bound);
                        self.unify_at(pat.span, pty, body_ty);
                        let rt = self.infer_expr(rbody);
                        self.unify_at(rbody.span, rt, result.clone());
                    }
                    None => self.unify_at(expr.span, result.clone(), body_ty),
                }
                result
            }

            hir::Expr::Error => Type::Error,
        }
    }

    // --- patterns ------------------------------------------------------------

    fn infer_pat(&mut self, pat: &hir::LPat, bound: &mut Vec<(VarId, Type)>) -> Type {
        let ty = self.infer_pat_inner(pat, bound);
        self.table.set(pat.id, ty.clone());
        ty
    }

    fn infer_pat_inner(&mut self, pat: &hir::LPat, bound: &mut Vec<(VarId, Type)>) -> Type {
        match pat.value() {
            // `(f -> p)`: what is matched here is `f`'s argument, and `p`
            // matches its answer. Applying it is done where the match is, as
            // any call there would be.
            hir::Pat::View(f, p) => {
                let fty = self.infer_expr(f);
                let pt = self.infer_pat(p, bound);
                let arg = self.arena.fresh();
                let phi = self.arena.fresh_effect();
                self.unify_at(
                    f.span,
                    fty,
                    Type::Fun(vec![arg.clone()], Box::new(pt), Box::new(phi.clone())),
                );
                self.join_effect(f.span, phi);
                arg
            }
            hir::Pat::Wildcard => self.arena.fresh(),
            hir::Pat::Unit => Type::unit(),
            hir::Pat::Lit(hir::Lit::Int(_)) => self.arena.fresh_num(),
            hir::Pat::Lit(hir::Lit::Float(_)) => self.arena.fresh_in(VarKind::Frac),
            hir::Pat::Lit(hir::Lit::String(_)) => Type::string(),
            hir::Pat::Lit(hir::Lit::Char(_)) => Type::char(),

            hir::Pat::Var(ident) => {
                let vid = *ident.value();
                let t = self.arena.fresh();
                self.bind_mono(vid, &t, ident.span);
                self.table.set(ident.id, t.clone());
                bound.push((vid, t.clone()));
                t
            }

            // `(p : T)` — infer the pattern, then hold it to the annotation.
            //
            // A type variable written here is *not* one of an enclosing
            // declaration's parameters: there is no enclosing declaration to be
            // a parameter of. Each distinct one is quantified over the
            // annotation alone and then instantiated, so `(x : a)` constrains
            // nothing and `(f : a -> a)` constrains the two ends to agree —
            // which is what writing it twice is for.
            hir::Pat::Ann(inner, ann) => {
                let inferred = self.infer_pat(inner, bound);
                let declared = self.annotation(ann);
                self.unify_at(pat.span, inferred.clone(), declared);
                inferred
            }

            hir::Pat::As(ident, sub) => {
                let st = self.infer_pat(sub, bound);
                let vid = *ident.value();
                self.bind_mono(vid, &st, ident.span);
                self.table.set(ident.id, st.clone());
                bound.push((vid, st.clone()));
                st
            }

            hir::Pat::Cons(label, args) => {
                let name = *label.value();
                let known = if self.overloads.contains_key(&label.id) {
                    Some(self.defer(label.id, label.span, vec![pat.id, label.id]))
                } else {
                    self.ctor_type(name)
                };
                match known {
                    Some(mut cty) => {
                        for sub in args {
                            let sty = self.infer_pat(sub, bound);
                            let ret = self.arena.fresh();
                            let eff = self.arena.fresh_effect();
                            self.unify_at(
                                sub.span,
                                cty,
                                Type::Fun(vec![sty], Box::new(ret.clone()), Box::new(eff)),
                            );
                            cty = ret;
                        }
                        self.table.set(label.id, cty.clone());
                        cty
                    }
                    None => {
                        // Unknown constructor, which the resolver has reported.
                        for a in args {
                            self.infer_pat(a, bound);
                        }
                        Type::Error
                    }
                }
            }

            hir::Pat::Tuple(items) => {
                Type::Tuple(items.iter().map(|p| self.infer_pat(p, bound)).collect())
            }

            hir::Pat::Array(items) => {
                let elem = self.arena.fresh();
                for p in items {
                    let t = self.infer_pat(p, bound);
                    self.unify_at(p.span, t, elem.clone());
                }
                Type::array(elem)
            }

            hir::Pat::List(items) => {
                let elem = self.arena.fresh();
                for p in items {
                    let t = self.infer_pat(p, bound);
                    self.unify_at(p.span, t, elem.clone());
                }
                Type::list(elem)
            }

            hir::Pat::Record(fields, open) => {
                let labels = self.distinct_labels(fields);
                let mut row = if *open {
                    let rest = self.arena.fresh_row();
                    // The rest of `{ x | .. }` is what is not `x`.
                    let _ = self.arena.require_lacks(rest.clone(), &labels);
                    rest
                } else {
                    Type::RowEmpty
                };
                for (label, sub) in fields.iter().rev() {
                    let st = self.infer_pat(sub, bound);
                    self.table.set(label.id, st.clone());
                    row = Type::RowExtend(*label.value(), Box::new(st), Box::new(row));
                }
                Type::Record(Box::new(row))
            }

            hir::Pat::Error => Type::Error,
        }
    }

    // --- helpers -------------------------------------------------------------

    /// The (fully instantiated) type of a data constructor: user-declared
    /// constructors first, then the hard-wired built-ins.
    ///
    /// - `Nil  : List a`
    /// - `Cons : a -> List a -> List a`
    /// - `True`, `False : Bool`
    ///
    /// Its arrows are opened as a named function's are ([`Self::open_arrows`]):
    /// building a value performs nothing, so a constructor passed to `V.map`
    /// in a function that performs `Fetch` is an `Int -> K ! { Fetch | e }`,
    /// as a lambda written there would be.
    fn ctor_type(&mut self, name: InternedString) -> Option<Type> {
        let ty = if let Some(scheme) = self.ctors.get(&name).cloned() {
            self.instantiate(&scheme)
        } else {
            match &*name {
                "List.Nil" => Type::list(self.arena.fresh()),
                "List.Cons" => {
                    let a = self.arena.fresh();
                    Type::func(vec![a.clone(), Type::list(a.clone())], Type::list(a))
                }
                "Bool.True" | "Bool.False" => Type::bool(),
                _ => return None,
            }
        };
        Some(self.open_arrows(ty))
    }

    /// Export the operations of every `effect` in `decls`, so a dependent can
    /// reach them by qualifier (`State.get`) or name them in a `use`.
    ///
    /// [`register_types`] gives every operation of every dependency a scheme in
    /// the environment, which is what makes them callable unqualified — but that
    /// happens for dependencies too, so it cannot tell which are *this* unit's to
    /// export. The caller knows, and calls this with only the local modules.
    ///
    /// [`register_types`]: Self::register_types
    pub fn export_effect_ops(&mut self, decls: &[hir::LDecl]) {
        for d in decls {
            if let hir::Decl::Effect(ed) = d.value() {
                for (_, op, _) in &ed.ops {
                    self.exports.push(*op.value());
                }
            }
        }
    }

    /// The `impl`s among `decls`. After [`Infer::register_types`] has seen every
    /// module of the unit, since an `impl` may be of a sibling's trait.
    pub fn register_impls(&mut self, decls: &[hir::LDecl]) {
        // An `impl` of a trait after those of the traits it requires, whatever
        // the order they were written in: `impl Visual T` reads `Token T` off
        // `impl Stream T`, which has to be known by then.
        let mut impls: Vec<&hir::ImplDecl> = decls
            .iter()
            .filter_map(|d| match d.value() {
                hir::Decl::Impl(id) => Some(id),
                _ => None,
            })
            .collect();
        impls.sort_by_key(|id| self.trait_depth(*id.tr.value(), 0));
        for id in impls {
            self.register_impl(id);
        }
    }

    /// Populate `ctors` / `record_fields` from `data` / `record` declarations.
    /// Call before `infer_module`.
    pub fn register_types(&mut self, decls: &[hir::LDecl]) {
        // Aliases before everything else, which may be written in terms of them.
        let mut added = Vec::new();
        for d in decls {
            match d.value() {
                hir::Decl::Alias(ad) => {
                    self.aliases.insert(
                        ad.name,
                        AliasDef {
                            params: param_map(&ad.params),
                            body: AliasBody::Type(ad.ty.clone()),
                        },
                    );
                    added.push((ad.name, ad.name_span));
                }
                hir::Decl::EffectAlias(ad) => {
                    self.aliases.insert(
                        ad.name,
                        AliasDef {
                            params: param_map(&ad.params),
                            body: AliasBody::Effect(ad.row.clone()),
                        },
                    );
                    added.push((ad.name, ad.name_span));
                }
                _ => {}
            }
        }
        for (name, span) in added {
            if alias_reaches(&self.aliases, name, name, &mut Vec::new()) {
                let (what, instead) = match self.aliases.get(&name).map(|d| &d.body) {
                    Some(AliasBody::Effect(_)) => (
                        "effect alias",
                        "an alias names other effects, never itself, however indirectly",
                    ),
                    _ => (
                        "type alias",
                        "an alias is only another name; use `data` for a recursive type",
                    ),
                };
                self.errors.push(Diagnostic {
                    msg: format!("{what} `{}` refers to itself", hir::spelling(&name)),
                    filename: self.filename.clone(),
                    label: (instead.to_string(), span),
                    extra_labels: vec![],
                });
                // Expanded, it would never end; as the error type (or no
                // effects) it agrees with everything, so nothing else is
                // reported for it.
                if let Some(def) = self.aliases.get_mut(&name) {
                    def.body = match &def.body {
                        AliasBody::Type(t) => {
                            AliasBody::Type(hir::Node::new(t.id, hir::TypeExpr::Error, t.span))
                        }
                        AliasBody::Effect(_) => AliasBody::Effect(hir::EffectRow {
                            labels: Vec::new(),
                            tail: None,
                            assoc: None,
                        }),
                    };
                }
            }
        }
        // Traits after aliases, which their methods may be written in, and
        // before anything that may mention one; their `impl`s wait for every
        // module's -- see [`Infer::register_impls`].
        for d in decls {
            if let hir::Decl::Trait(td) = d.value() {
                self.register_trait(td);
            }
        }
        for d in decls {
            match d.value() {
                hir::Decl::Data(dd) => {
                    let params = param_map(&dd.params);
                    let quant = vec![VarKind::Type; dd.params.len()];
                    let head = Type::Con(
                        dd.name,
                        (0..dd.params.len() as u32).map(Type::Bound).collect(),
                    );
                    for v in &dd.variants {
                        let fields: Vec<(Option<InternedString>, Type)> = match &v.fields {
                            hir::VariantFields::Positional(ts) => ts
                                .iter()
                                .map(|t| (None, ty_of(t, &params, &self.aliases)))
                                .collect(),
                            hir::VariantFields::Named(fs) => fs
                                .iter()
                                .map(|(n, t)| (Some(*n), ty_of(t, &params, &self.aliases)))
                                .collect(),
                        };
                        self.record_ctor(dd.name, v.name, &quant, &head, &fields);
                    }
                }
                hir::Decl::Record(rd) => {
                    let params = param_map(&rd.params);
                    let quant = vec![VarKind::Type; rd.params.len()];
                    let head = Type::Con(
                        rd.name,
                        (0..rd.params.len() as u32).map(Type::Bound).collect(),
                    );
                    let fields: Vec<(Option<InternedString>, Type)> = rd
                        .fields
                        .iter()
                        .map(|(n, t)| (Some(*n), ty_of(t, &params, &self.aliases)))
                        .collect();
                    self.record_ctor(rd.name, rd.ctor, &quant, &head, &fields);
                }
                hir::Decl::Effect(ed) => {
                    let params = param_map(&ed.params);
                    let n = ed.params.len();
                    // `{ EffName p0 .. p{n-1} | e }` where `e = Bound(n)`
                    let head_args: Vec<Type> = (0..n as u32).map(Type::Bound).collect();
                    let mut ops = Vec::new();
                    for (opname, opvar, opty) in &ed.ops {
                        let t = ty_of(opty, &params, &self.aliases);
                        let (arg, ret) = match &t {
                            Type::Fun(a, r, _) => (a[0].clone(), (**r).clone()),
                            _ => (Type::unit(), t.clone()),
                        };
                        let mut quant = vec![VarKind::Type; n];
                        quant.push(VarKind::Effect);
                        let eff_row = Type::RowExtend(
                            ed.name,
                            Box::new(Type::Tuple(head_args.clone())),
                            Box::new(Type::Bound(n as u32)),
                        );
                        self.env.insert(
                            *opvar.value(),
                            Scheme {
                                lacks: Vec::new(),
                                preds: Vec::new(),
                                quant,
                                ty: Type::Fun(
                                    vec![arg.clone()],
                                    Box::new(ret.clone()),
                                    Box::new(eff_row),
                                ),
                            },
                        );
                        ops.push(EffOp {
                            name: *opname,
                            arg,
                            ret,
                        });
                    }
                    self.effects.insert(ed.name, EffectInfo { params: n, ops });
                }
                _ => {}
            }
        }
    }

    /// The effect a handler clause answers: the one the resolver found for
    /// it, which knows what is in scope where the `handle` is written.
    ///
    /// Once, it was the first effect with an operation of that name, looked
    /// for in a hash map: with two -- the standard library's `State` has a
    /// `get`, and so may a program's own -- which was handled changed from one
    /// run to the next, and the wrong one brought errors that made no sense.
    fn arm_effect(&self, arm: &hir::HandlerArm) -> Option<InternedString> {
        if self
            .effects
            .get(&arm.effect)
            .is_some_and(|info| info.ops.iter().any(|o| o.name == arm.op))
        {
            return Some(arm.effect);
        }
        self.op_effect(arm.op)
    }

    /// Some effect with an operation called `op` -- the same one every time.
    fn op_effect(&self, op: InternedString) -> Option<InternedString> {
        self.effects
            .iter()
            .filter(|(_, info)| info.ops.iter().any(|o| o.name == op))
            .map(|(name, _)| *name)
            .min_by(|a, b| a.to_string().cmp(&b.to_string()))
    }

    fn record_ctor(
        &mut self,
        tyname: InternedString,
        ctor: InternedString,
        quant: &[VarKind],
        head: &Type,
        fields: &[(Option<InternedString>, Type)],
    ) {
        let field_tys: Vec<Type> = fields.iter().map(|(_, t)| t.clone()).collect();
        let cty = if field_tys.is_empty() {
            head.clone()
        } else {
            Type::func(field_tys.clone(), head.clone())
        };
        self.ctors.insert(
            ctor,
            Scheme {
                lacks: Vec::new(),
                preds: Vec::new(),
                quant: quant.to_vec(),
                ty: cty,
            },
        );
        let labels: Option<Vec<InternedString>> = fields
            .iter()
            .map(|(n, _)| *n)
            .collect::<Option<Vec<_>>>()
            .filter(|ls| !ls.is_empty());
        let sig = VariantSig {
            name: ctor,
            fields: field_tys,
            labels,
        };
        // `register_types` runs once per unit and again for each dependency, so a
        // re-registered constructor replaces rather than duplicates its entry.
        let group = self.variants.entry(tyname).or_default();
        match group.iter_mut().find(|v| v.name == ctor) {
            Some(existing) => *existing = sig,
            None => group.push(sig),
        }
        let accessors = self.record_fields.entry(tyname).or_default();
        for (name, t) in fields {
            if let Some(name) = name {
                accessors.insert(
                    *name,
                    Scheme {
                        lacks: Vec::new(),
                        preds: Vec::new(),
                        quant: quant.to_vec(),
                        ty: Type::Fun(
                            vec![head.clone()],
                            Box::new(t.clone()),
                            Box::new(Type::RowEmpty),
                        ),
                    },
                );
            }
        }
    }

    /// A standalone signature as a polytype: each of its variables quantified,
    /// the ones standing for an effect as effects.
    /// The signature `vid` is held to, if it has one: written on a line of its
    /// own, or the trait's type for a method of an `impl`.
    fn sig_of(&mut self, vid: VarId) -> Option<(Scheme, Span)> {
        if let Some(found) = self.tr.method_sigs.get(&vid) {
            return Some(found.clone());
        }
        let (t, bounds) = self.sigs.get(&vid).cloned()?;
        Some((self.bounded_scheme(&t, &bounds), t.span))
    }

    /// Tie a binding about to be inferred to its signature, if it has one, so
    /// its body is checked against the types the signature gives -- the
    /// signature's variables still flexible here; [`Infer::check_sig`] holds
    /// the binding to them once it has been generalized.
    fn seed_sig(&mut self, vid: VarId, ty: &Type) {
        let Some((scheme, span)) = self.sig_of(vid) else {
            return;
        };
        // What its `where` says is what the body may assume.
        let (inst, given) = self.instantiate_with_preds(&scheme);
        self.tr.givens.extend(given);
        if let Err(err) = self.arena.unify(ty.clone(), inst) {
            self.sig_failed.insert(vid);
            let mut diag = self.unify_diagnostic(span, err);
            diag.msg = format!(
                "this definition does not have the type its signature gives, `{}`: {}",
                show_scheme_body(&scheme),
                diag.msg
            );
            diag.label.0 = "the signature".to_string();
            self.errors.push(diag);
        }
    }

    /// Is the generalized `scheme` of `vid` as general as its signature says?
    ///
    /// Seeded with the signature, the binding can only have come out as it or
    /// as an instance of it -- `a -> a` made `Int -> Int` by a body that adds
    /// one. The signature with its variables made rigid is an instance of the
    /// scheme exactly when the scheme is no less general.
    fn check_sig(&mut self, vid: VarId, scheme: &Scheme) {
        let Some((sig, span)) = self.sig_of(vid) else {
            return;
        };
        if self.sig_failed.contains(&vid) || scheme.ty.references_error() {
            return;
        }
        let rigid: Vec<Type> = (0..sig.quant.len())
            .map(|i| Type::Con(InternedString::from(format!("'sig{i}")), vec![]))
            .collect();
        let want = Arena::subst_bound(&sig.ty, &rigid);
        let have = self.normalize_scheme(scheme);
        let fail = |this: &mut Self, why: String| {
            this.errors.push(Diagnostic {
                msg: why,
                filename: this.filename.clone(),
                label: ("the signature".to_string(), span),
                extra_labels: vec![],
            });
        };
        match match_scheme(&have, &want) {
            None => fail(
                self,
                format!(
                    "this definition is less general than its signature: the signature says `{}`, and the definition is `{}`",
                    show_scheme_body(&sig),
                    show_scheme_body(&have)
                ),
            ),
            Some(args) => {
                let numeric = have.quant.iter().zip(&args).any(|(k, a)| {
                    matches!(k, VarKind::Num | VarKind::Frac)
                        && matches!(a, Type::Con(n, _) if n.starts_with("'sig"))
                });
                if numeric {
                    fail(
                        self,
                        format!(
                            "this definition needs a number where its signature `{}` has a type variable: it is `{}`",
                            show_scheme_body(&sig),
                            show_scheme_body(&have)
                        ),
                    );
                }
            }
        }
    }

    /// `{ base | x = v, … }`.
    ///
    /// A value of a record type -- one constructor, with named fields -- keeps
    /// its type, and each value has to have its field's. Otherwise it is an
    /// anonymous record, which must already have every field named, each at
    /// the type it is being given.
    fn infer_update(&mut self, base: &hir::LExpr, fields: &[(hir::Label, hir::LExpr)]) -> Type {
        let bt = self.infer_expr(base);
        let pruned = self.arena.zonk(&bt);
        if let Type::Con(tyname, _) = &pruned
            && let Some(accessors) = self.record_fields.get(tyname).cloned()
        {
            let one = self.variants.get(tyname).is_some_and(|vs| vs.len() == 1);
            if !one {
                self.errors.push(Diagnostic {
                    msg: format!(
                        "`{}` has more than one constructor, so there is no one record to update",
                        hir::spelling(tyname)
                    ),
                    filename: self.filename.clone(),
                    label: (
                        format!("a value of `{}`", hir::spelling(&tyname)),
                        base.span,
                    ),
                    extra_labels: vec![],
                });
                for (label, val) in fields {
                    self.infer_expr(val);
                    self.table.set(label.id, Type::Error);
                }
                return pruned;
            }
            for (label, val) in fields {
                let vt = self.infer_expr(val);
                let Some(scheme) = accessors.get(label.value()) else {
                    self.errors.push(Diagnostic {
                        msg: format!(
                            "`{}` has no field `{}`",
                            hir::spelling(tyname),
                            label.value()
                        ),
                        filename: self.filename.clone(),
                        label: ("not one of its fields".to_string(), label.span),
                        extra_labels: vec![],
                    });
                    self.table.set(label.id, Type::Error);
                    continue;
                };
                let accessor = self.instantiate(scheme);
                let field = self.arena.fresh();
                let eff = self.arena.fresh_effect();
                self.unify_at(
                    label.span,
                    accessor,
                    Type::Fun(vec![pruned.clone()], Box::new(field.clone()), Box::new(eff)),
                );
                self.unify_at(val.span, field.clone(), vt);
                self.table.set(label.id, field);
            }
            return pruned;
        }
        // One type per field named, however often: the resolver has reported a
        // repeat, and a row naming the label twice would report it again.
        let mut named: Vec<(InternedString, Type)> = Vec::new();
        for (label, _) in fields {
            if !named.iter().any(|(l, _)| l == label.value()) {
                named.push((*label.value(), self.arena.fresh()));
            }
        }
        let rest = self.arena.fresh_row();
        let row = named.iter().rev().fold(rest, |acc, (l, t)| {
            Type::RowExtend(*l, Box::new(t.clone()), Box::new(acc))
        });
        self.unify_at(base.span, bt.clone(), Type::Record(Box::new(row)));
        for (label, val) in fields {
            let vt = self.infer_expr(val);
            let t = named
                .iter()
                .find(|(l, _)| l == label.value())
                .map(|(_, t)| t.clone())
                .unwrap_or(Type::Error);
            self.unify_at(val.span, t.clone(), vt);
            self.table.set(label.id, t);
        }
        bt
    }

    /// Generalize, and record what was quantified under `vid`.
    ///
    /// The arena variables are kept, not just their count: a `TyLam` in core
    /// binds them and the annotations inside the binding's body mention them,
    /// so the two have to agree about which variable is which.
    fn generalize_named(
        &mut self,
        vid: Option<VarId>,
        ty: &Type,
        classes: bool,
        preds: &[Pred],
    ) -> Scheme {
        // A name still waiting on its overload has a type that is not settled,
        // and generalizing over it would let each use pick differently.
        let level = self.arena.level;
        for i in 0..self.pending.len() {
            let t = self.pending[i].ty.clone();
            self.arena.hold_back(&t, level);
        }
        let z = self.arena.zonk(ty);
        if !classes {
            // A `def` keeps its number type, so no later binding may generalize
            // over it either: it belongs to this level from here on.
            self.arena.hold_back_classes(&z, level);
        }
        let mut map = HashMap::new();
        let mut kinds = Vec::new();
        let body = self.arena.quantify(&z, &mut map, &mut kinds, classes);
        // Over the same variables: a trait is asked of one of the type's own,
        // and an associated type is one more beside them.
        let preds: Vec<Pred> = preds
            .iter()
            .map(|p| {
                let mut q = |t: &Type| {
                    let t = self.arena.zonk(t);
                    self.arena.quantify(&t, &mut map, &mut kinds, classes)
                };
                Pred {
                    tr: p.tr,
                    tys: p.tys.iter().map(&mut q).collect(),
                    assocs: p.assocs.iter().map(&mut q).collect(),
                }
            })
            .collect();
        // A quantified row variable that must lack some labels says which, so
        // every instance promises it again.
        let mut lacks: Vec<(u32, Vec<InternedString>)> = map
            .iter()
            .filter_map(|(&var, &idx)| {
                let labels = self.arena.lacks_of(var);
                (!labels.is_empty()).then_some((idx, labels))
            })
            .collect();
        lacks.sort_by_key(|(i, _)| *i);
        let scheme = Scheme {
            quant: kinds,
            preds,
            ty: body,
            lacks,
        };
        if let Some(vid) = vid {
            // `map` is arena var -> quantifier index; invert it.
            let mut vars = vec![0u32; scheme.quant.len()];
            for (&var, &idx) in &map {
                vars[idx as usize] = var;
            }
            self.generalized.insert(
                vid,
                Generalized {
                    scheme: scheme.clone(),
                    vars,
                },
            );
        }
        scheme
    }

    /// Record a binding that was *not* generalized, so lowering can find every
    /// binding in one place.
    fn record_mono(&mut self, vid: VarId, scheme: &Scheme) {
        self.generalized.insert(
            vid,
            Generalized {
                scheme: scheme.clone(),
                vars: Vec::new(),
            },
        );
    }

    // --- overloaded names ------------------------------------------------------

    /// Hand over the names the resolver found several meanings for. Call before
    /// [`Infer::infer_module`].
    pub fn set_overloads(&mut self, overloads: hir::Overloads) {
        self.overloads = overloads;
    }

    /// Give an overloaded name a placeholder type and put off choosing its
    /// meaning until the context has constrained that type.
    ///
    /// Choosing is settled by unification alone: a candidate *fits* when its
    /// type unifies with everything the placeholder has been asked to be. The
    /// set of fitting candidates can only shrink as constraints arrive, so the
    /// moment one is left it is the answer, and the moment none is there never
    /// will be one -- which is what lets [`Infer::solve_overloads`] decide
    /// early, at each binding, rather than only at the end of the unit.
    fn defer(&mut self, node: NodeId, span: Span, poison: Vec<NodeId>) -> Type {
        let ty = self.arena.fresh();
        let candidates = self.overloads.get(&node).cloned().unwrap_or_default();
        self.pending.push(Pending {
            node,
            candidates,
            ty: ty.clone(),
            level: self.arena.level,
            span,
            filename: self.filename.clone(),
            poison,
        });
        ty
    }

    /// Choose what can be chosen. `last` is the end of the unit, when a name
    /// that several candidates still fit is reported as ambiguous.
    fn solve_overloads(&mut self, last: bool) {
        if self.pending.is_empty() {
            return;
        }
        // Choosing one name can constrain another, so go round until nothing
        // changes.
        loop {
            let mut progress = false;
            let mut i = 0;
            while i < self.pending.len() {
                let fits = self.fitting(&self.pending[i].clone());
                match fits.as_slice() {
                    [] => {
                        let p = self.pending.remove(i);
                        self.report_overload(&p, &fits);
                        progress = true;
                    }
                    [only] => {
                        let p = self.pending.remove(i);
                        self.choose(&p, *only);
                        progress = true;
                    }
                    _ => i += 1,
                }
            }
            // One name at a time is stuck. Names whose types are tied together
            // may still have only one way to be read *together* --
            // `size (Just 5)`, with two `size`s and two `Just`s, say.
            if !progress {
                progress = self.solve_jointly();
            }
            if !progress {
                break;
            }
        }
        if last {
            for p in std::mem::take(&mut self.pending) {
                let fits = self.fitting(&p);
                self.report_overload(&p, &fits);
            }
        }
    }

    /// Settle what can be settled by reading tied-together names as a group.
    ///
    /// Pending names that share a type variable form a component, and each
    /// component is searched for every combination of candidates that unifies
    /// all at once. A name every such combination agrees on is decided; a
    /// component with no combination at all is an error. The search backtracks
    /// on snapshots and gives up past a fixed budget -- a component that large
    /// is left to be reported as ambiguous rather than explored.
    ///
    /// Returns whether anything was decided.
    fn solve_jointly(&mut self) -> bool {
        let n = self.pending.len();
        if n < 2 {
            return false;
        }
        // Components, by shared free variables.
        let vars: Vec<Vec<u32>> = (0..n)
            .map(|i| {
                let t = self.pending[i].ty.clone();
                let mut out = Vec::new();
                self.arena.free_vars(&t, &mut out);
                out
            })
            .collect();
        fn find(comp: &mut [usize], i: usize) -> usize {
            if comp[i] != i {
                let r = find(comp, comp[i]);
                comp[i] = r;
            }
            comp[i]
        }
        let mut comp: Vec<usize> = (0..n).collect();
        for i in 0..n {
            for j in (i + 1)..n {
                if vars[i].iter().any(|v| vars[j].contains(v)) {
                    let (a, b) = (find(&mut comp, i), find(&mut comp, j));
                    comp[a] = b;
                }
            }
        }
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut root_of: HashMap<usize, usize> = HashMap::new();
        for i in 0..n {
            let root = find(&mut comp, i);
            let g = *root_of.entry(root).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[g].push(i);
        }

        let mut decided: Vec<(NodeId, usize)> = Vec::new();
        let mut hopeless: Vec<Vec<NodeId>> = Vec::new();
        for members in &groups {
            if members.len() < 2 {
                continue;
            }
            let group: Vec<Pending> = members.iter().map(|&i| self.pending[i].clone()).collect();
            let mut search = Search {
                solutions: Vec::new(),
                budget: 512,
                truncated: false,
            };
            self.search(&group, &mut Vec::new(), &mut search);
            if search.truncated {
                continue;
            }
            if search.solutions.is_empty() {
                hopeless.push(group.iter().map(|p| p.node).collect());
                continue;
            }
            for (j, p) in group.iter().enumerate() {
                let k = search.solutions[0][j];
                if search.solutions.iter().all(|s| s[j] == k) {
                    decided.push((p.node, k));
                }
            }
        }

        let progress = !decided.is_empty() || !hopeless.is_empty();
        for (node, k) in decided {
            if let Some(i) = self.pending.iter().position(|p| p.node == node) {
                let p = self.pending.remove(i);
                self.choose(&p, k);
            }
        }
        for nodes in hopeless {
            let group: Vec<Pending> = nodes
                .iter()
                .filter_map(|node| {
                    let i = self.pending.iter().position(|p| p.node == *node)?;
                    Some(self.pending.remove(i))
                })
                .collect();
            self.report_combination(&group);
        }
        progress
    }

    /// Depth-first over the group's candidates, recording each full assignment
    /// that unifies, until the budget runs out.
    fn search(&mut self, group: &[Pending], chosen: &mut Vec<usize>, s: &mut Search) {
        if s.truncated {
            return;
        }
        let depth = chosen.len();
        if depth == group.len() {
            s.solutions.push(chosen.clone());
            if s.solutions.len() > 64 {
                s.truncated = true;
            }
            return;
        }
        let p = &group[depth];
        for (k, c) in p.candidates.iter().enumerate() {
            if s.budget == 0 {
                s.truncated = true;
                return;
            }
            s.budget -= 1;
            let snap = self.arena.snapshot();
            let outer = std::mem::replace(&mut self.arena.level, p.level);
            let fits = match self.candidate_type(c.alt) {
                Some(t) => self.arena.unify(p.ty.clone(), t).is_ok(),
                None => true,
            };
            self.arena.level = outer;
            if fits {
                chosen.push(k);
                self.search(group, chosen, s);
                chosen.pop();
            }
            self.arena.rollback(snap);
        }
    }

    /// Several names that each fit alone, but no reading of them fits together.
    fn report_combination(&mut self, group: &[Pending]) {
        let Some(first) = group.first() else { return };
        let mut names: Vec<String> = Vec::new();
        for p in group {
            let name = format!(
                "`{}`",
                p.candidates.first().map(|c| c.name).unwrap_or_default()
            );
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let mut lines = vec![
            format!("no reading of {} fits here", names.join(" and ")),
            "candidates in scope:".to_string(),
        ];
        for p in group {
            for c in &p.candidates {
                lines.push(self.candidate_line(c, ""));
            }
        }
        self.errors.push(Diagnostic {
            msg: lines.join("\n"),
            filename: first.filename.clone(),
            label: (
                "no combination of these candidates has a type".to_string(),
                first.span,
            ),
            extra_labels: group[1..]
                .iter()
                .map(|p| ("and this".to_string(), p.span))
                .collect(),
        });
        for p in group {
            for node in &p.poison {
                self.table.set(*node, Type::Error);
            }
            let _ = self.arena.unify(p.ty.clone(), Type::Error);
        }
    }

    /// One line of a candidate list: how to write it, its type, where it is from.
    fn candidate_line(&self, c: &hir::Candidate, mark: &str) -> String {
        let (spelled, scheme) = match c.alt {
            hir::Alt::Ctor(ctor) => (
                hir::ctor_spelling(&ctor.to_string()).to_string(),
                self.ctors.get(&ctor).cloned(),
            ),
            hir::Alt::Value(v) => (
                hir::spell_name(&c.name).into_owned(),
                self.env.get(&v).cloned(),
            ),
        };
        let ty = match scheme {
            Some(s) => show_scheme_body(&s),
            None => "?".to_string(),
        };
        let from = match (c.alt, c.origin) {
            (hir::Alt::Ctor(_), _) => String::new(),
            (_, Some(m)) => format!(", from `{m}`"),
            (_, None) => ", from this module".to_string(),
        };
        format!("  {spelled} : {ty}{from}{mark}")
    }

    /// The candidates of `p` whose type unifies with the one wanted, by index.
    /// Each is tried against a snapshot and rolled back.
    fn fitting(&mut self, p: &Pending) -> Vec<usize> {
        let (want, level) = (p.ty.clone(), p.level);
        let alts: Vec<hir::Alt> = p.candidates.iter().map(|c| c.alt).collect();
        let mut out = Vec::new();
        for (k, alt) in alts.into_iter().enumerate() {
            let snap = self.arena.snapshot();
            let outer = std::mem::replace(&mut self.arena.level, level);
            let fits = match self.candidate_type(alt) {
                Some(t) => self.arena.unify(want.clone(), t).is_ok(),
                // Nothing known about it yet: it cannot be ruled out.
                None => true,
            };
            self.arena.level = outer;
            self.arena.rollback(snap);
            if fits {
                out.push(k);
            }
        }
        out
    }

    /// A candidate's type, freshly instantiated -- or `None` for a value whose
    /// type is not known yet, which only a dependency cycle leaves behind.
    fn candidate_type(&mut self, alt: hir::Alt) -> Option<Type> {
        match alt {
            hir::Alt::Ctor(c) => self.ctor_type(c),
            hir::Alt::Value(v) => {
                let scheme = self.env.get(&v).cloned()?;
                let t = self.instantiate(&scheme);
                Some(self.open_latent(t))
            }
        }
    }

    /// `t` with the effect of every arrow it returns through left open: a
    /// function that does less than a use of it allows fits that use, as it
    /// does when it is the only candidate -- its effect is then only joined
    /// into where it is called. A `String.concat`, pure, is a `concat` a
    /// `println` argument can call. Not a parameter's: a callback that may do
    /// more is not one that does less.
    fn open_latent(&mut self, t: Type) -> Type {
        match self.arena.zonk(&t) {
            Type::Fun(params, ret, eff) => {
                let ret = self.open_latent(*ret);
                let eff = self.open_tail(*eff);
                Type::Fun(params, Box::new(ret), Box::new(eff))
            }
            other => other,
        }
    }

    /// A closed effect row made open, with a fresh variable for the rest.
    fn open_tail(&mut self, row: Type) -> Type {
        match row {
            Type::RowExtend(l, f, rest) => {
                let rest = self.open_tail(*rest);
                Type::RowExtend(l, f, Box::new(rest))
            }
            Type::RowEmpty => self.arena.fresh_effect(),
            other => other,
        }
    }

    /// Commit to candidate `k`: unify for real and remember the choice.
    fn choose(&mut self, p: &Pending, k: usize) {
        let alt = p.candidates[k].alt;
        let outer = std::mem::replace(&mut self.arena.level, p.level);
        // For real this time, so what the candidate's `where` wants is wanted.
        let chosen = match alt {
            hir::Alt::Value(v) => self
                .env
                .get(&v)
                .cloned()
                .map(|s| self.instantiate_at(p.node, p.span, &s)),
            hir::Alt::Ctor(_) => self.candidate_type(alt),
        };
        let ty = match chosen {
            Some(t) => self.open_latent(t),
            None => {
                // The same placeholder an unordered reference would get (see
                // `Expr::Var`), so the definition still meets this use.
                let hir::Alt::Value(v) = alt else {
                    unreachable!("a constructor always has a type")
                };
                let t = self.arena.fresh_global();
                self.env.insert(v, Scheme::mono(t.clone()));
                t
            }
        };
        self.arena.level = outer;
        self.unify_at(p.span, p.ty.clone(), ty);
        self.resolutions.insert(p.node, alt);
    }

    /// A wanted type as a person would write it: free variables lettered, and
    /// an effect variable that says nothing left out, as a scheme prints.
    fn show_wanted(&mut self, ty: &Type) -> String {
        let z = self.arena.zonk(ty);
        let (mut map, mut kinds) = (HashMap::new(), Vec::new());
        let body = self
            .arena
            .quantify_from(&z, None, &mut map, &mut kinds, true);
        show_scheme_body(&Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: kinds,
            ty: body,
        })
    }

    /// Say why a name could not be settled -- no candidate fits (`fits` is
    /// empty), or more than one does -- and poison it so that is all that is said.
    fn report_overload(&mut self, p: &Pending, fits: &[usize]) {
        let want = self.show_wanted(&p.ty);
        let name = p.candidates.first().map(|c| c.name).unwrap_or_default();
        let (msg, label) = if fits.is_empty() {
            (
                format!("no `{name}` in scope has the type needed here, `{}`", want),
                format!("none of the {} `{name}`s in scope fits", p.candidates.len()),
            )
        } else {
            (
                format!(
                    "ambiguous `{name}`: {} of the candidates in scope fit `{}`",
                    fits.len(),
                    want
                ),
                "add a type annotation, or write the name qualified".to_string(),
            )
        };
        let mut lines = vec![msg, "candidates in scope:".to_string()];
        for (k, c) in p.candidates.iter().enumerate() {
            let mark = if !fits.is_empty() && fits.contains(&k) {
                "  (fits)"
            } else {
                ""
            };
            lines.push(self.candidate_line(c, mark));
        }
        let diag = Diagnostic {
            msg: lines.join("\n"),
            filename: p.filename.clone(),
            label: (label, p.span),
            extra_labels: vec![],
        };
        self.errors.push(diag);
        // Unresolved, the node keeps the resolver's placeholder meaning, which
        // is as likely to be wrong as right: nothing after this should trust it.
        for n in &p.poison {
            self.table.set(*n, Type::Error);
        }
        let _ = self.arena.unify(p.ty.clone(), Type::Error);
    }

    /// The declared result type of a binding, or a fresh variable when there is
    /// none.
    ///
    /// A declared one is an ordinary meta variable that the body is unified
    /// against, so it constrains rather than merely records: writing
    /// `fun f x : Int = x` makes `f` an `Int -> Int` and an incompatible body
    /// an error, instead of generalising over whatever the body happened to be.
    /// A function's declared result, and the effect of its body when the
    /// declaration says one: `: R ! e`, which the parser keeps as a function
    /// type of no parameters. Its variables are the declaration's, as every
    /// annotation's are, so the `e` of `(f : a -> b ! e) : b ! e` is one row.
    ///
    /// A result written without a `!` says the body performs nothing -- as an
    /// arrow without one does -- and only a result not written at all leaves
    /// the effect to be inferred.
    fn declared_result(&mut self, declared: &Option<hir::LTypeExpr>) -> (Type, Option<Type>) {
        match declared {
            Some(t) => match t.value() {
                hir::TypeExpr::Fun(ps, r, Some(row)) if ps.is_empty() => {
                    let ret = self.annotation(r);
                    let eff = self.effect_annotation(t, row);
                    (ret, Some(eff))
                }
                _ => (self.annotation(t), Some(Type::RowEmpty)),
            },
            None => (self.arena.fresh(), None),
        }
    }

    /// The effect row `row`, written in the annotation `whole`, as an inference
    /// type -- its variables the declaration's, like [`Self::annotation`]'s.
    fn effect_annotation(&mut self, whole: &hir::LTypeExpr, row: &hir::EffectRow) -> Type {
        let mut vars = HashMap::new();
        collect_tyvars(whole, &mut vars);
        let eff = eff_of(row, &vars, &self.aliases);
        // Where it stands says what it is: on an arrow, a row.
        let mut kinds = vec![VarKind::Type; vars.len()];
        let probe = Type::Fun(
            vec![Type::unit()],
            Box::new(Type::unit()),
            Box::new(eff.clone()),
        );
        mark_effect_vars(&probe, &mut kinds);
        let fresh = self.metas_for(&vars, &kinds);
        Arena::subst_bound(&eff, &fresh)
    }

    /// The meta each of an annotation's variables stands for, made the first
    /// time the declaration mentions it and the same one every time after.
    fn metas_for(&mut self, vars: &HashMap<VarId, u32>, kinds: &[VarKind]) -> Vec<Type> {
        let mut fresh = vec![Type::unit(); vars.len()];
        for (var, i) in vars {
            let meta = match self.ann_tyvars.get(var) {
                Some(t) => t.clone(),
                None => {
                    let t = self.arena.fresh_of(kinds[*i as usize]);
                    self.ann_tyvars.insert(*var, t.clone());
                    t
                }
            };
            fresh[*i as usize] = meta;
        }
        fresh
    }

    /// A pattern annotation as an inference type.
    ///
    /// Each type variable becomes a meta variable, and the *same* one every
    /// time that variable appears — which is why the map is keyed by `VarId`
    /// and not rebuilt per annotation. The resolver gives a declaration's
    /// annotations one scope, so the two `a`s in
    /// `fun twice (f : a -> a) (x : a)` share a `VarId`, and sharing a `VarId`
    /// has to mean sharing a type or the annotation would be decoration.
    ///
    /// Nothing needs clearing between declarations: the resolver clears its own
    /// scope, so a different declaration's `a` is a different `VarId`.
    fn annotation(&mut self, t: &hir::LTypeExpr) -> Type {
        let mut vars = HashMap::new();
        collect_tyvars(t, &mut vars);
        // Index them for `ty_of`, then map each `Bound` to the meta this
        // declaration has already agreed on for that variable.
        let ty = ty_of(t, &vars, &self.aliases);
        // What each one stands for, by where it stands: a row tail has to be a
        // row variable, or it could not absorb the rest of a record's fields.
        let mut kinds = vec![VarKind::Type; vars.len()];
        mark_effect_vars(&ty, &mut kinds);
        let fresh = self.metas_for(&vars, &kinds);
        let ty = Arena::subst_bound(&ty, &fresh);
        self.lift_assocs_wanted(ty, t.span)
    }

    fn instantiate(&mut self, scheme: &Scheme) -> Type {
        let fresh = self.arena.fresh_quantifiers(scheme);
        Arena::subst_bound(&scheme.ty, &fresh)
    }

    /// A function-typed parameter, with each latent effect replaced by a fresh
    /// one for the argument to fill in.
    ///
    /// The fresh one is what the argument does; the original is what the callee
    /// allows; [`Infer::solve_subsumptions`] requires only that the first be
    /// part of the second. That is the sound direction -- a callee calling a
    /// function that does less than it allowed for is fine -- and it is what
    /// lets a `runSt` inside a library function call a caller's callback: the
    /// callee's type says the callback may perform `{ St s | e }`, and a
    /// callback from outside, whose effect cannot name `s`, is tied to the `e`.
    ///
    /// Everywhere else the two end up equal, as they would have by unifying
    /// straight away -- only later, so the purity of a binding is decided after
    /// they are ([`Infer::infer_bind`]).
    fn loosen_parameter(&mut self, span: Span, ty: Type) -> Type {
        match self.arena.zonk(&ty) {
            Type::Fun(params, ret, eff) => {
                let ret = self.loosen_parameter(span, *ret);
                let actual = self.arena.fresh_effect();
                self.subsumptions.push((actual.clone(), *eff, span));
                Type::Fun(params, Box::new(ret), Box::new(actual))
            }
            other => other,
        }
    }

    /// Tie each argument effect recorded since `mark` into what its parameter
    /// allowed -- a join, so the `St` labels of deeper `runSt`s are left out.
    fn solve_subsumptions(&mut self, mark: usize) {
        // What waits on an `impl` is joined once that is found, which is
        // asked for first; one still a variable after that is generic here,
        // and is the region's own.
        let waiting: Vec<Type> = self.subsumptions[mark..]
            .iter()
            .map(|(phi, ..)| phi.clone())
            .collect();
        let waits = waiting.iter().any(|phi| self.is_assoc_effect(phi));
        if waits {
            self.solve_wanted();
        }
        let pending: Vec<_> = self.subsumptions.drain(mark..).collect();
        let was = std::mem::replace(&mut self.settling_joins, true);
        for (actual, allowed, span) in pending {
            self.join_effect_into(span, allowed, actual);
        }
        self.settling_joins = was;
    }

    /// `runSt body`, applied directly: the one place a type is polymorphic in
    /// its argument, which ordinary inference cannot express.
    ///
    /// The body must be a `() -> a ! { St s | e }` for a `s` made fresh here,
    /// one level in, and rigid -- so nothing the body is given can depend on
    /// which `runSt` it is, and nothing tied to `s` gets back out: the result
    /// `a` and the leftover effects `e` are made outside, and binding them to
    /// anything mentioning `s` fails ([`Arena::fresh_skolem`]). What the call
    /// performs is the body's effects minus that one `St s`, which is how a
    /// function doing all its mutation inside a `runSt` comes out pure.
    ///
    /// Written as a value rather than applied -- `map runSt bodies` -- it gets
    /// its ordinary scheme instead, `(() -> a ! e) -> a ! e`, which discharges
    /// nothing and so is sound without any of this.
    fn infer_run_st(&mut self, span: Span, func: &hir::LExpr, body: &hir::LExpr) -> Type {
        let result = self.arena.fresh();
        let outside = self.arena.fresh_effect();
        let mark = self.subsumptions.len();
        self.arena.enter_level();
        self.st_levels.push(self.arena.level);
        let state = self.arena.fresh_skolem();
        let st_label = InternedString::from("St");
        let body_ty = match body.value() {
            // The usual shape, `runSt (\() -> ...)`: start the lambda's effect
            // row with this `runSt`'s `St s` rather than discovering it at the
            // end. A label is matched by name, first come first served, so a
            // cell made in here meets this `St s` before any other -- and an
            // outer `runSt`'s cell, used in here, meets it already rigid and
            // is kept apart (`Arena::rigidly_distinct`). Inferring the body
            // first and unifying afterwards would bind whichever cell's state
            // came first, outer or not.
            hir::Expr::Lam(params, lam_body) if params.len() == 1 => {
                let mut bound = Vec::new();
                let pty = self.infer_pat(&params[0], &mut bound);
                self.unify_at(params[0].span, pty, Type::unit());
                let tail = self.arena.fresh_effect();
                let seeded = Type::RowExtend(
                    st_label,
                    Box::new(Type::Tuple(vec![state.clone()])),
                    Box::new(tail),
                );
                let saved = std::mem::replace(&mut self.cur_effect, seeded.clone());
                let result_ty = self.infer_expr(lam_body);
                self.cur_effect = saved;
                let fty = Type::func_eff(vec![Type::unit()], result_ty, seeded);
                self.table.set(body.id, fty.clone());
                fty
            }
            _ => self.infer_expr(body),
        };
        let body_row = Type::RowExtend(
            st_label,
            Box::new(Type::Tuple(vec![state])),
            Box::new(outside.clone()),
        );
        let want = Type::func_eff(vec![Type::unit()], result.clone(), body_row);
        self.unify_at(body.span, body_ty, want.clone());
        // What the body asked of a trait at a type that names `s` is answered
        // in here too: the `impl` found for it is over variables of its own,
        // which made out there could not stand for `s` -- that would be the
        // state escaping -- though nothing of it gets out.
        self.solve_wanted();
        // Everything the body passed along is settled while `s` is still this
        // `runSt`'s, so an outer callback's effect can be told apart from it.
        self.solve_subsumptions(mark);
        // What still waits has a region that never took shape: nothing in it
        // performed this `runSt`'s `St s`, so it is safe to tie now.
        self.st_levels.pop();
        self.solve_subsumptions(mark);
        self.arena.exit_level();
        self.join_effect(span, outside);
        if let hir::Expr::Var(ident) = func.value() {
            let fty = Type::func(vec![want], result.clone());
            self.table.set(ident.id, fty.clone());
            self.table.set(func.id, fty);
        }
        result
    }

    fn unify_at(&mut self, span: Span, a: Type, b: Type) {
        let (whole_a, whole_b) = (a.clone(), b.clone());
        if let Err(err) = self.arena.unify(a, b) {
            // Part of either side is already an error: the mismatch is that
            // error seen again from here, not a new one.
            if self.arena.zonk(&whole_a).references_error()
                || self.arena.zonk(&whole_b).references_error()
            {
                return;
            }
            let diag = self.unify_diagnostic(span, err);
            self.errors.push(diag);
        }
    }

    /// The labels of a record literal or pattern, each reported if it is
    /// given more than once.
    fn distinct_labels<T>(&mut self, fields: &[(hir::Label, T)]) -> Vec<InternedString> {
        let mut seen: Vec<InternedString> = Vec::with_capacity(fields.len());
        for (label, _) in fields {
            let l = *label.value();
            if seen.contains(&l) {
                let diag = self.unify_diagnostic(label.span, UnifyError::Duplicate(l));
                self.errors.push(diag);
            } else {
                seen.push(l);
            }
        }
        seen
    }

    fn unify_diagnostic(&mut self, span: Span, err: UnifyError) -> Diagnostic {
        let (msg, label) = match err {
            UnifyError::Mismatch(a, b) => {
                let a = self.arena.zonk(&a);
                let b = self.arena.zonk(&b);
                let (sa, sb) = (show(&a), show(&b));
                // Two packages' types of one name print alike: say whose each is.
                let whose = |t: &Type, shown: String| match t {
                    Type::Con(name, _) => match hir::type_package(name) {
                        Some(p) => format!("{shown}` (from `{p}`)"),
                        None => format!("{shown}` (from `Std`)"),
                    },
                    _ => format!("{shown}`"),
                };
                let (ta, tb) = if sa == sb {
                    (whose(&a, sa), whose(&b, sb))
                } else {
                    (format!("{sa}`"), format!("{sb}`"))
                };
                (
                    format!("type mismatch: `{ta} vs `{tb}"),
                    "types do not unify here".to_string(),
                )
            }
            UnifyError::Occurs(a, b) => {
                let a = self.arena.zonk(&a);
                let b = self.arena.zonk(&b);
                (
                    format!("infinite type: `{}` occurs in `{}`", show(&a), show(&b)),
                    "recursive type".to_string(),
                )
            }
            UnifyError::NotANumber(integer, ty) => {
                let ty = self.arena.zonk(&ty);
                let (class, operators) = if integer {
                    ("an integer type", "`+`, `<` and the rest")
                } else {
                    ("a float type", "`+.`, `<.` and the rest")
                };
                (
                    format!("type mismatch: `{}` is not {class}", show(&ty)),
                    format!("{operators} and literals like this one need {class}"),
                )
            }
            UnifyError::Escape => (
                "state from inside a `runSt` escapes it".to_string(),
                "a cell, an array, or something that uses one, would outlive its `runSt`"
                    .to_string(),
            ),
            UnifyError::Arity(x, y) => (
                format!(
                    "function applied to the wrong number of arguments: expected {x}, found {y}"
                ),
                "arity mismatch".to_string(),
            ),
            // The same row machinery serves records and effects, and the label
            // says which: an effect is named like a type, a field like a value.
            UnifyError::MissingLabel(l)
                if hir::spelling(&l).starts_with(|c: char| c.is_uppercase()) =>
            {
                (
                    format!(
                        "type mismatch: the effect `{}` is not allowed here",
                        hir::spelling(&l)
                    ),
                    format!("performs `{}`", hir::spelling(&l)),
                )
            }
            UnifyError::MissingLabel(l) => (
                format!("record has no field `{l}`"),
                format!("missing field `{l}`"),
            ),
            UnifyError::Duplicate(l) => (
                format!("the record already has a field `{l}`"),
                format!("a second `{l}`"),
            ),
        };
        Diagnostic {
            msg,
            filename: self.filename.clone(),
            label: (label, span),
            extra_labels: vec![],
        }
    }
}

// ===========================================================================
// data / record declaration -> types
// ===========================================================================

/// Map each type parameter `VarId` to its quantifier index.
fn param_map(params: &[hir::Ident]) -> HashMap<VarId, u32> {
    params
        .iter()
        .enumerate()
        .map(|(i, p)| (*p.value(), i as u32))
        .collect()
}

/// A `type` or `effect` alias: its parameters, numbered, and what it stands
/// for.
#[derive(Debug, Clone)]
struct AliasDef {
    params: HashMap<VarId, u32>,
    body: AliasBody,
}

#[derive(Debug, Clone)]
enum AliasBody {
    /// `type Span = (Int, Int)`
    Type(hir::LTypeExpr),
    /// `effect Eff = { Console, Fs }`: labels, and no tail.
    Effect(hir::EffectRow),
}

type Aliases = HashMap<InternedString, AliasDef>;

/// Does expanding alias `from` ever come back to `target`?
fn alias_reaches(
    aliases: &Aliases,
    from: InternedString,
    target: InternedString,
    seen: &mut Vec<InternedString>,
) -> bool {
    fn names(t: &hir::LTypeExpr, out: &mut Vec<InternedString>) {
        match t.value() {
            hir::TypeExpr::Con(n, args) => {
                out.push(*n.value());
                args.iter().for_each(|a| names(a, out));
            }
            hir::TypeExpr::Fun(ps, r, eff) => {
                ps.iter().for_each(|p| names(p, out));
                names(r, out);
                for (_, args) in eff.iter().flat_map(|e| &e.labels) {
                    args.iter().for_each(|a| names(a, out));
                }
            }
            hir::TypeExpr::Tuple(ts) => ts.iter().for_each(|x| names(x, out)),
            hir::TypeExpr::Vector(x) | hir::TypeExpr::List(x) => names(x, out),
            hir::TypeExpr::Record(fs, _) => fs.iter().for_each(|(_, x)| names(x, out)),
            hir::TypeExpr::Row(row) => {
                for (_, args) in &row.labels {
                    args.iter().for_each(|a| names(a, out));
                }
            }
            hir::TypeExpr::Var(_) | hir::TypeExpr::Error => {}
        }
    }
    let Some(def) = aliases.get(&from) else {
        return false;
    };
    let mut mentioned = Vec::new();
    match &def.body {
        AliasBody::Type(t) => names(t, &mut mentioned),
        AliasBody::Effect(row) => {
            for (label, args) in &row.labels {
                mentioned.push(*label);
                args.iter().for_each(|a| names(a, &mut mentioned));
            }
        }
    }
    for n in mentioned {
        if n == target {
            return true;
        }
        if aliases.contains_key(&n) && !seen.contains(&n) {
            seen.push(n);
            if alias_reaches(aliases, n, target, seen) {
                return true;
            }
        }
    }
    false
}

/// Mark the quantifiers of `ty` that stand for a row rather than a type: the
/// latent effect of an arrow or the tail of its row as an effect, and the tail
/// of a record's row as a record row. A variable is told apart only by where it
/// stands, so this is how a written `{ name : String | r }` gets an `r` that
/// can absorb the rest of a record's fields.
fn mark_effect_vars(ty: &Type, quant: &mut [VarKind]) {
    match ty {
        Type::Fun(args, ret, eff) => {
            args.iter().for_each(|a| mark_effect_vars(a, quant));
            mark_effect_vars(ret, quant);
            let mut row = &**eff;
            while let Type::RowExtend(_, field, rest) = row {
                mark_effect_vars(field, quant);
                row = rest;
            }
            if let Type::Bound(i) = row
                && let Some(k) = quant.get_mut(*i as usize)
            {
                *k = VarKind::Effect;
            }
        }
        Type::Con(_, args) | Type::Tuple(args) => {
            args.iter().for_each(|a| mark_effect_vars(a, quant))
        }
        Type::Record(row) => {
            let mut row = &**row;
            while let Type::RowExtend(_, field, rest) = row {
                mark_effect_vars(field, quant);
                row = rest;
            }
            if let Type::Bound(i) = row
                && let Some(k) = quant.get_mut(*i as usize)
            {
                *k = VarKind::Row;
            }
        }
        Type::RowExtend(_, field, rest) => {
            mark_effect_vars(field, quant);
            mark_effect_vars(rest, quant);
        }
        Type::Var(_) | Type::Bound(_) | Type::RowEmpty | Type::Error => {}
    }
}

/// Convert a resolved HIR type expression into an inference [`Type`], with the
/// declaration's parameters as `Bound` variables and every alias expanded.
fn ty_of(t: &hir::LTypeExpr, params: &HashMap<VarId, u32>, aliases: &Aliases) -> Type {
    match t.value() {
        hir::TypeExpr::Var(v) => match params.get(v.value()) {
            Some(&i) => Type::Bound(i),
            None => Type::Error, // rename already reported the unbound tyvar
        },
        hir::TypeExpr::Con(name, args) => {
            let args: Vec<Type> = args.iter().map(|a| ty_of(a, params, aliases)).collect();
            match &**name.value() {
                "Int" | "Int64" => Type::int(),
                "BigInt" => Type::bigint(),
                "Float" | "Float64" => Type::float(),
                "String" => Type::string(),
                "Bool" => Type::bool(),
                "Unit" => Type::unit(),
                "List" => Type::list(args.into_iter().next().unwrap_or_else(Type::unit)),
                "Array" => Type::array(args.into_iter().next().unwrap_or_else(Type::unit)),
                "Ref" => Type::reference(args.into_iter().next().unwrap_or_else(Type::unit)),
                _ => match aliases.get(name.value()) {
                    // What it stands for, with the arguments in place of its
                    // parameters. The resolver has checked there are as many.
                    Some(AliasDef {
                        params: ps,
                        body: AliasBody::Type(body),
                    }) if ps.len() == args.len() => {
                        Arena::subst_bound(&ty_of(body, ps, aliases), &args)
                    }
                    Some(_) => Type::Error,
                    None => Type::Con(*name.value(), args),
                },
            }
        }
        hir::TypeExpr::Fun(ps, r, eff) => {
            let effty = match eff {
                Some(row) => eff_of(row, params, aliases),
                None => Type::RowEmpty,
            };
            Type::func_eff(
                ps.iter().map(|p| ty_of(p, params, aliases)).collect(),
                ty_of(r, params, aliases),
                effty,
            )
        }
        hir::TypeExpr::Tuple(ts) => {
            Type::Tuple(ts.iter().map(|x| ty_of(x, params, aliases)).collect())
        }
        // `[T]` type syntax now denotes the RRB `Vector`; write `List T` for a list.
        hir::TypeExpr::Vector(x) => Type::vector(ty_of(x, params, aliases)),
        hir::TypeExpr::List(x) => Type::list(ty_of(x, params, aliases)),
        // Built in written order, as a record literal's type is: unification
        // compares rows up to reordering, so the order is only what prints.
        hir::TypeExpr::Record(fields, tail) => {
            let rest = match tail {
                Some(v) => params
                    .get(v.value())
                    .map_or(Type::Error, |&i| Type::Bound(i)),
                None => Type::RowEmpty,
            };
            let row = fields.iter().rev().fold(rest, |rest, (label, ft)| {
                Type::RowExtend(
                    *label.value(),
                    Box::new(ty_of(ft, params, aliases)),
                    Box::new(rest),
                )
            });
            Type::Record(Box::new(row))
        }
        hir::TypeExpr::Row(row) => eff_of(row, params, aliases),
        hir::TypeExpr::Error => Type::Error,
    }
}

/// Convert a resolved effect row into an effect [`Type`], with every effect
/// alias it names replaced by the effects it stands for.
fn eff_of(row: &hir::EffectRow, params: &HashMap<VarId, u32>, aliases: &Aliases) -> Type {
    let tail = match (&row.tail, &row.assoc) {
        (Some(v), _) => params
            .get(v.value())
            .map(|&i| Type::Bound(i))
            .unwrap_or(Type::RowEmpty),
        // A trait's associated effect, at these types: a type by that name
        // for now, which `lift_assocs` makes the variable its trait settles
        // -- as it does an associated type's.
        (None, Some((name, args))) => Type::Con(
            *name,
            args.iter().map(|a| ty_of(a, params, aliases)).collect(),
        ),
        (None, None) => Type::RowEmpty,
    };
    let mut labels = Vec::new();
    row_labels(row, params, aliases, &mut labels);
    labels.into_iter().rev().fold(tail, |rest, (name, args)| {
        Type::RowExtend(name, Box::new(args), Box::new(rest))
    })
}

/// The labels of `row`, each with its arguments as a tuple, onto `out`: an
/// alias's own, in its place, and one said twice -- `{ Eff, Console }` --
/// once.
fn row_labels(
    row: &hir::EffectRow,
    params: &HashMap<VarId, u32>,
    aliases: &Aliases,
    out: &mut Vec<(InternedString, Type)>,
) {
    for (name, args) in &row.labels {
        let args: Vec<Type> = args.iter().map(|a| ty_of(a, params, aliases)).collect();
        match aliases.get(name) {
            Some(AliasDef {
                params: ps,
                body: AliasBody::Effect(body),
            }) if ps.len() == args.len() => {
                let mut inner = Vec::new();
                row_labels(body, ps, aliases, &mut inner);
                for (label, of) in inner {
                    let of = Arena::subst_bound(&of, &args);
                    if !out.iter().any(|(l, o)| *l == label && *o == of) {
                        out.push((label, of));
                    }
                }
            }
            _ => {
                let args = Type::Tuple(args);
                if !out.iter().any(|(l, o)| l == name && *o == args) {
                    out.push((*name, args));
                }
            }
        }
    }
}

// ===========================================================================
// Primitive signatures (indexed by `rename::PRIMS`)
// ===========================================================================

/// The row `{ Mut | Bound(tail) }` — the effect every `Ref` operation carries.
fn mut_row(tail: u32) -> Type {
    Type::RowExtend(
        InternedString::from("Mut"),
        Box::new(Type::Tuple(vec![])),
        Box::new(Type::Bound(tail)),
    )
}

/// The row `{ Thread | Bound(tail) }` -- what starting, waiting on or talking to
/// a green thread costs.
fn thread_row(tail: u32) -> Type {
    Type::RowExtend(
        InternedString::from("Thread"),
        Box::new(Type::Tuple(vec![])),
        Box::new(Type::Bound(tail)),
    )
}

/// The effects a spawned thread may perform, as a closed row: what the runtime
/// itself answers. A thread starts with no handlers, so anything else it
/// performed would have nobody to answer it -- a handler in the thread that
/// spawned it is on another heap, and may not even be running. An effect the
/// function handles itself does not appear in its type, so it is allowed.
pub const THREAD_EFFECTS: &[&str] = &[
    "Thread", "Console", "Fs", "Process", "Random", "Time", "Test", "Mut",
];

/// The row `{ Stm | Bound(tail) }` -- what a transaction operation costs.
fn stm_row(tail: u32) -> Type {
    Type::RowExtend(
        InternedString::from("Stm"),
        Box::new(Type::Tuple(vec![])),
        Box::new(Type::Bound(tail)),
    )
}

fn thread_body_row() -> Type {
    THREAD_EFFECTS.iter().rev().fold(Type::RowEmpty, |rest, e| {
        Type::RowExtend(
            InternedString::from(*e),
            Box::new(Type::Tuple(vec![])),
            Box::new(rest),
        )
    })
}

/// The row `{ St Bound(state) | Bound(tail) }` -- what touching local state costs.
fn st_row(state: u32, tail: u32) -> Type {
    Type::RowExtend(
        InternedString::from("St"),
        Box::new(Type::Tuple(vec![Type::Bound(state)])),
        Box::new(Type::Bound(tail)),
    )
}

/// `scheme` with the effect of each arrow on its result spine left open: a
/// closed row `{ E1, E2 }` becomes `{ E1, E2 | e }` for a fresh quantified `e`.
///
/// A primitive's type is written with closed rows, but a function a user
/// writes is generalized over its effect -- `fun f x = x` is
/// `a -> a ! e` -- and the two have to behave alike when passed around: a
/// primitive handed to a higher-order function inside a `@test` must not force
/// the call's effect to be exactly `{}` while the test performs `Test`.
/// Parameters keep their rows as written, since a primitive that demands a
/// pure callback must go on demanding one.
fn open_effects(scheme: Scheme) -> Scheme {
    fn open_tail(row: Type, quant: &mut Vec<VarKind>) -> Type {
        match row {
            Type::RowEmpty => {
                quant.push(VarKind::Effect);
                Type::Bound((quant.len() - 1) as u32)
            }
            Type::RowExtend(label, field, rest) => {
                Type::RowExtend(label, field, Box::new(open_tail(*rest, quant)))
            }
            other => other,
        }
    }
    fn go(ty: Type, quant: &mut Vec<VarKind>) -> Type {
        match ty {
            Type::Fun(params, ret, eff) => {
                let ret = go(*ret, quant);
                let eff = open_tail(*eff, quant);
                Type::Fun(params, Box::new(ret), Box::new(eff))
            }
            other => other,
        }
    }
    let Scheme {
        preds,
        mut quant,
        ty,
        lacks,
    } = scheme;
    let ty = go(ty, &mut quant);
    Scheme {
        preds,
        quant,
        ty,
        lacks,
    }
}

/// A primitive's scheme as a program sees it, its effects opened as
/// [`Infer::load_prelude`] opens them: what a tool that keeps its own table of
/// them -- MeadowBoot's is written from this -- has to agree with.
pub fn primitive_scheme(name: &str) -> Option<Scheme> {
    prim_scheme(name).map(open_effects)
}

fn prim_scheme(name: &str) -> Option<Scheme> {
    use Type::*;
    // `∀a. <ty>` where `a` is `Bound(0)`.
    let a1 = |ty: Type| Scheme {
        lacks: Vec::new(),
        preds: Vec::new(),
        quant: vec![VarKind::Type],
        ty,
    };
    // The same over any integer type, and over either float type.
    let num = |ty: Type| Scheme {
        lacks: Vec::new(),
        preds: Vec::new(),
        quant: vec![VarKind::Num],
        ty,
    };
    let frac = |ty: Type| Scheme {
        lacks: Vec::new(),
        preds: Vec::new(),
        quant: vec![VarKind::Frac],
        ty,
    };
    let s = match name {
        // --- numbers ---
        //
        // One set of operators per family, over a `Num` (any integer type) or
        // a `Frac` (either float type) -- see `VarKind::Num`.
        "_primAdd" | "_primSub" | "_primMul" | "_primDiv" | "_primMod" | "_primPow" => {
            num(Type::func(vec![Bound(0), Bound(0)], Bound(0)))
        }
        "_primLt" | "_primGt" | "_primLe" | "_primGe" => {
            num(Type::func(vec![Bound(0), Bound(0)], Type::bool()))
        }
        "neg" => num(Type::func(vec![Bound(0)], Bound(0))),
        "_primAddF" | "_primSubF" | "_primMulF" | "_primDivF" => {
            frac(Type::func(vec![Bound(0), Bound(0)], Bound(0)))
        }
        "_primLtF" | "_primGtF" | "_primLeF" | "_primGeF" => {
            frac(Type::func(vec![Bound(0), Bound(0)], Type::bool()))
        }
        "toFloat" => num(Type::func(vec![Bound(0)], Type::float())),
        "toFloat64" => frac(Type::func(vec![Bound(0)], Type::float())),
        "toFloat32" => frac(Type::func(vec![Bound(0)], Type::con("Float32"))),
        "floor" => frac(Type::func(vec![Bound(0)], Type::int())),
        "toBigInt" => num(Type::func(vec![Bound(0)], Type::bigint())),
        "toInt" | "toInt64" => num(Type::func(vec![Bound(0)], Type::int())),
        "toInt8" | "toInt16" | "toInt32" | "toUInt8" | "toUInt16" | "toUInt32" | "toUInt64" => {
            num(Type::func(vec![Bound(0)], Type::con(&name[2..])))
        }
        // --- builtin `Array` (all `∀a. …`) ---
        "arrayLen" => a1(Type::func(vec![Type::array(Bound(0))], Type::int())),
        "arrayGet" => a1(Type::func(
            vec![Type::array(Bound(0)), Type::int()],
            Bound(0),
        )),
        "arrayGetOr" => a1(Type::func(
            vec![Bound(0), Type::array(Bound(0)), Type::int()],
            Bound(0),
        )),
        "arraySet" => a1(Type::func(
            vec![Type::array(Bound(0)), Type::int(), Bound(0)],
            Type::array(Bound(0)),
        )),
        "arrayPush" => a1(Type::func(
            vec![Type::array(Bound(0)), Bound(0)],
            Type::array(Bound(0)),
        )),
        "arrayPop" => a1(Type::func(
            vec![Type::array(Bound(0))],
            Type::array(Bound(0)),
        )),
        "arraySlice" => a1(Type::func(
            vec![Type::array(Bound(0)), Type::int(), Type::int()],
            Type::array(Bound(0)),
        )),
        "arrayConcat" => a1(Type::func(
            vec![Type::array(Bound(0)), Type::array(Bound(0))],
            Type::array(Bound(0)),
        )),
        // --- bitwise, on any integer type ---
        // A shift amount is a bit position, which is an `Int` whatever is being
        // shifted -- as in Rust.
        "shl" | "shr" | "ushr" | "_primShl" | "_primShr" | "_primUshr" => {
            num(Type::func(vec![Bound(0), Type::int()], Bound(0)))
        }
        "bitAnd" | "bitOr" | "bitXor" => num(Type::func(vec![Bound(0), Bound(0)], Bound(0))),
        "bitWidth" => num(Type::func(vec![Bound(0)], Type::int())),
        "bitNot" => num(Type::func(vec![Bound(0)], Bound(0))),
        "popCount" => num(Type::func(vec![Bound(0)], Type::int())),
        // --- bytes, which are `#[UInt8]` ---
        "stringToBytes" => Scheme::mono(Type::func(
            vec![Type::string()],
            Type::array(Type::con("UInt8")),
        )),
        "bytesToString" => Scheme::mono(Type::func(
            vec![Type::array(Type::con("UInt8"))],
            Type::string(),
        )),
        "bytesToHex" => Scheme::mono(Type::func(
            vec![Type::array(Type::con("UInt8"))],
            Type::string(),
        )),
        "show" | "display" | "_primDisplay" => a1(Type::func(vec![Bound(0)], Type::string())),
        "hash" => a1(Type::func(vec![Bound(0)], Type::int())),
        "charCode" => Scheme::mono(Type::func(vec![Type::char()], Type::int())),
        "charFromCode" => Scheme::mono(Type::func(vec![Type::int()], Type::char())),
        "stringToChars" => {
            Scheme::mono(Type::func(vec![Type::string()], Type::array(Type::char())))
        }
        "charsToString" => {
            Scheme::mono(Type::func(vec![Type::array(Type::char())], Type::string()))
        }
        "concatStrings" => Scheme::mono(Type::func(
            vec![Type::array(Type::string())],
            Type::string(),
        )),
        "stringByteLength" => Scheme::mono(Type::func(vec![Type::string()], Type::int())),
        "stringByteAt" => Scheme::mono(Type::func(
            vec![Type::string(), Type::int()],
            Type::con("UInt8"),
        )),
        "stringSlice" => Scheme::mono(Type::func(
            vec![Type::string(), Type::int(), Type::int()],
            Type::string(),
        )),
        "stringCompare" => Scheme::mono(Type::func(
            vec![Type::string(), Type::string()],
            Type::int(),
        )),
        "stringIndexOf" => Scheme::mono(Type::func(
            vec![Type::string(), Type::string(), Type::int()],
            Type::int(),
        )),
        "bytesFromHex" => Scheme::mono(Type::func(
            vec![Type::string()],
            Type::Con(
                InternedString::from("Maybe"),
                vec![Type::array(Type::con("UInt8"))],
            ),
        )),
        "_primEq" | "_primNe" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type],
            ty: Type::func(vec![Bound(0), Bound(0)], Type::bool()),
        },
        // --- the mutable cell ---
        //
        // Every one of these carries `{ Mut | e }`, which is what makes mutation
        // visible in a caller's type and what stops `def r = newRef []` being
        // generalized: the binding's right-hand side is no longer pure, and the
        // effect-based value restriction refuses to quantify it.
        //
        // `Mut` is a label with no operations -- nothing handles it -- rather
        // than an effect a handler could reinterpret. It is in the row so that
        // "this touches memory" shows, and stays told apart from "this touches
        // the outside world": a caller can reasonably care about one and not the
        // other.
        "newRef" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(vec![Bound(0)], Type::reference(Bound(0)), mut_row(1)),
        },
        "getRef" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(vec![Type::reference(Bound(0))], Bound(0), mut_row(1)),
        },
        "setRef" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::reference(Bound(0)), Bound(0)],
                Type::unit(),
                mut_row(1),
            ),
        },
        // --- local state ---
        //
        // Everything below is tied to a state type `s` (`Bound(0)`) and carries
        // `{ St s | e }`. Only `runSt` removes an `St`, and only when it is
        // applied directly -- `Infer::infer_run_st` -- so this, its scheme as a
        // mere value, is the ordinary application it is at run time.
        "runSt" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::func_eff(vec![Type::unit()], Bound(0), Bound(1))],
                Bound(0),
                Bound(1),
            ),
        },
        "stNewRef" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Bound(1)],
                Type::st_ref(Bound(0), Bound(1)),
                st_row(0, 2),
            ),
        },
        "stGetRef" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::st_ref(Bound(0), Bound(1))],
                Bound(1),
                st_row(0, 2),
            ),
        },
        "stSetRef" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::st_ref(Bound(0), Bound(1)), Bound(1)],
                Type::unit(),
                st_row(0, 2),
            ),
        },
        "stNewArray" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::int(), Bound(1)],
                Type::st_array(Bound(0), Bound(1)),
                st_row(0, 2),
            ),
        },
        "stGetArray" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::st_array(Bound(0), Bound(1)), Type::int()],
                Bound(1),
                st_row(0, 2),
            ),
        },
        "stSetArray" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::st_array(Bound(0), Bound(1)), Type::int(), Bound(1)],
                Type::unit(),
                st_row(0, 2),
            ),
        },
        // An array's length is fixed when it is made, so asking is pure.
        "stArrayLen" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type],
            ty: Type::func(vec![Type::st_array(Bound(0), Bound(1))], Type::int()),
        },
        "stFreeze" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::st_array(Bound(0), Bound(1))],
                Type::array(Bound(1)),
                st_row(0, 2),
            ),
        },
        "stThaw" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::array(Bound(1))],
                Type::st_array(Bound(0), Bound(1)),
                st_row(0, 2),
            ),
        },
        // Pure: a compacted value is equal to the original, and nothing about
        // where it lives can be observed but `compactSize`.
        "compact" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type],
            ty: Type::func(vec![Bound(0)], Type::compact(Bound(0))),
        },
        "getCompact" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type],
            ty: Type::func(vec![Type::compact(Bound(0))], Bound(0)),
        },
        "compactAdd" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type],
            ty: Type::func(
                vec![Type::compact(Bound(0)), Bound(1)],
                Type::compact(Bound(1)),
            ),
        },
        "compactSize" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type],
            ty: Type::func(vec![Type::compact(Bound(0))], Type::int()),
        },
        "threadSpawn" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::Fun(
                    vec![Type::unit()],
                    Box::new(Bound(0)),
                    Box::new(thread_body_row()),
                )],
                Type::task(Bound(0)),
                thread_row(1),
            ),
        },
        "threadAwait" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(vec![Type::task(Bound(0))], Bound(0), thread_row(1)),
        },
        "threadYield" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Effect],
            ty: Type::func_eff(vec![Type::unit()], Type::unit(), thread_row(0)),
        },
        "channelNew" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(vec![Type::unit()], Type::channel(Bound(0)), thread_row(1)),
        },
        "channelSend" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::channel(Bound(0)), Bound(0)],
                Type::unit(),
                thread_row(1),
            ),
        },
        "channelReceive" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(vec![Type::channel(Bound(0))], Bound(0), thread_row(1)),
        },
        "stmNew" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(vec![Bound(0)], Type::tvar(Bound(0)), stm_row(1)),
        },
        "stmNewIO" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(vec![Bound(0)], Type::tvar(Bound(0)), thread_row(1)),
        },
        "stmRead" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::tvar(Bound(0))],
                Type::Con(InternedString::from("Maybe"), vec![Bound(0)]),
                stm_row(1),
            ),
        },
        "stmWrite" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Effect],
            ty: Type::func_eff(
                vec![Type::tvar(Bound(0)), Bound(0)],
                Type::unit(),
                stm_row(1),
            ),
        },
        "stmBegin" | "stmWait" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Effect],
            ty: Type::func_eff(vec![Type::unit()], Type::unit(), thread_row(0)),
        },
        "stmCommit" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Effect],
            ty: Type::func_eff(vec![Type::unit()], Type::bool(), thread_row(0)),
        },
        "stmNest" | "stmMerge" | "stmRollback" => Scheme {
            lacks: Vec::new(),
            preds: Vec::new(),
            quant: vec![VarKind::Effect],
            ty: Type::func_eff(vec![Type::unit()], Type::unit(), stm_row(0)),
        },
        _ => return None,
    };
    Some(s)
}

// ===========================================================================
// Pretty printing
// ===========================================================================

use std::collections::HashSet;

/// A scheme without its `forall` -- how a candidate reads in a list of them,
/// where the quantifiers are noise.
fn show_scheme_body(s: &Scheme) -> String {
    let full = s.to_string();
    match full.strip_prefix("forall") {
        Some(rest) => rest
            .split_once(". ")
            .map_or(full.clone(), |(_, body)| body.to_string()),
        None => full,
    }
}

fn show(ty: &Type) -> String {
    let mut namer = Namer::default();
    let mut s = String::new();
    let _ = write_type(&mut s, ty, &mut namer, Prec::Top, &HashSet::new());
    s
}

/// `Bound` indices (of effect kind) that a `Scheme` should NOT surface: a lone
/// effect variable that occurs once and isn't the tail of a labelled row is pure
/// noise, so `∀a e. a -> a ! e` prints as `∀a. a -> a`.
fn hidden_effect_vars(scheme: &Scheme) -> HashSet<u32> {
    let mut count: HashMap<u32, u32> = HashMap::new();
    let mut labelled_tail: HashSet<u32> = HashSet::new();
    fn walk(ty: &Type, count: &mut HashMap<u32, u32>, tails: &mut HashSet<u32>) {
        match ty {
            Type::Bound(i) => *count.entry(*i).or_default() += 1,
            Type::Var(_) | Type::RowEmpty | Type::Error => {}
            Type::Con(_, args) | Type::Tuple(args) => {
                args.iter().for_each(|a| walk(a, count, tails))
            }
            Type::Fun(args, ret, eff) => {
                args.iter().for_each(|a| walk(a, count, tails));
                walk(ret, count, tails);
                walk(eff, count, tails);
            }
            Type::Record(row) => walk(row, count, tails),
            Type::RowExtend(_, field, rest) => {
                walk(field, count, tails);
                // if this row has a label and its tail is a Bound var, that var is
                // "visible" (it prints as `{ … | e }`)
                if let Type::Bound(i) = &**rest {
                    tails.insert(*i);
                }
                walk(rest, count, tails);
            }
        }
    }
    walk(&scheme.ty, &mut count, &mut labelled_tail);
    (0..scheme.quant.len() as u32)
        .filter(|&i| scheme.quant[i as usize] == VarKind::Effect)
        .filter(|i| count.get(i).copied().unwrap_or(0) < 2 && !labelled_tail.contains(i))
        .collect()
}

#[derive(Default)]
struct Namer {
    names: HashMap<u32, String>,
    next: u32,
    /// The kinds of free variables that are not plain type variables, and how
    /// many of each kind have been named -- see [`TypeTable`].
    classes: HashMap<u32, VarKind>,
    class_next: HashMap<VarKind, u32>,
    /// The enclosing scheme's `quant` kinds, so a `Bound(i)` of row/effect kind
    /// prints as `r` / `e` rather than a plain type-variable letter.
    bound_kinds: Vec<VarKind>,
}

impl Namer {
    fn name(&mut self, id: u32) -> String {
        if let Some(n) = self.names.get(&id) {
            return n.clone();
        }
        if let Some(&kind) = self.classes.get(&id) {
            let rank = self.class_next.entry(kind).or_insert(0);
            let n = kinded_var_name(kind, *rank);
            *rank += 1;
            self.names.insert(id, n.clone());
            return n;
        }
        let n = var_name(self.next);
        self.next += 1;
        self.names.insert(id, n.clone());
        n
    }

    /// Name a bound variable of a scheme by its kind: type vars get `a`, `b`, `c`
    /// …; record-row vars `r`, `r1`, `r2` …; effect-row vars `e`, `e1`, `e2` ….
    fn bound_name(&self, i: u32) -> String {
        let kind = self
            .bound_kinds
            .get(i as usize)
            .copied()
            .unwrap_or(VarKind::Type);
        // Plain type vars share one `a, b, c…` sequence; rows, effects, and the
        // two number classes each get their own.
        let same = |k: VarKind| match kind {
            VarKind::Type => k == VarKind::Type,
            other => k == other,
        };
        let rank = self
            .bound_kinds
            .iter()
            .take(i as usize)
            .filter(|&&k| same(k))
            .count();
        kinded_var_name(kind, rank as u32)
    }
}

/// Whether a type name is one of the sized integer types.
fn meadow_num_width(name: &str) -> bool {
    matches!(
        name,
        "Int8" | "Int16" | "Int32" | "UInt8" | "UInt16" | "UInt32" | "UInt64"
    )
}

fn var_name(mut n: u32) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, char::from(b'a' + (n % 26) as u8));
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    s
}

/// A display name for the `rank`-th variable of a given kind. Multiple rows /
/// effects are disambiguated with a numeric suffix (`r`, `r1`, `r2`; `e`, `e1`).
fn kinded_var_name(kind: VarKind, rank: u32) -> String {
    let base = match kind {
        VarKind::Row => 'r',
        VarKind::Effect => 'e',
        VarKind::Num => 'n',
        VarKind::Frac => 'f',
        VarKind::Type => return var_name(rank),
    };
    if rank == 0 {
        base.to_string()
    } else {
        format!("{base}{rank}")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Prec {
    Top,
    Arrow,
    App,
}

fn write_type(
    out: &mut impl fmt::Write,
    ty: &Type,
    namer: &mut Namer,
    prec: Prec,
    hidden: &HashSet<u32>,
) -> fmt::Result {
    match ty {
        Type::Var(id) => write!(out, "{}", namer.name(*id)),
        Type::Bound(i) => write!(out, "{}", namer.bound_name(*i)),
        Type::Error => out.write_str("{error}"),
        // The unit type is written `()` — the same way its one value is, and
        // the same way an empty parameter list is.
        Type::Con(name, args) if args.is_empty() && &**name == "Unit" => out.write_str("()"),
        Type::Con(name, args) if args.is_empty() => out.write_str(hir::spelling(name)),
        // `[T]` now prints the RRB `Vector`; `List T` prints as a plain application.
        Type::Con(name, args) if &**name == "Vector" && args.len() == 1 => {
            out.write_char('[')?;
            write_type(out, &args[0], namer, Prec::Top, hidden)?;
            out.write_char(']')
        }
        Type::Con(name, args) if &**name == "Array" => {
            out.write_str("#[")?;
            write_type(out, &args[0], namer, Prec::Top, hidden)?;
            out.write_char(']')
        }
        Type::Con(name, args) => {
            let wrap = prec >= Prec::App;
            if wrap {
                out.write_char('(')?;
            }
            out.write_str(hir::spelling(name))?;
            for a in args {
                out.write_char(' ')?;
                write_type(out, a, namer, Prec::App, hidden)?;
            }
            if wrap {
                out.write_char(')')?;
            }
            Ok(())
        }
        Type::Fun(args, ret, eff) => {
            let wrap = prec >= Prec::Arrow;
            if wrap {
                out.write_char('(')?;
            }
            if args.len() == 1 {
                write_type(out, &args[0], namer, Prec::Arrow, hidden)?;
            } else {
                out.write_char('(')?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.write_str(", ")?;
                    }
                    write_type(out, a, namer, Prec::Top, hidden)?;
                }
                out.write_char(')')?;
            }
            out.write_str(" -> ")?;
            write_type(out, ret, namer, Prec::Top, hidden)?;
            write_effect_suffix(out, eff, namer, hidden)?;
            if wrap {
                out.write_char(')')?;
            }
            Ok(())
        }
        Type::Tuple(items) => {
            out.write_char('(')?;
            for (i, a) in items.iter().enumerate() {
                if i > 0 {
                    out.write_str(", ")?;
                }
                write_type(out, a, namer, Prec::Top, hidden)?;
            }
            out.write_char(')')
        }
        Type::Record(row) => {
            out.write_str("{ ")?;
            write_row(out, row, namer, hidden)?;
            out.write_str(" }")
        }
        Type::RowEmpty => out.write_str("()"),
        Type::RowExtend(..) => {
            out.write_str("(| ")?;
            write_row(out, ty, namer, hidden)?;
            out.write_str(" |)")
        }
    }
}

/// Print ` ! e` after an arrow when its latent effect is non-pure. Effect rows
/// read as `Console`, `State Int`, `{ Console, State Int }`, `{ Console | e }`
/// — a single closed label needs no braces; a lone `hidden` variable prints
/// nothing.
fn write_effect_suffix(
    out: &mut impl fmt::Write,
    eff: &Type,
    namer: &mut Namer,
    hidden: &HashSet<u32>,
) -> fmt::Result {
    let mut labels: Vec<(InternedString, &[Type])> = Vec::new();
    let mut tail: Option<String> = None;
    let mut cur = eff;
    loop {
        match cur {
            Type::RowEmpty => break,
            Type::RowExtend(name, field, rest) => {
                let args: &[Type] = match &**field {
                    Type::Tuple(a) => a,
                    _ => &[],
                };
                labels.push((*name, args));
                cur = rest;
            }
            Type::Var(id) => {
                tail = Some(namer.name(*id));
                break;
            }
            Type::Bound(i) => {
                if labels.is_empty() && hidden.contains(i) {
                    return Ok(());
                }
                tail = Some(namer.bound_name(*i));
                break;
            }
            _ => break,
        }
    }
    if labels.is_empty() && tail.is_none() {
        return Ok(()); // pure arrow
    }
    if labels.is_empty() {
        return write!(out, " ! {}", tail.unwrap()); // `a -> b ! e`
    }
    // Every effect the runtime answers, and no other: `Std.Eff`'s name for it.
    if tail.is_none()
        && labels.len() == THREAD_EFFECTS.len()
        && labels
            .iter()
            .all(|(l, args)| args.is_empty() && THREAD_EFFECTS.contains(&&**l))
    {
        return out.write_str(" ! Eff");
    }
    out.write_str(" ! ")?;
    let braces = labels.len() != 1 || tail.is_some();
    if braces {
        out.write_str("{ ")?;
    }
    for (i, (name, args)) in labels.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        out.write_str(hir::spelling(name))?;
        for a in *args {
            out.write_char(' ')?;
            write_type(out, a, namer, Prec::App, hidden)?;
        }
    }
    if let Some(t) = tail {
        out.write_char(' ')?;
        write!(out, "| {t}")?;
    }
    if braces {
        out.write_str(" }")?;
    }
    Ok(())
}

fn write_row(
    out: &mut impl fmt::Write,
    row: &Type,
    namer: &mut Namer,
    hidden: &HashSet<u32>,
) -> fmt::Result {
    let mut first = true;
    let mut cur = row;
    loop {
        match cur {
            Type::RowExtend(label, field, rest) => {
                if !first {
                    out.write_str(", ")?;
                }
                first = false;
                write!(out, "{label} : ")?;
                write_type(out, field, namer, Prec::Top, hidden)?;
                cur = rest;
            }
            Type::RowEmpty => break,
            Type::Var(id) => {
                if !first {
                    out.write_char(' ')?;
                }
                write!(out, "| {}", namer.name(*id))?;
                break;
            }
            Type::Bound(i) => {
                if !first {
                    out.write_char(' ')?;
                }
                write!(out, "| {}", namer.bound_name(*i))?;
                break;
            }
            other => {
                if !first {
                    out.write_str(", ")?;
                }
                write_type(out, other, namer, Prec::Top, hidden)?;
                break;
            }
        }
    }
    Ok(())
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut namer = Namer::default();
        write_type(f, self, &mut namer, Prec::Top, &HashSet::new())
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hidden = hidden_effect_vars(self);
        let visible: Vec<u32> = (0..self.quant.len() as u32)
            .filter(|i| !hidden.contains(i))
            .collect();
        let mut namer = Namer {
            bound_kinds: self.quant.clone(),
            ..Namer::default()
        };
        if !visible.is_empty() {
            f.write_str("forall")?;
            for i in &visible {
                write!(f, " {}", namer.bound_name(*i))?;
            }
            f.write_str(". ")?;
        }
        // `Show a => …`, or `(Show a, Ord b) => …`, as a signature says it.
        let several = self.preds.len() > 1;
        if several {
            f.write_str("(")?;
        }
        for (i, p) in self.preds.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}", hir::spelling(&p.tr))?;
            for t in &p.tys {
                f.write_str(" ")?;
                write_type(f, t, &mut namer, Prec::App, &hidden)?;
            }
        }
        if several {
            f.write_str(")")?;
        }
        if !self.preds.is_empty() {
            f.write_str(" => ")?;
        }
        write_type(f, &self.ty, &mut namer, Prec::Top, &hidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_display() {
        assert_eq!(Type::int().to_string(), "Int");
        // `[T]` syntax denotes the RRB `Vector`; a plain list prints as `List T`.
        assert_eq!(Type::vector(Type::int()).to_string(), "[Int]");
        assert_eq!(Type::list(Type::int()).to_string(), "List Int");
        assert_eq!(
            Type::func(vec![Type::int(), Type::int()], Type::bool()).to_string(),
            "Int -> Int -> Bool"
        );
        assert_eq!(
            Type::Tuple(vec![Type::int(), Type::string()]).to_string(),
            "(Int, String)"
        );
        // free (unbound) variables get printed as `a`, `b`, …
        assert_eq!(
            Type::func(vec![Type::Var(3)], Type::Var(3)).to_string(),
            "a -> a"
        );
        assert_eq!(
            Type::func(vec![Type::Var(1)], Type::Var(9)).to_string(),
            "a -> b"
        );
    }

    #[test]
    fn record_type_display() {
        let row = Type::RowExtend(
            InternedString::from("x"),
            Box::new(Type::int()),
            Box::new(Type::RowExtend(
                InternedString::from("y"),
                Box::new(Type::bool()),
                Box::new(Type::Var(0)),
            )),
        );
        assert_eq!(
            Type::Record(Box::new(row)).to_string(),
            "{ x : Int, y : Bool | a }"
        );
    }

    #[test]
    fn scheme_display_names_quantifiers() {
        let s = Scheme {
            preds: Vec::new(),
            lacks: Vec::new(),
            quant: vec![VarKind::Type, VarKind::Type],
            ty: Type::func(vec![Type::Bound(0)], Type::Bound(1)),
        };
        assert_eq!(s.to_string(), "forall a b. a -> b");
        assert_eq!(Scheme::mono(Type::int()).to_string(), "Int");
    }

    #[test]
    fn arena_unifies_var_with_concrete() {
        let mut a = Arena::new();
        let v = a.fresh();
        a.unify(v.clone(), Type::int()).unwrap();
        assert_eq!(a.zonk(&v), Type::int());
    }

    #[test]
    fn arena_unifies_two_vars_transitively() {
        let mut a = Arena::new();
        let (x, y) = (a.fresh(), a.fresh());
        a.unify(x.clone(), y.clone()).unwrap();
        a.unify(y, Type::bool()).unwrap();
        assert_eq!(a.zonk(&x), Type::bool());
    }

    #[test]
    fn arena_reports_mismatch() {
        let mut a = Arena::new();
        assert!(a.unify(Type::int(), Type::bool()).is_err());
    }

    #[test]
    fn arena_occurs_check() {
        let mut a = Arena::new();
        let v = a.fresh();
        // v = v -> v  must be rejected
        let recursive = Type::func(vec![v.clone()], v.clone());
        assert!(matches!(a.unify(v, recursive), Err(UnifyError::Occurs(..))));
    }

    #[test]
    fn arena_rewrites_rows_to_align_labels() {
        let mut a = Arena::new();
        // { x : Int | r }  ~  { y : Bool, x : Int }
        let lhs = Type::Record(Box::new(Type::RowExtend(
            InternedString::from("x"),
            Box::new(Type::int()),
            Box::new(a.fresh_row()),
        )));
        let rhs = Type::Record(Box::new(Type::RowExtend(
            InternedString::from("y"),
            Box::new(Type::bool()),
            Box::new(Type::RowExtend(
                InternedString::from("x"),
                Box::new(Type::int()),
                Box::new(Type::RowEmpty),
            )),
        )));
        a.unify(lhs.clone(), rhs).unwrap();
        assert_eq!(a.zonk(&lhs).to_string(), "{ x : Int, y : Bool }");
    }

    #[test]
    fn generalization_respects_levels() {
        // A var created at a deeper level is generalized; one at the current
        // level is not.
        let mut infer = Infer::new("t", 0);
        infer.arena.enter_level();
        let deep = infer.arena.fresh();
        infer.arena.exit_level();
        let shallow = infer.arena.fresh();

        assert_eq!(
            infer.generalize_named(None, &deep, true, &[]).quant.len(),
            1
        );
        assert_eq!(
            infer
                .generalize_named(None, &shallow, true, &[])
                .quant
                .len(),
            0
        );
    }
}

/// Number the distinct type variables of an annotation, in order of appearance.
///
/// [`ty_of`] maps a variable to `Bound(i)` through this, so the two occurrences
/// of `a` in `(f : a -> a)` become the same `Bound` and therefore, after
/// instantiation, the same meta variable.
fn collect_tyvars(t: &hir::LTypeExpr, out: &mut HashMap<VarId, u32>) {
    match t.value() {
        hir::TypeExpr::Var(v) => {
            let next = out.len() as u32;
            out.entry(*v.value()).or_insert(next);
        }
        hir::TypeExpr::Con(_, args) => args.iter().for_each(|a| collect_tyvars(a, out)),
        hir::TypeExpr::Fun(ps, r, eff) => {
            ps.iter().for_each(|p| collect_tyvars(p, out));
            collect_tyvars(r, out);
            if let Some(row) = eff {
                for (_, args) in &row.labels {
                    args.iter().for_each(|a| collect_tyvars(a, out));
                }
                if let Some(tail) = &row.tail {
                    let next = out.len() as u32;
                    out.entry(*tail.value()).or_insert(next);
                }
                if let Some((_, args)) = &row.assoc {
                    args.iter().for_each(|a| collect_tyvars(a, out));
                }
            }
        }
        hir::TypeExpr::Tuple(ts) => ts.iter().for_each(|x| collect_tyvars(x, out)),
        hir::TypeExpr::Vector(x) | hir::TypeExpr::List(x) => collect_tyvars(x, out),
        hir::TypeExpr::Record(fields, tail) => {
            fields.iter().for_each(|(_, x)| collect_tyvars(x, out));
            if let Some(tail) = tail {
                let next = out.len() as u32;
                out.entry(*tail.value()).or_insert(next);
            }
        }
        hir::TypeExpr::Row(row) => {
            for (_, args) in &row.labels {
                args.iter().for_each(|a| collect_tyvars(a, out));
            }
            if let Some(tail) = &row.tail {
                let next = out.len() as u32;
                out.entry(*tail.value()).or_insert(next);
            }
            if let Some((_, args)) = &row.assoc {
                args.iter().for_each(|a| collect_tyvars(a, out));
            }
        }
        hir::TypeExpr::Error => {}
    }
}

/// Renders several types with **one** variable naming.
///
/// `Type`'s `Display` starts a fresh [`Namer`] each time, so two independent
/// type variables both come out as `a`. That is fine for one type on its own
/// and misleading for several shown together: `fun snd (a, b) = b` would have
/// its parameters hinted `(a : a)` and `(b : a)`, which says they are the same
/// type when nothing has said so.
///
/// Sharing the namer across a declaration's types fixes that — the same
/// variable gets the same letter, and different ones do not.
#[derive(Default)]
pub struct Renderer {
    namer: Namer,
}

impl Renderer {
    pub fn new() -> Renderer {
        Renderer::default()
    }

    /// A renderer for types out of `table`, which knows which of their free
    /// variables stand for numbers.
    pub fn for_table(table: &TypeTable) -> Renderer {
        let mut r = Renderer::default();
        r.namer.classes = table.classes.clone();
        r
    }

    /// A function's *result*, as it would read after the last arrow: the return
    /// type, then the arrow's latent effect if it has one.
    ///
    /// Needed because an effect is a property of the arrow, not of the type it
    /// returns. Rendering the body's type alone loses it, and a result that
    /// silently drops `! { Console | e }` is worse than no annotation at all —
    /// it reads as a claim that the function is pure.
    pub fn render_result(&mut self, ret: &Type, eff: &Type) -> String {
        let mut out = self.render(ret);
        let _ = write_effect_suffix(&mut out, eff, &mut self.namer, &HashSet::new());
        out
    }

    pub fn render(&mut self, ty: &Type) -> String {
        let mut out = String::new();
        // Writing into a `String` cannot fail.
        let _ = Wrapper(&mut out).write_type(ty, &mut self.namer);
        out
    }
}

/// A type with every row written in one canonical order.
///
/// A row is a *set* of labels: `{ Console, Mut | e }` and `{ Mut, Console | e }` are the
/// same effect, and inference is free to build either. Anything that compares
/// two types structurally — a rename check, the core type checker, the
/// matcher below — has to put them in the same order first or it will reject
/// a type for agreeing with itself.
pub fn normalize(ty: &Type) -> Type {
    match ty {
        Type::Var(_) | Type::Bound(_) | Type::RowEmpty | Type::Error => ty.clone(),
        Type::Con(n, args) => Type::Con(*n, args.iter().map(normalize).collect()),
        Type::Fun(args, ret, eff) => Type::Fun(
            args.iter().map(normalize).collect(),
            Box::new(normalize(ret)),
            Box::new(normalize(eff)),
        ),
        Type::Tuple(items) => Type::Tuple(items.iter().map(normalize).collect()),
        Type::Record(row) => Type::Record(Box::new(normalize(row))),
        Type::RowExtend(..) => {
            let (mut labels, tail) = row_parts(ty);
            // By name, and stable — a row may legitimately repeat a label
            // (`{ Test, Test | e }` is what two nested handlers produce), and
            // the repeats have to stay next to each other.
            labels.sort_by(|(a, _), (b, _)| a.to_string().cmp(&b.to_string()));
            labels.into_iter().rev().fold(
                tail.as_ref().map_or(Type::RowEmpty, normalize),
                |acc, (l, f)| Type::RowExtend(l, Box::new(normalize(&f)), Box::new(acc)),
            )
        }
    }
}

/// Are these the same type, rows aside from their order?
pub fn equal(a: &Type, b: &Type) -> bool {
    normalize(a) == normalize(b)
}

/// The type arguments that instantiate `scheme` to `concrete`, in quantifier
/// order — or `None` if `concrete` is not an instance of it.
///
/// One-way matching, not unification: `concrete` came from a use site that
/// inference already checked, so it *is* a substitution instance, and this
/// only has to read the substitution back off. Lowering needs it because
/// instantiation happens by replacing quantifiers with fresh variables and
/// then solving them — the arguments are never written down anywhere, and
/// core has to write them down.
pub fn match_scheme(scheme: &Scheme, concrete: &Type) -> Option<Vec<Type>> {
    let mut out: HashMap<u32, Type> = HashMap::new();
    if !match_ty(&scheme.ty, concrete, &mut out) {
        return None;
    }
    // Some quantifiers can go unmentioned — a phantom row variable, or one that
    // only appears under a part the match skipped. They are unconstrained, so
    // any type will do, and the unit type is the least surprising one.
    Some(
        (0..scheme.quant.len() as u32)
            .map(|i| match (out.get(&i), scheme.quant[i as usize]) {
                (Some(t), _) => t.clone(),
                (None, VarKind::Row) | (None, VarKind::Effect) => Type::RowEmpty,
                (None, _) => Type::unit(),
            })
            .collect(),
    )
}

/// [`match_scheme`], also reading what the scheme's `where` says: `dicts[i]` is
/// the type of the dictionary its `i`th trait was answered with, when that is
/// known. An associated type appears in a scheme's `where` and sometimes
/// nowhere else, and its dictionary's type is then the only thing that says
/// what it was instantiated to.
pub fn match_scheme_with(
    scheme: &Scheme,
    concrete: &Type,
    dicts: &[Option<Type>],
) -> Option<Vec<Type>> {
    let mut out: HashMap<u32, Type> = HashMap::new();
    if !match_ty(&scheme.ty, concrete, &mut out) {
        return None;
    }
    for (p, d) in scheme.preds.iter().zip(dicts) {
        if let Some(d) = d {
            // Best effort: what the type already settled stays settled.
            let _ = match_ty(&pred_type(p), d, &mut out);
        }
    }
    Some(
        (0..scheme.quant.len() as u32)
            .map(|i| match (out.get(&i), scheme.quant[i as usize]) {
                (Some(t), _) => t.clone(),
                (None, VarKind::Row) | (None, VarKind::Effect) => Type::RowEmpty,
                (None, _) => Type::unit(),
            })
            .collect(),
    )
}

/// The type of the dictionary that answers `p`: the trait's name applied to
/// the type and its associated types.
pub fn pred_type(p: &Pred) -> Type {
    let mut args = p.tys.clone();
    args.extend(p.assocs.iter().cloned());
    Type::Con(p.tr, args)
}

fn match_ty(pat: &Type, conc: &Type, out: &mut HashMap<u32, Type>) -> bool {
    match (pat, conc) {
        // The one interesting case: a quantifier binds, or has to agree with
        // what it bound earlier.
        (Type::Bound(i), _) => match out.get(i) {
            // Up to row order: the same variable can be matched against the
            // same effect row written two different ways.
            Some(prev) => equal(prev, conc),
            None => {
                out.insert(*i, normalize(conc));
                true
            }
        },
        (Type::Var(a), Type::Var(b)) => a == b,
        (Type::RowEmpty, Type::RowEmpty) => true,
        (Type::Con(n1, a1), Type::Con(n2, a2)) => {
            n1 == n2 && a1.len() == a2.len() && a1.iter().zip(a2).all(|(x, y)| match_ty(x, y, out))
        }
        (Type::Fun(p1, r1, e1), Type::Fun(p2, r2, e2)) => {
            p1.len() == p2.len()
                && p1.iter().zip(p2).all(|(x, y)| match_ty(x, y, out))
                && match_ty(r1, r2, out)
                && match_effect(e1, e2, out)
        }
        (Type::Tuple(a), Type::Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| match_ty(x, y, out))
        }
        (Type::Record(a), Type::Record(b)) => match_ty(a, b, out),
        // A row is a set, not a list: the same labels can be written in either
        // order, so find each of the pattern's labels in the concrete row and
        // match what is left over against the pattern's tail.
        (Type::RowExtend(..), _) => match_row(pat, conc, out),
        _ => false,
    }
}

/// Match a function's effect: as any row, or -- when the scheme's row is
/// closed -- as that row opened at a use (see `Infer::open_arrows`): every label
/// it lists is there, and its end is whatever the use left it.
fn match_effect(pat: &Type, conc: &Type, out: &mut HashMap<u32, Type>) -> bool {
    let saved = out.clone();
    if match_ty(pat, conc, out) {
        return true;
    }
    *out = saved;
    let (want, ptail) = row_parts(pat);
    if ptail.is_some() {
        return false;
    }
    let (have, _) = row_parts(conc);
    want.iter()
        .all(|(l, pf)| have.iter().any(|(h, hf)| h == l && match_ty(pf, hf, out)))
}

/// Match a row pattern against a concrete row, label by label.
fn match_row(pat: &Type, conc: &Type, out: &mut HashMap<u32, Type>) -> bool {
    let (mut want, ptail) = row_parts(pat);
    let (mut have, ctail) = row_parts(conc);
    want.sort_by_key(|(l, _)| l.to_string());
    have.sort_by_key(|(l, _)| l.to_string());

    let mut leftover: Vec<(InternedString, Type)> = Vec::new();
    for (label, field) in have {
        match want.iter().position(|(l, _)| *l == label) {
            Some(i) => {
                let (_, pf) = want.remove(i);
                if !match_ty(&pf, &field, out) {
                    return false;
                }
            }
            None => leftover.push((label, field)),
        }
    }
    // Labels the pattern demanded and the row does not have.
    if !want.is_empty() {
        return false;
    }
    let rest = leftover
        .into_iter()
        .rev()
        .fold(ctail.unwrap_or(Type::RowEmpty), |acc, (l, f)| {
            Type::RowExtend(l, Box::new(f), Box::new(acc))
        });
    match ptail {
        Some(t) => match_ty(&t, &rest, out),
        None => rest == Type::RowEmpty,
    }
}

/// A row as its labels and its tail (`None` when the row is closed).
fn row_parts(mut ty: &Type) -> (Vec<(InternedString, Type)>, Option<Type>) {
    let mut labels = Vec::new();
    loop {
        match ty {
            Type::RowExtend(l, f, rest) => {
                labels.push((*l, (**f).clone()));
                ty = rest;
            }
            Type::RowEmpty => return (labels, None),
            other => return (labels, Some(other.clone())),
        }
    }
}

/// Bridges `write_type`, which wants a `fmt::Formatter`, to a `String`.
struct Wrapper<'a>(&'a mut String);

impl Wrapper<'_> {
    fn write_type(&mut self, ty: &Type, namer: &mut Namer) -> fmt::Result {
        struct Show<'a>(&'a Type, std::cell::RefCell<&'a mut Namer>);
        impl fmt::Display for Show<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_type(
                    f,
                    self.0,
                    &mut self.1.borrow_mut(),
                    Prec::Top,
                    &HashSet::new(),
                )
            }
        }
        use fmt::Write;
        write!(self.0, "{}", Show(ty, std::cell::RefCell::new(namer)))
    }
}

// --- what a value can be piped into ------------------------------------------

/// Where a value piped into a function lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piped {
    /// `x |> f` is `f x`: the value is the function's first parameter.
    First,
    /// `x |> f a b`: the value is its last parameter, and `n` come before it,
    /// which the caller has to supply. This is what a library written to chain
    /// looks like -- `table |> setWidth 40` is `setWidth 40 table`.
    Last(usize),
}

/// How a value of one type fits a function it is piped into.
#[derive(Debug, Clone)]
pub struct PipeFit {
    pub how: Piped,
    /// The parameter it fits was a type variable, so this function fits every
    /// value rather than this one. `id` and `const` are the reason to know.
    pub generic: bool,
    /// What the call gives back once the value is piped in -- for ranking a
    /// candidate against the type the surrounding expression wants.
    pub result: Type,
    /// Applying it performs an effect. A pipeline is usually a chain of plain
    /// transformations, and reading a file in the middle of one is deliberate
    /// rather than routine, so this is worth knowing when ordering a menu.
    pub effectful: bool,
}

/// Whether a value of type `subject` can be piped into `scheme`, and how.
///
/// Both are instantiated afresh, so nothing here touches the arena that
/// inferred them: the free variables of `subject` become variables of their
/// own, which is what makes a `Vector a` fit a `Vector b -> Int`.
pub fn pipes_into(scheme: &Scheme, subject: &Type) -> Option<PipeFit> {
    let mut arena = Arena::new();
    let subject = freshen(&mut arena, subject);
    let fresh = arena.fresh_quantifiers(scheme);
    let ty = Arena::subst_bound(&scheme.ty, &fresh);
    let (params, _) = parameters(&ty);
    if params.is_empty() {
        return None;
    }
    let generic = |t: &Type| matches!(t, Type::Var(_) | Type::Bound(_));
    // `x |> f` is `f x`, so the first parameter is the one it lands in.
    // What a full call performs, from this parameter on: in a curried type the
    // effect sits on the arrow that completes the application, so
    // `String -> String -> String ! Fs` performs `Fs` however early the value
    // goes in.
    let performs = |at: usize| {
        let (with, _) = parameters_with_effects(&ty);
        with[at..]
            .iter()
            .any(|(_, eff)| !matches!(arena_free(eff), Type::RowEmpty))
    };
    if let Some(result) = fits(&mut arena, &ty, 0, &subject) {
        return Some(PipeFit {
            how: Piped::First,
            generic: generic(&params[0]),
            result,
            effectful: performs(0),
        });
    }
    // A function written to chain takes its subject last, and everything
    // before it is supplied at the call: `setWidth 40`.
    let last = params.len() - 1;
    if last > 0
        && let Some(result) = fits(&mut arena, &ty, last, &subject)
    {
        return Some(PipeFit {
            how: Piped::Last(last),
            generic: generic(&params[last]),
            result,
            effectful: performs(last),
        });
    }
    None
}

/// The parameters of a curried type, in order, and what is left after them.
fn parameters(ty: &Type) -> (Vec<Type>, Type) {
    let (with, rest) = parameters_with_effects(ty);
    (with.into_iter().map(|(p, _)| p).collect(), rest)
}

/// The same, keeping what each arrow performs.
fn parameters_with_effects(ty: &Type) -> (Vec<(Type, Type)>, Type) {
    let mut params = Vec::new();
    let mut rest = ty.clone();
    while let Type::Fun(args, ret, eff) = rest {
        for a in args {
            params.push((a, (*eff).clone()));
        }
        rest = *ret;
    }
    (params, rest)
}

/// Unify parameter `at` with `subject`, and give what the function returns
/// once that parameter is supplied -- or `None`, leaving the arena as it was.
fn fits(arena: &mut Arena, ty: &Type, at: usize, subject: &Type) -> Option<Type> {
    let snap = arena.snapshot();
    let (params, result) = parameters(ty);
    let ok = arena.unify(params[at].clone(), subject.clone()).is_ok();
    if !ok {
        arena.rollback(snap);
        return None;
    }
    // What is left: the parameters after this one, then the result.
    let mut out = arena.zonk(&result);
    for p in params[at + 1..].iter().rev() {
        out = Type::Fun(vec![arena.zonk(p)], Box::new(out), Box::new(Type::RowEmpty));
    }
    arena.rollback(snap);
    Some(out)
}

/// An effect row as written, for asking whether there is one: a row that is
/// still a variable constrains nothing, and counts as none.
fn arena_free(eff: &Type) -> Type {
    match eff {
        Type::Var(_) => Type::RowEmpty,
        other => other.clone(),
    }
}

/// `ty` with every free variable replaced by one of `arena`'s, so that a type
/// inferred elsewhere can be unified here.
fn freshen(arena: &mut Arena, ty: &Type) -> Type {
    fn walk(arena: &mut Arena, ty: &Type, seen: &mut HashMap<u32, Type>) -> Type {
        match ty {
            Type::Var(id) => seen.entry(*id).or_insert_with(|| arena.fresh()).clone(),
            Type::Bound(_) | Type::RowEmpty | Type::Error => ty.clone(),
            Type::Con(n, args) => {
                Type::Con(*n, args.iter().map(|a| walk(arena, a, seen)).collect())
            }
            Type::Fun(args, ret, eff) => Type::Fun(
                args.iter().map(|a| walk(arena, a, seen)).collect(),
                Box::new(walk(arena, ret, seen)),
                Box::new(walk(arena, eff, seen)),
            ),
            Type::Tuple(items) => Type::Tuple(items.iter().map(|a| walk(arena, a, seen)).collect()),
            Type::Record(row) => Type::Record(Box::new(walk(arena, row, seen))),
            Type::RowExtend(l, t, rest) => Type::RowExtend(
                *l,
                Box::new(walk(arena, t, seen)),
                Box::new(walk(arena, rest, seen)),
            ),
        }
    }
    let mut seen = HashMap::new();
    walk(arena, ty, &mut seen)
}

#[cfg(test)]
mod pipe_tests {
    use super::*;

    fn con(name: &str) -> Type {
        Type::con(name)
    }

    /// `a -> b -> c`, curried as the language writes it.
    fn arrows(params: &[Type], ret: Type) -> Type {
        let mut out = ret;
        for p in params.iter().rev() {
            out = Type::Fun(vec![p.clone()], Box::new(out), Box::new(Type::RowEmpty));
        }
        out
    }

    fn mono(ty: Type) -> Scheme {
        Scheme {
            preds: Vec::new(),
            lacks: Vec::new(),
            quant: Vec::new(),
            ty,
        }
    }

    #[test]
    fn a_value_pipes_into_a_function_of_its_type() {
        let width = mono(arrows(&[con("String")], con("Int")));
        let fit = pipes_into(&width, &con("String")).expect("it fits");
        assert_eq!(fit.how, Piped::First);
        assert!(!fit.generic);
        assert!(equal(&fit.result, &con("Int")), "{:?}", fit.result);
    }

    #[test]
    fn a_value_does_not_pipe_into_a_function_of_another_type() {
        let width = mono(arrows(&[con("String")], con("Int")));
        assert!(pipes_into(&width, &con("Int")).is_none());
    }

    #[test]
    fn a_function_written_to_chain_takes_it_last() {
        // `setWidth : Int -> Table -> Table`, so `table |> setWidth 40`.
        let set_width = mono(arrows(&[con("Int"), con("Table")], con("Table")));
        let fit = pipes_into(&set_width, &con("Table")).expect("it fits");
        assert_eq!(fit.how, Piped::Last(1));
        // What is left to write is the one argument before it.
        assert!(equal(&fit.result, &con("Table")), "{:?}", fit.result);
    }

    #[test]
    fn the_first_parameter_wins_when_both_would_fit() {
        // `append : Table -> Table -> Table` fits either way; piping means the
        // first, which is what `x |> f` does.
        let append = mono(arrows(&[con("Table"), con("Table")], con("Table")));
        let fit = pipes_into(&append, &con("Table")).expect("it fits");
        assert_eq!(fit.how, Piped::First);
    }

    #[test]
    fn a_function_of_a_type_variable_fits_everything_and_says_so() {
        // `id : forall a. a -> a`
        let id = Scheme {
            preds: Vec::new(),
            lacks: Vec::new(),
            quant: vec![VarKind::Type],
            ty: arrows(&[Type::Bound(0)], Type::Bound(0)),
        };
        let fit = pipes_into(&id, &con("String")).expect("it fits");
        assert!(fit.generic, "a bare variable fits anything");
        assert!(equal(&fit.result, &con("String")), "{:?}", fit.result);
    }

    #[test]
    fn a_generic_container_fits_a_function_over_it() {
        // `len : forall a. Vector a -> Int` takes a `Vector String`.
        let len = Scheme {
            preds: Vec::new(),
            lacks: Vec::new(),
            quant: vec![VarKind::Type],
            ty: arrows(
                &[Type::Con("Vector".into(), vec![Type::Bound(0)])],
                con("Int"),
            ),
        };
        let subject = Type::Con("Vector".into(), vec![con("String")]);
        let fit = pipes_into(&len, &subject).expect("it fits");
        assert_eq!(fit.how, Piped::First);
        assert!(!fit.generic, "the parameter is a `Vector`, not a variable");
    }

    #[test]
    fn a_value_is_not_piped_into_something_that_is_not_a_function() {
        assert!(pipes_into(&mono(con("Int")), &con("Int")).is_none());
    }
}
