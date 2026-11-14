//! **Grades**: which code may keep its continuations as frames, and which
//! handlers may run their bodies on segments of the stack of their own.
//!
//! A frame, and a segment, can each be returned into once. So nothing may be
//! either between an operation that may be resumed twice (`@many`) and its
//! handler: resuming would return into it again. And between any operation
//! and its handler there must be what the operation's `perform` expects to
//! find there: a segment to cut off, if its handler gave its body one, or
//! none, if it did not -- a clause that answers its handler's continuation
//! from a body with no segment returns from wherever that body is, and one
//! that cuts the stack off finds nothing to cut.
//!
//! So the grade is an effect's, not a place's. An effect is **heap** if one
//! of its operations is `@many`, or a handler of it has a body that can
//! perform a heap effect it does not handle, or it is handled beside one;
//! the rows (`docs/CUT.md`, "Rows") say what each body can perform. A
//! handler of a heap effect gives its body no segment, and its body keeps
//! its continuations on the heap; a `perform` of one cuts nothing off.
//! Every other effect is handled on segments, everywhere. And code whose row
//! may hold a heap effect -- names one, has a `ManyEffect` variable, or an
//! `Effect` variable while some heap effect has no `@many` operation, or
//! says nothing -- keeps its continuations on the heap; other code may make
//! them frames, since nothing it can perform is resumed twice or handled
//! without a segment while its frames are live.
//!
//! A program with no `@many` operation has no heap effect at all, and is
//! lowered as if there were no grades.

use std::collections::HashSet;

use crate::{Consumer, Handle, Kind, Producer, Program, Row, Statement, Symbol};

/// What a piece of code can perform, as far as the program says: a row and
/// the kinds of the variables it names, or nothing known.
#[derive(Clone)]
enum Can {
    Unknown,
    Known {
        effects: HashSet<Symbol>,
        many_var: bool,
        any_var: bool,
    },
}

impl Can {
    fn of(row: Option<&Row>, kinds: &[(String, Kind)]) -> Can {
        let Some(row) = row else {
            return Can::Unknown;
        };
        let kind = |v: &String| kinds.iter().find(|(n, _)| n == v).map(|(_, k)| *k);
        Can::Known {
            effects: row.effects.iter().cloned().collect(),
            many_var: row.vars.iter().any(|v| kind(v) == Some(Kind::ManyEffect)),
            any_var: !row.vars.is_empty(),
        }
    }

    /// And what a handler's body can perform besides: what it handles.
    fn with(&self, handled: &HashSet<Symbol>) -> Can {
        match self {
            Can::Unknown => Can::Unknown,
            Can::Known {
                effects,
                many_var,
                any_var,
            } => Can::Known {
                effects: effects.union(handled).cloned().collect(),
                many_var: *many_var,
                any_var: *any_var,
            },
        }
    }
}

/// The program's grades.
pub struct Grades {
    /// The heap effects.
    heap: HashSet<Symbol>,
    /// Whether a heap effect has no `@many` operation: one an `Effect`
    /// variable may stand for.
    heap_once: bool,
}

/// The effect an operation is of: `E` of `E.op`.
pub fn effect_of(op: &Symbol) -> Symbol {
    let mut e = op.clone();
    e.path.pop();
    e
}

impl Grades {
    pub fn of(p: &Program) -> Grades {
        let many: HashSet<Symbol> = p
            .effects
            .iter()
            .filter(|e| e.ops.iter().any(|o| o.many))
            .map(|e| e.symbol.clone())
            .collect();
        let mut handles = Vec::new();
        for v in &p.vals {
            walk(&v.body, &Can::Unknown, &[], &mut handles);
        }
        for d in &p.defs {
            let can = Can::of(d.row.as_ref(), &d.effect_vars);
            walk(&d.body, &can, &d.effect_vars, &mut handles);
        }
        let mut g = Grades {
            heap: many.clone(),
            heap_once: false,
        };
        // Until nothing more is heap: each handler that handles a heap effect,
        // or whose body can perform one it does not, makes all it handles so.
        loop {
            let mut grew = false;
            for (handled, can) in &handles {
                let reached = handled.iter().any(|e| g.heap.contains(e))
                    || g.may_hold_heap(can, Some(handled));
                if reached {
                    for e in handled {
                        grew |= g.heap.insert(e.clone());
                    }
                }
            }
            g.heap_once = g.heap.iter().any(|e| !many.contains(e));
            if !grew {
                break;
            }
        }
        g
    }

    /// Whether `can` may hold a heap effect, other than those `except` names.
    fn may_hold_heap(&self, can: &Can, except: Option<&HashSet<Symbol>>) -> bool {
        let outside = |e: &&Symbol| except.is_none_or(|x| !x.contains(*e));
        match can {
            Can::Unknown => !self.heap.is_empty(),
            Can::Known {
                effects,
                many_var,
                any_var,
            } => {
                effects
                    .iter()
                    .filter(outside)
                    .any(|e| self.heap.contains(e))
                    || *many_var
                    || (*any_var && self.heap_once)
            }
        }
    }

    /// Whether `e` is a heap effect: a `perform` of it cuts no segment off.
    pub fn heap_effect(&self, e: &Symbol) -> bool {
        self.heap.contains(e)
    }

    /// Whether a handler handles a heap effect, and so gives its body no
    /// segment and keeps the body's continuations on the heap.
    pub fn heap_handle(&self, h: &Handle) -> bool {
        h.clauses
            .iter()
            .any(|c| self.heap.contains(&effect_of(&c.op)))
    }

    /// Whether code of `row`, in a definition of effect variables `kinds`,
    /// keeps its continuations on the heap.
    pub fn heap_code(&self, row: Option<&Row>, kinds: &[(String, Kind)]) -> bool {
        self.may_hold_heap(&Can::of(row, kinds), None)
    }
}

/// Each handler in `s`, with what it handles and what its body can perform
/// besides, `can` being what `s` can.
fn walk(s: &Statement, can: &Can, kinds: &[(String, Kind)], out: &mut Vec<(HashSet<Symbol>, Can)>) {
    match s {
        Statement::Cut(p, c) => {
            producer(p, can, kinds, out);
            consumer(c, can, kinds, out);
        }
        Statement::Call(_, ps, cs) | Statement::Prim(_, ps, cs) => {
            ps.iter().for_each(|p| producer(p, can, kinds, out));
            cs.iter().for_each(|c| consumer(c, can, kinds, out));
        }
        Statement::Let(_, p, rest) => {
            producer(p, can, kinds, out);
            walk(rest, can, kinds, out);
        }
        Statement::Perform(_, ps, c) => {
            ps.iter().for_each(|p| producer(p, can, kinds, out));
            consumer(c, can, kinds, out);
        }
        Statement::Handle(h) => {
            let handled: HashSet<Symbol> = h.clauses.iter().map(|c| effect_of(&c.op)).collect();
            // The clauses and the `return` clause run outside the handler;
            // its body can perform what it handles, too.
            for c in &h.clauses {
                walk(&c.body, can, kinds, out);
            }
            walk(&h.ret.2, can, kinds, out);
            let inside = can.with(&handled);
            walk(&h.body, &inside, kinds, out);
            consumer(&h.cont, can, kinds, out);
            out.push((handled, can.clone()));
        }
        Statement::At(_, s) => walk(s, can, kinds, out),
        Statement::Error(_) => {}
    }
}

fn producer(
    p: &Producer,
    can: &Can,
    kinds: &[(String, Kind)],
    out: &mut Vec<(HashSet<Symbol>, Can)>,
) {
    match p {
        Producer::Con(_, xs) | Producer::Tuple(xs) | Producer::Array(xs) => {
            xs.iter().for_each(|x| producer(x, can, kinds, out))
        }
        Producer::Record(fs) => fs.iter().for_each(|(_, x)| producer(x, can, kinds, out)),
        Producer::Mu(_, s) => walk(s, can, kinds, out),
        // A method can perform what its row says, wherever it is made.
        Producer::Cocase(ms) => {
            for m in ms {
                walk(&m.body, &Can::of(m.row.as_ref(), kinds), kinds, out);
            }
        }
        _ => {}
    }
}

fn consumer(
    c: &Consumer,
    can: &Can,
    kinds: &[(String, Kind)],
    out: &mut Vec<(HashSet<Symbol>, Can)>,
) {
    match c {
        Consumer::MuTilde(_, s) => walk(s, can, kinds, out),
        Consumer::Case(arms) => arms.iter().for_each(|a| walk(&a.body, can, kinds, out)),
        Consumer::Method(_, ps, cs) => {
            ps.iter().for_each(|p| producer(p, can, kinds, out));
            cs.iter().for_each(|k| consumer(k, can, kinds, out));
        }
        Consumer::Var(_) | Consumer::Halt => {}
    }
}
