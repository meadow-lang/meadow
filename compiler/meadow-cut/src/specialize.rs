//! **A definition generic in a representation, copied for each one it is
//! called at.**
//!
//! A front end that does not make these copies itself writes a generic
//! function once: `def f <'a = d> (d: desc, x: 'a; k)`, taking a descriptor
//! that says at run time what an `'a` is. Everything the function does with
//! an `'a` then asks the descriptor -- to share it, to let it go, to store it
//! -- where code that knew it was a pointer, or a number, would just do it.
//! `meadow_core::specialize::release` makes these copies for the Rust front
//! end, on core; this makes them on Cut, for a program whose front end left
//! them out, as MeadowBoot does.
//!
//! A call that gives every descriptor of a generic definition as a
//! representation in plain sight -- `f(desc(ptr), x; k)` -- is a call of the
//! copy made at those: the body with each variable put for what it stands
//! for, and each mention of the descriptor parameter the descriptor itself,
//! so that the generic calls inside it are in plain sight in turn. There are
//! finitely many representations, so this stops, polymorphic recursion
//! included.
//!
//! The copy keeps its descriptor parameters, which nothing in it reads: its
//! callers pass what they passed. A generic definition nothing calls any more
//! is dropped, since it was most of the program.

use crate::{
    Arm, Binder, Clause, Consumer, Def, Handle, Method, Producer, Program, Rep, Statement, Symbol,
};
use std::collections::{HashMap, HashSet, VecDeque};

/// `p` with each generic definition copied for the representations it is
/// called at, where a call says them.
pub fn program(p: &Program) -> Program {
    let mut generics: HashMap<&Symbol, (&Def, Vec<usize>)> = HashMap::new();
    for d in &p.defs {
        if d.rep_vars.is_empty() {
            continue;
        }
        // Where each variable's descriptor is among the parameters; a
        // definition that names one it does not take is left alone.
        let at: Option<Vec<usize>> = d
            .rep_vars
            .iter()
            .map(|(_, desc)| d.params.iter().position(|b| &b.name == desc))
            .collect();
        if let Some(at) = at {
            generics.insert(&d.symbol, (d, at));
        }
    }
    if generics.is_empty() {
        return p.clone();
    }
    let mut cx = Cx {
        generics: &generics,
        taken: p.defs.iter().map(|d| d.symbol.clone()).collect(),
        made: HashMap::new(),
        wanted: VecDeque::new(),
    };
    let none = Known::default();
    let mut out = p.clone();
    for v in &mut out.vals {
        v.body = cx.statement(&v.body, &none);
    }
    for d in &mut out.defs {
        // A generic definition's own body is left as it is written: it is
        // kept only if something still calls it without saying at what.
        if d.rep_vars.is_empty() {
            d.body = cx.statement(&d.body, &none);
        }
    }
    while let Some((symbol, reps, name)) = cx.wanted.pop_front() {
        let (d, _) = generics[&symbol];
        let known = Known {
            reps: d
                .rep_vars
                .iter()
                .zip(&reps)
                .map(|((var, _), r)| (var.clone(), r.clone()))
                .collect(),
            descs: d
                .rep_vars
                .iter()
                .zip(&reps)
                .map(|((_, desc), r)| (desc.clone(), r.clone()))
                .collect(),
        };
        out.defs.push(Def {
            symbol: name,
            rep_vars: Vec::new(),
            effect_vars: d.effect_vars.clone(),
            params: d.params.iter().map(|b| known.binder(b)).collect(),
            conts: d.conts.clone(),
            body: cx.statement(&d.body, &known),
        });
    }
    let copies = cx.made.len();
    drop_uncalled(&mut out);
    // `MEADOW_CUT_COPIES=1`: how many definitions were generic, how many
    // copies were made of them, and which are still called as they were --
    // the ones the most is still asked of descriptors in.
    if std::env::var_os("MEADOW_CUT_COPIES").is_some() {
        let kept: Vec<&Def> = out.defs.iter().filter(|d| !d.rep_vars.is_empty()).collect();
        eprintln!(
            "cut: {} generic definitions, {copies} copies, {} still generic of {} definitions",
            generics.len(),
            kept.len(),
            out.defs.len()
        );
        let mut by: HashMap<String, usize> = HashMap::new();
        for d in &out.defs {
            let mut called = Vec::new();
            calls(&d.body, &mut called);
            for c in called {
                if kept.iter().any(|k| &k.symbol == c) {
                    *by.entry(format!(
                        "{} from {}",
                        c,
                        if d.rep_vars.is_empty() {
                            "a copy or plain definition"
                        } else {
                            "a generic one"
                        }
                    ))
                    .or_default() += 1;
                }
            }
        }
        let mut by: Vec<(String, usize)> = by.into_iter().collect();
        by.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (what, n) in by.iter().take(25) {
            eprintln!("cut: {n:>6}  {what}");
        }
    }
    out
}

/// What a copy is made at: each representation variable's representation,
/// and each descriptor parameter's.
#[derive(Default)]
struct Known {
    reps: HashMap<String, Rep>,
    descs: HashMap<String, Rep>,
}

impl Known {
    fn rep(&self, r: &Rep) -> Rep {
        match r {
            Rep::Var(a) => self.reps.get(a).cloned().unwrap_or_else(|| r.clone()),
            other => other.clone(),
        }
    }

    fn binder(&self, b: &Binder) -> Binder {
        Binder {
            name: b.name.clone(),
            rep: self.rep(&b.rep),
        }
    }
}

struct Cx<'a> {
    generics: &'a HashMap<&'a Symbol, (&'a Def, Vec<usize>)>,
    /// Every symbol a definition has, so that a copy's is new.
    taken: HashSet<Symbol>,
    made: HashMap<(Symbol, Vec<Rep>), Symbol>,
    wanted: VecDeque<(Symbol, Vec<Rep>, Symbol)>,
}

impl Cx<'_> {
    /// The copy of `symbol` at `reps`, asked for if this is the first call
    /// of it.
    fn copy(&mut self, symbol: &Symbol, reps: Vec<Rep>) -> Symbol {
        if let Some(name) = self.made.get(&(symbol.clone(), reps.clone())) {
            return name.clone();
        }
        let mut name = symbol.clone();
        let last = name.path.pop().unwrap_or_default();
        let at: Vec<&str> = reps.iter().map(rep_name).collect();
        let mut spelt = format!("{last}_{}", at.join("_"));
        loop {
            name.path.push(spelt.clone());
            if self.taken.insert(name.clone()) {
                break;
            }
            name.path.pop();
            spelt.push('_');
        }
        self.made
            .insert((symbol.clone(), reps.clone()), name.clone());
        self.wanted.push_back((symbol.clone(), reps, name.clone()));
        name
    }

    fn statement(&mut self, s: &Statement, k: &Known) -> Statement {
        match s {
            Statement::Cut(p, c) => Statement::Cut(self.producer(p, k), self.consumer(c, k)),
            Statement::Call(symbol, args, conts) => {
                let args: Vec<Producer> = args.iter().map(|a| self.producer(a, k)).collect();
                let conts = conts.iter().map(|c| self.consumer(c, k)).collect();
                let at: Option<Vec<Rep>> = self.generics.get(symbol).and_then(|(_, at)| {
                    at.iter()
                        .map(|i| match args.get(*i) {
                            Some(Producer::Desc(r)) if !matches!(r, Rep::Var(_)) => Some(r.clone()),
                            _ => None,
                        })
                        .collect()
                });
                match at {
                    Some(reps) => Statement::Call(self.copy(symbol, reps), args, conts),
                    None => Statement::Call(symbol.clone(), args, conts),
                }
            }
            Statement::Prim(op, args, conts) => Statement::Prim(
                op.clone(),
                args.iter().map(|a| self.producer(a, k)).collect(),
                conts.iter().map(|c| self.consumer(c, k)).collect(),
            ),
            Statement::Let(b, p, rest) => Statement::Let(
                k.binder(b),
                self.producer(p, k),
                Box::new(self.statement(rest, k)),
            ),
            Statement::Handle(h) => Statement::Handle(Box::new(Handle {
                clauses: h
                    .clauses
                    .iter()
                    .map(|c| Clause {
                        op: c.op.clone(),
                        params: c.params.iter().map(|b| k.binder(b)).collect(),
                        resumption: c.resumption.clone(),
                        cont: c.cont.clone(),
                        body: self.statement(&c.body, k),
                    })
                    .collect(),
                ret: (
                    k.binder(&h.ret.0),
                    h.ret.1.clone(),
                    self.statement(&h.ret.2, k),
                ),
                body_cont: h.body_cont.clone(),
                body: self.statement(&h.body, k),
                cont: self.consumer(&h.cont, k),
            })),
            Statement::Perform(op, args, c) => Statement::Perform(
                op.clone(),
                args.iter().map(|a| self.producer(a, k)).collect(),
                self.consumer(c, k),
            ),
            Statement::Error(m) => Statement::Error(m.clone()),
        }
    }

    fn producer(&mut self, p: &Producer, k: &Known) -> Producer {
        match p {
            // A descriptor parameter mentioned is the descriptor it was
            // given, which the copy knows.
            Producer::Var(x) => match k.descs.get(x) {
                Some(r) => Producer::Desc(r.clone()),
                None => p.clone(),
            },
            Producer::Desc(r) => Producer::Desc(k.rep(r)),
            Producer::Con(symbol, fields) => Producer::Con(
                symbol.clone(),
                fields.iter().map(|f| self.producer(f, k)).collect(),
            ),
            Producer::Tuple(items) => {
                Producer::Tuple(items.iter().map(|f| self.producer(f, k)).collect())
            }
            Producer::Array(items) => {
                Producer::Array(items.iter().map(|f| self.producer(f, k)).collect())
            }
            Producer::Record(fields) => Producer::Record(
                fields
                    .iter()
                    .map(|(l, f)| (l.clone(), self.producer(f, k)))
                    .collect(),
            ),
            Producer::Mu(c, s) => Producer::Mu(c.clone(), Box::new(self.statement(s, k))),
            Producer::Cocase(methods) => Producer::Cocase(
                methods
                    .iter()
                    .map(|m| Method {
                        name: m.name.clone(),
                        params: m.params.iter().map(|b| k.binder(b)).collect(),
                        conts: m.conts.clone(),
                        body: self.statement(&m.body, k),
                    })
                    .collect(),
            ),
            Producer::Val(_)
            | Producer::Int(_)
            | Producer::Float(_)
            | Producer::Char(_)
            | Producer::Str(_)
            | Producer::Bool(_)
            | Producer::Unit => p.clone(),
        }
    }

    fn consumer(&mut self, c: &Consumer, k: &Known) -> Consumer {
        match c {
            Consumer::Var(_) | Consumer::Halt => c.clone(),
            Consumer::MuTilde(b, s) => {
                Consumer::MuTilde(k.binder(b), Box::new(self.statement(s, k)))
            }
            Consumer::Case(arms) => Consumer::Case(
                arms.iter()
                    .map(|a| Arm {
                        pattern: a.pattern.clone(),
                        fields: a.fields.iter().map(|b| k.binder(b)).collect(),
                        body: self.statement(&a.body, k),
                    })
                    .collect(),
            ),
            Consumer::Method(name, args, conts) => Consumer::Method(
                name.clone(),
                args.iter().map(|a| self.producer(a, k)).collect(),
                conts.iter().map(|c| self.consumer(c, k)).collect(),
            ),
        }
    }
}

/// A representation as part of a copy's name.
fn rep_name(r: &Rep) -> &str {
    match r {
        Rep::I64 => "i64",
        Rep::F64 => "f64",
        Rep::F32 => "f32",
        Rep::I8 => "i8",
        Rep::I16 => "i16",
        Rep::I32 => "i32",
        Rep::U8 => "u8",
        Rep::U16 => "u16",
        Rep::U32 => "u32",
        Rep::U64 => "u64",
        Rep::Bool => "bool",
        Rep::Char => "char",
        Rep::Unit => "unit",
        Rep::Sym => "sym",
        Rep::Str => "str",
        Rep::Ptr => "ptr",
        Rep::Desc => "desc",
        Rep::Any => "any",
        Rep::Var(a) => a,
    }
}

/// Drop each generic definition nothing reaches: from the entry, the values
/// and the definitions that are not generic, by the calls they make.
fn drop_uncalled(p: &mut Program) {
    let by: HashMap<&Symbol, &Def> = p.defs.iter().map(|d| (&d.symbol, d)).collect();
    let mut reached: HashSet<&Symbol> = HashSet::new();
    let mut todo: Vec<&Symbol> = Vec::new();
    let mut called: Vec<&Symbol> = Vec::new();
    for v in &p.vals {
        calls(&v.body, &mut called);
    }
    for d in &p.defs {
        if d.rep_vars.is_empty() {
            calls(&d.body, &mut called);
        }
    }
    loop {
        for s in called.drain(..) {
            if let Some(d) = by.get(s) {
                if !d.rep_vars.is_empty() && reached.insert(&d.symbol) {
                    todo.push(&d.symbol);
                }
            }
        }
        let Some(s) = todo.pop() else { break };
        calls(&by[s].body, &mut called);
    }
    let keep: HashSet<Symbol> = reached.into_iter().cloned().collect();
    p.defs
        .retain(|d| d.rep_vars.is_empty() || keep.contains(&d.symbol));
}

/// Every definition `s` calls, into `out`.
fn calls<'a>(s: &'a Statement, out: &mut Vec<&'a Symbol>) {
    match s {
        Statement::Cut(p, c) => {
            calls_in(p, out);
            calls_of(c, out);
        }
        Statement::Call(symbol, args, conts) => {
            out.push(symbol);
            args.iter().for_each(|a| calls_in(a, out));
            conts.iter().for_each(|c| calls_of(c, out));
        }
        Statement::Prim(_, args, conts) => {
            args.iter().for_each(|a| calls_in(a, out));
            conts.iter().for_each(|c| calls_of(c, out));
        }
        Statement::Let(_, p, rest) => {
            calls_in(p, out);
            calls(rest, out);
        }
        Statement::Handle(h) => {
            h.clauses.iter().for_each(|c| calls(&c.body, out));
            calls(&h.ret.2, out);
            calls(&h.body, out);
            calls_of(&h.cont, out);
        }
        Statement::Perform(_, args, c) => {
            args.iter().for_each(|a| calls_in(a, out));
            calls_of(c, out);
        }
        Statement::Error(_) => {}
    }
}

fn calls_in<'a>(p: &'a Producer, out: &mut Vec<&'a Symbol>) {
    match p {
        Producer::Con(_, items) | Producer::Tuple(items) | Producer::Array(items) => {
            items.iter().for_each(|a| calls_in(a, out));
        }
        Producer::Record(fields) => fields.iter().for_each(|(_, a)| calls_in(a, out)),
        Producer::Mu(_, s) => calls(s, out),
        Producer::Cocase(methods) => methods.iter().for_each(|m| calls(&m.body, out)),
        // A definition is mentioned only where it is called; a value named
        // is a value, which is kept whatever happens here.
        Producer::Var(_)
        | Producer::Val(_)
        | Producer::Int(_)
        | Producer::Float(_)
        | Producer::Char(_)
        | Producer::Str(_)
        | Producer::Bool(_)
        | Producer::Unit
        | Producer::Desc(_) => {}
    }
}

fn calls_of<'a>(c: &'a Consumer, out: &mut Vec<&'a Symbol>) {
    match c {
        Consumer::Var(_) | Consumer::Halt => {}
        Consumer::MuTilde(_, s) => calls(s, out),
        Consumer::Case(arms) => arms.iter().for_each(|a| calls(&a.body, out)),
        Consumer::Method(_, args, conts) => {
            args.iter().for_each(|a| calls_in(a, out));
            conts.iter().for_each(|c| calls_of(c, out));
        }
    }
}
