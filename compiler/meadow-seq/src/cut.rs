//! Core lowered to Cut: what `bootstrap/src/Seq.mw` is in Meadow, here, for
//! this compiler's own programs.
//!
//! **This is not yet how a program is compiled** -- [`crate::lower_program`]
//! still takes core to AxCut itself. It is how a program is *shown* as Cut,
//! and how the names of its source are followed into it: [`Lowered::names`]
//! says which declaration and variable of the Cut each variable of core's
//! is.
//!
//! Lowering a term is saying where its value goes: `term` carries the
//! consumer down, and each case answers the statement that gives its term's
//! value to it. A term another needs the value of is lowered with a consumer
//! that names the value and goes on, `μ̃ x: rep. s`, which is the whole of
//! how evaluation order is said.
//!
//! A function is codata with one method, `apply`, of one argument, and every
//! top-level definition is a `val`: the closure, or the value. A local
//! function that calls itself is a definition at the top, of what it
//! mentions from around it.
//!
//! What is not lowered answers `error "unsupported: …"` where it would run,
//! so that the rest of a program still reads: a join point; and anything
//! generic in how a value is represented, which is written as the `ptr` it
//! may not be.

use std::collections::{HashMap, HashSet};

use meadow_core as core;
use meadow_core::{Lit, Pat, Term, Ty, Var};
use meadow_cut::{
    Arm, Binder, Clause, Consumer, DataDecl, Def, EffectDecl, Handle, Method, OpDecl, Pattern,
    Producer, Program, Rep, Statement, Symbol, Val,
};
use meadow_intern::InternedString;
use meadow_rt::Prim;

/// A program as Cut, and what its source's names are there.
pub struct Lowered {
    pub program: Program,
    /// Each top-level definition's symbol.
    pub symbols: HashMap<Var, Symbol>,
    /// Each variable a definition binds: the declaration of the Cut it is
    /// in, and its name there. A local function that calls itself is in a
    /// declaration of its own, and so is what it binds.
    pub names: HashMap<Var, (Symbol, String)>,
}

/// A definition a program mentions and does not have: another package's,
/// or one left out of what is being shown.
#[derive(Clone, Debug)]
pub struct Outside {
    pub symbol: Symbol,
    pub poly: core::Poly,
}

/// The symbol of the definition called `name` in `module`, package first --
/// or in no module, as a REPL's entry is.
pub fn symbol_of(module: &str, name: &str) -> Symbol {
    let mut path: Vec<String> = module.split('.').map(str::to_string).collect();
    let package = if path.first().is_some_and(|p| !p.is_empty()) {
        path.remove(0)
    } else {
        path.clear();
        "main".to_string()
    };
    // A package is written plainly in a symbol: a REPL entry's module,
    // `repl:3`, is `repl-3` there.
    let package: String = package
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '@') {
                c
            } else {
                '-'
            }
        })
        .collect();
    path.push(name.to_string());
    Symbol {
        lang: "meadow".to_string(),
        package,
        path,
    }
}

/// `program` as a program of Cut. Nothing is its entry: it is to be read.
pub fn to_cut(program: &core::Program) -> Lowered {
    to_cut_in(program, &HashMap::new())
}

/// The same, of a program that is part of one: a mention of a definition
/// in `outside` reads that definition's value, by its symbol.
pub fn to_cut_in(program: &core::Program, outside: &HashMap<Var, Outside>) -> Lowered {
    let mut l = Lower {
        symbols: HashMap::new(),
        names: HashMap::new(),
        types: HashMap::new(),
        polys: HashMap::new(),
        current: None,
        fresh: 0,
        lifted: Vec::new(),
        effects: Vec::new(),
        variants: &program.variants,
    };
    for (v, o) in outside {
        l.symbols.insert(*v, o.symbol.clone());
        l.types.insert(*v, o.poly.ty.clone());
        l.polys.insert(*v, o.poly.clone());
    }
    let mut taken: HashSet<Symbol> = outside.values().map(|o| o.symbol.clone()).collect();
    for d in &program.defs {
        let mut symbol = symbol_of(&d.module, &d.name);
        if !taken.insert(symbol.clone()) {
            // A name defined again, or a copy a pass made of it.
            symbol.path.pop();
            symbol.path.push(format!("{}#{}", d.name, d.var.0));
        }
        l.symbols.insert(d.var, symbol);
        l.types.insert(d.var, d.poly.ty.clone());
        l.polys.insert(d.var, d.poly.clone());
    }
    let mut vals = Vec::new();
    for d in &program.defs {
        let symbol = l.symbols[&d.var].clone();
        l.current = Some(symbol.clone());
        vals.push(Val {
            symbol,
            rep: rep_of(&d.poly.ty),
            body: l.term(&d.term, Consumer::Halt),
        });
    }
    let mut datas: Vec<DataDecl> = program
        .variants
        .iter()
        .filter(|(_, ctors)| !ctors.is_empty())
        .map(|(ty, ctors)| {
            let arity = ctors
                .iter()
                .flat_map(|c| c.fields.iter())
                .map(bound_in)
                .max()
                .unwrap_or(0);
            DataDecl {
                symbol: type_symbol(ty),
                rep_vars: (0..arity).map(|i| format!("a{i}")).collect(),
                ctors: ctors
                    .iter()
                    .map(|c| {
                        (
                            bare(&c.name).to_string(),
                            c.fields.iter().map(field_rep).collect(),
                        )
                    })
                    .collect(),
            }
        })
        .collect();
    datas.sort_by_key(|d| d.symbol.to_string());
    let mut effects = std::mem::take(&mut l.effects);
    effects.sort_by_key(|e| e.symbol.to_string());
    Lowered {
        program: Program {
            version: meadow_cut::VERSION,
            entry: None,
            answer: meadow_cut::Answer::None,
            exports: Vec::new(),
            datas,
            roles: Vec::new(),
            natives: Vec::new(),
            effects,
            vals,
            defs: std::mem::take(&mut l.lifted),
        },
        symbols: l.symbols,
        names: l.names,
    }
}

struct Lower<'p> {
    symbols: HashMap<Var, Symbol>,
    names: HashMap<Var, (Symbol, String)>,
    /// Every binder's type, as it is met.
    types: HashMap<Var, Ty>,
    /// Each top-level definition's type, over the types it abstracts.
    polys: HashMap<Var, core::Poly>,
    /// The declaration being lowered.
    current: Option<Symbol>,
    fresh: u32,
    /// The definitions made of local functions that call themselves.
    lifted: Vec<Def>,
    /// Each effect an operation of which is performed or handled.
    effects: Vec<EffectDecl>,
    variants: &'p meadow_infer::VariantEnv,
}

fn var(v: Var) -> String {
    format!("v{}", v.0)
}

fn binder(name: &str, rep: Rep) -> Binder {
    Binder {
        name: name.to_string(),
        rep,
    }
}

fn give(p: Producer, c: Consumer) -> Statement {
    Statement::Cut(p, c)
}

fn bind(name: &str, rep: Rep, rest: Statement) -> Consumer {
    Consumer::MuTilde(binder(name, rep), Box::new(rest))
}

fn unsupported(what: &str) -> Statement {
    Statement::Error(format!("unsupported: {what}"))
}

impl Lower<'_> {
    fn name(&mut self, prefix: &str) -> String {
        self.fresh += 1;
        format!("{prefix}{}", self.fresh)
    }

    /// `v` is bound here, at type `ty`: its name in the Cut, which is
    /// remembered as its declaration's.
    fn binds(&mut self, v: Var, ty: &Ty) -> String {
        self.types.insert(v, ty.clone());
        let name = var(v);
        if let Some(of) = &self.current {
            self.names.insert(v, (of.clone(), name.clone()));
        }
        name
    }

    /// The type of `t`, as far as a walk down its spine can tell: enough to
    /// know how its value is represented.
    fn type_of(&self, t: &Term) -> Option<Ty> {
        Some(match t {
            Term::Loc(_, inner) | Term::TyLam(_, inner) => return self.type_of(inner),
            // A definition at the types it is used at here.
            Term::TyApp(f, args) => match f.peel() {
                Term::Var(v) => match self.polys.get(v) {
                    Some(poly) if poly.binders.len() == args.len() => poly.instantiate(args),
                    _ => return self.type_of(f),
                },
                other => return self.type_of(other),
            },
            Term::Var(v) => self.types.get(v)?.clone(),
            Term::Lit(l) => lit_type(l),
            Term::Lam(_, param, body) => Ty::Fun(
                vec![param.clone()],
                Box::new(self.type_of(body).unwrap_or_else(core::unknown)),
                Box::new(Ty::RowEmpty),
            ),
            Term::App(f, _) => match self.type_of(f)? {
                Ty::Fun(params, ret, eff) if params.len() > 1 => {
                    Ty::Fun(params[1..].to_vec(), ret, eff)
                }
                Ty::Fun(_, ret, _) => *ret,
                _ => return None,
            },
            Term::Let(_, _, _, body) | Term::LetRec(_, body) => return self.type_of(body),
            Term::Join { ty, .. } | Term::Jump(_, _, ty) => ty.clone(),
            Term::If(_, then, _) => return self.type_of(then),
            Term::Tuple(items) => Ty::Tuple(
                items
                    .iter()
                    .map(|x| self.type_of(x).unwrap_or_else(core::unknown))
                    .collect(),
            ),
            Term::Proj(t, i) => match self.type_of(t)? {
                Ty::Tuple(items) => items.get(*i)?.clone(),
                _ => return None,
            },
            Term::Array(_, elem) => Ty::Con(InternedString::from("Array"), vec![elem.clone()]),
            Term::Sel(_, _, ty)
            | Term::Ctor(_, ty, _)
            | Term::Case(_, _, ty)
            | Term::Prim(_, _, ty)
            | Term::Perform(_, _, _, ty)
            | Term::Handle { ty, .. } => ty.clone(),
            Term::Record(_) | Term::Extend(..) | Term::Error => return None,
        })
    }

    fn rep(&self, t: &Term) -> Rep {
        self.type_of(t).as_ref().map(rep_of).unwrap_or(Rep::Ptr)
    }

    /// What `build` makes with somewhere to send a value more than once: `k`
    /// itself, if that is a name already, and otherwise a name for it.
    fn shared(
        &mut self,
        k: Consumer,
        build: impl FnOnce(&mut Self, Consumer) -> Statement,
    ) -> Statement {
        if matches!(k, Consumer::Var(_) | Consumer::Halt) {
            return build(self, k);
        }
        let j = self.name("j");
        let body = build(self, Consumer::Var(j.clone()));
        give(Producer::Mu(j, Box::new(body)), k)
    }

    /// `t` lowered so that its value is `name` in `rest`.
    fn named(&mut self, t: &Term, name: &str, rest: Statement) -> Statement {
        let rep = self.rep(t);
        self.term(t, bind(name, rep, rest))
    }

    /// Each of `ts` named in turn, then `last` of their names.
    fn all(
        &mut self,
        ts: &[Term],
        prefix: &str,
        last: impl FnOnce(Vec<Producer>) -> Statement,
    ) -> Statement {
        let names: Vec<String> = ts.iter().map(|_| self.name(prefix)).collect();
        let mut rest = last(names.iter().cloned().map(Producer::Var).collect());
        for (t, name) in ts.iter().zip(&names).rev() {
            rest = self.named(t, name, rest);
        }
        rest
    }

    fn term(&mut self, t: &Term, k: Consumer) -> Statement {
        match t {
            Term::Loc(_, inner) | Term::TyLam(_, inner) | Term::TyApp(inner, _) => {
                self.term(inner, k)
            }
            Term::Var(v) => match self.symbols.get(v) {
                Some(s) => give(Producer::Val(s.clone()), k),
                None => give(Producer::Var(var(*v)), k),
            },
            Term::Lit(l) => give(literal(l), k),
            Term::Lam(v, ty, body) => {
                let x = self.binds(*v, ty);
                let ret = self.name("k");
                let inner = self.term(body, Consumer::Var(ret.clone()));
                give(
                    Producer::Cocase(vec![Method {
                        name: "apply".to_string(),
                        params: vec![binder(&x, rep_of(ty))],
                        conts: vec![ret],
                        body: inner,
                    }]),
                    k,
                )
            }
            Term::App(f, a) => {
                let (fname, aname) = (self.name("f"), self.name("a"));
                let call = give(
                    Producer::Var(fname.clone()),
                    Consumer::Method(
                        "apply".to_string(),
                        vec![Producer::Var(aname.clone())],
                        vec![k],
                    ),
                );
                let given = self.named(a, &aname, call);
                self.named(f, &fname, given)
            }
            Term::Let(v, poly, rhs, body) => {
                let x = self.binds(*v, &poly.ty);
                let rest = self.term(body, k);
                self.term(rhs, bind(&x, rep_of(&poly.ty), rest))
            }
            Term::LetRec(binds, body) => self.lifted(binds, body, k),
            Term::If(c, yes, no) => self.shared(k, |this, k| {
                let cname = this.name("c");
                let u = this.name("u");
                let on_no = this.term(no, k.clone());
                let on_yes = this.term(yes, k);
                let test = Statement::Prim(
                    "if".to_string(),
                    vec![Producer::Var(cname.clone())],
                    vec![bind(&u, Rep::Unit, on_no), bind(&u, Rep::Unit, on_yes)],
                );
                this.named(c, &cname, test)
            }),
            Term::Tuple(items) => self.all(items, "t", |xs| give(Producer::Tuple(xs), k)),
            Term::Array(items, _) => self.all(items, "y", |xs| give(Producer::Array(xs), k)),
            Term::Ctor(name, _, args) => {
                let symbol = ctor_symbol(name);
                self.all(args, "x", |xs| give(Producer::Con(symbol, xs), k))
            }
            Term::Prim(p, args, _) => {
                let op = prim_name(*p);
                self.all(args, "p", |xs| Statement::Prim(op, xs, vec![k]))
            }
            Term::Proj(tuple, index) => match self.type_of(tuple) {
                Some(Ty::Tuple(tys)) => {
                    let whole = self.name("q");
                    let fields: Vec<Binder> = tys
                        .iter()
                        .map(|ty| binder(&self.name("q"), rep_of(ty)))
                        .collect();
                    let picked = fields
                        .get(*index)
                        .map(|b| b.name.clone())
                        .unwrap_or_default();
                    let taken = give(
                        Producer::Var(whole.clone()),
                        Consumer::Case(vec![Arm {
                            pattern: Pattern::Tuple,
                            fields,
                            body: give(Producer::Var(picked), k),
                        }]),
                    );
                    self.named(tuple, &whole, taken)
                }
                _ => unsupported("an item of what is not known to be a tuple"),
            },
            Term::Record(fields) => {
                let labels: Vec<String> = fields.iter().map(|(l, _)| l.to_string()).collect();
                let values: Vec<Term> = fields.iter().map(|(_, x)| x.clone()).collect();
                self.all(&values, "r", |xs| {
                    give(Producer::Record(labels.into_iter().zip(xs).collect()), k)
                })
            }
            Term::Sel(record, label, _) => {
                let r = self.name("r");
                let read = Statement::Prim(
                    "select".to_string(),
                    vec![Producer::Var(r.clone()), Producer::Str(label.to_string())],
                    vec![k],
                );
                self.named(record, &r, read)
            }
            Term::Extend(record, label, value) => {
                let (r, v) = (self.name("r"), self.name("e"));
                let set = Statement::Prim(
                    "extend".to_string(),
                    vec![
                        Producer::Var(r.clone()),
                        Producer::Str(label.to_string()),
                        Producer::Var(v.clone()),
                    ],
                    vec![k],
                );
                let valued = self.named(value, &v, set);
                self.named(record, &r, valued)
            }
            Term::Case(scrutinee, arms, _) => self.shared(k, |this, k| {
                let s = this.name("s");
                let matching = this.arms(&s, arms, k);
                this.named(scrutinee, &s, matching)
            }),
            Term::Perform(effect, op, arg, ty) => {
                let a = self.name("a");
                let arg_rep = self.rep(arg);
                let symbol = self.operation(effect, op, arg_rep, rep_of(ty));
                let performed = Statement::Perform(symbol, vec![Producer::Var(a.clone())], k);
                self.named(arg, &a, performed)
            }
            Term::Handle {
                body,
                clauses,
                ret,
                ty,
            } => {
                let h = self.name("h");
                let b = self.name("b");
                let answered = self.rep(body);
                let inner = Consumer::Var(h.clone());
                let clauses = clauses
                    .iter()
                    .map(|c| {
                        let x = self.binds(c.param, &c.param_ty);
                        let r = self.binds(c.resume, &c.resume_ty);
                        // What the operation is resumed with is what the
                        // resumption takes.
                        let resumed = match &c.resume_ty {
                            Ty::Fun(params, ..) => params.first().map(rep_of).unwrap_or(Rep::Ptr),
                            _ => Rep::Ptr,
                        };
                        let op = self.operation(&c.effect, &c.op, rep_of(&c.param_ty), resumed);
                        Clause {
                            op,
                            params: vec![binder(&x, rep_of(&c.param_ty))],
                            resumption: r,
                            cont: h.clone(),
                            body: self.term(&c.body, inner.clone()),
                        }
                    })
                    .collect();
                let ret = match ret {
                    Some((v, vty, body)) => {
                        let x = self.binds(*v, vty);
                        (
                            binder(&x, rep_of(vty)),
                            h.clone(),
                            self.term(body, inner.clone()),
                        )
                    }
                    None => {
                        let x = self.name("r");
                        (
                            binder(&x, answered),
                            h.clone(),
                            give(Producer::Var(x), inner.clone()),
                        )
                    }
                };
                let _ = ty;
                Statement::Handle(Box::new(Handle {
                    clauses,
                    ret,
                    body_cont: b.clone(),
                    body: self.term(body, Consumer::Var(b)),
                    cont: k,
                }))
            }
            Term::Join { .. } => unsupported("a join"),
            Term::Jump(..) => unsupported("a jump"),
            Term::Error => Statement::Error("this did not compile".to_string()),
        }
    }

    /// The symbol of operation `op` of `effect`, which is declared if it was
    /// not: taking `arg` and resumed with `result`.
    fn operation(
        &mut self,
        effect: &InternedString,
        op: &InternedString,
        arg: Rep,
        result: Rep,
    ) -> Symbol {
        let symbol = type_symbol(effect);
        let decl = match self.effects.iter().position(|e| e.symbol == symbol) {
            Some(i) => &mut self.effects[i],
            None => {
                self.effects.push(EffectDecl {
                    symbol: symbol.clone(),
                    ops: Vec::new(),
                });
                self.effects.last_mut().expect("just pushed")
            }
        };
        if !decl.ops.iter().any(|o| o.name == **op) {
            decl.ops.push(OpDecl {
                name: op.to_string(),
                many: false,
                params: vec![arg],
                result,
            });
        }
        symbol.child(op)
    }

    // --- local recursion ------------------------------------------------------
    //
    // Cut has no function that names itself where it is made, so one is a
    // definition at the top: of what any of the group mentions from around
    // them, answering the function. Where any is in scope -- in each one's
    // body, and in what follows -- every one of them is asked for again.

    fn lifted(&mut self, binds: &[(Var, core::Poly, Term)], body: &Term, k: Consumer) -> Statement {
        if binds.is_empty() {
            return self.term(body, k);
        }
        if binds
            .iter()
            .any(|(_, _, t)| !matches!(spine(t), Term::Lam(..)))
        {
            return unsupported("a local value that names itself");
        }
        let ids: Vec<Var> = binds.iter().map(|(v, ..)| *v).collect();
        for (v, poly, _) in binds {
            self.types.insert(*v, poly.ty.clone());
        }
        let mut around = Vec::new();
        for (_, _, t) in binds {
            free(t, &mut ids.clone(), &mut around);
        }
        around.retain(|v| !self.symbols.contains_key(v));
        let outer = self.current.clone().expect("lowering a declaration");
        let symbols: Vec<Symbol> = ids
            .iter()
            .map(|v| {
                let mut s = outer.clone();
                let last = s.path.pop().unwrap_or_default();
                s.path.push(format!("{last}#l{}", v.0));
                s
            })
            .collect();
        let given: Vec<Producer> = around.iter().map(|v| Producer::Var(var(*v))).collect();
        // Every one of the functions, by its name, around `rest`.
        let each = |rest: Statement| {
            ids.iter()
                .zip(&symbols)
                .rev()
                .fold(rest, |rest, (v, symbol)| {
                    Statement::Call(
                        symbol.clone(),
                        given.clone(),
                        vec![bind(&var(*v), Rep::Ptr, rest)],
                    )
                })
        };
        for ((v, _, t), symbol) in binds.iter().zip(&symbols) {
            let Term::Lam(param, pty, lam_body) = spine(t) else {
                unreachable!("checked above")
            };
            let was = self.current.replace(symbol.clone());
            self.names.insert(*v, (symbol.clone(), var(*v)));
            let x = self.binds(*param, pty);
            let ret = self.name("k");
            let inside = each(self.term(lam_body, Consumer::Var(ret.clone())));
            let params: Vec<Binder> = around
                .iter()
                .map(|a| {
                    let rep = self.types.get(a).map(rep_of).unwrap_or(Rep::Ptr);
                    binder(&var(*a), rep)
                })
                .collect();
            self.lifted.push(Def {
                symbol: symbol.clone(),
                rep_vars: Vec::new(),
                effect_vars: Vec::new(),
                params,
                conts: vec!["k".to_string()],
                body: give(
                    Producer::Cocase(vec![Method {
                        name: "apply".to_string(),
                        params: vec![binder(&x, rep_of(pty))],
                        conts: vec![ret],
                        body: inside,
                    }]),
                    Consumer::Var("k".to_string()),
                ),
            });
            self.current = was;
        }
        let rest = self.term(body, k);
        each(rest)
    }

    // --- matching -------------------------------------------------------------
    //
    // A `case` of Cut takes one constructor apart and no more, so a pattern
    // is matched a layer at a time. An arm that does not match gives up --
    // `unit` to a continuation the arm is built with the name of -- and the
    // arms after it are what that continuation does.

    fn arms(&mut self, s: &str, arms: &[(Pat, Option<Term>, Term)], k: Consumer) -> Statement {
        let Some(((pat, guard, body), rest)) = arms.split_first() else {
            return Statement::Error("non-exhaustive pattern match".to_string());
        };
        let fail = self.name("n");
        let scrutinee_ty = None;
        let first = {
            let k = k.clone();
            let fail_name = fail.clone();
            self.matched(s, pat, scrutinee_ty, &fail, &mut |this| match guard {
                None => this.term(body, k.clone()),
                Some(g) => {
                    let name = this.name("g");
                    let u = this.name("u");
                    let gave_up = give(Producer::Unit, Consumer::Var(fail_name.clone()));
                    let taken = this.term(body, k.clone());
                    let test = Statement::Prim(
                        "if".to_string(),
                        vec![Producer::Var(name.clone())],
                        vec![bind(&u, Rep::Unit, gave_up), bind(&u, Rep::Unit, taken)],
                    );
                    this.named(g, &name, test)
                }
            })
        };
        let next = self.arms(s, rest, k);
        let u = self.name("u");
        give(
            Producer::Mu(fail, Box::new(first)),
            bind(&u, Rep::Unit, next),
        )
    }

    /// The value named `x` matched against `p`: what `success` builds where
    /// it matches, with what `p` binds bound, and giving up to `fail` where
    /// it does not.
    fn matched(
        &mut self,
        x: &str,
        p: &Pat,
        ty: Option<&Ty>,
        fail: &str,
        success: &mut dyn FnMut(&mut Self) -> Statement,
    ) -> Statement {
        let value = Producer::Var(x.to_string());
        let gave_up = || give(Producer::Unit, Consumer::Var(fail.to_string()));
        match p {
            Pat::Wild => success(self),
            Pat::Var(v, vty) => {
                let name = self.binds(*v, vty);
                let rest = success(self);
                give(value, bind(&name, rep_of(vty), rest))
            }
            Pat::As(v, vty, inner) => {
                let name = self.binds(*v, vty);
                let rest = self.matched(x, inner, Some(vty), fail, success);
                give(value, bind(&name, rep_of(vty), rest))
            }
            Pat::Lit(Lit::Unit) => success(self),
            Pat::Lit(Lit::Bool(b)) => {
                let u = self.name("u");
                let (no, yes) = if *b {
                    (gave_up(), success(self))
                } else {
                    (success(self), gave_up())
                };
                Statement::Prim(
                    "if".to_string(),
                    vec![value],
                    vec![bind(&u, Rep::Unit, no), bind(&u, Rep::Unit, yes)],
                )
            }
            Pat::Lit(l) => {
                let u = self.name("u");
                let same = success(self);
                Statement::Prim(
                    "eq".to_string(),
                    vec![value, literal(l)],
                    vec![bind(&u, Rep::Unit, gave_up()), bind(&u, Rep::Unit, same)],
                )
            }
            Pat::Tuple(items) => {
                let tys: Vec<Option<Ty>> = match ty {
                    Some(Ty::Tuple(tys)) if tys.len() == items.len() => {
                        tys.iter().cloned().map(Some).collect()
                    }
                    _ => vec![None; items.len()],
                };
                let fields: Vec<Binder> = items
                    .iter()
                    .zip(&tys)
                    .map(|(item, ty)| binder(&self.name("m"), pat_rep(item, ty.as_ref())))
                    .collect();
                let body = self.matched_all(&fields, items, &tys, fail, success);
                give(
                    value,
                    Consumer::Case(vec![Arm {
                        pattern: Pattern::Tuple,
                        fields,
                        body,
                    }]),
                )
            }
            Pat::Ctor(name, args) => {
                let tys: Vec<Option<Ty>> = self
                    .field_types(name, ty)
                    .filter(|tys| tys.len() == args.len())
                    .map(|tys| tys.into_iter().map(Some).collect())
                    .unwrap_or_else(|| vec![None; args.len()]);
                let fields: Vec<Binder> = args
                    .iter()
                    .zip(&tys)
                    .map(|(arg, ty)| binder(&self.name("m"), pat_rep(arg, ty.as_ref())))
                    .collect();
                let body = self.matched_all(&fields, args, &tys, fail, success);
                give(
                    value,
                    Consumer::Case(vec![
                        Arm {
                            pattern: Pattern::Con(ctor_symbol(name)),
                            fields,
                            body,
                        },
                        Arm {
                            pattern: Pattern::Default,
                            fields: Vec::new(),
                            body: gave_up(),
                        },
                    ]),
                )
            }
            Pat::Array(items) => {
                let len = self.name("l");
                let u = self.name("u");
                let names: Vec<String> = items.iter().map(|_| self.name("m")).collect();
                let fields: Vec<Binder> = names
                    .iter()
                    .zip(items)
                    .map(|(n, item)| binder(n, pat_rep(item, None)))
                    .collect();
                let tys = vec![None; items.len()];
                let mut taken = self.matched_all(&fields, items, &tys, fail, success);
                for (i, b) in fields.iter().enumerate().rev() {
                    taken = Statement::Prim(
                        "arrayGet".to_string(),
                        vec![value.clone(), Producer::Int(i as i64)],
                        vec![Consumer::MuTilde(b.clone(), Box::new(taken))],
                    );
                }
                Statement::Prim(
                    "arrayLen".to_string(),
                    vec![value],
                    vec![bind(
                        &len,
                        Rep::I64,
                        Statement::Prim(
                            "eq".to_string(),
                            vec![
                                Producer::Var(len.clone()),
                                Producer::Int(items.len() as i64),
                            ],
                            vec![bind(&u, Rep::Unit, gave_up()), bind(&u, Rep::Unit, taken)],
                        ),
                    )],
                )
            }
            // Each field the pattern names read, and matched in turn.
            Pat::Record(fields) => {
                let binders: Vec<Binder> = fields
                    .iter()
                    .map(|(_, p)| binder(&self.name("m"), pat_rep(p, None)))
                    .collect();
                let pats: Vec<Pat> = fields.iter().map(|(_, p)| p.clone()).collect();
                let tys = vec![None; pats.len()];
                let mut read = self.matched_all(&binders, &pats, &tys, fail, success);
                for ((label, _), b) in fields.iter().zip(&binders).rev() {
                    read = Statement::Prim(
                        "select".to_string(),
                        vec![value.clone(), Producer::Str(label.to_string())],
                        vec![Consumer::MuTilde(b.clone(), Box::new(read))],
                    );
                }
                read
            }
        }
    }

    /// Each named value matched against its pattern, in turn.
    fn matched_all(
        &mut self,
        fields: &[Binder],
        pats: &[Pat],
        tys: &[Option<Ty>],
        fail: &str,
        success: &mut dyn FnMut(&mut Self) -> Statement,
    ) -> Statement {
        let (Some(field), Some(pat)) = (fields.first(), pats.first()) else {
            return success(self);
        };
        let ty = tys.first().and_then(|t| t.as_ref());
        self.matched(&field.name, pat, ty, fail, &mut |this| {
            this.matched_all(&fields[1..], &pats[1..], &tys[1..], fail, &mut *success)
        })
    }

    /// The types of constructor `ctor`'s fields in a value of type `of`,
    /// where the program's declarations say.
    fn field_types(&self, ctor: &InternedString, of: Option<&Ty>) -> Option<Vec<Ty>> {
        let args: &[Ty] = match of {
            Some(Ty::Con(_, args)) => args,
            _ => &[],
        };
        let sig = self
            .variants
            .values()
            .flat_map(|ctors| ctors.iter())
            .find(|c| c.name == *ctor)?;
        Some(
            sig.fields
                .iter()
                .map(|f| meadow_infer::subst_bound(f, args))
                .collect(),
        )
    }
}

/// `t` under what says nothing of how it runs: where it was written, and
/// the types it abstracts over.
fn spine(t: &Term) -> &Term {
    match t {
        Term::Loc(_, inner) | Term::TyLam(_, inner) => spine(inner),
        other => other,
    }
}

/// The variables `t` mentions and does not bind, after `out`, each once:
/// the definitions it reads among them.
pub fn mentioned(t: &Term, out: &mut Vec<Var>) {
    free(t, &mut Vec::new(), out)
}

/// Whether `t` binds or mentions `v`.
pub fn names(t: &Term, v: Var) -> bool {
    let mut out = Vec::new();
    all_vars(t, &mut out);
    out.contains(&v)
}

/// Every variable `t` binds or mentions.
pub fn variables(t: &Term) -> Vec<Var> {
    let mut out = Vec::new();
    all_vars(t, &mut out);
    out
}

fn all_vars(t: &Term, out: &mut Vec<Var>) {
    fn walk(t: &Term, out: &mut Vec<Var>) {
        match t {
            Term::Lam(v, _, body) => {
                out.push(*v);
                walk(body, out);
            }
            Term::Loc(_, inner) | Term::TyLam(_, inner) | Term::TyApp(inner, _) => walk(inner, out),
            Term::App(f, a) => {
                walk(f, out);
                walk(a, out);
            }
            Term::Let(v, _, rhs, body) => {
                out.push(*v);
                walk(rhs, out);
                walk(body, out);
            }
            Term::LetRec(binds, body) => {
                for (v, _, t) in binds {
                    out.push(*v);
                    walk(t, out);
                }
                walk(body, out);
            }
            Term::If(c, a, b) => {
                walk(c, out);
                walk(a, out);
                walk(b, out);
            }
            Term::Tuple(xs) | Term::Array(xs, _) | Term::Ctor(_, _, xs) | Term::Prim(_, xs, _) => {
                xs.iter().for_each(|x| walk(x, out))
            }
            Term::Jump(v, xs, _) => {
                out.push(*v);
                xs.iter().for_each(|x| walk(x, out));
            }
            Term::Proj(x, _) | Term::Sel(x, _, _) | Term::Perform(_, _, x, _) => walk(x, out),
            Term::Record(fs) => fs.iter().for_each(|(_, x)| walk(x, out)),
            Term::Extend(r, _, v) => {
                walk(r, out);
                walk(v, out);
            }
            Term::Case(s, arms, _) => {
                walk(s, out);
                for (p, g, b) in arms {
                    binders(p, out);
                    if let Some(g) = g {
                        walk(g, out);
                    }
                    walk(b, out);
                }
            }
            Term::Handle {
                body, clauses, ret, ..
            } => {
                walk(body, out);
                for c in clauses {
                    out.extend([c.param, c.resume]);
                    walk(&c.body, out);
                }
                if let Some((v, _, r)) = ret {
                    out.push(*v);
                    walk(r, out);
                }
            }
            Term::Join {
                var,
                params,
                rhs,
                body,
                ..
            } => {
                out.push(*var);
                out.extend(params.iter().map(|(v, _)| *v));
                walk(rhs, out);
                walk(body, out);
            }
            Term::Var(v) => out.push(*v),
            Term::Lit(_) | Term::Error => {}
        }
    }
    walk(t, out);
}

fn free(t: &Term, bound: &mut Vec<Var>, out: &mut Vec<Var>) {
    let mark = bound.len();
    match t {
        Term::Var(v) => {
            if !bound.contains(v) && !out.contains(v) {
                out.push(*v);
            }
        }
        Term::Lit(_) | Term::Error => {}
        Term::Lam(v, _, body) => {
            bound.push(*v);
            free(body, bound, out);
        }
        Term::Loc(_, inner) | Term::TyLam(_, inner) | Term::TyApp(inner, _) => {
            free(inner, bound, out)
        }
        Term::App(f, a) => {
            free(f, bound, out);
            free(a, bound, out);
        }
        Term::Let(v, _, rhs, body) => {
            free(rhs, bound, out);
            bound.push(*v);
            free(body, bound, out);
        }
        Term::LetRec(binds, body) => {
            bound.extend(binds.iter().map(|(v, ..)| *v));
            binds.iter().for_each(|(_, _, t)| free(t, bound, out));
            free(body, bound, out);
        }
        Term::If(c, a, b) => {
            free(c, bound, out);
            free(a, bound, out);
            free(b, bound, out);
        }
        Term::Tuple(xs) | Term::Array(xs, _) | Term::Ctor(_, _, xs) | Term::Prim(_, xs, _) => {
            xs.iter().for_each(|x| free(x, bound, out))
        }
        Term::Jump(v, xs, _) => {
            if !bound.contains(v) && !out.contains(v) {
                out.push(*v);
            }
            xs.iter().for_each(|x| free(x, bound, out));
        }
        Term::Proj(x, _) | Term::Sel(x, _, _) | Term::Perform(_, _, x, _) => free(x, bound, out),
        Term::Record(fs) => fs.iter().for_each(|(_, x)| free(x, bound, out)),
        Term::Extend(r, _, v) => {
            free(r, bound, out);
            free(v, bound, out);
        }
        Term::Case(s, arms, _) => {
            free(s, bound, out);
            for (p, g, b) in arms {
                let inner = bound.len();
                binders(p, bound);
                if let Some(g) = g {
                    free(g, bound, out);
                }
                free(b, bound, out);
                bound.truncate(inner);
            }
        }
        Term::Handle {
            body, clauses, ret, ..
        } => {
            free(body, bound, out);
            for c in clauses {
                let inner = bound.len();
                bound.extend([c.param, c.resume]);
                free(&c.body, bound, out);
                bound.truncate(inner);
            }
            if let Some((v, _, r)) = ret {
                bound.push(*v);
                free(r, bound, out);
            }
        }
        Term::Join {
            var,
            params,
            rhs,
            body,
            ..
        } => {
            let inner = bound.len();
            bound.extend(params.iter().map(|(v, _)| *v));
            free(rhs, bound, out);
            bound.truncate(inner);
            bound.push(*var);
            free(body, bound, out);
        }
    }
    bound.truncate(mark);
}

/// The variables a pattern binds.
fn binders(p: &Pat, out: &mut Vec<Var>) {
    match p {
        Pat::Wild | Pat::Lit(_) => {}
        Pat::Var(v, _) => out.push(*v),
        Pat::As(v, _, inner) => {
            out.push(*v);
            binders(inner, out);
        }
        Pat::Tuple(ps) | Pat::Array(ps) | Pat::Ctor(_, ps) => {
            ps.iter().for_each(|p| binders(p, out))
        }
        Pat::Record(fs) => fs.iter().for_each(|(_, p)| binders(p, out)),
    }
}

/// How what a pattern matches is represented: by the type its binder says,
/// or the declaration's for its place.
fn pat_rep(p: &Pat, declared: Option<&Ty>) -> Rep {
    match p {
        Pat::Var(_, ty) | Pat::As(_, ty, _) => rep_of(ty),
        Pat::Lit(l) => rep_of(&lit_type(l)),
        _ => declared.map(rep_of).unwrap_or(Rep::Ptr),
    }
}

fn literal(l: &Lit) -> Producer {
    match l {
        Lit::Int(n) | Lit::BigInt(n) | Lit::AnyInt(n, _) => Producer::Int(*n),
        Lit::Word(_, bits) => Producer::Int(*bits as i64),
        Lit::Float(x) | Lit::AnyFloat(x, _) => Producer::Float(*x),
        Lit::Float32(x) => Producer::Float(f64::from(*x)),
        Lit::Str(s) | Lit::Sym(s) => Producer::Str(s.to_string()),
        Lit::Char(c) => Producer::Char(*c),
        Lit::Bool(b) => Producer::Bool(*b),
        Lit::Unit => Producer::Unit,
    }
}

fn lit_type(l: &Lit) -> Ty {
    let con = |n: &str| Ty::Con(InternedString::from(n), Vec::new());
    match l {
        Lit::Int(_) => con("Int"),
        Lit::BigInt(_) => con("BigInt"),
        Lit::Float(_) => con("Float"),
        Lit::Word(w, _) => con(w.name()),
        Lit::Float32(_) => con("Float32"),
        Lit::AnyInt(_, v) | Lit::AnyFloat(_, v) => Ty::Var(*v),
        Lit::Str(_) => con("String"),
        Lit::Sym(_) => con(core::desc::SYMBOL_TYPE),
        Lit::Char(_) => con("Char"),
        Lit::Bool(_) => con("Bool"),
        Lit::Unit => con("Unit"),
    }
}

/// How a value of type `ty` is represented.
fn rep_of(ty: &Ty) -> Rep {
    let Ty::Con(name, args) = ty else {
        return Rep::Ptr;
    };
    if !args.is_empty() {
        return Rep::Ptr;
    }
    match &**name {
        "Int" | "Int64" => Rep::I64,
        "Float" | "Float64" => Rep::F64,
        "Float32" => Rep::F32,
        "Bool" => Rep::Bool,
        "Char" => Rep::Char,
        "String" => Rep::Str,
        "Unit" => Rep::Unit,
        "Int8" => Rep::I8,
        "Int16" => Rep::I16,
        "Int32" => Rep::I32,
        "UInt8" => Rep::U8,
        "UInt16" => Rep::U16,
        "UInt32" => Rep::U32,
        "UInt64" => Rep::U64,
        _ => Rep::Ptr,
    }
}

/// How a field of a declared type is represented: one of the type's own
/// parameters as that parameter.
fn field_rep(ty: &Ty) -> Rep {
    match ty {
        Ty::Bound(i) => Rep::Var(format!("a{i}")),
        other => rep_of(other),
    }
}

/// One more than the last of a type's parameters `ty` mentions by itself.
fn bound_in(ty: &Ty) -> usize {
    match ty {
        Ty::Bound(i) => *i as usize + 1,
        _ => 0,
    }
}

/// A primitive's name in Cut: `meadow_rt::Prim`'s, its first letter small,
/// and a conversion to a sized integer by its type's.
fn prim_name(p: Prim) -> String {
    if let Prim::ToWord(w) = p {
        return format!("to{}", w.name());
    }
    let shown = format!("{p:?}");
    let mut cs = shown.chars();
    match cs.next() {
        Some(c) => format!("{}{}", c.to_lowercase(), cs.as_str()),
        None => shown,
    }
}

/// The symbol of the type or effect core names `canon`: its package, and
/// then its path. One with no module is the standard library's.
fn type_symbol(canon: &str) -> Symbol {
    let segs: Vec<&str> = canon.split('.').collect();
    let (package, path) = match segs.split_first() {
        Some((first, rest)) if !rest.is_empty() => (*first, rest.to_vec()),
        _ => ("Std", segs.clone()),
    };
    Symbol {
        lang: "meadow".to_string(),
        package: package.to_string(),
        path: path.into_iter().map(str::to_string).collect(),
    }
}

/// A constructor's symbol: its type's, and then its own name.
fn ctor_symbol(canon: &str) -> Symbol {
    match canon.rsplit_once('.') {
        Some((ty, name)) => type_symbol(ty).child(name),
        None => type_symbol(canon),
    }
}

/// A constructor's own name: the last of its path.
fn bare(canon: &str) -> &str {
    canon.rsplit_once('.').map_or(canon, |(_, name)| name)
}
