//! **What is given to `any`**: refused where it is a word.
//!
//! A value of `any` is boxed and says at run time what it is (`docs/CUT.md`,
//! "Representations"); v0 has no box yet (open question 2), so `any` holds
//! what is a box already -- a pointer to a block, which says what it is --
//! and a runtime that cannot tell a word from an address, as Silo cannot,
//! counts what it holds as a reference. An `i64` given to `any` is then a
//! number taken for an address, and erasing it writes to memory that is not
//! a block's.
//!
//! So a program is refused where it can be seen to give `any` a word: a
//! value of a word's representation -- `i64`, `f64`, a `char`, a `bool`, a
//! descriptor and the rest; not `ptr` or `str`, which are blocks -- bound to
//! a binder of `any`, by a `μ̃`, a `let`, a definition's parameter, a
//! constructor's field, an operation's parameter, or the parameter of a
//! method of an object the call can see: one made where it is called, or
//! bound to the variable it is called through.

use crate::{Consumer, Def, Method, Producer, Program, Rep, Statement, Symbol};
use std::collections::HashMap;

/// Whether `p` gives `any` a word anywhere it can be seen to, and where
/// first if it does.
pub fn check(p: &Program) -> Result<(), String> {
    let defs: HashMap<&Symbol, &Def> = p.defs.iter().map(|d| (&d.symbol, d)).collect();
    let mut fields: HashMap<Symbol, &[Rep]> = HashMap::new();
    for d in &p.datas {
        for (name, reps) in &d.ctors {
            fields.insert(d.symbol.child(name), reps.as_slice());
        }
    }
    let mut ops: HashMap<Symbol, &[Rep]> = HashMap::new();
    for e in &p.effects {
        for o in &e.ops {
            ops.insert(e.symbol.child(&o.name), o.params.as_slice());
        }
    }
    let vals: HashMap<&Symbol, &Rep> = p.vals.iter().map(|v| (&v.symbol, &v.rep)).collect();
    let c = Checker {
        defs,
        fields,
        ops,
        vals,
    };
    for v in &p.vals {
        c.stmt(&v.body, &Scope::default(), &v.symbol)?;
    }
    for d in &p.defs {
        let mut sc = Scope::default();
        for b in &d.params {
            sc.reps.insert(b.name.clone(), b.rep.clone());
        }
        c.stmt(&d.body, &sc, &d.symbol)?;
    }
    Ok(())
}

/// What is known of the names in scope: how each is represented, and which
/// are objects whose methods are in sight.
#[derive(Clone, Default)]
struct Scope<'p> {
    reps: HashMap<String, Rep>,
    objects: HashMap<String, &'p [Method]>,
}

struct Checker<'p> {
    defs: HashMap<&'p Symbol, &'p Def>,
    fields: HashMap<Symbol, &'p [Rep]>,
    ops: HashMap<Symbol, &'p [Rep]>,
    vals: HashMap<&'p Symbol, &'p Rep>,
}

/// Whether a value of `r` is a word, not a block.
fn word(r: &Rep) -> bool {
    !matches!(r, Rep::Ptr | Rep::Str | Rep::Any | Rep::Var(_))
}

/// A name as the program wrote it: without the `__n` the inliner gives a
/// copy of it.
fn shown(name: &str) -> &str {
    match name.rsplit_once("__") {
        Some((base, n)) if !base.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => name,
    }
}

/// A literal, as the program wrote it, for saying which value is meant.
fn literal(p: &Producer) -> String {
    match p {
        Producer::Int(n) => n.to_string(),
        Producer::Float(x) => format!("{x:?}"),
        Producer::Char(c) => format!("{c:?}"),
        Producer::Bool(b) => b.to_string(),
        Producer::Unit => "unit".into(),
        Producer::Desc(r) => format!("desc({r})"),
        Producer::Val(s) => s.to_string(),
        _ => "a value".into(),
    }
}

impl<'p> Checker<'p> {
    /// How `p` is represented, where that is plain from it.
    fn rep_of(&self, p: &Producer, sc: &Scope) -> Option<Rep> {
        Some(match p {
            Producer::Var(x) => sc.reps.get(x)?.clone(),
            Producer::Val(s) => (*self.vals.get(s)?).clone(),
            Producer::Int(_) => Rep::I64,
            Producer::Float(_) => Rep::F64,
            Producer::Char(_) => Rep::Char,
            Producer::Bool(_) => Rep::Bool,
            Producer::Unit => Rep::Unit,
            Producer::Str(_) => Rep::Str,
            Producer::Desc(_) => Rep::Desc,
            Producer::Con(..)
            | Producer::Tuple(_)
            | Producer::Cocase(_)
            | Producer::Record(_)
            | Producer::Array(_) => Rep::Ptr,
            Producer::Mu(..) => return None,
        })
    }

    /// `p` given to `to`, which `what` names: refused if `to` is `any` and
    /// `p` a word.
    fn given(
        &self,
        p: &Producer,
        to: &Rep,
        what: &str,
        sc: &Scope,
        at: &Symbol,
    ) -> Result<(), String> {
        if *to != Rep::Any {
            return Ok(());
        }
        let Some(r) = self.rep_of(p, sc) else {
            return Ok(());
        };
        if !word(&r) {
            return Ok(());
        }
        let value = match p {
            Producer::Var(x) => format!("`{}`", shown(x)),
            _ => format!("`{}`", literal(p)),
        };
        Err(format!(
            "in {at}: {what} is `any`, and is given {value}, which is `{r}`: a value of `any` is \
             boxed, and `{r}` is a word's representation, not a box's -- box it, as a \
             constructor's field, before it is given to `any` (docs/CUT.md, open question 2)"
        ))
    }

    /// The methods of the object `p` is, where they are in sight: one made
    /// here, or one a name in scope is bound to.
    fn object(&self, p: &'p Producer, sc: &Scope<'p>) -> Option<&'p [Method]> {
        match p {
            Producer::Cocase(ms) => Some(ms),
            Producer::Var(x) => sc.objects.get(x).copied(),
            // `μ q. <cocase {..} | q>`: the object itself.
            Producer::Mu(q, s) => match &**s {
                Statement::Cut(Producer::Cocase(ms), Consumer::Var(k)) if k == q => Some(ms),
                _ => None,
            },
            _ => None,
        }
    }

    fn stmt(&self, s: &'p Statement, sc: &Scope<'p>, at: &Symbol) -> Result<(), String> {
        match s {
            Statement::Cut(p, c) => {
                self.producer(p, sc, at)?;
                match c {
                    Consumer::MuTilde(b, body) => {
                        self.given(p, &b.rep, &format!("`{}`", shown(&b.name)), sc, at)?;
                        let mut inner = sc.clone();
                        inner.reps.insert(b.name.clone(), b.rep.clone());
                        match self.object(p, sc) {
                            Some(ms) => inner.objects.insert(b.name.clone(), ms),
                            None => inner.objects.remove(&b.name),
                        };
                        self.stmt(body, &inner, at)
                    }
                    Consumer::Method(name, args, conts) => {
                        if let Some(ms) = self.object(p, sc)
                            && let Some(m) = ms.iter().find(|m| m.name == *name)
                        {
                            for (a, b) in args.iter().zip(&m.params) {
                                let what = format!("`{}` of `{name}`", shown(&b.name));
                                self.given(a, &b.rep, &what, sc, at)?;
                            }
                        }
                        for a in args {
                            self.producer(a, sc, at)?;
                        }
                        for k in conts {
                            self.consumer(k, sc, at)?;
                        }
                        Ok(())
                    }
                    c => self.consumer(c, sc, at),
                }
            }
            Statement::Call(f, args, conts) => {
                if let Some(d) = self.defs.get(f) {
                    for (a, b) in args.iter().zip(&d.params) {
                        let what = format!("`{}` of {f}", shown(&b.name));
                        self.given(a, &b.rep, &what, sc, at)?;
                    }
                }
                for a in args {
                    self.producer(a, sc, at)?;
                }
                for k in conts {
                    self.consumer(k, sc, at)?;
                }
                Ok(())
            }
            Statement::Prim(_, args, conts) => {
                for a in args {
                    self.producer(a, sc, at)?;
                }
                for k in conts {
                    self.consumer(k, sc, at)?;
                }
                Ok(())
            }
            Statement::Let(b, p, rest) => {
                self.producer(p, sc, at)?;
                self.given(p, &b.rep, &format!("`{}`", shown(&b.name)), sc, at)?;
                let mut inner = sc.clone();
                inner.reps.insert(b.name.clone(), b.rep.clone());
                match self.object(p, sc) {
                    Some(ms) => inner.objects.insert(b.name.clone(), ms),
                    None => inner.objects.remove(&b.name),
                };
                self.stmt(rest, &inner, at)
            }
            Statement::Handle(h) => {
                for c in &h.clauses {
                    let mut inner = sc.clone();
                    for b in &c.params {
                        inner.reps.insert(b.name.clone(), b.rep.clone());
                        inner.objects.remove(&b.name);
                    }
                    self.stmt(&c.body, &inner, at)?;
                }
                let (b, _, ret) = &h.ret;
                let mut inner = sc.clone();
                inner.reps.insert(b.name.clone(), b.rep.clone());
                inner.objects.remove(&b.name);
                self.stmt(ret, &inner, at)?;
                self.stmt(&h.body, sc, at)?;
                self.consumer(&h.cont, sc, at)
            }
            Statement::Perform(op, args, c) => {
                if let Some(reps) = self.ops.get(op) {
                    for (i, (a, r)) in args.iter().zip(reps.iter()).enumerate() {
                        let what = format!("the {} argument of {op}", ordinal(i));
                        self.given(a, r, &what, sc, at)?;
                    }
                }
                for a in args {
                    self.producer(a, sc, at)?;
                }
                self.consumer(c, sc, at)
            }
            Statement::Error(_) => Ok(()),
        }
    }

    fn producer(&self, p: &'p Producer, sc: &Scope<'p>, at: &Symbol) -> Result<(), String> {
        match p {
            Producer::Mu(_, s) => self.stmt(s, sc, at),
            Producer::Cocase(ms) => {
                for m in ms {
                    let mut inner = sc.clone();
                    for b in &m.params {
                        inner.reps.insert(b.name.clone(), b.rep.clone());
                        inner.objects.remove(&b.name);
                    }
                    self.stmt(&m.body, &inner, at)?;
                }
                Ok(())
            }
            Producer::Con(c, args) => {
                if let Some(reps) = self.fields.get(c) {
                    for (i, (a, r)) in args.iter().zip(reps.iter()).enumerate() {
                        let what = format!("the {} field of {c}", ordinal(i));
                        self.given(a, r, &what, sc, at)?;
                    }
                }
                for a in args {
                    self.producer(a, sc, at)?;
                }
                Ok(())
            }
            Producer::Tuple(xs) | Producer::Array(xs) => {
                for x in xs {
                    self.producer(x, sc, at)?;
                }
                Ok(())
            }
            Producer::Record(fs) => {
                for (_, x) in fs {
                    self.producer(x, sc, at)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn consumer(&self, c: &'p Consumer, sc: &Scope<'p>, at: &Symbol) -> Result<(), String> {
        match c {
            Consumer::MuTilde(b, body) => {
                let mut inner = sc.clone();
                inner.reps.insert(b.name.clone(), b.rep.clone());
                inner.objects.remove(&b.name);
                self.stmt(body, &inner, at)
            }
            Consumer::Case(arms) => {
                for a in arms {
                    let mut inner = sc.clone();
                    for b in &a.fields {
                        inner.reps.insert(b.name.clone(), b.rep.clone());
                        inner.objects.remove(&b.name);
                    }
                    self.stmt(&a.body, &inner, at)?;
                }
                Ok(())
            }
            Consumer::Method(_, args, conts) => {
                for a in args {
                    self.producer(a, sc, at)?;
                }
                for k in conts {
                    self.consumer(k, sc, at)?;
                }
                Ok(())
            }
            Consumer::Var(_) | Consumer::Halt => Ok(()),
        }
    }
}

fn ordinal(i: usize) -> String {
    match i {
        0 => "first".into(),
        1 => "second".into(),
        2 => "third".into(),
        _ => format!("{}th", i + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As lowering checks it: once its small definitions are inlined, which
    /// is what brings an object into sight of the call of its method.
    fn checked(text: &str) -> Result<(), String> {
        let p = crate::parse(text).expect("it parses");
        check(&p).and_then(|()| check(&crate::lower::optimized(&p)))
    }

    /// The program that found it: `(\a b -> a) m x`, with `x` an `i64` given
    /// to the inner function's `any`, through the variable the outer one's
    /// answer is bound to.
    #[test]
    fn a_word_given_to_any_through_an_object_in_sight_is_refused() {
        let text = r#"cut 0
entry t:Main/main
answer none

def t:Main/f (m: ptr, x: i64; k: ptr) =
  <cocase {
    apply(p0: any; q0: ptr) =>
      <cocase {
        apply(p1: any; q1: ptr) =>
          <p0 | q1>
      } | q0>
  } | apply(m; μ̃ g: ptr.
    <g | apply(x; μ̃ m2: any. <m2 | k>)>)>

def t:Main/main (; k: ptr) = <unit | k>
"#;
        let e = checked(text).unwrap_err();
        assert!(
            e.contains("`p1` of `apply` is `any`, and is given `x`, which is `i64`"),
            "{e}"
        );
    }

    #[test]
    fn a_pointer_given_to_any_is_not() {
        let text = r#"cut 0
entry t:Main/main
answer none

data t:Main/Box { B(i64) }

def t:Main/f (x: i64; k: ptr) =
  <t:Main/Box.B(x) | μ̃ b: any. <b | k>>

def t:Main/main (; k: ptr) = <unit | k>
"#;
        assert_eq!(checked(text), Ok(()));
    }

    #[test]
    fn a_word_given_to_any_by_a_let_or_a_call_is_refused() {
        let call = r#"cut 0
entry t:Main/main
answer none

def t:Main/g (a: any; k: ptr) = <a | k>

def t:Main/main (; k: ptr) = t:Main/g(7; k)
"#;
        let e = checked(call).unwrap_err();
        assert!(
            e.contains("`a` of t:Main/g is `any`, and is given `7`"),
            "{e}"
        );
        let bound = r#"cut 0
entry t:Main/main
answer none

def t:Main/main (; k: ptr) = <2.5 | μ̃ y: any. <unit | k>>
"#;
        let e = checked(bound).unwrap_err();
        assert!(
            e.contains("`y` is `any`, and is given `2.5`, which is `f64`"),
            "{e}"
        );
    }
}
