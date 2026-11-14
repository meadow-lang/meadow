//! A program without what is there for a debugger: its positions
//! ([`crate::Pos`]), the files they are in, and the names its binders and
//! declarations are shown by ([`crate::Shown`], [`crate::Def::shown`]).
//!
//! Neither does anything, and a program runs the same with them as without.
//! But a statement under a position is a statement the passes that look for
//! a shape -- a call in tail position, a context to fill in place, a clause
//! that only resumes -- have to look under, and a program built to be run
//! rather than debugged should not depend on every one of them doing so. So
//! what runs a program without a debugger takes them off first, and what it
//! compiles is what it would have compiled of the same program written
//! without them, to the byte.

use crate::*;

/// `p` with its positions, its files and its shown names taken off.
pub fn program(p: &mut Program) {
    p.sources.clear();
    for v in &mut p.vals {
        v.shown = None;
        statement(&mut v.body);
    }
    for d in &mut p.defs {
        d.shown = None;
        binders(&mut d.params);
        statement(&mut d.body);
    }
}

fn binders(bs: &mut [Binder]) {
    for b in bs {
        b.shown = None;
    }
}

fn statement(s: &mut Statement) {
    loop {
        match s {
            Statement::At(_, inner) => {
                let inner = std::mem::replace(&mut **inner, Statement::Error(String::new()));
                *s = inner;
            }
            Statement::Cut(p, c) => {
                producer(p);
                consumer(c);
                return;
            }
            Statement::Call(_, args, conts) | Statement::Prim(_, args, conts) => {
                args.iter_mut().for_each(producer);
                conts.iter_mut().for_each(consumer);
                return;
            }
            Statement::Let(x, p, body) => {
                x.shown = None;
                producer(p);
                statement(body);
                return;
            }
            Statement::Handle(h) => {
                for c in &mut h.clauses {
                    binders(&mut c.params);
                    statement(&mut c.body);
                }
                h.ret.0.shown = None;
                statement(&mut h.ret.2);
                statement(&mut h.body);
                consumer(&mut h.cont);
                return;
            }
            Statement::Perform(_, args, c) => {
                args.iter_mut().for_each(producer);
                consumer(c);
                return;
            }
            Statement::Error(_) => return,
        }
    }
}

fn producer(p: &mut Producer) {
    match p {
        Producer::Con(_, args) | Producer::Tuple(args) | Producer::Array(args) => {
            args.iter_mut().for_each(producer)
        }
        Producer::Record(fields) => fields.iter_mut().for_each(|(_, p)| producer(p)),
        Producer::Mu(_, s) => statement(s),
        Producer::Cocase(methods) => {
            for m in methods {
                binders(&mut m.params);
                statement(&mut m.body);
            }
        }
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

fn consumer(c: &mut Consumer) {
    match c {
        Consumer::MuTilde(x, s) => {
            x.shown = None;
            statement(s);
        }
        Consumer::Case(arms) => {
            for a in arms {
                binders(&mut a.fields);
                statement(&mut a.body);
            }
        }
        Consumer::Method(_, args, conts) => {
            args.iter_mut().for_each(producer);
            conts.iter_mut().for_each(consumer);
        }
        Consumer::Var(_) | Consumer::Halt => {}
    }
}
