//! **The reference interpreter**: what a Cut program means.
//!
//! It runs Cut as it is written, by cut elimination: a value given to a
//! `case` picks the arm, to a `μ̃` binds and continues, to a method call
//! enters the method; `<μ k. s | c>` runs `s` with `k` bound to `c`. What it
//! answers, what it prints and what it fails with are what the program does,
//! and the AxCut machine, Glade and Silo must all agree with it.
//!
//! It is a loop over states, never a recursion of the host's: a program that
//! recurses a million deep runs in constant host stack, as Cut's own
//! continuations are values on the heap.
//!
//! **A `μ` not in a cut.** `f(x, μ k. s)` gives `f` the value `s` gives `k`,
//! and so has to run `s` first, with `k` the rest of the call. That is
//! focusing -- `f(x, μ k. s)` is `<μ k. s | μ̃ y. f(x, y)>` -- and the
//! interpreter does it as it goes: evaluating a statement's producers is
//! pure, so on meeting a `μ` it runs the `μ`'s body with a continuation that
//! notes the value and runs the statement again, which this time finds it.
//!
//! **Effects.** A `handle` pushes a frame on the handlers in scope; a
//! `perform` runs the nearest clause for its operation, outside that frame,
//! with the resumption and the frame's continuation. Handlers are deep: the
//! resumption continues the body inside the frame. The frame keeps where its
//! value goes in a cell, which resuming sets to the resumption's own
//! continuation, so the clause that resumed gets the body's answer back -- as
//! Meadow's evidence `target` does. A consumer runs under the handlers that
//! were in scope where it was written; a method or a definition, under its
//! caller's. An operation no handler answers is performed by the runtime the
//! `native` table binds it to.

use crate::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// What a run did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outcome {
    /// What it wrote to standard output, the answer included.
    pub output: String,
    /// What it wrote to standard error.
    pub errors: String,
    /// Its exit status: `0`, or what `Process.exit` was given.
    pub status: i64,
}

/// What the program reads, and how far it may go.
#[derive(Debug, Clone)]
pub struct Options {
    /// The lines `Console.readLine` answers, in order.
    pub input: Vec<String>,
    /// What `Process.argv` answers.
    pub argv: Vec<String>,
    /// Steps before giving up, so that a test of a program that loops ends.
    pub fuel: u64,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            input: Vec::new(),
            argv: Vec::new(),
            fuel: 100_000_000,
        }
    }
}

/// Run `p` from its vals and then its entry. `Err` is a run-time error, with
/// what had been printed before it.
pub fn run(p: &Program, opts: &Options) -> Result<Outcome, (String, Outcome)> {
    let mut m = Machine::new(p, opts.clone());
    match m.main() {
        Ok(()) => Ok(m.outcome),
        Err(e) => Err((e, m.outcome)),
    }
}

// --- values ---------------------------------------------------------------------------

/// A value, as the interpreter holds one.
#[derive(Clone)]
pub enum Value<'p> {
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    Unit,
    Str(Rc<str>),
    Desc(Rep),
    Data(Rc<Data<'p>>),
    Tuple(Rc<Vec<Value<'p>>>),
    Array(Rc<Vec<Value<'p>>>),
    Record(Rc<Vec<(String, Value<'p>)>>),
    Obj(Rc<Obj<'p>>),
    Cont(Rc<Cont<'p>>),
    Resume(Rc<Resumption<'p>>),
}

pub struct Data<'p> {
    pub ctor: &'p Symbol,
    pub fields: Vec<Value<'p>>,
}

/// An object: its methods, and the environment they close over.
pub struct Obj<'p> {
    methods: &'p [Method],
    env: Env<'p>,
}

/// A continuation, as a value.
pub enum Cont<'p> {
    Halt,
    /// A consumer as written, with the names and the handlers in scope there.
    Consumer {
        c: &'p Consumer,
        env: Env<'p>,
        handlers: Handlers<'p>,
    },
    /// A handle's body's own continuation: what it answers goes to `return`.
    Return(Rc<Frame<'p>>),
    /// The rest of a statement whose `μ` is being run: see the module docs.
    Focus {
        at: Focused<'p>,
        hole: usize,
        filled: Rc<HashMap<usize, Value<'p>>>,
        env: Env<'p>,
        handlers: Handlers<'p>,
    },
}

/// What a [`Cont::Focus`] runs again.
#[derive(Clone)]
pub enum Focused<'p> {
    Statement(&'p Statement),
    /// A method call, given the object it was given.
    Method(&'p Consumer, Value<'p>),
}

/// A `perform`'s resumption: the frame it came through, and the
/// continuation the operation's value goes to.
pub struct Resumption<'p> {
    frame: Rc<Frame<'p>>,
    cont: Rc<Cont<'p>>,
}

/// A handler in scope.
pub struct Frame<'p> {
    handle: &'p Handle,
    env: Env<'p>,
    /// The handlers outside this one, which its clauses run under.
    outer: Handlers<'p>,
    /// Where the handle's value goes now.
    target: RefCell<Rc<Cont<'p>>>,
}

pub type Handlers<'p> = Option<Rc<HandlerList<'p>>>;

pub struct HandlerList<'p> {
    frame: Rc<Frame<'p>>,
    next: Handlers<'p>,
}

/// Names and their values: a persistent list, newest first.
pub type Env<'p> = Option<Rc<Binding<'p>>>;

pub struct Binding<'p> {
    name: &'p str,
    value: Value<'p>,
    next: Env<'p>,
}

fn bind<'p>(env: &Env<'p>, name: &'p str, value: Value<'p>) -> Env<'p> {
    Some(Rc::new(Binding {
        name,
        value,
        next: env.clone(),
    }))
}

fn lookup<'p>(env: &Env<'p>, name: &str) -> Option<Value<'p>> {
    let mut at = env;
    while let Some(b) = at {
        if b.name == name {
            return Some(b.value.clone());
        }
        at = &b.next;
    }
    None
}

impl<'p> Value<'p> {
    fn kind(&self) -> &'static str {
        match self {
            Value::Int(_) => "an i64",
            Value::Float(_) => "a float",
            Value::Bool(_) => "a bool",
            Value::Char(_) => "a char",
            Value::Unit => "unit",
            Value::Str(_) => "a str",
            Value::Desc(_) => "a descriptor",
            Value::Data(_) => "data",
            Value::Tuple(_) => "a tuple",
            Value::Array(_) => "an array",
            Value::Record(_) => "a record",
            Value::Obj(_) => "an object",
            Value::Cont(_) => "a continuation",
            Value::Resume(_) => "a resumption",
        }
    }
}

/// Structural equality, as `prim eq` sees it.
fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Char(x), Value::Char(y)) => x == y,
        (Value::Unit, Value::Unit) => true,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Desc(x), Value::Desc(y)) => x == y,
        (Value::Data(x), Value::Data(y)) => {
            x.ctor == y.ctor
                && x.fields.len() == y.fields.len()
                && x.fields.iter().zip(&y.fields).all(|(a, b)| equal(a, b))
        }
        (Value::Tuple(x), Value::Tuple(y)) | (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| equal(a, b))
        }
        (Value::Record(x), Value::Record(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|((l, a), (m, b))| l == m && equal(a, b))
        }
        _ => false,
    }
}

// --- the machine ---------------------------------------------------------------------

enum State<'p> {
    Exec(Exec<'p>),
    Give(Value<'p>, Rc<Cont<'p>>),
    Done,
}

/// A statement to run, where, and what of it is already known.
struct Exec<'p> {
    at: Focused<'p>,
    env: Env<'p>,
    handlers: Handlers<'p>,
    filled: Rc<HashMap<usize, Value<'p>>>,
}

/// Evaluating a producer: its value, or a `μ` that must run first.
enum Eval<'p> {
    Value(Value<'p>),
    Mu(&'p Producer),
}

struct Machine<'p> {
    program: &'p Program,
    defs: HashMap<&'p Symbol, &'p Def>,
    vals: HashMap<&'p Symbol, Value<'p>>,
    natives: HashMap<&'p Symbol, &'p str>,
    roles: HashMap<&'p str, &'p Symbol>,
    opts: Options,
    input: std::collections::VecDeque<String>,
    outcome: Outcome,
    /// What a `halt` received last.
    halted: Option<Value<'p>>,
    steps: u64,
    /// Set by `Process.exit`: the run is over.
    exited: bool,
    /// `Random`'s state: `0` until it is first asked.
    seed: u64,
    /// When the run began, for `Time.monotonic`.
    started: std::time::Instant,
}

type R<T> = Result<T, String>;

fn err<T>(msg: impl Into<String>) -> R<T> {
    Err(msg.into())
}

impl<'p> Machine<'p> {
    fn new(program: &'p Program, opts: Options) -> Machine<'p> {
        Machine {
            program,
            defs: program.defs.iter().map(|d| (&d.symbol, d)).collect(),
            vals: HashMap::new(),
            natives: program
                .natives
                .iter()
                .map(|(op, rt)| (op, rt.as_str()))
                .collect(),
            roles: program.roles.iter().map(|(r, k)| (r.as_str(), k)).collect(),
            input: opts.input.iter().cloned().collect(),
            opts,
            outcome: Outcome::default(),
            halted: None,
            steps: 0,
            exited: false,
            seed: 0,
            started: std::time::Instant::now(),
        }
    }

    fn main(&mut self) -> R<()> {
        for v in &self.program.vals {
            let value = self.run_to_halt(&v.body)?;
            if self.exited {
                return Ok(());
            }
            self.vals.insert(&v.symbol, value);
        }
        let Some(entry) = &self.program.entry else {
            return Ok(());
        };
        let Some(def) = self.defs.get(entry).copied() else {
            return err(format!("the entry `{entry}` is not defined"));
        };
        if !def.params.is_empty() || def.conts.len() != 1 {
            return err(format!(
                "the entry `{entry}` must take only its continuation"
            ));
        }
        let env = bind(&None, &def.conts[0], Value::Cont(Rc::new(Cont::Halt)));
        let answer = self.run(State::Exec(Exec {
            at: Focused::Statement(&def.body),
            env,
            handlers: None,
            filled: Rc::default(),
        }))?;
        if self.exited {
            return Ok(());
        }
        if self.program.answer == Answer::Str {
            match answer {
                Value::Str(s) => self.outcome.output.push_str(&s),
                other => return err(format!("the answer is {}, not a str", other.kind())),
            }
        }
        Ok(())
    }

    /// Run `s` until it gives `halt` a value.
    fn run_to_halt(&mut self, s: &'p Statement) -> R<Value<'p>> {
        self.run(State::Exec(Exec {
            at: Focused::Statement(s),
            env: None,
            handlers: None,
            filled: Rc::default(),
        }))
    }

    fn run(&mut self, mut state: State<'p>) -> R<Value<'p>> {
        self.halted = None;
        loop {
            self.steps += 1;
            if self.steps > self.opts.fuel {
                return err("out of fuel: the program ran too long");
            }
            state = match state {
                State::Exec(e) => self.exec(e)?,
                State::Give(v, k) => self.give(v, k)?,
                State::Done => {
                    return Ok(self.halted.take().unwrap_or(Value::Unit));
                }
            };
        }
    }

    // --- giving a value to a continuation -------------------------------------------------

    fn give(&mut self, v: Value<'p>, k: Rc<Cont<'p>>) -> R<State<'p>> {
        match &*k {
            Cont::Halt => {
                self.halted = Some(v);
                Ok(State::Done)
            }
            Cont::Return(frame) => {
                let (x, kname, body) = &frame.handle.ret;
                let target = frame.target.borrow().clone();
                let env = bind(&frame.env, &x.name, v);
                let env = bind(&env, kname, Value::Cont(target));
                Ok(exec(Focused::Statement(body), env, frame.outer.clone()))
            }
            Cont::Focus {
                at,
                hole,
                filled,
                env,
                handlers,
            } => {
                let mut filled = (**filled).clone();
                filled.insert(*hole, v);
                Ok(State::Exec(Exec {
                    at: at.clone(),
                    env: env.clone(),
                    handlers: handlers.clone(),
                    filled: Rc::new(filled),
                }))
            }
            Cont::Consumer { c, env, handlers } => match c {
                Consumer::MuTilde(x, s) => Ok(exec(
                    Focused::Statement(s),
                    bind(env, &x.name, v),
                    handlers.clone(),
                )),
                Consumer::Case(arms) => self.case(arms, v, env, handlers),
                Consumer::Method(..) => Ok(State::Exec(Exec {
                    at: Focused::Method(c, v),
                    env: env.clone(),
                    handlers: handlers.clone(),
                    filled: Rc::default(),
                })),
                Consumer::Var(_) | Consumer::Halt => {
                    unreachable!("a variable or `halt` is resolved before it is given anything")
                }
            },
        }
    }

    fn case(
        &mut self,
        arms: &'p [Arm],
        v: Value<'p>,
        env: &Env<'p>,
        handlers: &Handlers<'p>,
    ) -> R<State<'p>> {
        let fields: Vec<Value<'p>> = match &v {
            Value::Data(d) => d.fields.clone(),
            Value::Tuple(t) => (**t).clone(),
            _ => Vec::new(),
        };
        for arm in arms {
            let hit = match (&arm.pattern, &v) {
                (Pattern::Con(k), Value::Data(d)) => k == d.ctor,
                (Pattern::Tuple, Value::Tuple(_)) => true,
                (Pattern::Default, _) => true,
                _ => false,
            };
            if !hit {
                continue;
            }
            let mut env = env.clone();
            if arm.pattern != Pattern::Default {
                if arm.fields.len() != fields.len() {
                    return err(format!(
                        "an arm binds {} fields of a value with {}",
                        arm.fields.len(),
                        fields.len()
                    ));
                }
                for (b, f) in arm.fields.iter().zip(fields.iter()) {
                    env = bind(&env, &b.name, f.clone());
                }
            }
            return Ok(exec(Focused::Statement(&arm.body), env, handlers.clone()));
        }
        let what = match &v {
            Value::Data(d) => format!("`{}`", d.ctor),
            other => other.kind().to_string(),
        };
        err(format!("no arm of a case matches {what}"))
    }

    // --- running a statement -----------------------------------------------------------

    fn exec(&mut self, e: Exec<'p>) -> R<State<'p>> {
        let Exec {
            at,
            env,
            handlers,
            filled,
        } = e;
        // A `μ` met while evaluating: run its body, then come back here.
        let suspend = |mu: &'p Producer, at: Focused<'p>| -> State<'p> {
            let Producer::Mu(k, s) = mu else {
                unreachable!("only a μ suspends")
            };
            let back = Cont::Focus {
                at,
                hole: mu as *const Producer as usize,
                filled: filled.clone(),
                env: env.clone(),
                handlers: handlers.clone(),
            };
            exec(
                Focused::Statement(s),
                bind(&env, k, Value::Cont(Rc::new(back))),
                handlers.clone(),
            )
        };
        let s = match at {
            Focused::Method(c, obj) => {
                let Consumer::Method(m, args, conts) = c else {
                    unreachable!("a method is focused on a method call")
                };
                let vals = match self.evals(args, &env, &handlers, &filled)? {
                    Ok(vs) => vs,
                    Err(mu) => return Ok(suspend(mu, Focused::Method(c, obj))),
                };
                let ks = self.conts(conts, &env, &handlers)?;
                return self.invoke(obj, m, vals, ks, handlers);
            }
            // A position does nothing.
            Focused::Statement(s) => s.bare(),
        };
        match s {
            // A `μ` cut against a consumer runs its body with the consumer as `k`.
            Statement::Cut(Producer::Mu(k, body), c) => {
                let k_val = self.cont(c, &env, &handlers)?;
                Ok(exec(
                    Focused::Statement(body),
                    bind(&env, k, Value::Cont(k_val)),
                    handlers,
                ))
            }
            Statement::Cut(p, c) => {
                let v = match self.eval(p, &env, &handlers, &filled)? {
                    Eval::Value(v) => v,
                    Eval::Mu(mu) => return Ok(suspend(mu, Focused::Statement(s))),
                };
                let k = self.cont(c, &env, &handlers)?;
                Ok(State::Give(v, k))
            }
            Statement::Let(x, p, body) => {
                let v = match self.eval(p, &env, &handlers, &filled)? {
                    Eval::Value(v) => v,
                    Eval::Mu(mu) => return Ok(suspend(mu, Focused::Statement(s))),
                };
                Ok(exec(
                    Focused::Statement(body),
                    bind(&env, &x.name, v),
                    handlers,
                ))
            }
            Statement::Call(f, args, conts) => {
                let vals = match self.evals(args, &env, &handlers, &filled)? {
                    Ok(vs) => vs,
                    Err(mu) => return Ok(suspend(mu, Focused::Statement(s))),
                };
                let ks = self.conts(conts, &env, &handlers)?;
                let Some(def) = self.defs.get(f).copied() else {
                    return err(format!("`{f}` is not defined"));
                };
                if def.params.len() != vals.len() || def.conts.len() != ks.len() {
                    return err(format!(
                        "`{f}` takes {} values and {} continuations, and was given {} and {}",
                        def.params.len(),
                        def.conts.len(),
                        vals.len(),
                        ks.len()
                    ));
                }
                let mut env = None;
                for (b, v) in def.params.iter().zip(vals) {
                    env = bind(&env, &b.name, v);
                }
                for (k, v) in def.conts.iter().zip(ks) {
                    env = bind(&env, k, Value::Cont(v));
                }
                Ok(exec(Focused::Statement(&def.body), env, handlers))
            }
            Statement::Prim(op, args, conts) => {
                let vals = match self.evals(args, &env, &handlers, &filled)? {
                    Ok(vs) => vs,
                    Err(mu) => return Ok(suspend(mu, Focused::Statement(s))),
                };
                let ks = self.conts(conts, &env, &handlers)?;
                self.prim(op, vals, ks)
            }
            Statement::Perform(op, args, c) => {
                let vals = match self.evals(args, &env, &handlers, &filled)? {
                    Ok(vs) => vs,
                    Err(mu) => return Ok(suspend(mu, Focused::Statement(s))),
                };
                let k = self.cont(c, &env, &handlers)?;
                self.perform(op, vals, k, handlers)
            }
            Statement::Handle(h) => {
                let target = self.cont(&h.cont, &env, &handlers)?;
                let frame = Rc::new(Frame {
                    handle: h,
                    env: env.clone(),
                    outer: handlers.clone(),
                    target: RefCell::new(target),
                });
                let inside = Some(Rc::new(HandlerList {
                    frame: frame.clone(),
                    next: handlers,
                }));
                let b = Value::Cont(Rc::new(Cont::Return(frame)));
                Ok(exec(
                    Focused::Statement(&h.body),
                    bind(&env, &h.body_cont, b),
                    inside,
                ))
            }
            Statement::Error(msg) => err(msg.clone()),
            Statement::At(..) => unreachable!("a position is looked under"),
        }
    }

    fn invoke(
        &mut self,
        obj: Value<'p>,
        m: &str,
        vals: Vec<Value<'p>>,
        ks: Vec<Rc<Cont<'p>>>,
        handlers: Handlers<'p>,
    ) -> R<State<'p>> {
        match obj {
            Value::Obj(o) => {
                let Some(method) = o.methods.iter().find(|x| x.name == m) else {
                    return err(format!("the object has no method `{m}`"));
                };
                if method.params.len() != vals.len() || method.conts.len() != ks.len() {
                    return err(format!(
                        "`{m}` takes {} values and {} continuations, and was given {} and {}",
                        method.params.len(),
                        method.conts.len(),
                        vals.len(),
                        ks.len()
                    ));
                }
                let mut env = o.env.clone();
                for (b, v) in method.params.iter().zip(vals) {
                    env = bind(&env, &b.name, v);
                }
                for (k, v) in method.conts.iter().zip(ks) {
                    env = bind(&env, k, Value::Cont(v));
                }
                Ok(exec(Focused::Statement(&method.body), env, handlers))
            }
            Value::Resume(r) => {
                // A function's method, since a resumption goes wherever a
                // function can; `resume` names the same method.
                if !matches!(m, "apply" | "resume") || vals.len() != 1 || ks.len() != 1 {
                    return err("a resumption is a function: `apply(v; k)`");
                }
                let (v, k) = (vals.into_iter().next(), ks.into_iter().next());
                *r.frame.target.borrow_mut() = k.expect("one continuation");
                Ok(State::Give(v.expect("one value"), r.cont.clone()))
            }
            other => err(format!("`{m}` called on {}, not an object", other.kind())),
        }
    }

    fn perform(
        &mut self,
        op: &'p Symbol,
        vals: Vec<Value<'p>>,
        k: Rc<Cont<'p>>,
        handlers: Handlers<'p>,
    ) -> R<State<'p>> {
        let mut at = &handlers;
        while let Some(h) = at {
            if let Some(clause) = h.frame.handle.clauses.iter().find(|c| &c.op == op) {
                if clause.params.len() != vals.len() {
                    return err(format!(
                        "`{op}` takes {} arguments, and was given {}",
                        clause.params.len(),
                        vals.len()
                    ));
                }
                let frame = h.frame.clone();
                let resumption = Value::Resume(Rc::new(Resumption {
                    frame: frame.clone(),
                    cont: k,
                }));
                let target = frame.target.borrow().clone();
                let mut env = frame.env.clone();
                for (b, v) in clause.params.iter().zip(vals) {
                    env = bind(&env, &b.name, v);
                }
                env = bind(&env, &clause.resumption, resumption);
                env = bind(&env, &clause.cont, Value::Cont(target));
                return Ok(exec(
                    Focused::Statement(&clause.body),
                    env,
                    frame.outer.clone(),
                ));
            }
            at = &h.next;
        }
        match self.natives.get(op).copied() {
            Some(native) => self.native(native, op, vals, k),
            None => err(format!(
                "`{op}` is performed, but no handler answers it and the native table does not bind it"
            )),
        }
    }

    // --- evaluating ---------------------------------------------------------------------

    fn cont(&mut self, c: &'p Consumer, env: &Env<'p>, handlers: &Handlers<'p>) -> R<Rc<Cont<'p>>> {
        Ok(match c {
            Consumer::Var(k) => match lookup(env, k) {
                Some(Value::Cont(c)) => c,
                Some(other) => {
                    return err(format!("`{k}` is {}, not a continuation", other.kind()));
                }
                None => return err(format!("`{k}` is not bound")),
            },
            Consumer::Halt => Rc::new(Cont::Halt),
            _ => Rc::new(Cont::Consumer {
                c,
                env: env.clone(),
                handlers: handlers.clone(),
            }),
        })
    }

    fn conts(
        &mut self,
        cs: &'p [Consumer],
        env: &Env<'p>,
        handlers: &Handlers<'p>,
    ) -> R<Vec<Rc<Cont<'p>>>> {
        cs.iter().map(|c| self.cont(c, env, handlers)).collect()
    }

    /// Every one of `ps`, or the first `μ` among them still to run.
    fn evals(
        &mut self,
        ps: &'p [Producer],
        env: &Env<'p>,
        handlers: &Handlers<'p>,
        filled: &HashMap<usize, Value<'p>>,
    ) -> R<Result<Vec<Value<'p>>, &'p Producer>> {
        let mut out = Vec::with_capacity(ps.len());
        for p in ps {
            match self.eval(p, env, handlers, filled)? {
                Eval::Value(v) => out.push(v),
                Eval::Mu(mu) => return Ok(Err(mu)),
            }
        }
        Ok(Ok(out))
    }

    fn eval(
        &mut self,
        p: &'p Producer,
        env: &Env<'p>,
        handlers: &Handlers<'p>,
        filled: &HashMap<usize, Value<'p>>,
    ) -> R<Eval<'p>> {
        let all = |m: &mut Self, ps: &'p [Producer]| m.evals(ps, env, handlers, filled);
        Ok(Eval::Value(match p {
            Producer::Var(x) => match lookup(env, x) {
                Some(v) => v,
                None => return err(format!("`{x}` is not bound")),
            },
            Producer::Val(s) => match self.vals.get(s) {
                Some(v) => v.clone(),
                None => return err(format!("`{s}` is read before it is computed, or is no val")),
            },
            Producer::Int(n) => Value::Int(*n),
            Producer::Float(x) => Value::Float(*x),
            Producer::Char(c) => Value::Char(*c),
            Producer::Str(s) => Value::Str(Rc::from(s.as_str())),
            Producer::Bool(b) => Value::Bool(*b),
            Producer::Unit => Value::Unit,
            Producer::Desc(r) => Value::Desc(r.clone()),
            Producer::Con(k, args) => match all(self, args)? {
                Ok(fields) => Value::Data(Rc::new(Data { ctor: k, fields })),
                Err(mu) => return Ok(Eval::Mu(mu)),
            },
            Producer::Tuple(args) => match all(self, args)? {
                Ok(vs) => Value::Tuple(Rc::new(vs)),
                Err(mu) => return Ok(Eval::Mu(mu)),
            },
            Producer::Array(args) => match all(self, args)? {
                Ok(vs) => Value::Array(Rc::new(vs)),
                Err(mu) => return Ok(Eval::Mu(mu)),
            },
            Producer::Record(fields) => {
                let mut out = Vec::new();
                for (l, p) in fields {
                    match self.eval(p, env, handlers, filled)? {
                        Eval::Value(v) => out.push((l.clone(), v)),
                        Eval::Mu(mu) => return Ok(Eval::Mu(mu)),
                    }
                }
                out.sort_by(|a, b| a.0.cmp(&b.0));
                Value::Record(Rc::new(out))
            }
            Producer::Cocase(methods) => Value::Obj(Rc::new(Obj {
                methods,
                env: env.clone(),
            })),
            Producer::Mu(..) => match filled.get(&(p as *const Producer as usize)) {
                Some(v) => v.clone(),
                None => return Ok(Eval::Mu(p)),
            },
        }))
    }

    // --- primitives ------------------------------------------------------------------------

    /// Give `v` to the one continuation a primitive takes.
    fn one(&self, op: &str, v: Value<'p>, ks: Vec<Rc<Cont<'p>>>) -> R<State<'p>> {
        match <[_; 1]>::try_from(ks) {
            Ok([k]) => Ok(State::Give(v, k)),
            Err(ks) => err(format!(
                "`{op}` takes one continuation, and was given {}",
                ks.len()
            )),
        }
    }

    /// A test: its answer as a bool to one continuation, or `unit` to the
    /// first of two if false and the second if true.
    fn test(&self, op: &str, b: bool, ks: Vec<Rc<Cont<'p>>>) -> R<State<'p>> {
        match ks.len() {
            1 => self.one(op, Value::Bool(b), ks),
            2 => {
                let k = ks.into_iter().nth(usize::from(b)).expect("two");
                Ok(State::Give(Value::Unit, k))
            }
            n => err(format!(
                "`{op}` takes one continuation or two, and was given {n}"
            )),
        }
    }

    fn prim(&mut self, op: &str, args: Vec<Value<'p>>, ks: Vec<Rc<Cont<'p>>>) -> R<State<'p>> {
        let arity = |n: usize| -> R<()> {
            if args.len() == n {
                Ok(())
            } else {
                err(format!(
                    "`{op}` takes {n} arguments, and was given {}",
                    args.len()
                ))
            }
        };
        let int = |v: &Value| match v {
            Value::Int(n) => Ok(*n),
            other => err(format!("`{op}` expects an i64, got {}", other.kind())),
        };
        let index = |v: &Value| match v {
            Value::Int(n) => Ok(*n),
            other => err(format!("`{op}` expects an index, got {}", other.kind())),
        };
        let array = |v: &Value<'p>| match v {
            Value::Array(a) => Ok(a.clone()),
            other => err(format!("`{op}` expects an array, got {}", other.kind())),
        };
        let string = |v: &Value| match v {
            Value::Str(s) => Ok(s.clone()),
            other => err(format!("`{op}` expects a str, got {}", other.kind())),
        };
        match op {
            "add" | "sub" | "mul" | "div" | "mod" => {
                arity(2)?;
                let v = match (&args[0], &args[1]) {
                    (Value::Int(x), Value::Int(y)) => Value::Int(match op {
                        "add" => x.wrapping_add(*y),
                        "sub" => x.wrapping_sub(*y),
                        "mul" => x.wrapping_mul(*y),
                        "div" if *y == 0 => return err("division by zero"),
                        "div" => x.wrapping_div(*y),
                        _ if *y == 0 => return err("modulo by zero"),
                        _ => x.wrapping_rem(*y),
                    }),
                    (Value::Float(x), Value::Float(y)) => Value::Float(match op {
                        "add" => x + y,
                        "sub" => x - y,
                        "mul" => x * y,
                        "div" => x / y,
                        _ => x % y,
                    }),
                    (a, b) => {
                        return err(format!(
                            "`{op}` expects two numbers of one kind, got {} and {}",
                            a.kind(),
                            b.kind()
                        ));
                    }
                };
                self.one(op, v, ks)
            }
            "neg" => {
                arity(1)?;
                let v = match &args[0] {
                    Value::Int(x) => Value::Int(x.wrapping_neg()),
                    Value::Float(x) => Value::Float(-x),
                    other => return err(format!("`neg` expects a number, got {}", other.kind())),
                };
                self.one(op, v, ks)
            }
            "eq" | "ne" => {
                arity(2)?;
                let same = equal(&args[0], &args[1]);
                self.test(op, if op == "eq" { same } else { !same }, ks)
            }
            "lt" | "le" | "gt" | "ge" => {
                arity(2)?;
                let ord = match (&args[0], &args[1]) {
                    (Value::Int(x), Value::Int(y)) => x.cmp(y),
                    (Value::Char(x), Value::Char(y)) => x.cmp(y),
                    (Value::Str(x), Value::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
                    (Value::Float(x), Value::Float(y)) => match x.partial_cmp(y) {
                        Some(o) => o,
                        None => return self.test(op, false, ks),
                    },
                    (a, b) => {
                        return err(format!(
                            "`{op}` expects two values of one ordered kind, got {} and {}",
                            a.kind(),
                            b.kind()
                        ));
                    }
                };
                use std::cmp::Ordering::*;
                let b = match op {
                    "lt" => ord == Less,
                    "le" => ord != Greater,
                    "gt" => ord == Greater,
                    _ => ord != Less,
                };
                self.test(op, b, ks)
            }
            "if" => {
                arity(1)?;
                match &args[0] {
                    Value::Bool(b) => self.test(op, *b, ks),
                    other => err(format!("`if` expects a bool, got {}", other.kind())),
                }
            }
            "concatStrings" => {
                arity(1)?;
                let mut out = String::new();
                for s in array(&args[0])?.iter() {
                    out.push_str(&string(s)?);
                }
                self.one(op, Value::Str(Rc::from(out)), ks)
            }
            "stringByteLength" => {
                arity(1)?;
                let n = string(&args[0])?.len() as i64;
                self.one(op, Value::Int(n), ks)
            }
            // The byte at an index, as an i64: Glade's answers a `u8`, which
            // the lowering widens.
            "stringByteAt" => {
                arity(2)?;
                let s = string(&args[0])?;
                let i = index(&args[1])?;
                match usize::try_from(i).ok().and_then(|at| s.as_bytes().get(at)) {
                    Some(b) => self.one(op, Value::Int(i64::from(*b)), ks),
                    None => err(format!(
                        "stringByteAt: index {i} out of bounds (len {})",
                        s.len()
                    )),
                }
            }
            "stringSlice" => {
                arity(3)?;
                let s = string(&args[0])?;
                let n = s.len() as i64;
                let from = index(&args[1])?.clamp(0, n) as usize;
                let to = (index(&args[2])?.clamp(from as i64, n)) as usize;
                let slice = s.get(from..to).ok_or("`stringSlice` cuts a character")?;
                self.one(op, Value::Str(Rc::from(slice)), ks)
            }
            "stringCompare" => {
                arity(2)?;
                let (a, b) = (string(&args[0])?, string(&args[1])?);
                let c = a.as_bytes().cmp(b.as_bytes()) as i64;
                self.one(op, Value::Int(c), ks)
            }
            // A record's field, by its label -- a literal, a name and not a
            // value -- read, and set or added.
            "select" => {
                arity(2)?;
                let label = string(&args[1])?;
                match &args[0] {
                    Value::Record(fields) => match fields.iter().find(|(l, _)| **l == *label) {
                        Some((_, v)) => self.one(op, v.clone(), ks),
                        None => err(format!("the record has no field `{label}`")),
                    },
                    other => err(format!("`select` expects a record, got {}", other.kind())),
                }
            }
            "extend" => {
                arity(3)?;
                let label = string(&args[1])?;
                match &args[0] {
                    Value::Record(fields) => {
                        let mut out: Vec<(String, Value<'p>)> = fields
                            .iter()
                            .filter(|(l, _)| **l != *label)
                            .cloned()
                            .collect();
                        out.push((label.to_string(), args[2].clone()));
                        out.sort_by(|a, b| a.0.cmp(&b.0));
                        self.one(op, Value::Record(Rc::new(out)), ks)
                    }
                    other => err(format!("`extend` expects a record, got {}", other.kind())),
                }
            }
            "arrayLen" => {
                arity(1)?;
                let n = array(&args[0])?.len() as i64;
                self.one(op, Value::Int(n), ks)
            }
            "arrayGet" => {
                arity(2)?;
                let a = array(&args[0])?;
                let i = index(&args[1])?;
                match usize::try_from(i).ok().and_then(|i| a.get(i)) {
                    Some(v) => self.one(op, v.clone(), ks),
                    None => err(format!(
                        "arrayGet: index {i} out of bounds (len {})",
                        a.len()
                    )),
                }
            }
            "arrayGetOr" => {
                arity(3)?;
                let a = array(&args[1])?;
                let i = index(&args[2])?;
                let v = usize::try_from(i)
                    .ok()
                    .and_then(|i| a.get(i).cloned())
                    .unwrap_or_else(|| args[0].clone());
                self.one(op, v, ks)
            }
            "arraySet" => {
                arity(3)?;
                let mut a = (*array(&args[0])?).clone();
                let i = index(&args[1])?;
                let Some(slot) = usize::try_from(i).ok().and_then(|i| a.get_mut(i)) else {
                    return err(format!("arraySet: index {i} out of bounds"));
                };
                *slot = args[2].clone();
                self.one(op, Value::Array(Rc::new(a)), ks)
            }
            "arrayPush" => {
                arity(2)?;
                let mut a = (*array(&args[0])?).clone();
                a.push(args[1].clone());
                self.one(op, Value::Array(Rc::new(a)), ks)
            }
            "arrayPop" => {
                arity(1)?;
                let mut a = (*array(&args[0])?).clone();
                if a.pop().is_none() {
                    return err("arrayPop: empty array");
                }
                self.one(op, Value::Array(Rc::new(a)), ks)
            }
            "arraySlice" => {
                arity(3)?;
                let a = array(&args[0])?;
                let n = a.len() as i64;
                let from = int(&args[1])?.clamp(0, n) as usize;
                let to = int(&args[2])?.clamp(from as i64, n) as usize;
                self.one(op, Value::Array(Rc::new(a[from..to].to_vec())), ks)
            }
            "arrayConcat" => {
                arity(2)?;
                let mut a = (*array(&args[0])?).clone();
                a.extend(array(&args[1])?.iter().cloned());
                self.one(op, Value::Array(Rc::new(a)), ks)
            }
            other => err(format!(
                "`{other}` is not a primitive this interpreter knows"
            )),
        }
    }

    // --- the runtime's operations ------------------------------------------------------------

    /// The data playing `role`, with `fields`.
    fn role(&self, role: &str, fields: Vec<Value<'p>>) -> R<Value<'p>> {
        match self.roles.get(role) {
            Some(k) => Ok(Value::Data(Rc::new(Data { ctor: k, fields }))),
            None => err(format!(
                "the runtime would build a `{role}`, and the program declares no constructor for it"
            )),
        }
    }

    /// `Ok v`, as the program's `ok` role builds it.
    fn ok(&self, v: Value<'p>) -> R<Value<'p>> {
        self.role("ok", vec![v])
    }

    /// `Err why` for a failed operation on the world.
    fn io_error(&self, e: std::io::Error) -> R<Value<'p>> {
        self.role("err", vec![Value::Str(Rc::from(e.to_string()))])
    }

    fn io_unit(&self, r: std::io::Result<()>) -> R<Value<'p>> {
        match r {
            Ok(()) => self.ok(Value::Unit),
            Err(e) => self.io_error(e),
        }
    }

    /// The two values an operation takes: given as two, as a front end
    /// performs it, or as the tuple the runtime takes.
    fn pair(&self, native: &str, vals: &[Value<'p>]) -> R<[Value<'p>; 2]> {
        match vals {
            [a, b] => Ok([a.clone(), b.clone()]),
            [Value::Tuple(t)] if t.len() == 2 => Ok([t[0].clone(), t[1].clone()]),
            _ => err(format!(
                "`{native}` takes two values; it was given {}",
                vals.len()
            )),
        }
    }

    /// The next of a SplitMix64 sequence, seeded from the clock once.
    fn random(&mut self) -> u64 {
        if self.seed == 0 {
            self.seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64)
                | 1;
        }
        self.seed = self.seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A sequence the runtime makes: a plain array, unless the program
    /// declares the vector roles, which this interpreter does not build yet.
    fn sequence(&self, items: Vec<Value<'p>>) -> R<Value<'p>> {
        if self.roles.contains_key("vector-empty") {
            return err("a vector the runtime builds is not in this interpreter yet");
        }
        Ok(Value::Array(Rc::new(items)))
    }

    fn native(
        &mut self,
        native: &'p str,
        op: &Symbol,
        vals: Vec<Value<'p>>,
        k: Rc<Cont<'p>>,
    ) -> R<State<'p>> {
        let text = |v: &Value| match v {
            Value::Str(s) => Ok(s.clone()),
            other => err(format!("`{native}` expects a str, got {}", other.kind())),
        };
        let one = |vals: &[Value<'p>]| match vals {
            [v] => Ok(v.clone()),
            _ => err(format!(
                "`{op}` is bound to `{native}`, which takes one value; it was given {}",
                vals.len()
            )),
        };
        let answer = match native {
            "Console.writeOutput" => {
                let s = text(&one(&vals)?)?;
                self.outcome.output.push_str(&s);
                Value::Unit
            }
            "Console.writeError" => {
                let s = text(&one(&vals)?)?;
                self.outcome.errors.push_str(&s);
                Value::Unit
            }
            "Console.readLine" => match self.input.pop_front() {
                Some(line) => self.role("some", vec![Value::Str(Rc::from(line))])?,
                None => self.role("none", vec![])?,
            },
            "Process.exit" => {
                match one(&vals)? {
                    Value::Int(n) => self.outcome.status = n,
                    other => {
                        return err(format!(
                            "`Process.exit` expects an i64, got {}",
                            other.kind()
                        ));
                    }
                }
                self.exited = true;
                return Ok(State::Done);
            }
            "Process.argv" => {
                let items = self
                    .opts
                    .argv
                    .iter()
                    .map(|a| Value::Str(Rc::from(a.as_str())))
                    .collect();
                self.sequence(items)?
            }
            "Fs.readToString" => {
                let path = text(&one(&vals)?)?;
                match std::fs::read_to_string(&*path) {
                    Ok(s) => self.ok(Value::Str(Rc::from(s)))?,
                    Err(e) => self.io_error(e)?,
                }
            }
            "Fs.writeString" | "Fs.appendString" => {
                let [path, contents] = self.pair(native, &vals)?;
                let (path, contents) = (text(&path)?, text(&contents)?);
                let r = if native == "Fs.writeString" {
                    std::fs::write(&*path, contents.as_bytes())
                } else {
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&*path)
                        .and_then(|mut f| f.write_all(contents.as_bytes()))
                };
                self.io_unit(r)?
            }
            "Fs.removeFile" | "Fs.createDir" | "Fs.createDirAll" | "Fs.removeDir"
            | "Fs.removeDirAll" => {
                let path = text(&one(&vals)?)?;
                let r = match native {
                    "Fs.removeFile" => std::fs::remove_file(&*path),
                    "Fs.createDir" => std::fs::create_dir(&*path),
                    "Fs.createDirAll" => std::fs::create_dir_all(&*path),
                    "Fs.removeDir" => std::fs::remove_dir(&*path),
                    _ => std::fs::remove_dir_all(&*path),
                };
                self.io_unit(r)?
            }
            "Fs.rename" => {
                let [from, to] = self.pair(native, &vals)?;
                let r = std::fs::rename(&*text(&from)?, &*text(&to)?);
                self.io_unit(r)?
            }
            "Fs.copy" => {
                let [from, to] = self.pair(native, &vals)?;
                match std::fs::copy(&*text(&from)?, &*text(&to)?) {
                    Ok(n) => self.ok(Value::Int(n as i64))?,
                    Err(e) => self.io_error(e)?,
                }
            }
            "Fs.readDir" => {
                let path = text(&one(&vals)?)?;
                match std::fs::read_dir(&*path) {
                    Ok(entries) => {
                        let mut names = Vec::new();
                        let mut failed = None;
                        for e in entries {
                            match e {
                                Ok(e) => names.push(Value::Str(Rc::from(
                                    e.file_name().to_string_lossy().as_ref(),
                                ))),
                                Err(e) => {
                                    failed = Some(e);
                                    break;
                                }
                            }
                        }
                        match failed {
                            Some(e) => self.io_error(e)?,
                            None => {
                                let seq = self.sequence(names)?;
                                self.ok(seq)?
                            }
                        }
                    }
                    Err(e) => self.io_error(e)?,
                }
            }
            "Fs.exists" | "Fs.isFile" | "Fs.isDir" => {
                let path = text(&one(&vals)?)?;
                let p = std::path::Path::new(&*path);
                Value::Bool(match native {
                    "Fs.exists" => p.exists(),
                    "Fs.isFile" => p.is_file(),
                    _ => p.is_dir(),
                })
            }
            "Process.currentPid" => Value::Int(i64::from(std::process::id())),
            "Process.currentDir" => Value::Str(Rc::from(meadow_rt::args::current_dir())),
            "Process.getEnv" => {
                let name = text(&one(&vals)?)?;
                match std::env::var(&*name) {
                    Ok(v) => self.role("some", vec![Value::Str(Rc::from(v))])?,
                    Err(_) => self.role("none", vec![])?,
                }
            }
            "Process.setEnv" => {
                let [name, value] = self.pair(native, &vals)?;
                let (name, value) = (text(&name)?, text(&value)?);
                // Safety: the interpreter runs one program on one thread.
                unsafe { std::env::set_var(&*name, &*value) };
                Value::Unit
            }
            "Process.removeEnv" => {
                let name = text(&one(&vals)?)?;
                // Safety: as above.
                unsafe { std::env::remove_var(&*name) };
                Value::Unit
            }
            "Process.isTerminal" => {
                use std::io::IsTerminal;
                Value::Bool(match one(&vals)? {
                    Value::Int(0) => std::io::stdin().is_terminal(),
                    Value::Int(1) => std::io::stdout().is_terminal(),
                    Value::Int(2) => std::io::stderr().is_terminal(),
                    _ => false,
                })
            }
            "Random.nextInt" | "Random.nextSeed" => Value::Int(self.random() as i64),
            "Random.nextFloat" => Value::Float((self.random() >> 11) as f64 / (1u64 << 53) as f64),
            "Random.intBetween" => {
                let [lo, hi] = self.pair(native, &vals)?;
                match (lo, hi) {
                    (Value::Int(lo), Value::Int(hi)) if hi <= lo => Value::Int(lo),
                    (Value::Int(lo), Value::Int(hi)) => {
                        let span = hi.wrapping_sub(lo) as u64;
                        Value::Int(lo.wrapping_add((self.random() % span) as i64))
                    }
                    _ => return err("`Random.intBetween` expects two i64s"),
                }
            }
            "Time.now" => Value::Int(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64),
            ),
            "Time.monotonic" => Value::Int(self.started.elapsed().as_nanos() as i64),
            "Time.sleep" => {
                match one(&vals)? {
                    Value::Int(ms) if ms > 0 => {
                        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                    }
                    Value::Int(_) => {}
                    other => {
                        return err(format!("`Time.sleep` expects an i64, got {}", other.kind()));
                    }
                }
                Value::Unit
            }
            "Test.fail" => {
                let s = text(&one(&vals)?)?;
                return err(s.to_string());
            }
            other => {
                return err(format!(
                    "`{other}` is not a runtime operation this interpreter performs"
                ));
            }
        };
        Ok(State::Give(answer, k))
    }
}

/// A statement to run, with nothing of it known yet.
fn exec<'p>(at: Focused<'p>, env: Env<'p>, handlers: Handlers<'p>) -> State<'p> {
    State::Exec(Exec {
        at,
        env,
        handlers,
        filled: Rc::default(),
    })
}
