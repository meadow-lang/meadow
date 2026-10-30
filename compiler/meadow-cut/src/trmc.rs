//! **Tail recursion modulo context**, on Cut, for every front end and both
//! runtimes.
//!
//! ```text
//! def map (f: ptr, xs: ptr; k: ptr) =
//!   <xs | case { Nil => <Nil | k>;
//!                Cons(x: i64, r: ptr) =>
//!                  <f | apply(x; μ̃ y: i64. map(f, r; μ̃ ys: ptr. <Cons(y, ys) | k>))> }>
//! ```
//!
//! The recursive call is not a tail call: what it answers goes to a
//! continuation that builds a cell around it before anything reaches `k`.
//! So every element is a frame on the way down and a return on the way back
//! up, all of them live until the last element is reached. In Cut that
//! continuation is there to be read: `μ̃ ys. <Cons(y, ys) | k>` is a
//! *context*, a constructor with a hole where the call's answer goes, and
//! everything else in it -- `y` -- was computed before the call was made.
//!
//! So build the cell first, with a placeholder where the hole is, and have
//! the call write its answer into that field rather than give it back
//! (Leijen and Lorenzen, _Tail Recursion Modulo Context_, POPL 2023). The
//! call is then the last thing done -- a jump -- and the list is built from
//! the front, in constant stack. For each definition with such a call this
//! makes a twin in destination-passing style:
//!
//! ```text
//! def map#dps (dst: ptr, at: i64, hole: ptr, f: ptr, xs: ptr; k: ptr)
//! ```
//!
//! which writes what `map(f, xs)` would have answered into field `at` of
//! `dst`, and answers `unit`. Its body is `map`'s, with every tail position
//! changed:
//!
//! * a call of a definition with a twin under a context --
//!   `g(args; μ̃ r. <K(.., r, ..) | k>)` -- builds `K(.., hole, ..)`, writes it
//!   into `dst`, and goes on as `g#dps(cell, i, hole, args; k)`: a tail call;
//! * a tail call of one -- `g(args; k)` -- goes on as
//!   `g#dps(dst, at, hole, args; k)`;
//! * anything else given to `k` is written into `dst` instead.
//!
//! `map` itself changes only at those calls under a context: it builds the
//! first cell, has the twin fill it, and answers it.
//!
//! # Which contexts
//!
//! A context is what the call's continuation does with the answer, when all
//! it does is build: `let`s of constructors and tuples of names and literals,
//! then one of those given to `k`, with the answer in exactly one field of
//! one constructor on the way down from it. So `Cons(y, r)`, but also
//! `Cons(a, Cons(b, r))` -- a cell of two, the inner one the destination --
//! and `Node(l, #tuple(v, r))`, and the same written with `let`s. The calls
//! may be of the definition itself or of any definition in its group: the
//! ones whose tail positions call each other in a cycle that goes through a
//! context at least once, which is how `evens` and `odds` take turns
//! building one list.
//!
//! What is not a context is left as it is: a continuation that computes
//! -- adds, calls, performs -- after the answer comes back, or uses it twice,
//! or puts it in a record, an array or an object.
//!
//! # Order of evaluation
//!
//! Nothing a program can see moves. Cut has already fixed the order: the
//! arguments of the call, and every other field of the cell, are computed
//! before the call is made, in the order the front end wrote; the context
//! does nothing but allocate. Building the cells before the call rather than
//! after it moves an allocation, which no program can tell. Meadow's left to
//! right order, `f x` before the recursive call, is in the Cut as written,
//! and is the same afterwards.
//!
//! # Effects
//!
//! A twin's in-place writes are safe if nothing can run the rest of a
//! recursion twice. Every resumption is one-shot unless its operation is
//! `@many` (see `docs/CUT.md`), so the half-built chain is reachable only
//! from the activation filling it, and a handler that never resumes leaves
//! it to be freed. A program with a `@many` operation is rewritten only in
//! its definitions whose activations no handler can capture
//! ([`crate::pure`]). A call under a `handle` is not under a context of the
//! definition's own `k`, so it is never taken.
//!
//! # The placeholder
//!
//! The hole holds a nullary constructor until it is filled: any of the
//! program's, or the empty tuple. It is made once, by the outermost call, and
//! handed down the twin as `hole`. A value of `ptr`'s representation, so
//! every cell is built with the descriptor its field will have: Silo's inline
//! store does not rewrite a field's descriptor, and erases what it overwrites
//! by the new value's. On Silo a nullary constructor is an odd word that is
//! never counted, so the placeholder costs nothing and ties no knot -- the
//! destination itself, which the Rust front end's pass uses, would make each
//! cell and its parent point at each other until the next step. On Glade it
//! is one small object per outermost call, not per cell.
//!
//! # In the pipeline
//!
//! After [`crate::inline`], in [`crate::lower::optimized`], so that what is
//! inlined is inlined first and nothing afterwards copies a cell or looks
//! into it. `MEADOW_CUT_TRMC=0` leaves it out, to measure it by.

use crate::{Arm, Binder, Consumer, Def, Producer, Program, Rep, Statement, Symbol};
use std::collections::{HashMap, HashSet};

/// The destination-passing twin of the definition `of`.
pub fn twin(of: &Symbol) -> Symbol {
    of.child("#dps")
}

/// `p` with each definition that calls its group under a context given a
/// twin in destination-passing style.
pub fn program(p: &Program) -> Program {
    let many = p.effects.iter().any(|e| e.ops.iter().any(|o| o.many));
    let pure = many.then(|| crate::pure::defs(p));
    let at: HashMap<&Symbol, usize> = p
        .defs
        .iter()
        .enumerate()
        .map(|(i, d)| (&d.symbol, i))
        .collect();
    // What each definition the rewrite can take calls from its tail
    // positions, and whether through a context.
    let calls: Vec<Option<Vec<(usize, bool)>>> = p
        .defs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let [k] = d.conts.as_slice() else {
                return None;
            };
            if pure.as_ref().is_some_and(|pure| !pure.contains(&i)) {
                return None;
            }
            let mut seen = Collect::default();
            Walk::new(k).statement(&d.body, &mut seen)?;
            Some(
                seen.calls
                    .iter()
                    .filter_map(|(g, site)| at.get(g).map(|j| (*j, *site)))
                    .collect(),
            )
        })
        .collect();
    let chosen = chosen(&calls);
    if chosen.is_empty() {
        return p.clone();
    }
    let symbols: HashSet<Symbol> = chosen.iter().map(|i| p.defs[*i].symbol.clone()).collect();
    let hole = placeholder(p);
    let mut out = p.clone();
    for &i in &chosen {
        let d = &p.defs[i];
        let k = &d.conts[0];
        let mut names = Names::of(d);
        let mut entry = Entry {
            chosen: &symbols,
            names: &mut names,
            hole: &hole,
        };
        let walked = Walk::new(k).statement(&d.body, &mut entry);
        out.defs[i].body = walked.expect("walked once already");
        let dst = names.fresh("dst");
        let at = names.fresh("at");
        let hole = names.fresh("hole");
        let mut dps = Dps {
            chosen: &symbols,
            names: &mut names,
            dst: dst.clone(),
            at: at.clone(),
            hole: hole.clone(),
        };
        let body = Walk::new(k).statement(&d.body, &mut dps);
        let ptr = |name: &str| Binder {
            name: name.to_string(),
            rep: Rep::Ptr,
        };
        let mut params = vec![
            ptr(&dst),
            Binder {
                name: at,
                rep: Rep::I64,
            },
            ptr(&hole),
        ];
        params.extend(d.params.iter().cloned());
        out.defs.push(Def {
            symbol: twin(&d.symbol),
            rep_vars: d.rep_vars.clone(),
            effect_vars: d.effect_vars.clone(),
            params,
            conts: d.conts.clone(),
            body: body.expect("walked once already"),
        });
    }
    out
}

/// The definitions to rewrite: those in a cycle of tail calls that goes
/// through a context at least once. Tarjan's components, without recursion,
/// since a program may chain many definitions by tail calls.
fn chosen(calls: &[Option<Vec<(usize, bool)>>]) -> Vec<usize> {
    let n = calls.len();
    let edges = |i: usize| -> &[(usize, bool)] { calls[i].as_deref().unwrap_or(&[]) };
    let taken = |j: usize| calls[j].is_some();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on = vec![false; n];
    let mut stack = Vec::new();
    let mut component = vec![usize::MAX; n];
    let mut components = 0;
    let mut next = 0;
    for root in (0..n).filter(|i| taken(*i)) {
        if index[root] != usize::MAX {
            continue;
        }
        // Each frame is a node and how many of its edges are looked at.
        let mut work = vec![(root, 0usize)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on[root] = true;
        while let Some(&mut (v, ref mut e)) = work.last_mut() {
            if let Some(&(w, _)) = edges(v).get(*e) {
                *e += 1;
                if !taken(w) {
                    continue;
                }
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on[w] = true;
                    work.push((w, 0));
                } else if on[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(u, _)) = work.last() {
                low[u] = low[u].min(low[v]);
            }
            if low[v] == index[v] {
                while let Some(w) = stack.pop() {
                    on[w] = false;
                    component[w] = components;
                    if w == v {
                        break;
                    }
                }
                components += 1;
            }
        }
    }
    let mut through = vec![false; components];
    for i in (0..n).filter(|i| taken(*i)) {
        for &(j, site) in edges(i) {
            if site && taken(j) && component[i] == component[j] {
                through[component[i]] = true;
            }
        }
    }
    (0..n)
        .filter(|i| taken(*i) && through[component[*i]])
        .collect()
}

/// What the hole holds until it is filled: a nullary constructor of the
/// program's, or the empty tuple where it declares none.
fn placeholder(p: &Program) -> Producer {
    p.datas
        .iter()
        .find_map(|d| {
            d.ctors
                .iter()
                .find(|(_, fields)| fields.is_empty())
                .map(|(name, _)| Producer::Con(d.symbol.child(name), Vec::new()))
        })
        .unwrap_or(Producer::Tuple(Vec::new()))
}

// --- contexts --------------------------------------------------------------

/// A call's continuation that only builds around what the call answers.
struct Context {
    /// What is built on the way that does not hold the answer, in order.
    lets: Vec<(Binder, Producer)>,
    /// The constructors that hold it, the outermost first: each one's
    /// fields, with the next one -- or, for the last, the answer -- at `at`.
    cells: Vec<Cell>,
    /// The name of `k` the outermost is given to.
    to: String,
}

struct Cell {
    /// The constructor, or `None` for a tuple.
    con: Option<Symbol>,
    fields: Vec<Producer>,
    at: usize,
}

impl Cell {
    fn with(&self, at: Producer) -> Producer {
        let mut fields = self.fields.clone();
        fields[self.at] = at;
        match &self.con {
            Some(k) => Producer::Con(k.clone(), fields),
            None => Producer::Tuple(fields),
        }
    }
}

/// A constructor or tuple of names and literals, or of more of the same:
/// what may be built at another time than written, since building it is all
/// it does.
fn built(p: &Producer) -> bool {
    match p {
        Producer::Con(_, xs) | Producer::Tuple(xs) => xs.iter().all(|x| {
            built(x)
                || matches!(
                    x,
                    Producer::Var(_)
                        | Producer::Int(_)
                        | Producer::Float(_)
                        | Producer::Char(_)
                        | Producer::Str(_)
                        | Producer::Bool(_)
                        | Producer::Unit
                        | Producer::Desc(_)
                )
        }),
        _ => false,
    }
}

/// `p` with each name `held` binds put in its place.
fn substitute(p: &Producer, held: &HashMap<String, Producer>) -> Producer {
    match p {
        Producer::Var(x) => held.get(x).cloned().unwrap_or_else(|| p.clone()),
        Producer::Con(k, xs) => {
            Producer::Con(k.clone(), xs.iter().map(|x| substitute(x, held)).collect())
        }
        Producer::Tuple(xs) => Producer::Tuple(xs.iter().map(|x| substitute(x, held)).collect()),
        _ => p.clone(),
    }
}

/// How many times `p`, built as [`built`] says, mentions `x`.
fn occurrences(p: &Producer, x: &str) -> usize {
    match p {
        Producer::Var(y) => usize::from(y == x),
        Producer::Con(_, xs) | Producer::Tuple(xs) => xs.iter().map(|y| occurrences(y, x)).sum(),
        _ => 0,
    }
}

// --- the walk --------------------------------------------------------------

/// What becomes of each tail position of a definition: where it gives
/// something to its continuation `k`, or to a name `k` is bound to.
trait Leaves {
    /// Whether a call of `g` under a context is one to take.
    fn takes(&self, g: &Symbol) -> bool;
    /// `<p | to>`.
    fn give(&mut self, p: &Producer, to: &str) -> Statement;
    /// `g(args; to)`.
    fn call(&mut self, g: &Symbol, args: &[Producer], to: &str) -> Statement;
    /// `g(args; μ̃ r. context)`, as written in `original`.
    fn site(
        &mut self,
        g: &Symbol,
        args: &[Producer],
        context: &Context,
        original: &Statement,
    ) -> Statement;
    /// `to`, where it is the continuation of anything else.
    fn answer(&mut self, to: &str) -> Consumer;
}

/// A definition's body, walked to its tail positions: `None` where `k` is
/// used anywhere else -- captured by an object, or in a handler -- which the
/// rewrite does not follow.
struct Walk {
    /// `k`, and the names a `μ` cut against it gives it.
    ks: Vec<String>,
}

impl Walk {
    fn new(k: &str) -> Walk {
        Walk {
            ks: vec![k.to_string()],
        }
    }

    fn is_k(&self, c: &Consumer) -> Option<String> {
        match c {
            Consumer::Var(j) if self.ks.contains(j) => Some(j.clone()),
            _ => None,
        }
    }

    /// `Some` where `p` does not mention `k`.
    fn free(&self, p: &Producer) -> Option<()> {
        (!self.ks.iter().any(|k| mentions_p(p, k))).then_some(())
    }

    fn statement(&mut self, s: &Statement, v: &mut dyn Leaves) -> Option<Statement> {
        Some(match s {
            Statement::Cut(p, c) => match (p, self.is_k(c)) {
                (Producer::Mu(j, body), Some(_)) => {
                    self.ks.push(j.clone());
                    let body = self.statement(body, v);
                    self.ks.pop();
                    Statement::Cut(Producer::Mu(j.clone(), Box::new(body?)), c.clone())
                }
                (p, Some(to)) => {
                    self.free(p)?;
                    v.give(p, &to)
                }
                (p, None) => {
                    self.free(p)?;
                    Statement::Cut(p.clone(), self.consumer(c, v)?)
                }
            },
            Statement::Call(g, args, conts) => {
                for a in args {
                    self.free(a)?;
                }
                match conts.as_slice() {
                    [c] if self.is_k(c).is_some() => v.call(g, args, &self.is_k(c)?),
                    [Consumer::MuTilde(r, body)] if v.takes(g) => match self.context(r, body) {
                        Some(context) => v.site(g, args, &context, s),
                        None => Statement::Call(g.clone(), args.clone(), self.consumers(conts, v)?),
                    },
                    _ => Statement::Call(g.clone(), args.clone(), self.consumers(conts, v)?),
                }
            }
            Statement::Prim(op, args, conts) => {
                for a in args {
                    self.free(a)?;
                }
                Statement::Prim(op.clone(), args.clone(), self.consumers(conts, v)?)
            }
            Statement::Let(b, p, rest) => {
                self.free(p)?;
                Statement::Let(b.clone(), p.clone(), Box::new(self.statement(rest, v)?))
            }
            Statement::Handle(h) => {
                let inside = std::iter::once(&h.body)
                    .chain(std::iter::once(&h.ret.2))
                    .chain(h.clauses.iter().map(|c| &c.body));
                for s in inside {
                    if self.ks.iter().any(|k| mentions_s(s, k)) {
                        return None;
                    }
                }
                let mut h = (**h).clone();
                h.cont = self.consumer(&h.cont, v)?;
                Statement::Handle(Box::new(h))
            }
            Statement::Perform(op, args, c) => {
                for a in args {
                    self.free(a)?;
                }
                Statement::Perform(op.clone(), args.clone(), self.consumer(c, v)?)
            }
            Statement::Error(_) => s.clone(),
        })
    }

    fn consumers(&mut self, cs: &[Consumer], v: &mut dyn Leaves) -> Option<Vec<Consumer>> {
        cs.iter().map(|c| self.consumer(c, v)).collect()
    }

    fn consumer(&mut self, c: &Consumer, v: &mut dyn Leaves) -> Option<Consumer> {
        Some(match c {
            Consumer::Var(_) if self.is_k(c).is_some() => v.answer(&self.is_k(c)?),
            Consumer::Var(_) | Consumer::Halt => c.clone(),
            Consumer::MuTilde(b, s) => {
                Consumer::MuTilde(b.clone(), Box::new(self.statement(s, v)?))
            }
            Consumer::Case(arms) => Consumer::Case(
                arms.iter()
                    .map(|a| {
                        Some(Arm {
                            pattern: a.pattern.clone(),
                            fields: a.fields.clone(),
                            body: self.statement(&a.body, v)?,
                        })
                    })
                    .collect::<Option<_>>()?,
            ),
            Consumer::Method(m, args, conts) => {
                for a in args {
                    self.free(a)?;
                }
                Consumer::Method(m.clone(), args.clone(), self.consumers(conts, v)?)
            }
        })
    }

    /// `μ̃ r. s` as a context, if it is one.
    fn context(&self, r: &Binder, s: &Statement) -> Option<Context> {
        if r.rep != Rep::Ptr {
            return None;
        }
        let mut lets = Vec::new();
        let mut s = s;
        let (root, to) = loop {
            match s {
                Statement::Let(b, p, rest) if built(p) => {
                    lets.push((b.clone(), p.clone()));
                    s = rest;
                }
                Statement::Cut(p, c) if built(p) => break (p.clone(), self.is_k(c)?),
                _ => return None,
            }
        };
        for (_, p) in &lets {
            self.free(p)?;
        }
        self.free(&root)?;
        // What holds the answer, or holds what does, is put where it is
        // used, so that the cells are one constructor in another; what does
        // not is built before the call, as written.
        let mut holding = HashSet::from([r.name.clone()]);
        let mut held = HashMap::new();
        let mut kept = Vec::new();
        for (b, p) in lets {
            let p = substitute(&p, &held);
            if holding.iter().any(|x| occurrences(&p, x) > 0) {
                holding.insert(b.name.clone());
                held.insert(b.name, p);
            } else {
                kept.push((b, p));
            }
        }
        let root = substitute(&root, &held);
        // Once, and so each `let` that holds it used once: the hole is one
        // field of one cell.
        if occurrences(&root, &r.name) != 1 {
            return None;
        }
        let mut cells = Vec::new();
        let mut cur = root;
        loop {
            let (con, fields) = match cur {
                Producer::Con(k, xs) => (Some(k), xs),
                Producer::Tuple(xs) => (None, xs),
                _ => return None,
            };
            let at = fields.iter().position(|f| occurrences(f, &r.name) > 0)?;
            let next = fields[at].clone();
            cells.push(Cell { con, fields, at });
            if next == Producer::Var(r.name.clone()) {
                break;
            }
            cur = next;
        }
        Some(Context {
            lets: kept,
            cells,
            to,
        })
    }
}

/// Fresh names for a definition: ones nothing in it is called.
struct Names {
    taken: HashSet<String>,
    next: usize,
}

impl Names {
    fn of(d: &Def) -> Names {
        let mut taken = HashSet::new();
        taken.extend(d.params.iter().map(|b| b.name.clone()));
        taken.extend(d.conts.iter().cloned());
        taken.extend(d.rep_vars.iter().map(|(_, desc)| desc.clone()));
        binders_s(&d.body, &mut taken);
        Names { taken, next: 0 }
    }

    fn fresh(&mut self, stem: &str) -> String {
        loop {
            self.next += 1;
            let name = format!("{stem}__tm{}", self.next);
            if self.taken.insert(name.clone()) {
                return name;
            }
        }
    }
}

/// The cells of `context`, built with `hole` in the last one's field, as
/// `let`s: what they are, the outermost's name, and the innermost's.
fn cells(
    context: &Context,
    hole: &str,
    names: &mut Names,
) -> (Vec<(Binder, Producer)>, String, String) {
    let mut lets = context.lets.clone();
    let named: Vec<String> = context.cells.iter().map(|_| names.fresh("cell")).collect();
    for (i, cell) in context.cells.iter().enumerate().rev() {
        let inner = match named.get(i + 1) {
            Some(next) => Producer::Var(next.clone()),
            None => Producer::Var(hole.to_string()),
        };
        lets.push((
            Binder {
                name: named[i].clone(),
                rep: Rep::Ptr,
            },
            cell.with(inner),
        ));
    }
    let root = named[0].clone();
    let last = named[named.len() - 1].clone();
    (lets, root, last)
}

fn with_lets(lets: Vec<(Binder, Producer)>, body: Statement) -> Statement {
    lets.into_iter()
        .rev()
        .fold(body, |s, (b, p)| Statement::Let(b, p, Box::new(s)))
}

fn set_field(dst: &str, at: Producer, value: Producer, then: Consumer) -> Statement {
    Statement::Prim(
        "setField".to_string(),
        vec![Producer::Var(dst.to_string()), at, value],
        vec![then],
    )
}

/// The arguments of a twin's call: where, the placeholder, then the call's.
fn twin_args(dst: &str, at: Producer, hole: &str, args: &[Producer]) -> Vec<Producer> {
    let mut all = vec![
        Producer::Var(dst.to_string()),
        at,
        Producer::Var(hole.to_string()),
    ];
    all.extend(args.iter().cloned());
    all
}

/// Only looks: what each tail position calls, and whether through a context.
#[derive(Default)]
struct Collect {
    calls: Vec<(Symbol, bool)>,
}

impl Leaves for Collect {
    fn takes(&self, _: &Symbol) -> bool {
        true
    }
    fn give(&mut self, _: &Producer, _: &str) -> Statement {
        Statement::Error(String::new())
    }
    fn call(&mut self, g: &Symbol, _: &[Producer], _: &str) -> Statement {
        self.calls.push((g.clone(), false));
        Statement::Error(String::new())
    }
    fn site(&mut self, g: &Symbol, _: &[Producer], _: &Context, _: &Statement) -> Statement {
        self.calls.push((g.clone(), true));
        Statement::Error(String::new())
    }
    fn answer(&mut self, _: &str) -> Consumer {
        Consumer::Halt
    }
}

/// The definition itself: at a call of its group under a context, build the
/// cells, have the twin fill the innermost, and answer the outermost.
struct Entry<'a> {
    chosen: &'a HashSet<Symbol>,
    names: &'a mut Names,
    hole: &'a Producer,
}

impl Leaves for Entry<'_> {
    fn takes(&self, g: &Symbol) -> bool {
        self.chosen.contains(g)
    }
    fn give(&mut self, p: &Producer, to: &str) -> Statement {
        Statement::Cut(p.clone(), Consumer::Var(to.to_string()))
    }
    fn call(&mut self, g: &Symbol, args: &[Producer], to: &str) -> Statement {
        Statement::Call(
            g.clone(),
            args.to_vec(),
            vec![Consumer::Var(to.to_string())],
        )
    }
    fn site(&mut self, g: &Symbol, args: &[Producer], c: &Context, _: &Statement) -> Statement {
        let hole = self.names.fresh("hole");
        let (mut lets, root, last) = cells(c, &hole, self.names);
        let at = Producer::Int(c.cells[c.cells.len() - 1].at as i64);
        let filled = Binder {
            name: self.names.fresh("filled"),
            rep: Rep::Unit,
        };
        let answer = Statement::Cut(Producer::Var(root), Consumer::Var(c.to.clone()));
        let fill = Statement::Call(
            twin(g),
            twin_args(&last, at, &hole, args),
            vec![Consumer::MuTilde(filled, Box::new(answer))],
        );
        lets.insert(
            0,
            (
                Binder {
                    name: hole,
                    rep: Rep::Ptr,
                },
                self.hole.clone(),
            ),
        );
        with_lets(lets, fill)
    }
    fn answer(&mut self, to: &str) -> Consumer {
        Consumer::Var(to.to_string())
    }
}

/// The twin: every tail position writes into field `at` of `dst`.
struct Dps<'a> {
    chosen: &'a HashSet<Symbol>,
    names: &'a mut Names,
    dst: String,
    at: String,
    hole: String,
}

impl Dps<'_> {
    fn write(&self, value: Producer, to: &str) -> Statement {
        set_field(
            &self.dst,
            Producer::Var(self.at.clone()),
            value,
            Consumer::Var(to.to_string()),
        )
    }
}

impl Leaves for Dps<'_> {
    fn takes(&self, g: &Symbol) -> bool {
        self.chosen.contains(g)
    }
    fn give(&mut self, p: &Producer, to: &str) -> Statement {
        match p {
            // What the lowering makes a value of only where a definition is
            // called: bound first.
            Producer::Mu(..) => Statement::Cut(p.clone(), self.answer(to)),
            p => self.write(p.clone(), to),
        }
    }
    fn call(&mut self, g: &Symbol, args: &[Producer], to: &str) -> Statement {
        if self.chosen.contains(g) {
            let args = twin_args(&self.dst, Producer::Var(self.at.clone()), &self.hole, args);
            Statement::Call(twin(g), args, vec![Consumer::Var(to.to_string())])
        } else {
            Statement::Call(g.clone(), args.to_vec(), vec![self.answer(to)])
        }
    }
    fn site(&mut self, g: &Symbol, args: &[Producer], c: &Context, _: &Statement) -> Statement {
        let (lets, root, last) = cells(c, &self.hole, self.names);
        let at = Producer::Int(c.cells[c.cells.len() - 1].at as i64);
        let filled = Binder {
            name: self.names.fresh("filled"),
            rep: Rep::Unit,
        };
        let next = Statement::Call(
            twin(g),
            twin_args(&last, at, &self.hole, args),
            vec![Consumer::Var(c.to.clone())],
        );
        let write = set_field(
            &self.dst,
            Producer::Var(self.at.clone()),
            Producer::Var(root),
            Consumer::MuTilde(filled, Box::new(next)),
        );
        with_lets(lets, write)
    }
    fn answer(&mut self, to: &str) -> Consumer {
        let v = self.names.fresh("answer");
        let write = self.write(Producer::Var(v.clone()), to);
        Consumer::MuTilde(
            Binder {
                name: v,
                rep: Rep::Ptr,
            },
            Box::new(write),
        )
    }
}

// --- names -----------------------------------------------------------------

/// Whether `p` mentions `x` anywhere, bound again or not: an answer that may
/// say yes where the name is shadowed, which only leaves a definition as it
/// was.
fn mentions_p(p: &Producer, x: &str) -> bool {
    match p {
        Producer::Var(y) => y == x,
        Producer::Con(_, xs) | Producer::Tuple(xs) | Producer::Array(xs) => {
            xs.iter().any(|y| mentions_p(y, x))
        }
        Producer::Record(fs) => fs.iter().any(|(_, y)| mentions_p(y, x)),
        Producer::Mu(_, s) => mentions_s(s, x),
        Producer::Cocase(ms) => ms.iter().any(|m| mentions_s(&m.body, x)),
        _ => false,
    }
}

fn mentions_c(c: &Consumer, x: &str) -> bool {
    match c {
        Consumer::Var(y) => y == x,
        Consumer::Halt => false,
        Consumer::MuTilde(_, s) => mentions_s(s, x),
        Consumer::Case(arms) => arms.iter().any(|a| mentions_s(&a.body, x)),
        Consumer::Method(_, args, conts) => {
            args.iter().any(|a| mentions_p(a, x)) || conts.iter().any(|c| mentions_c(c, x))
        }
    }
}

fn mentions_s(s: &Statement, x: &str) -> bool {
    match s {
        Statement::Cut(p, c) => mentions_p(p, x) || mentions_c(c, x),
        Statement::Call(_, args, conts) | Statement::Prim(_, args, conts) => {
            args.iter().any(|a| mentions_p(a, x)) || conts.iter().any(|c| mentions_c(c, x))
        }
        Statement::Let(_, p, rest) => mentions_p(p, x) || mentions_s(rest, x),
        Statement::Handle(h) => {
            mentions_s(&h.body, x)
                || mentions_s(&h.ret.2, x)
                || h.clauses.iter().any(|c| mentions_s(&c.body, x))
                || mentions_c(&h.cont, x)
        }
        Statement::Perform(_, args, c) => args.iter().any(|a| mentions_p(a, x)) || mentions_c(c, x),
        Statement::Error(_) => false,
    }
}

/// Every name `s` binds, into `out`.
fn binders_s(s: &Statement, out: &mut HashSet<String>) {
    match s {
        Statement::Cut(p, c) => {
            binders_p(p, out);
            binders_c(c, out);
        }
        Statement::Call(_, args, conts) | Statement::Prim(_, args, conts) => {
            args.iter().for_each(|a| binders_p(a, out));
            conts.iter().for_each(|c| binders_c(c, out));
        }
        Statement::Let(b, p, rest) => {
            out.insert(b.name.clone());
            binders_p(p, out);
            binders_s(rest, out);
        }
        Statement::Handle(h) => {
            out.insert(h.body_cont.clone());
            out.insert(h.ret.0.name.clone());
            out.insert(h.ret.1.clone());
            binders_s(&h.body, out);
            binders_s(&h.ret.2, out);
            for c in &h.clauses {
                out.extend(c.params.iter().map(|b| b.name.clone()));
                out.insert(c.resumption.clone());
                out.insert(c.cont.clone());
                binders_s(&c.body, out);
            }
            binders_c(&h.cont, out);
        }
        Statement::Perform(_, args, c) => {
            args.iter().for_each(|a| binders_p(a, out));
            binders_c(c, out);
        }
        Statement::Error(_) => {}
    }
}

fn binders_p(p: &Producer, out: &mut HashSet<String>) {
    match p {
        Producer::Con(_, xs) | Producer::Tuple(xs) | Producer::Array(xs) => {
            xs.iter().for_each(|x| binders_p(x, out));
        }
        Producer::Record(fs) => fs.iter().for_each(|(_, x)| binders_p(x, out)),
        Producer::Mu(k, s) => {
            out.insert(k.clone());
            binders_s(s, out);
        }
        Producer::Cocase(ms) => {
            for m in ms {
                out.extend(m.params.iter().map(|b| b.name.clone()));
                out.extend(m.conts.iter().cloned());
                binders_s(&m.body, out);
            }
        }
        _ => {}
    }
}

fn binders_c(c: &Consumer, out: &mut HashSet<String>) {
    match c {
        Consumer::Var(_) | Consumer::Halt => {}
        Consumer::MuTilde(b, s) => {
            out.insert(b.name.clone());
            binders_s(s, out);
        }
        Consumer::Case(arms) => {
            for a in arms {
                out.extend(a.fields.iter().map(|b| b.name.clone()));
                binders_s(&a.body, out);
            }
        }
        Consumer::Method(_, args, conts) => {
            args.iter().for_each(|a| binders_p(a, out));
            conts.iter().for_each(|c| binders_c(c, out));
        }
    }
}
