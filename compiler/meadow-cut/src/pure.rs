//! **Which definitions can never have their activation captured.**
//!
//! `meadow_llvm::linear` lets a call's frame borrow what its definition
//! borrowed -- and so lets a parameter be borrowed at all, in a definition
//! that calls anything -- only where no handler can take the continuation the
//! frame is part of and keep it: [`meadow_axcut::Program::pure`]. The Rust
//! front end knows those definitions as the ones that take no evidence
//! (`meadow_seq::lower`). A program of Cut says nothing of evidence, and
//! without the set every frame owned everything it held: borrowing did
//! nothing for a program lowered from Cut, and a compiler built that way
//! erased half as many blocks again as the same compiler built by the Rust
//! front end.
//!
//! The rule is that front end's, read off the code. A definition is pure when
//! its own code -- what runs in its activation: its body and the
//! continuations it writes, not the bodies of the objects it makes, which
//! run in their callers' -- performs no operation, handles none, invokes no
//! object, and calls only definitions that are pure. An object invoked might
//! be any function, one that performs among them; a definition called is
//! known.

use crate::{Consumer, Producer, Program, Statement, Symbol};
use std::collections::{HashMap, HashSet};

/// The definitions of `p` that are pure, by where each is in `p.defs`.
pub fn defs(p: &Program) -> HashSet<usize> {
    let at: HashMap<&Symbol, usize> = p
        .defs
        .iter()
        .enumerate()
        .map(|(i, d)| (&d.symbol, i))
        .collect();
    // Each definition's callees, or `None` for one that is not pure whatever
    // they are.
    let calls: Vec<Option<Vec<usize>>> = p
        .defs
        .iter()
        .map(|d| {
            let mut called = Vec::new();
            statement(&d.body, &mut called).then_some(())?;
            called.into_iter().map(|s| at.get(s).copied()).collect()
        })
        .collect();
    let mut pure: Vec<bool> = calls.iter().map(Option::is_some).collect();
    // From all that might be down to those that are: one that calls one
    // that is not, is not.
    loop {
        let mut changed = false;
        for (i, called) in calls.iter().enumerate() {
            if pure[i] && called.iter().flatten().any(|c| !pure[*c]) {
                pure[i] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    (0..pure.len()).filter(|i| pure[*i]).collect()
}

/// Whether `s` itself does nothing that could capture its activation, with
/// the definitions it calls into `called`.
fn statement<'a>(s: &'a Statement, called: &mut Vec<&'a Symbol>) -> bool {
    match s {
        Statement::Cut(p, c) => producer(p, called) && consumer(c, called),
        Statement::Call(symbol, args, conts) => {
            called.push(symbol);
            args.iter().all(|a| producer(a, called)) && conts.iter().all(|c| consumer(c, called))
        }
        Statement::Prim(_, args, conts) => {
            args.iter().all(|a| producer(a, called)) && conts.iter().all(|c| consumer(c, called))
        }
        Statement::Let(_, p, rest) => producer(p, called) && statement(rest, called),
        Statement::Handle(_) | Statement::Perform(..) => false,
        Statement::Error(_) => true,
    }
}

fn producer<'a>(p: &'a Producer, called: &mut Vec<&'a Symbol>) -> bool {
    match p {
        Producer::Con(_, items) | Producer::Tuple(items) | Producer::Array(items) => {
            items.iter().all(|a| producer(a, called))
        }
        Producer::Record(fields) => fields.iter().all(|(_, a)| producer(a, called)),
        Producer::Mu(_, s) => statement(s, called),
        // An object made is not an object run: its methods' activations are
        // their own.
        Producer::Cocase(_) => true,
        Producer::Var(_)
        | Producer::Val(_)
        | Producer::Int(_)
        | Producer::Float(_)
        | Producer::Char(_)
        | Producer::Str(_)
        | Producer::Bool(_)
        | Producer::Unit
        | Producer::Desc(_) => true,
    }
}

fn consumer<'a>(c: &'a Consumer, called: &mut Vec<&'a Symbol>) -> bool {
    match c {
        Consumer::Var(_) | Consumer::Halt => true,
        Consumer::MuTilde(_, s) => statement(s, called),
        Consumer::Case(arms) => arms.iter().all(|a| statement(&a.body, called)),
        // An object invoked: any function at all.
        Consumer::Method(..) => false,
    }
}
