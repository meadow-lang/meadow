//! **Small definitions inlined at their calls, and what that leaves
//! simplified.**
//!
//! A front end hands the back end a program of Cut, and nothing between the
//! two inlined anything: Meadow's front ends inline on their core, before
//! Cut, and a front end that does not -- Idyll's -- left every small function
//! a call, every pair a value built and taken apart at once, and every
//! closure an object made to be called once. LLVM cannot take those out on
//! Silo, since making a block is a call into the runtime, and on Glade
//! nothing else would. So this does, on Cut, for every front end and both
//! runtimes:
//!
//! - **a call of a small definition** -- one of at most [`SMALL`] nodes, not
//!   generic in a representation, that does not call itself -- is its body,
//!   its parameters bound to the arguments and its continuations to what the
//!   call was given. What it calls is inlined in turn, to [`DEPTH`], but never
//!   a definition already being inlined there;
//! - **a known case**: a constructor or tuple given to a `case` is the arm
//!   it takes, its fields bound to what the constructor was given;
//! - **a known call**: an object given one of its own methods is that
//!   method's body;
//! - **a binding**: a name bound to a variable or a literal is that, where it
//!   is used; one bound to a constructor of those or an object, and used once
//!   and not in a method or under a handler -- where it may run many times, or
//!   with other evidence -- is made where it is used, so that a case or a
//!   method call there is known; and one used nowhere is not made.
//!   `<μ k. s | c>` is `s` with `c` for `k` where `c` is a name, or `k` is
//!   used at most once and not in a method or under a handler;
//! - **`prim if` of a literal** is the branch it takes.
//!
//! Binding is substitution, which is safe because every name a statement
//! binds is renamed first to one nothing else is called, and a copy of a
//! definition gets names of its own. What is substituted is either an atom
//! -- a variable, a literal -- or something used once, so nothing is copied
//! but what is inlined. What cannot be substituted is bound where it was:
//! a value by `<v | μ̃ x. s>`, a continuation by `<μ k. s | c>`, which
//! lowering makes a join point.
//!
//! It runs on a program to run, after [`crate::specialize`] and before
//! lowering; `MEADOW_CUT_INLINE=0` leaves it out, to measure it by, and
//! `meadow cut --emit inlined` prints what it made.

use crate::{
    Arm, Binder, Clause, Consumer, Def, Handle, Method, Pattern, Producer, Program, Statement,
    Symbol,
};
use std::collections::HashMap;

/// How large, in nodes, a definition inlined at its calls may be.
pub const SMALL: usize = 40;
/// How deep inlining goes into what it inlined.
pub const DEPTH: usize = 4;

/// `p` with its small definitions inlined at their calls, and what that
/// leaves simplified.
pub fn program(p: &Program) -> Program {
    let mut fresh = Fresh::default();
    let mut out = p.clone();
    for d in &mut out.defs {
        *d = fresh.def(d);
    }
    for v in &mut out.vals {
        v.body = Rename::new(&mut fresh).statement(&v.body);
    }
    let mut uses = HashMap::new();
    for d in &out.defs {
        count(&d.body, false, &mut uses);
    }
    for v in &out.vals {
        count(&v.body, false, &mut uses);
    }
    let table: HashMap<Symbol, Def> = out
        .defs
        .iter()
        .filter(|d| d.rep_vars.is_empty() && size(&d.body) <= SMALL && !calls(&d.body, &d.symbol))
        .map(|d| (d.symbol.clone(), d.clone()))
        .collect();
    let mut s = Simplify {
        table,
        uses,
        vals: HashMap::new(),
        conts: HashMap::new(),
        fresh,
        stack: Vec::new(),
    };
    for d in &mut out.defs {
        s.stack = vec![d.symbol.clone()];
        d.body = s.stmt(&d.body);
    }
    s.stack.clear();
    for v in &mut out.vals {
        v.body = s.stmt(&v.body);
    }
    out
}

// --- names -----------------------------------------------------------------

#[derive(Default)]
struct Fresh {
    next: usize,
}

impl Fresh {
    /// A name nothing else is called: `x__12`, of `x` or of `x__3`.
    fn name(&mut self, old: &str) -> String {
        self.next += 1;
        let stem = old.split("__").next().unwrap_or(old);
        format!("{stem}__{}", self.next)
    }

    /// `d` with every name it binds, its parameters among them, renamed.
    fn def(&mut self, d: &Def) -> Def {
        let mut r = Rename::new(self);
        let mut out = d.clone();
        out.params = d.params.iter().map(|b| r.bind(b)).collect();
        out.conts = d.conts.iter().map(|k| r.bind_name(k)).collect();
        out.rep_vars = d
            .rep_vars
            .iter()
            .map(|(v, desc)| (v.clone(), r.var(desc)))
            .collect();
        out.body = r.statement(&d.body);
        out
    }
}

/// Renaming every name a statement binds, as it goes: the names in scope,
/// by what each was called, and what each name bound in a scope was before
/// it, to put back when the scope ends.
struct Rename<'a> {
    fresh: &'a mut Fresh,
    map: HashMap<String, String>,
    undo: Vec<(String, Option<String>)>,
}

impl<'a> Rename<'a> {
    fn new(fresh: &'a mut Fresh) -> Self {
        Rename {
            fresh,
            map: HashMap::new(),
            undo: Vec::new(),
        }
    }

    fn var(&self, x: &str) -> String {
        self.map.get(x).cloned().unwrap_or_else(|| x.to_string())
    }

    /// `old` bound here: its new name, which it is to the end of the scope.
    fn bind_name(&mut self, old: &str) -> String {
        let new = self.fresh.name(old);
        let was = self.map.insert(old.to_string(), new.clone());
        self.undo.push((old.to_string(), was));
        new
    }

    fn bind(&mut self, b: &Binder) -> Binder {
        Binder {
            name: self.bind_name(&b.name),
            rep: b.rep.clone(),
        }
    }

    /// `f` renaming a scope: what its binders shadowed is in scope again
    /// after it.
    fn scoped<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let mark = self.undo.len();
        let out = f(self);
        while self.undo.len() > mark {
            let Some((name, was)) = self.undo.pop() else {
                break;
            };
            match was {
                Some(w) => self.map.insert(name, w),
                None => self.map.remove(&name),
            };
        }
        out
    }

    fn statement(&mut self, s: &Statement) -> Statement {
        match s {
            Statement::Cut(p, c) => Statement::Cut(self.producer(p), self.consumer(c)),
            Statement::Call(f, args, conts) => Statement::Call(
                f.clone(),
                args.iter().map(|a| self.producer(a)).collect(),
                conts.iter().map(|c| self.consumer(c)).collect(),
            ),
            Statement::Prim(name, args, conts) => Statement::Prim(
                name.clone(),
                args.iter().map(|a| self.producer(a)).collect(),
                conts.iter().map(|c| self.consumer(c)).collect(),
            ),
            Statement::Let(b, p, rest) => {
                let p = self.producer(p);
                self.scoped(|r| {
                    let b = r.bind(b);
                    Statement::Let(b, p, Box::new(r.statement(rest)))
                })
            }
            Statement::Handle(h) => {
                let cont = self.consumer(&h.cont);
                let (body_cont, body) = self.scoped(|r| {
                    let k = r.bind_name(&h.body_cont);
                    (k, r.statement(&h.body))
                });
                let ret = self.scoped(|r| {
                    let b = r.bind(&h.ret.0);
                    let k = r.bind_name(&h.ret.1);
                    (b, k, r.statement(&h.ret.2))
                });
                let clauses = h
                    .clauses
                    .iter()
                    .map(|c| {
                        self.scoped(|r| Clause {
                            op: c.op.clone(),
                            params: c.params.iter().map(|b| r.bind(b)).collect(),
                            resumption: r.bind_name(&c.resumption),
                            cont: r.bind_name(&c.cont),
                            body: r.statement(&c.body),
                        })
                    })
                    .collect();
                Statement::Handle(Box::new(Handle {
                    clauses,
                    ret,
                    body_cont,
                    body,
                    cont,
                }))
            }
            Statement::Perform(op, args, c) => Statement::Perform(
                op.clone(),
                args.iter().map(|a| self.producer(a)).collect(),
                self.consumer(c),
            ),
            Statement::Error(m) => Statement::Error(m.clone()),
        }
    }

    fn producer(&mut self, p: &Producer) -> Producer {
        match p {
            Producer::Var(x) => Producer::Var(self.var(x)),
            Producer::Mu(k, s) => self.scoped(|r| {
                let k = r.bind_name(k);
                Producer::Mu(k, Box::new(r.statement(s)))
            }),
            Producer::Cocase(ms) => Producer::Cocase(
                ms.iter()
                    .map(|m| {
                        self.scoped(|r| Method {
                            name: m.name.clone(),
                            params: m.params.iter().map(|b| r.bind(b)).collect(),
                            conts: m.conts.iter().map(|k| r.bind_name(k)).collect(),
                            body: r.statement(&m.body),
                        })
                    })
                    .collect(),
            ),
            Producer::Con(c, args) => {
                Producer::Con(c.clone(), args.iter().map(|a| self.producer(a)).collect())
            }
            Producer::Tuple(xs) => Producer::Tuple(xs.iter().map(|a| self.producer(a)).collect()),
            Producer::Record(fs) => Producer::Record(
                fs.iter()
                    .map(|(l, a)| (l.clone(), self.producer(a)))
                    .collect(),
            ),
            Producer::Array(xs) => Producer::Array(xs.iter().map(|a| self.producer(a)).collect()),
            other => other.clone(),
        }
    }

    fn consumer(&mut self, c: &Consumer) -> Consumer {
        match c {
            Consumer::Var(k) => Consumer::Var(self.var(k)),
            Consumer::Halt => Consumer::Halt,
            Consumer::MuTilde(b, s) => self.scoped(|r| {
                let b = r.bind(b);
                Consumer::MuTilde(b, Box::new(r.statement(s)))
            }),
            Consumer::Case(arms) => Consumer::Case(
                arms.iter()
                    .map(|a| {
                        self.scoped(|r| Arm {
                            pattern: a.pattern.clone(),
                            fields: a.fields.iter().map(|b| r.bind(b)).collect(),
                            body: r.statement(&a.body),
                        })
                    })
                    .collect(),
            ),
            Consumer::Method(name, args, conts) => Consumer::Method(
                name.clone(),
                args.iter().map(|a| self.producer(a)).collect(),
                conts.iter().map(|c| self.consumer(c)).collect(),
            ),
        }
    }
}

// --- what is used where ----------------------------------------------------

/// How many times a name is mentioned, and whether any of them is where it
/// may run many times: in an object's method, or a handler's clause.
#[derive(Clone, Copy)]
struct Uses {
    n: u32,
    looped: bool,
}

/// What a name nothing counted is taken to be: used everywhere.
const MANY: Uses = Uses {
    n: u32::MAX,
    looped: true,
};

fn mention(x: &str, looped: bool, out: &mut HashMap<String, Uses>) {
    let u = out.entry(x.to_string()).or_insert(Uses {
        n: 0,
        looped: false,
    });
    u.n = u.n.saturating_add(1);
    u.looped |= looped;
}

fn count(s: &Statement, looped: bool, out: &mut HashMap<String, Uses>) {
    match s {
        Statement::Cut(p, c) => {
            count_p(p, looped, out);
            count_c(c, looped, out);
        }
        Statement::Call(_, args, conts) | Statement::Prim(_, args, conts) => {
            args.iter().for_each(|a| count_p(a, looped, out));
            conts.iter().for_each(|c| count_c(c, looped, out));
        }
        Statement::Let(_, p, rest) => {
            count_p(p, looped, out);
            count(rest, looped, out);
        }
        // A handler's body, its `return` and its clauses run under other
        // evidence than the statement around them: what is made there
        // performs to that handler, so nothing but an atom or a name is
        // moved in.
        Statement::Handle(h) => {
            count_c(&h.cont, looped, out);
            count(&h.body, true, out);
            count(&h.ret.2, true, out);
            h.clauses.iter().for_each(|c| count(&c.body, true, out));
        }
        Statement::Perform(_, args, c) => {
            args.iter().for_each(|a| count_p(a, looped, out));
            count_c(c, looped, out);
        }
        Statement::Error(_) => {}
    }
}

fn count_p(p: &Producer, looped: bool, out: &mut HashMap<String, Uses>) {
    match p {
        Producer::Var(x) => mention(x, looped, out),
        Producer::Mu(_, s) => count(s, looped, out),
        Producer::Cocase(ms) => ms.iter().for_each(|m| count(&m.body, true, out)),
        Producer::Con(_, xs) | Producer::Tuple(xs) | Producer::Array(xs) => {
            xs.iter().for_each(|a| count_p(a, looped, out))
        }
        Producer::Record(fs) => fs.iter().for_each(|(_, a)| count_p(a, looped, out)),
        _ => {}
    }
}

fn count_c(c: &Consumer, looped: bool, out: &mut HashMap<String, Uses>) {
    match c {
        Consumer::Var(k) => mention(k, looped, out),
        Consumer::Halt => {}
        Consumer::MuTilde(_, s) => count(s, looped, out),
        Consumer::Case(arms) => arms.iter().for_each(|a| count(&a.body, looped, out)),
        Consumer::Method(_, args, conts) => {
            args.iter().for_each(|a| count_p(a, looped, out));
            conts.iter().for_each(|c| count_c(c, looped, out));
        }
    }
}

/// How many nodes `s` is: what a definition is weighed by.
fn size(s: &Statement) -> usize {
    fn p(x: &Producer) -> usize {
        1 + match x {
            Producer::Mu(_, s) => size(s),
            Producer::Cocase(ms) => ms.iter().map(|m| size(&m.body)).sum(),
            Producer::Con(_, xs) | Producer::Tuple(xs) | Producer::Array(xs) => {
                xs.iter().map(p).sum()
            }
            Producer::Record(fs) => fs.iter().map(|(_, a)| p(a)).sum(),
            _ => 0,
        }
    }
    fn c(x: &Consumer) -> usize {
        1 + match x {
            Consumer::MuTilde(_, s) => size(s),
            Consumer::Case(arms) => arms.iter().map(|a| size(&a.body)).sum(),
            Consumer::Method(_, args, conts) => {
                args.iter().map(p).sum::<usize>() + conts.iter().map(c).sum::<usize>()
            }
            _ => 0,
        }
    }
    1 + match s {
        Statement::Cut(x, y) => p(x) + c(y),
        Statement::Call(_, args, conts) | Statement::Prim(_, args, conts) => {
            args.iter().map(p).sum::<usize>() + conts.iter().map(c).sum::<usize>()
        }
        Statement::Let(_, x, rest) => p(x) + size(rest),
        Statement::Handle(h) => {
            c(&h.cont)
                + size(&h.body)
                + size(&h.ret.2)
                + h.clauses.iter().map(|cl| size(&cl.body)).sum::<usize>()
        }
        Statement::Perform(_, args, k) => args.iter().map(p).sum::<usize>() + c(k),
        Statement::Error(_) => 0,
    }
}

/// Whether `s` calls `f` itself.
fn calls(s: &Statement, f: &Symbol) -> bool {
    fn p(x: &Producer, f: &Symbol) -> bool {
        match x {
            Producer::Mu(_, s) => calls(s, f),
            Producer::Cocase(ms) => ms.iter().any(|m| calls(&m.body, f)),
            Producer::Con(_, xs) | Producer::Tuple(xs) | Producer::Array(xs) => {
                xs.iter().any(|a| p(a, f))
            }
            Producer::Record(fs) => fs.iter().any(|(_, a)| p(a, f)),
            _ => false,
        }
    }
    fn c(x: &Consumer, f: &Symbol) -> bool {
        match x {
            Consumer::MuTilde(_, s) => calls(s, f),
            Consumer::Case(arms) => arms.iter().any(|a| calls(&a.body, f)),
            Consumer::Method(_, args, conts) => {
                args.iter().any(|a| p(a, f)) || conts.iter().any(|k| c(k, f))
            }
            _ => false,
        }
    }
    match s {
        Statement::Cut(x, y) => p(x, f) || c(y, f),
        Statement::Call(g, args, conts) => {
            g == f || args.iter().any(|a| p(a, f)) || conts.iter().any(|k| c(k, f))
        }
        Statement::Prim(_, args, conts) => {
            args.iter().any(|a| p(a, f)) || conts.iter().any(|k| c(k, f))
        }
        Statement::Let(_, x, rest) => p(x, f) || calls(rest, f),
        Statement::Handle(h) => {
            c(&h.cont, f)
                || calls(&h.body, f)
                || calls(&h.ret.2, f)
                || h.clauses.iter().any(|cl| calls(&cl.body, f))
        }
        Statement::Perform(_, args, k) => args.iter().any(|a| p(a, f)) || c(k, f),
        Statement::Error(_) => false,
    }
}

/// A variable or a literal: what may be put wherever a name is, as often as
/// it is.
fn atom(p: &Producer) -> bool {
    matches!(
        p,
        Producer::Var(_)
            | Producer::Int(_)
            | Producer::Float(_)
            | Producer::Char(_)
            | Producer::Bool(_)
            | Producer::Unit
            | Producer::Desc(_)
    )
}

/// What may be made where it is used rather than where it was: a
/// constructor or tuple of atoms, or an object -- nothing that computes.
fn movable(p: &Producer) -> bool {
    match p {
        Producer::Con(_, xs) | Producer::Tuple(xs) => xs.iter().all(atom),
        Producer::Cocase(_) => true,
        _ => false,
    }
}

// --- the pass --------------------------------------------------------------

struct Simplify {
    /// The definitions small enough to inline, as renamed.
    table: HashMap<Symbol, Def>,
    uses: HashMap<String, Uses>,
    /// What is put for a name of a value, from where it was bound on.
    vals: HashMap<String, Producer>,
    /// What is put for a name of a continuation.
    conts: HashMap<String, Consumer>,
    fresh: Fresh,
    /// The definitions being inlined, outermost first: the one whose body
    /// this is, then each inlined into it.
    stack: Vec<Symbol>,
}

impl Simplify {
    fn uses(&self, x: &str) -> Uses {
        self.uses.get(x).copied().unwrap_or(MANY)
    }

    /// Whether `x`, bound to `v`, is put for where it is used rather than
    /// bound: `v` an atom, or made at its one use, or used nowhere.
    fn binds(&mut self, x: &str, v: &Producer) -> bool {
        if atom(v) {
            self.vals.insert(x.to_string(), v.clone());
            return true;
        }
        let u = self.uses(x);
        if movable(v) && u.n == 0 {
            return true;
        }
        if movable(v) && u.n == 1 && !u.looped {
            self.vals.insert(x.to_string(), v.clone());
            return true;
        }
        false
    }

    /// Whether `k`, bound to `c`, is put for where it is used: `c` a name,
    /// or `k` used at most once, and not in a method or under a handler,
    /// where `c` would run with other evidence than it was written with.
    fn binds_cont(&mut self, k: &str, c: &Consumer) -> bool {
        let u = self.uses(k);
        if matches!(c, Consumer::Var(_) | Consumer::Halt) || u.n == 0 || (u.n == 1 && !u.looped) {
            self.conts.insert(k.to_string(), c.clone());
            return true;
        }
        false
    }

    /// `body` with `params` bound to `args` and `conts` to `given`: what
    /// cannot be put for its name bound around it.
    fn bound(
        &mut self,
        params: &[Binder],
        args: Vec<Producer>,
        conts: &[String],
        given: Vec<Consumer>,
        body: &Statement,
    ) -> Statement {
        let mut values = Vec::new();
        for (b, a) in params.iter().zip(args) {
            if !self.binds(&b.name, &a) {
                values.push((b.clone(), a));
            }
        }
        let mut ks = Vec::new();
        for (k, c) in conts.iter().zip(given) {
            if !self.binds_cont(k, &c) {
                ks.push((k.clone(), c));
            }
        }
        let mut out = self.stmt(body);
        for (k, c) in ks.into_iter().rev() {
            out = Statement::Cut(Producer::Mu(k, Box::new(out)), c);
        }
        for (b, a) in values.into_iter().rev() {
            out = Statement::Cut(a, Consumer::MuTilde(b, Box::new(out)));
        }
        out
    }

    fn stmt(&mut self, s: &Statement) -> Statement {
        match s {
            Statement::Cut(Producer::Mu(k, body), c) => {
                let c = self.cons(c);
                if self.binds_cont(k, &c) {
                    self.stmt(body)
                } else {
                    Statement::Cut(Producer::Mu(k.clone(), Box::new(self.stmt(body))), c)
                }
            }
            Statement::Cut(p, c) => {
                let p = self.prod(p);
                self.cut(p, c, false)
            }
            Statement::Let(b, p, rest) => {
                let p = self.prod(p);
                if self.binds(&b.name, &p) {
                    self.stmt(rest)
                } else {
                    Statement::Let(b.clone(), p, Box::new(self.stmt(rest)))
                }
            }
            Statement::Call(f, args, conts) => {
                let args: Vec<Producer> = args.iter().map(|a| self.prod(a)).collect();
                let conts: Vec<Consumer> = conts.iter().map(|c| self.cons(c)).collect();
                self.call(f, args, conts)
            }
            Statement::Prim(name, args, conts) => {
                let args: Vec<Producer> = args.iter().map(|a| self.prod(a)).collect();
                // `prim if` takes what to do when it is not, first.
                if name == "if"
                    && conts.len() == 2
                    && let [Producer::Bool(b)] = args.as_slice()
                {
                    let chosen = &conts[usize::from(*b)];
                    return self.cut(Producer::Unit, chosen, false);
                }
                Statement::Prim(
                    name.clone(),
                    args,
                    conts.iter().map(|c| self.cons(c)).collect(),
                )
            }
            Statement::Handle(h) => {
                let cont = self.cons(&h.cont);
                Statement::Handle(Box::new(Handle {
                    clauses: h
                        .clauses
                        .iter()
                        .map(|c| Clause {
                            body: self.stmt(&c.body),
                            ..c.clone()
                        })
                        .collect(),
                    ret: (h.ret.0.clone(), h.ret.1.clone(), self.stmt(&h.ret.2)),
                    body_cont: h.body_cont.clone(),
                    body: self.stmt(&h.body),
                    cont,
                }))
            }
            Statement::Perform(op, args, c) => Statement::Perform(
                op.clone(),
                args.iter().map(|a| self.prod(a)).collect(),
                self.cons(c),
            ),
            Statement::Error(m) => Statement::Error(m.clone()),
        }
    }

    /// `p`, worked on already, given to `c`: worked on already if `done`.
    fn cut(&mut self, p: Producer, c: &Consumer, done: bool) -> Statement {
        match c {
            Consumer::Var(k) if !done => match self.conts.get(k).cloned() {
                Some(c) => self.cut(p, &c, true),
                None => Statement::Cut(p, c.clone()),
            },
            Consumer::MuTilde(b, s) => {
                if self.binds(&b.name, &p) {
                    self.stmt(s)
                } else if done {
                    Statement::Cut(p, c.clone())
                } else {
                    Statement::Cut(p, Consumer::MuTilde(b.clone(), Box::new(self.stmt(s))))
                }
            }
            Consumer::Case(arms) => match self.known_case(&p, arms) {
                Some(s) => s,
                None if done => Statement::Cut(p, c.clone()),
                None => Statement::Cut(p, self.cons(c)),
            },
            Consumer::Method(name, args, conts) => {
                if let Producer::Cocase(ms) = &p
                    && let Some(m) = ms.iter().find(|m| m.name == *name)
                    && m.params.len() == args.len()
                    && m.conts.len() == conts.len()
                {
                    let m = m.clone();
                    let (args, conts): (Vec<Producer>, Vec<Consumer>) = if done {
                        (args.clone(), conts.clone())
                    } else {
                        (
                            args.iter().map(|a| self.prod(a)).collect(),
                            conts.iter().map(|c| self.cons(c)).collect(),
                        )
                    };
                    return self.bound(&m.params, args, &m.conts, conts, &m.body);
                }
                if done {
                    Statement::Cut(p, c.clone())
                } else {
                    Statement::Cut(p, self.cons(c))
                }
            }
            _ if done => Statement::Cut(p, c.clone()),
            _ => Statement::Cut(p, self.cons(c)),
        }
    }

    /// The arm a constructor or tuple takes, its fields bound.
    fn known_case(&mut self, p: &Producer, arms: &[Arm]) -> Option<Statement> {
        let (args, arm) = match p {
            Producer::Con(k, args) => (
                args,
                arms.iter()
                    .find(|a| matches!(&a.pattern, Pattern::Con(c) if c == k))
                    .or_else(|| arms.iter().find(|a| a.pattern == Pattern::Default))?,
            ),
            Producer::Tuple(args) => (
                args,
                arms.iter()
                    .find(|a| matches!(a.pattern, Pattern::Tuple | Pattern::Default))?,
            ),
            _ => return None,
        };
        if arm.pattern == Pattern::Default {
            return Some(self.stmt(&arm.body));
        }
        if arm.fields.len() != args.len() {
            return None;
        }
        let arm = arm.clone();
        Some(self.bound(&arm.fields, args.clone(), &[], Vec::new(), &arm.body))
    }

    /// A call of `f`, its arguments worked on: `f`'s body, if it is small and
    /// not being inlined here already.
    fn call(&mut self, f: &Symbol, args: Vec<Producer>, conts: Vec<Consumer>) -> Statement {
        let inlines = self.stack.len() <= DEPTH && !self.stack.contains(f);
        let Some(d) = self.table.get(f).filter(|_| inlines) else {
            return Statement::Call(f.clone(), args, conts);
        };
        if d.params.len() != args.len() || d.conts.len() != conts.len() {
            return Statement::Call(f.clone(), args, conts);
        }
        let copy = self.fresh.def(d);
        count(&copy.body, false, &mut self.uses);
        self.stack.push(f.clone());
        let out = self.bound(&copy.params, args, &copy.conts, conts, &copy.body);
        self.stack.pop();
        out
    }

    fn prod(&mut self, p: &Producer) -> Producer {
        match p {
            Producer::Var(x) => self.vals.get(x).cloned().unwrap_or_else(|| p.clone()),
            Producer::Mu(k, s) => Producer::Mu(k.clone(), Box::new(self.stmt(s))),
            Producer::Cocase(ms) => Producer::Cocase(
                ms.iter()
                    .map(|m| Method {
                        body: self.stmt(&m.body),
                        ..m.clone()
                    })
                    .collect(),
            ),
            Producer::Con(c, args) => {
                Producer::Con(c.clone(), args.iter().map(|a| self.prod(a)).collect())
            }
            Producer::Tuple(xs) => Producer::Tuple(xs.iter().map(|a| self.prod(a)).collect()),
            Producer::Record(fs) => {
                Producer::Record(fs.iter().map(|(l, a)| (l.clone(), self.prod(a))).collect())
            }
            Producer::Array(xs) => Producer::Array(xs.iter().map(|a| self.prod(a)).collect()),
            other => other.clone(),
        }
    }

    fn cons(&mut self, c: &Consumer) -> Consumer {
        match c {
            Consumer::Var(k) => self.conts.get(k).cloned().unwrap_or_else(|| c.clone()),
            Consumer::Halt => Consumer::Halt,
            Consumer::MuTilde(b, s) => Consumer::MuTilde(b.clone(), Box::new(self.stmt(s))),
            Consumer::Case(arms) => Consumer::Case(
                arms.iter()
                    .map(|a| Arm {
                        body: self.stmt(&a.body),
                        ..a.clone()
                    })
                    .collect(),
            ),
            Consumer::Method(name, args, conts) => Consumer::Method(
                name.clone(),
                args.iter().map(|a| self.prod(a)).collect(),
                conts.iter().map(|c| self.cons(c)).collect(),
            ),
        }
    }
}
