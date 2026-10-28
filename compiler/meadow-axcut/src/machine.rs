//! The AxCut abstract machine — **the IR's reference semantics**.
//!
//! A transcription of `Semantics.idr` from the OOPSLA 2025 artifact: a
//! statement, a program, and an environment, and seven transitions between them
//! — plus three more for effects, which AxCut does not have.
//!
//! It lives beside the IR rather than in the runtime, because it is not a
//! runtime. `meadow_glade` executes bytecode and knows nothing about sequents; the
//! whole point of `meadow_codegen` is that everything in this file is compiled
//! away. What this is for:
//!
//! * **Pinning down what the IR means** before a code generator gets an opinion
//!   about it. When the VM and this disagree, one of them is wrong and there is
//!   a small enough thing to read to find out which.
//! * **Checking the lowering on its own.** [`crate::lower`] has to get the shape
//!   of the environment exactly right at every program point — see its module
//!   docs — and running the result against the CEK machine tests precisely
//!   that, with no register allocation in the way.
//!
//! # The environment is the whole story
//!
//! There is no stack of frames and no notion of "the current registers" beyond a
//! single ordered list of values. Every transition rebuilds it:
//!
//! ```text
//!   substitute [x, y]   env := [lookup x, lookup y]      (and nothing else)
//!   jump L              env unchanged, renamed by L's parameters
//!   let x = K(a, b)     env := K(a, b) :: env
//!   switch x            env := fields(x) ++ env
//!   new f {…}           env := f :: env
//!   invoke f#i          env := captures(f) ++ (env without f)
//!   extern p(…)         env := result :: env    (a branch leaves env alone)
//! ```
//!
//! A block's parameters name *the entire environment* on entry, not just what is
//! new — so every transition here checks that the length matches, and a mismatch
//! is a bug in the lowering rather than in the program. That check is the point:
//! it is what turns "the register allocation is implicit in the IR" from a claim
//! into something enforced.
//!
//! # Effects
//!
//! Nothing here knows about them. The lowering has turned `handle` and
//! `perform` into evidence passing -- ordinary objects, data and jumps -- so
//! the only effect this machine meets is an [`Extern::Native`], an operation no
//! handler in the program answers.
//!
//! # Divergences from the CEK machine
//!
//! Three, all recorded here rather than hidden:
//!
//! * **`let` and `new` do not consume.** The paper's environment is linear and
//!   both build from a prefix which they then drop. Here they prepend and leave
//!   the rest alone. `substitute` is still the only shrinking operation, and the
//!   lowering emits one at every call and return, so environments stay bounded.
//! * **A definition is re-evaluated at every reference.** The lowering turns a
//!   global — and a `letrec` binding — into a `jump`, which is what makes
//!   recursion need no back-patching; the CEK instead evaluates each definition
//!   once at load. For a right-hand side that is a lambda or a literal the two
//!   agree. For one that performs an effect they do not, and the fix is a
//!   memoising thunk per definition.
//! * **No native effects.** An unhandled `Fs`, `Process`, `Random` or `Time`
//!   operation is an error here; the CEK discharges it against the real world.
//!   `Test.fail` is the exception, because a test runner needs it.

use crate::{Block, Extern, Name, Program, Rep, Role, Statement, Tag};
use meadow_intern::InternedString;
use meadow_rt::num::{self, Arith, Bits, Cmp, IntTarget, Num, Width};
use meadow_rt::roles::TUPLE;
use meadow_rt::{Lit, Prim};
use num_bigint::BigInt;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

/// A value in the machine.
///
/// The paper's `Number` / `Product` / `Conduct`, with the scalars widened to
/// Meadow's literal types and three extras: the builtin `Array` and record,
/// which are not data in the constructor sense, and the resumption a `perform`
/// hands its handler.
#[derive(Debug, Clone)]
pub enum Value<'p> {
    Int(i64),
    BigInt(Rc<BigInt>),
    Float(f64),
    /// A sized integer: its width, and its bits masked to it.
    Word(Width, u64),
    Float32(f32),
    Bool(bool),
    Str(InternedString),
    Char(char),
    Unit,
    /// Data: a constructor and its fields. Tuples are the constructor `#tuple`.
    Data(InternedString, Tag, Rc<Fields<'p>>),
    /// The builtin `Array` — the only primitive collection.
    Array(Rc<Vec<Value<'p>>>),
    Record(Rc<BTreeMap<InternedString, Value<'p>>>),
    /// A mutable cell — a value with identity.
    Ref(Rc<RefCell<Value<'p>>>),
    /// A mutable array, from `stNewArray` — the other value with identity.
    MutArray(Rc<RefCell<Vec<Value<'p>>>>),
    /// Codata: captured values and a method table, which is a slice of the
    /// program rather than a copy of it.
    Obj(Rc<Object<'p>>),
    /// The continuation the entry point is handed. Invoking it stops the machine.
    Halt,
    /// A value in a compact region, and the region. Nothing moves here -- the
    /// value is shared as it is -- so a region is only the bookkeeping that makes
    /// `compactSize` agree with the VM's.
    Compact(CompactCell<'p>),
}

/// A `Compact`: the value, and the region it was put in.
pub type CompactCell<'p> = Rc<(Value<'p>, Rc<RefCell<Region<'p>>>)>;

/// What a compact region holds, as far as sizing it goes: every value put in
/// (which keeps each node alive, so an address counted is never reused), the
/// nodes already counted, and the VM slots they add up to.
#[derive(Debug, Default)]
pub struct Region<'p> {
    roots: Vec<Value<'p>>,
    seen: std::collections::HashSet<*const ()>,
    slots: usize,
}

/// A data value's fields.
///
/// A newtype so [`Drop`] can be iterative. A `Cons` chain is as deep as it is
/// long, and the derived drop glue recurses once per element — which is what
/// aborted the CEK machine at a few thousand elements before it was fixed. The
/// same shape appears here for the same reason.
///
/// And so that [`Prim::SetField`] can fill a field in place: tail recursion
/// modulo cons builds a cell with a placeholder and writes the field once the
/// recursion has an answer for it. Nothing reads a cell while that happens --
/// it is reachable only from the chain of calls filling it -- so no reference
/// into these fields is ever alive across the write.
pub struct Fields<'p>(std::cell::UnsafeCell<Vec<Value<'p>>>);

impl<'p> Fields<'p> {
    pub fn new(fields: Vec<Value<'p>>) -> Fields<'p> {
        Fields(std::cell::UnsafeCell::new(fields))
    }

    /// Replace field `i`. See the type's documentation for why this is sound:
    /// the machine holds values, not references into them, between steps.
    fn set(&self, i: usize, v: Value<'p>) -> Option<()> {
        // SAFETY: no `&Vec` from `deref` outlives the step that made it, and
        // this runs in a step of its own, on a cell only its builder can see.
        let fields = unsafe { &mut *self.0.get() };
        *fields.get_mut(i)? = v;
        Some(())
    }
}

impl Clone for Fields<'_> {
    fn clone(&self) -> Self {
        Fields::new((**self).clone())
    }
}

impl std::fmt::Debug for Fields<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Fields").field(&**self).finish()
    }
}

impl<'p> std::ops::Deref for Fields<'p> {
    type Target = Vec<Value<'p>>;
    fn deref(&self) -> &Vec<Value<'p>> {
        // SAFETY: see `set`, the only writer.
        unsafe { &*self.0.get() }
    }
}

impl Drop for Fields<'_> {
    fn drop(&mut self) {
        // Dismantle with a worklist: move each child out, and if this was the
        // last reference to another node, move its children onto the same list
        // rather than letting the drop nest. Arrays and records join in because
        // a `Vector` is a tree of arrays of data, so a deep value need not be
        // deep in constructors alone.
        let mut stack: Vec<Value> = std::mem::take(self.0.get_mut());
        while let Some(v) = stack.pop() {
            match v {
                Value::Data(_, _, rc) => {
                    if let Ok(mut fields) = Rc::try_unwrap(rc) {
                        stack.append(fields.0.get_mut());
                    }
                }
                Value::Array(rc) => {
                    if let Ok(mut xs) = Rc::try_unwrap(rc) {
                        stack.append(&mut xs);
                    }
                }
                Value::Record(rc) => {
                    if let Ok(map) = Rc::try_unwrap(rc) {
                        stack.extend(map.into_values());
                    }
                }
                _ => {}
            }
        }
    }
}

/// Codata: a closure, a continuation or a handler. The paper makes no
/// distinction between them and neither does this.
#[derive(Debug)]
pub struct Object<'p> {
    pub captures: Vec<Value<'p>>,
    pub methods: &'p [Block],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub msg: String,
}

fn err<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error { msg: msg.into() })
}

/// The machine: where we are, and what is in scope.
pub struct Machine<'p> {
    program: &'p Program,
    stmt: &'p Statement,
    /// Names for the environment's slots — the parameters of the block we are
    /// inside. The paper's environment is purely positional; keeping the names
    /// is what lets a statement say `invoke f#0` instead of counting.
    names: Vec<Name>,
    env: Vec<Value<'p>>,
    /// Top-level values evaluated so far, by definition -- see
    /// `meadow_core::globals`.
    globals: Vec<Option<Value<'p>>>,
    /// Transitions taken, for the fuel limit and for reporting.
    pub steps: u64,
}

impl<'p> Machine<'p> {
    /// Start at the program's entry point.
    ///
    /// Its block takes one parameter, the continuation to answer with, and that
    /// continuation is [`Value::Halt`].
    pub fn new(program: &'p Program) -> Result<Self, Error> {
        let Some(entry) = program.entry else {
            return err("program has no entry point");
        };
        Machine::at(program, entry)
    }

    /// Start at the definition labelled `entry` instead -- a test, say.
    pub fn at(program: &'p Program, entry: crate::Label) -> Result<Self, Error> {
        set_roles(&program.roles);
        let Some(block) = program.block(entry) else {
            return err(format!("entry label {entry:?} is not defined"));
        };
        if block.params.len() != 1 {
            return err(format!(
                "entry block takes {} parameters, expected 1 (the continuation)",
                block.params.len()
            ));
        }
        Ok(Machine {
            program,
            stmt: &block.body,
            names: block.params.clone(),
            env: vec![Value::Halt],
            globals: Vec::new(),
            steps: 0,
        })
    }

    /// A primitive, with the few that need the machine answered here.
    fn prim(&mut self, p: Prim, vals: &[Value<'p>]) -> Result<Value<'p>, Error> {
        let index = |v: &Value| match v {
            Value::Int(i) if *i >= 0 => Ok(*i as usize),
            other => err(format!("a definition index is {other}")),
        };
        match p {
            Prim::GlobalReady => {
                let i = index(&vals[0])?;
                Ok(Value::Bool(matches!(self.globals.get(i), Some(Some(_)))))
            }
            Prim::GlobalGet => {
                let i = index(&vals[0])?;
                match self.globals.get(i) {
                    Some(Some(v)) => Ok(v.clone()),
                    _ => err(format!(
                        "definition {i} read before it was evaluated, when {} of them were",
                        self.globals.iter().filter(|g| g.is_some()).count()
                    )),
                }
            }
            Prim::Once => Ok(Value::Ref(Rc::new(RefCell::new(Value::Bool(false))))),
            // This machine keeps every continuation on the heap, so the stack
            // primitives have nothing to cut or rejoin. `Detach` answers a
            // reference all the same, because a segment is one.
            Prim::Enter | Prim::Reattach => Ok(Value::Unit),
            Prim::Detach => Ok(Value::Ref(Rc::new(RefCell::new(Value::Unit)))),
            Prim::TakeOnce => match &vals[0] {
                Value::Ref(cell) => Ok(Value::Bool(!matches!(
                    cell.replace(Value::Bool(true)),
                    Value::Bool(true)
                ))),
                other => err(format!("takeOnce: expected a flag, got {}", kind(other))),
            },
            Prim::SetField => match (&vals[0], index(&vals[1])?) {
                (Value::Data(_, _, fields), i) => match fields.set(i, vals[2].clone()) {
                    Some(()) => Ok(Value::Unit),
                    None => err(format!("setField: no field {i}")),
                },
                (other, _) => err(format!("setField: expected data, got {}", kind(other))),
            },
            Prim::GlobalSet => {
                let i = index(&vals[0])?;
                if self.globals.len() <= i {
                    self.globals.resize(i + 1, None);
                }
                self.globals[i] = Some(vals[1].clone());
                Ok(Value::Unit)
            }
            _ => prim(p, vals, &self.program.tags),
        }
    }

    /// Run to completion, or until `fuel` transitions have been taken.
    ///
    /// Bounded rather than open-ended because this is what differential tests
    /// call: a lowering bug that loops should fail a test, not hang it.
    pub fn run(program: &'p Program, fuel: u64) -> Result<Value<'p>, Error> {
        let mut m = Machine::new(program)?;
        loop {
            if m.steps >= fuel {
                return err(format!("ran for {fuel} steps without finishing"));
            }
            if let Some(v) = m.step()? {
                return Ok(v);
            }
        }
    }

    /// One transition. `Some` means the machine has finished.
    pub fn step(&mut self) -> Result<Option<Value<'p>>, Error> {
        self.steps += 1;
        match self.stmt {
            Statement::Substitute(sel, block) => {
                if sel.len() != block.params.len() {
                    return err(format!(
                        "substitute of {} values into a block of {} parameters",
                        sel.len(),
                        block.params.len()
                    ));
                }
                let vals = self.lookup_all(sel)?;
                self.enter(block, vals)?;
                Ok(None)
            }

            // The environment is untouched — which is exactly why a global can be
            // reached by one, and why recursion needs no back-patching.
            Statement::Jump(label) => {
                let Some(block) = self.program.block(*label) else {
                    return err(format!("jump to undefined label {label:?}"));
                };
                let vals = std::mem::take(&mut self.env);
                self.enter(block, vals)?;
                Ok(None)
            }

            Statement::Let {
                name,
                tag,
                ctor,
                fields,
                rest,
            } => {
                let vals = self.lookup_all(fields)?;
                let v = Value::Data(*ctor, *tag, Rc::new(Fields::new(vals)));
                self.push(*name, v)?;
                self.stmt = rest;
                Ok(None)
            }

            Statement::Switch {
                scrutinee,
                arms,
                default,
            } => {
                let v = self.lookup(*scrutinee)?;
                let hit = match &v {
                    Value::Data(_, tag, fields) => arms
                        .iter()
                        .find(|(t, _)| t == tag)
                        .map(|(_, b)| (b, fields.clone())),
                    _ => None,
                };
                match hit {
                    // The fields go on the front and the scrutinee stays — see
                    // the note on `Statement::Switch`. `invoke` is the only
                    // statement that consumes what it names.
                    Some((block, fields)) => {
                        let mut vals: Vec<Value> = fields.iter().cloned().collect();
                        vals.append(&mut self.env);
                        self.enter(block, vals)?;
                    }
                    // No arm — a different constructor, or not data at all. The
                    // default binds the environment unchanged, because it has no
                    // fields to name.
                    None => {
                        let vals = std::mem::take(&mut self.env);
                        self.enter(default, vals)?;
                    }
                }
                Ok(None)
            }

            Statement::New {
                name,
                captures,
                methods,
                rest,
            } => {
                let captured = self.lookup_all(captures)?;
                let obj = Value::Obj(Rc::new(Object {
                    captures: captured,
                    methods,
                }));
                self.push(*name, obj)?;
                self.stmt = rest;
                Ok(None)
            }

            Statement::Invoke(target, tag) => self.invoke(*target, *tag),

            Statement::Extern { op, args, blocks } => self.extern_op(op, args, blocks),

            Statement::Error(msg) => err(*msg),

            Statement::Mark(_, inner) => {
                self.stmt = inner;
                Ok(None)
            }
        }
    }

    // --- transitions ------------------------------------------------------

    /// Enter a block with `vals` as the environment.
    ///
    /// The length check is the invariant that makes this machine worth running:
    /// a block's parameters name the whole environment, so if they disagree the
    /// lowering built a program with no meaning, and saying so here is far more
    /// useful than reading a wrong answer later.
    fn enter(&mut self, block: &'p Block, vals: Vec<Value<'p>>) -> Result<(), Error> {
        if block.params.len() != vals.len() {
            return err(format!(
                "block takes {} parameters but the environment has {} values",
                block.params.len(),
                vals.len()
            ));
        }
        self.names = block.params.clone();
        self.env = vals;
        self.stmt = &block.body;
        // A block that only hands control over need not carry the descriptors
        // of what it holds: nothing looks at them before the next block does.
        let strict = crate::describe::transfer(&block.body).is_none();
        for (n, v) in self.names.iter().zip(&self.env) {
            self.check(*n, v, strict)?;
        }
        Ok(())
    }

    fn invoke(&mut self, target: Name, tag: Tag) -> Result<Option<Value<'p>>, Error> {
        let v = self.lookup(target)?;
        match v {
            // Invoking the entry point's continuation is how a program ends: the
            // substitution before it left exactly the answer.
            Value::Halt => {
                let mut rest = self.without(target);
                match rest.len() {
                    1 => Ok(Some(rest.pop().expect("checked"))),
                    n => err(format!("halted with {n} values, expected 1")),
                }
            }
            Value::Obj(obj) => {
                let methods = obj.methods;
                let Some(block) = methods.get(tag as usize) else {
                    return err(format!(
                        "invoked method #{tag} of an object with {} methods",
                        methods.len()
                    ));
                };
                let mut vals = obj.captures.clone();
                vals.extend(self.without(target));
                self.enter(block, vals)?;
                Ok(None)
            }
            other => err(format!("invoked a non-object value ({})", kind(&other))),
        }
    }

    fn extern_op(
        &mut self,
        op: &'p Extern,
        args: &'p [Name],
        blocks: &'p [Block],
    ) -> Result<Option<Value<'p>>, Error> {
        // A branching primitive with two continuations. Neither changes the
        // environment, which is why `if` needs no statement of its own.
        if op.is_branch() {
            let [on_false, on_true] = blocks else {
                return err(format!(
                    "a branch needs 2 continuations, got {}",
                    blocks.len()
                ));
            };
            // The three forms differ only in where the boolean comes from: a
            // register, a comparison of two, or a comparison against a literal.
            let v = match op {
                Extern::Branch => match args {
                    [arg] => self.lookup(*arg)?,
                    _ => return err(format!("a branch needs 1 argument, got {}", args.len())),
                },
                Extern::BranchPrim(p) => {
                    let vals = self.lookup_all(args)?;
                    prim(*p, &vals, &self.program.tags)?
                }
                Extern::BranchPrimK(p, l) => {
                    let mut vals = self.lookup_all(args)?;
                    vals.push(literal(l));
                    prim(*p, &vals, &self.program.tags)?
                }
                _ => unreachable!("is_branch covers exactly these"),
            };
            let block = if is_falsey(&v) { on_false } else { on_true };
            let vals = std::mem::take(&mut self.env);
            self.enter(block, vals)?;
            return Ok(None);
        }

        let [block] = blocks else {
            return err(format!(
                "a value-producing extern needs 1 continuation, got {}",
                blocks.len()
            ));
        };
        let vals = self.lookup_all(args)?;
        let v = match op {
            Extern::Branch | Extern::BranchPrim(_) | Extern::BranchPrimK(_, _) => {
                unreachable!("handled above")
            }
            Extern::Lit(l) => literal(l),
            // The CEK discharges `Fs`, `Process`, `Random` and `Time` against
            // the real world. This machine does not, and says so rather than
            // inventing an answer. `Test.fail` is the exception: a failed
            // assertion is a runtime error with the assertion's own message,
            // which is what a test runner reads. Output is the other: printing
            // has to reach the terminal on every engine, or a program that
            // prints cannot be compared.
            Extern::Native(effect, op) => match (&**effect, &**op, &vals[..]) {
                ("Test", "fail", [v]) => return err(v.to_string()),
                // How much of the program has been reached that had not been:
                // nothing, as the CEK machine and native code answer -- this
                // machine counts no coverage -- so a fuzzer steered by it runs
                // as one that is not.
                ("Process", "covered", [_]) => Value::Int(0),
                ("Console", "writeOutput", [v]) => {
                    match v {
                        Value::Str(s) => print!("{s}"),
                        other => print!("{other}"),
                    }
                    Value::Unit
                }
                ("Console", "writeError", [v]) => {
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    match v {
                        Value::Str(s) => eprint!("{s}"),
                        other => eprint!("{other}"),
                    }
                    Value::Unit
                }
                _ => {
                    let effect = meadow_rt::spelling(&effect);
                    return err(format!("unhandled effect {effect}.{op}"));
                }
            },
            Extern::Prim(p) => self.prim(*p, &vals)?,
            Extern::PrimK(p, l) => {
                let mut vals = vals.clone();
                vals.push(literal(l));
                self.prim(*p, &vals)?
            }
            Extern::Array => Value::Array(Rc::new(vals)),
            Extern::Record(labels) => {
                if labels.len() != vals.len() {
                    return err(format!(
                        "a record of {} labels given {} values",
                        labels.len(),
                        vals.len()
                    ));
                }
                Value::Record(Rc::new(
                    labels.iter().copied().zip(vals).collect::<BTreeMap<_, _>>(),
                ))
            }
            Extern::Select(label, _) => match &vals[0] {
                Value::Record(map) => match map.get(label) {
                    Some(v) => v.clone(),
                    None => return err(format!("no field `{label}` on this record")),
                },
                // A `record` declaration's value: constructor data whose fields
                // have names.
                Value::Data(name, _, fields) => {
                    let at = self
                        .program
                        .ctor_fields
                        .get(name)
                        .and_then(|fs| fs.iter().position(|f| f == label));
                    match at.and_then(|i| fields.get(i)) {
                        Some(v) => v.clone(),
                        None => {
                            let name = meadow_rt::ctor_spelling(name);
                            return err(format!("`{name}` has no field `{label}`"));
                        }
                    }
                }
                other => return err(format!("selected `.{label}` from {}", kind(other))),
            },
            Extern::Extend(label, _) => match &vals[0] {
                Value::Record(map) => {
                    let mut map = (**map).clone();
                    map.insert(*label, vals[1].clone());
                    Value::Record(Rc::new(map))
                }
                other => return err(format!("extended {} with `.{label}`", kind(other))),
            },
            Extern::Field(i) => match &vals[0] {
                Value::Data(name, _, fields) => match fields.get(*i) {
                    Some(v) => v.clone(),
                    None => {
                        return err(format!("`{name}` has no field {i}"));
                    }
                },
                Value::Array(xs) => match xs.get(*i) {
                    Some(v) => v.clone(),
                    None => return err(format!("array of {} has no element {i}", xs.len())),
                },
                other => return err(format!("took field {i} of {}", kind(other))),
            },
        };
        self.produce(block, v)?;
        Ok(None)
    }

    /// An extern that produces a value: it goes on the front, and the
    /// continuation's parameters cover it and everything that was there.
    fn produce(&mut self, block: &'p Block, v: Value<'p>) -> Result<(), Error> {
        let mut vals = vec![v];
        vals.append(&mut self.env);
        self.enter(block, vals)
    }

    // --- environment ------------------------------------------------------

    fn lookup(&self, n: Name) -> Result<Value<'p>, Error> {
        match self.names.iter().position(|m| *m == n) {
            Some(i) => Ok(self.env[i].clone()),
            None => err(format!("{n:?} is not in scope: {}", self.describe())),
        }
    }

    fn lookup_all(&self, ns: &[Name]) -> Result<Vec<Value<'p>>, Error> {
        ns.iter().map(|n| self.lookup(*n)).collect()
    }

    /// The environment with the first slot named `n` removed — the paper's
    /// `drop` of the head, generalised to a name because this machine's
    /// statements say which value they mean.
    fn without(&mut self, n: Name) -> Vec<Value<'p>> {
        let mut env = std::mem::take(&mut self.env);
        if let Some(i) = self.names.iter().position(|m| *m == n) {
            env.remove(i);
        }
        env
    }

    fn push(&mut self, n: Name, v: Value<'p>) -> Result<(), Error> {
        self.names.insert(0, n);
        self.env.insert(0, v);
        self.check(n, &self.env[0], true)
    }

    /// Does `v` fit the representation the program declares for `n`? Values
    /// here still say what they are, which is what makes this a check of the
    /// lowering's claims: every name's representation, compared with what
    /// actually arrives in it, at every binding the machine makes. See
    /// [`crate::Rep`]. A program that declares none is not checked.
    ///
    /// A value of a type variable's type is checked against its descriptor,
    /// which has to be in the environment with it -- unless not `strict`.
    fn check(&self, n: Name, v: &Value<'p>, strict: bool) -> Result<(), Error> {
        if self.program.reps.is_empty() {
            return Ok(());
        }
        let Some(&rep) = self.program.reps.get(&n) else {
            return err(format!(
                "{n:?} has no representation, and holds {}",
                kind(v)
            ));
        };
        let fits = match rep {
            Rep::Ref => matches!(
                v,
                Value::Data(..)
                    | Value::Array(_)
                    | Value::Record(_)
                    | Value::Ref(_)
                    | Value::MutArray(_)
                    | Value::Obj(_)
                    | Value::Halt
                    | Value::Compact(_)
                    | Value::BigInt(_)
                    // A string is an object to the bytecode machine; this one
                    // keeps it interned.
                    | Value::Str(_)
            ),
            Rep::Int => matches!(v, Value::Int(_)),
            Rep::Float => matches!(v, Value::Float(_)),
            Rep::Bits(d) => described(d, v),
            Rep::Str => matches!(v, Value::Str(_)),
            Rep::Var(d) if d == crate::NO_DESC => {
                return err(format!(
                    "{n:?} holds {}, of a type variable nothing describes",
                    kind(v)
                ));
            }
            Rep::Var(d) => match self.names.iter().position(|m| m.0 == d) {
                Some(i) => match &self.env[i] {
                    Value::Int(code) => described(*code, v),
                    other => {
                        return err(format!(
                            "{n:?}'s descriptor {:?} holds {}, not a descriptor",
                            crate::VarId(d),
                            kind(other)
                        ));
                    }
                },
                None if strict => {
                    return err(format!(
                        "{n:?} holds {} without its descriptor {:?}: {}",
                        kind(v),
                        crate::VarId(d),
                        self.describe()
                    ));
                }
                None => true,
            },
            Rep::Unknown => false,
        };
        if fits {
            Ok(())
        } else {
            err(format!("{n:?} is declared {rep:?} but holds {}", kind(v)))
        }
    }

    // --- introspection ----------------------------------------------------

    /// The environment, named — what a stepper would show beside the statement.
    pub fn describe(&self) -> String {
        let mut out = String::from("[");
        for (i, (n, v)) in self.names.iter().zip(self.env.iter()).enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("{n:?} = {v}"));
        }
        out.push(']');
        out
    }
}

/// Is `v` what descriptor `d` says?
fn described(d: meadow_rt::desc::Desc, v: &Value) -> bool {
    use meadow_rt::desc;
    match d {
        desc::REF => matches!(
            v,
            Value::Data(..)
                | Value::Array(_)
                | Value::Record(_)
                | Value::Ref(_)
                | Value::MutArray(_)
                | Value::Obj(_)
                | Value::Halt
                | Value::Compact(_)
                | Value::BigInt(_)
                | Value::Str(_)
        ),
        desc::INT => matches!(v, Value::Int(_)),
        desc::FLOAT => matches!(v, Value::Float(_)),
        desc::STR => matches!(v, Value::Str(_)),
        desc::UNIT => matches!(v, Value::Unit),
        desc::BOOL => matches!(v, Value::Bool(_)),
        desc::CHAR => matches!(v, Value::Char(_)),
        desc::FLOAT32 => matches!(v, Value::Float32(_)),
        desc::ANY => true,
        d => matches!(v, Value::Word(w, _) if desc::word(*w) == d),
    }
}

fn literal<'p>(l: &Lit) -> Value<'p> {
    match l {
        Lit::Int(n) | Lit::AnyInt(n, _) => Value::Int(*n),
        Lit::BigInt(n) => Value::BigInt(Rc::new(BigInt::from(*n))),
        Lit::Float(x) | Lit::AnyFloat(x, _) => Value::Float(*x),
        Lit::Word(w, b) => Value::Word(*w, *b),
        Lit::Float32(x) => Value::Float32(*x),
        Lit::Str(s) | Lit::Sym(s) => Value::Str(*s),
        Lit::Char(c) => Value::Char(*c),
        Lit::Bool(b) => Value::Bool(*b),
        Lit::Unit => Value::Unit,
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "Int",
        Value::BigInt(_) => "BigInt",
        Value::Float(_) => "Float",
        Value::Word(w, _) => w.name(),
        Value::Float32(_) => "Float32",
        Value::Bool(_) => "Bool",
        Value::Str(_) => "String",
        Value::Char(_) => "Char",
        Value::Unit => "()",
        Value::Data(..) => "data",
        Value::Array(_) => "Array",
        Value::Record(_) => "record",
        Value::Ref(_) => "Ref",
        Value::MutArray(_) => "StArray",
        Value::Obj(_) => "codata",
        Value::Halt => "halt",
        Value::Compact(_) => "Compact",
    }
}

/// What `Extern::Branch` treats as false. The compiler lowers `True`/`False`
/// patterns to `Lit(Bool)`, but a value built by naming the constructor arrives
/// as data, so both spellings are accepted — the same rule the CEK uses.
fn is_falsey(v: &Value) -> bool {
    match v {
        Value::Bool(b) => !b,
        Value::Data(name, _, fields) => plays(name, Role::False) && fields.is_empty(),
        _ => false,
    }
}

// --- the shapes `Std` builds out of data ---------------------------------
//
// A `List` is a `Cons` chain and a `Vector` is a tree of arrays. Both print and
// compare as the sequence they denote rather than as the tree they are, and both
// have to agree with the CEK machine exactly — `show` is a primitive, and the
// differential tests compare printed results.

fn list_items<'p>(v: &Value<'p>) -> Option<Vec<Value<'p>>> {
    let mut out = Vec::new();
    let mut cur = v.clone();
    loop {
        match cur {
            Value::Data(ref n, _, ref fs) if plays(n, Role::Nil) && fs.is_empty() => {
                return Some(out);
            }
            Value::Data(ref n, _, ref fs) if plays(n, Role::Cons) && fs.len() == 2 => {
                out.push(fs[0].clone());
                let next = fs[1].clone();
                cur = next;
            }
            _ => return None,
        }
    }
}

thread_local! {
    /// The running program's roles (`Program::roles`), for what prints and
    /// compares values without the program at hand: a `List` prints as the
    /// sequence it is, whichever constructors a language makes one of.
    static ROLES: RefCell<HashMap<Role, InternedString>> = RefCell::new(HashMap::new());
}

/// Make `roles` the ones values are printed and compared by, on this thread.
pub fn set_roles(roles: &HashMap<Role, InternedString>) {
    ROLES.with(|r| *r.borrow_mut() = roles.clone());
}

/// The constructor playing `role` -- empty, which names no constructor, where
/// the program declares none.
fn role(role: Role) -> InternedString {
    ROLES.with(|r| {
        r.borrow()
            .get(&role)
            .copied()
            .unwrap_or_else(|| InternedString::from(""))
    })
}

/// Does the constructor called `name` play `role`?
fn plays(name: &str, role: Role) -> bool {
    ROLES.with(|r| r.borrow().get(&role).is_some_and(|c| &**c == name))
}

/// A constructor as a reader wants to see it: `Maybe.Just` prints as `Just`.
///
/// The third of three implementations of this — `meadow_eval`'s `Display` and
/// `meadow_glade::show` are the others — which is why
/// `constructors_print_bare_and_agree` in `glade/tests/differential.rs` runs all
/// three against each other rather than trusting any one of them.
fn bare_ctor(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((_, c)) => c,
        None => name,
    }
}

fn is_vector_ctor(name: &str) -> bool {
    plays(name, Role::VectorEmpty)
        || plays(name, Role::VectorSingle)
        || plays(name, Role::VectorFull)
}

fn vector_elems<'p>(v: &Value<'p>) -> Option<Vec<Value<'p>>> {
    match v {
        Value::Data(n, _, fs) if plays(n, Role::VectorEmpty) && fs.is_empty() => Some(Vec::new()),
        Value::Data(n, _, fs) if plays(n, Role::VectorSingle) && fs.len() == 1 => match &fs[0] {
            Value::Array(xs) => Some(xs.iter().cloned().collect()),
            _ => None,
        },
        Value::Data(n, _, fs) if plays(n, Role::VectorFull) && fs.len() == 7 => {
            let mut out = Vec::new();
            for i in [2usize, 3] {
                match &fs[i] {
                    Value::Array(xs) => out.extend(xs.iter().cloned()),
                    _ => return None,
                }
            }
            vector_node_elems(&fs[4], &mut out)?;
            for i in [5usize, 6] {
                match &fs[i] {
                    Value::Array(xs) => out.extend(xs.iter().cloned()),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn vector_node_elems<'p>(n: &Value<'p>, out: &mut Vec<Value<'p>>) -> Option<()> {
    match n {
        Value::Data(name, _, fs) if plays(name, Role::VNodeLeaf) && fs.len() == 1 => match &fs[0] {
            Value::Array(xs) => {
                out.extend(xs.iter().cloned());
                Some(())
            }
            _ => None,
        },
        Value::Data(name, _, fs) if plays(name, Role::VNodeBranch) && fs.len() == 2 => match &fs[1]
        {
            Value::Array(kids) => {
                for k in kids.iter() {
                    vector_node_elems(k, out)?;
                }
                Some(())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Structural equality, iteratively — a `Cons` chain is as deep as it is long.
pub fn value_eq<'p>(a: &Value<'p>, b: &Value<'p>) -> bool {
    let mut stack = vec![(a.clone(), b.clone())];
    while let Some((a, b)) = stack.pop() {
        match (&a, &b) {
            // A `Vector` is a tree, so two equal ones need not have equal
            // shapes. Compare the sequences they denote instead.
            (Value::Data(na, _, _), Value::Data(nb, _, _))
                if is_vector_ctor(na) && is_vector_ctor(nb) =>
            {
                match (vector_elems(&a), vector_elems(&b)) {
                    (Some(xs), Some(ys)) if xs.len() == ys.len() => {
                        stack.extend(xs.into_iter().zip(ys));
                    }
                    _ => return false,
                }
            }
            // Numbers by value, so a literal in generic code equals the value
            // it stands beside -- see `meadow_core::num`.
            (x, y) if to_num(x).is_some() && to_num(y).is_some() => {
                if !num::num_eq(&to_num(x).expect("a number"), &to_num(y).expect("a number")) {
                    return false;
                }
            }
            (Value::Bool(x), Value::Bool(y)) if x == y => {}
            (Value::Str(x), Value::Str(y)) if x == y => {}
            (Value::Char(x), Value::Char(y)) if x == y => {}
            (Value::Unit, Value::Unit) => {}
            (Value::Data(n1, _, x), Value::Data(n2, _, y)) => {
                if n1 != n2 {
                    return false;
                }
                // The same allocation is trivially equal to itself — which is
                // what makes `xs == xs` on a long list O(1) rather than O(n).
                if Rc::ptr_eq(x, y) {
                    continue;
                }
                if x.len() != y.len() {
                    return false;
                }
                stack.extend(x.iter().cloned().zip(y.iter().cloned()));
            }
            (Value::Array(x), Value::Array(y)) => {
                if Rc::ptr_eq(x, y) {
                    continue;
                }
                if x.len() != y.len() {
                    return false;
                }
                stack.extend(x.iter().cloned().zip(y.iter().cloned()));
            }
            (Value::Record(x), Value::Record(y)) => {
                if x.len() != y.len() {
                    return false;
                }
                for (k, v) in x.iter() {
                    match y.get(k) {
                        Some(w) => stack.push((v.clone(), w.clone())),
                        None => return false,
                    }
                }
            }
            // Identity, not contents: a `Ref` is a place, and two cells holding
            // the same thing are still two cells.
            (Value::Ref(x), Value::Ref(y)) if Rc::ptr_eq(x, y) => {}
            (Value::MutArray(x), Value::MutArray(y)) if Rc::ptr_eq(x, y) => {}
            // Immutable, so two are equal when what they hold is.
            (Value::Compact(x), Value::Compact(y)) => stack.push((x.0.clone(), y.0.clone())),
            _ => return false,
        }
    }
    true
}

/// `hash`, fed to [`meadow_core::hash::Hasher`] in the order every engine uses:
/// a value's head, then its parts left to right.
fn hash_value(v: &Value) -> Result<i64, Error> {
    use meadow_rt::hash::{Hasher, unhashable};
    enum Work<'p> {
        Val(Value<'p>),
        Label(String),
    }
    let mut h = Hasher::new();
    let mut stack = vec![Work::Val(v.clone())];
    while let Some(w) = stack.pop() {
        let v = match w {
            Work::Label(l) => {
                h.str(&l);
                continue;
            }
            Work::Val(v) => v,
        };
        match &v {
            Value::Int(_)
            | Value::BigInt(_)
            | Value::Float(_)
            | Value::Word(..)
            | Value::Float32(_) => {
                num::hash_into(&mut h, &to_num(&v).expect("a number"));
            }
            Value::Bool(b) => h.bool(*b),
            Value::Char(c) => h.char(*c),
            Value::Str(s) => h.str(s),
            Value::Unit => h.unit(),
            Value::Data(name, _, _) if is_vector_ctor(name) => {
                let Some(xs) = vector_elems(&v) else {
                    return err(format!("hash: a malformed vector ({name})"));
                };
                h.vector(xs.len());
                stack.extend(xs.into_iter().rev().map(Work::Val));
            }
            Value::Data(name, _, fields) => {
                h.data(name, fields.len());
                stack.extend(fields.iter().rev().cloned().map(Work::Val));
            }
            Value::Array(xs) => {
                h.array(xs.len());
                stack.extend(xs.iter().rev().cloned().map(Work::Val));
            }
            Value::Record(fields) => {
                h.record(fields.len());
                let mut sorted: Vec<(String, Value)> = fields
                    .iter()
                    .map(|(l, v)| (l.to_string(), v.clone()))
                    .collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                for (label, value) in sorted.into_iter().rev() {
                    stack.push(Work::Val(value));
                    stack.push(Work::Label(label));
                }
            }
            Value::Ref(_) => return err(unhashable("a Ref")),
            Value::MutArray(_) => return err(unhashable("a mutable array")),
            Value::Obj(_) | Value::Halt => {
                return err(unhashable("a function"));
            }
            Value::Compact(c) => {
                h.compact();
                stack.push(Work::Val(c.0.clone()));
            }
        }
    }
    Ok(h.finish())
}

/// The primitives, mirroring `meadow_eval::run_prim` operation for operation —
/// including the wrapping arithmetic and the two zero checks, because the
/// differential tests compare answers and an overflow that panicked on one side
/// and wrapped on the other would be a difference in the harness rather than in
/// the machine.
fn prim<'p>(
    op: Prim,
    args: &[Value<'p>],
    tags: &HashMap<InternedString, Tag>,
) -> Result<Value<'p>, Error> {
    use Prim::*;
    // A typed primitive is its untyped one, at a type this machine has no use
    // for knowing.
    let op = op.untyped();

    let n = |i: usize| {
        to_num(&args[i]).ok_or_else(|| Error {
            msg: format!("expected a number, got {}", args[i]),
        })
    };
    let done = |r: Result<Num, String>| r.map(from_num).map_err(|msg| Error { msg });
    let truth = |r: Result<bool, String>| r.map(Value::Bool).map_err(|msg| Error { msg });
    let data = |name: &str, fields: Vec<Value<'p>>| {
        let name = InternedString::from(name);
        // A constructor a primitive builds must carry the tag the program's
        // `switch` arms were compiled with, or a `match` on the result would
        // miss. A name the program never mentions gets a tag no arm has.
        let tag = tags.get(&name).copied().unwrap_or(Tag::MAX);
        Value::Data(name, tag, Rc::new(Fields::new(fields)))
    };

    match op {
        // --- numbers: see `meadow_core::num` --------------------------------
        Add | Sub | Mul | Div | Mod | Pow => done(num::int_arith(arith(op), n(0)?, n(1)?)),
        Lt | Gt | Le | Ge => truth(num::int_cmp(cmp(op), n(0)?, n(1)?)),
        Neg => done(num::int_neg(n(0)?)),
        AddF | SubF | MulF | DivF => done(num::float_arith(arith(op), n(0)?, n(1)?)),
        LtF | GtF | LeF | GeF => truth(num::float_cmp(cmp(op), n(0)?, n(1)?)),
        ToFloat => done(num::to_float(n(0)?).map(Num::Float)),
        ToFloat32 => done(num::to_float32(n(0)?).map(Num::Float32)),
        Floor => done(num::floor(n(0)?).map(Num::Int)),
        ToBig => done(num::to_int(IntTarget::Big, n(0)?)),
        ToInt => done(num::to_int(IntTarget::Int, n(0)?)),
        ToWord(w) => done(num::to_int(IntTarget::Word(w), n(0)?)),
        Eq => Ok(Value::Bool(value_eq(&args[0], &args[1]))),
        Ne => Ok(Value::Bool(!value_eq(&args[0], &args[1]))),
        Hash => hash_value(&args[0]).map(Value::Int),
        Display => Ok(Value::Str(InternedString::from(match &args[0] {
            Value::Str(s) => s.to_string(),
            other => other.to_string(),
        }))),

        // --- the builtin `Array` ---------------------------------------------
        ArrayLen => Ok(Value::Int(as_array(&args[0])?.len() as i64)),
        ArrayGet => {
            let a = as_array(&args[0])?;
            let i = as_index(&args[1])?;
            a.get(i).cloned().ok_or_else(|| Error {
                msg: format!("arrayGet: index {i} out of bounds (len {})", a.len()),
            })
        }
        ArrayGetOr => {
            let a = as_array(&args[1])?;
            let i = as_index(&args[2])?;
            Ok(a.get(i).cloned().unwrap_or_else(|| args[0].clone()))
        }
        ArraySet => {
            let mut rc = as_array(&args[0])?.clone();
            let i = as_index(&args[1])?;
            let buf = Rc::make_mut(&mut rc);
            if i >= buf.len() {
                return err(format!(
                    "arraySet: index {i} out of bounds (len {})",
                    buf.len()
                ));
            }
            buf[i] = args[2].clone();
            Ok(Value::Array(rc))
        }
        ArrayPush => {
            let mut rc = as_array(&args[0])?.clone();
            Rc::make_mut(&mut rc).push(args[1].clone());
            Ok(Value::Array(rc))
        }
        ArrayPop => {
            let mut rc = as_array(&args[0])?.clone();
            let buf = Rc::make_mut(&mut rc);
            if buf.pop().is_none() {
                return err("arrayPop: empty array");
            }
            Ok(Value::Array(rc))
        }
        ArraySlice => {
            let a = as_array(&args[0])?;
            let n = a.len() as i64;
            let from = as_int(&args[1])?.clamp(0, n) as usize;
            let to = as_int(&args[2])?.clamp(from as i64, n) as usize;
            Ok(Value::Array(Rc::new(a[from..to].to_vec())))
        }
        ArrayConcat => {
            let x = as_array(&args[0])?;
            let y = as_array(&args[1])?;
            if x.is_empty() {
                return Ok(args[1].clone());
            }
            if y.is_empty() {
                return Ok(args[0].clone());
            }
            let mut out = Vec::with_capacity(x.len() + y.len());
            out.extend(x.iter().cloned());
            out.extend(y.iter().cloned());
            Ok(Value::Array(Rc::new(out)))
        }

        // --- bitwise ----------------------------------------------------------
        Shl | Shr | Ushr | BitAnd | BitOr | BitXor => done(num::int_bits(bits(op), n(0)?, n(1)?)),
        BitNot => done(num::int_not(n(0)?)),
        PopCount => done(num::pop_count(n(0)?).map(Num::Int)),
        BitWidth => done(num::bit_width(&n(0)?).map(Num::Int)),

        // --- text and bytes ---------------------------------------------------
        StringToBytes => match &args[0] {
            Value::Str(s) => Ok(Value::Array(Rc::new(
                s.bytes()
                    .map(|b| Value::Word(Width::U8, b as u64))
                    .collect(),
            ))),
            other => err(format!("`stringToBytes` expects a String, got {other}")),
        },
        BytesToString => {
            let buf = bytes_of(&args[0], "bytesToString")?;
            Ok(Value::Str(InternedString::from(
                String::from_utf8_lossy(&buf).into_owned(),
            )))
        }
        BytesToHex => {
            let buf = bytes_of(&args[0], "bytesToHex")?;
            let mut s = String::with_capacity(buf.len() * 2);
            for b in buf {
                s.push(char::from_digit((b >> 4) as u32, 16).expect("nibble"));
                s.push(char::from_digit((b & 0xf) as u32, 16).expect("nibble"));
            }
            Ok(Value::Str(InternedString::from(s)))
        }
        BytesFromHex => {
            let s = match &args[0] {
                Value::Str(s) => *s,
                other => return err(format!("`bytesFromHex` expects a String, got {other}")),
            };
            let bytes = s.as_bytes();
            if bytes.len() % 2 != 0 {
                return Ok(data(&role(Role::None), vec![]));
            }
            let mut out = Vec::with_capacity(bytes.len() / 2);
            for pair in bytes.chunks_exact(2) {
                match (
                    (pair[0] as char).to_digit(16),
                    (pair[1] as char).to_digit(16),
                ) {
                    (Some(h), Some(l)) => out.push(Value::Word(Width::U8, ((h << 4) | l) as u64)),
                    _ => return Ok(data(&role(Role::None), vec![])),
                }
            }
            Ok(data(&role(Role::Some), vec![Value::Array(Rc::new(out))]))
        }
        Show => Ok(Value::Str(InternedString::from(args[0].to_string()))),
        CharCode => match &args[0] {
            Value::Char(c) => Ok(Value::Int(*c as i64)),
            other => err(format!("charCode: expected a Char, got {other}")),
        },
        CharFromCode => match &args[0] {
            Value::Int(n) => u32::try_from(*n)
                .ok()
                .and_then(char::from_u32)
                .map(Value::Char)
                .ok_or_else(|| Error {
                    msg: format!("charFromCode: {n} is not a Unicode scalar value"),
                }),
            other => err(format!("charFromCode: expected an Int, got {other}")),
        },
        StringToChars => match &args[0] {
            Value::Str(s) => Ok(Value::Array(Rc::new(s.chars().map(Value::Char).collect()))),
            other => err(format!("stringToChars: expected a String, got {other}")),
        },
        CharsToString => match &args[0] {
            Value::Array(xs) => {
                let mut out = String::with_capacity(xs.len());
                for v in xs.iter() {
                    match v {
                        Value::Char(c) => out.push(*c),
                        other => {
                            return err(format!("charsToString: expected a Char, got {other}"));
                        }
                    }
                }
                Ok(Value::Str(InternedString::from(out)))
            }
            other => err(format!("charsToString: expected an Array, got {other}")),
        },
        ConcatStrings => match &args[0] {
            Value::Array(xs) => {
                let mut out = String::new();
                for v in xs.iter() {
                    match v {
                        Value::Str(s) => out.push_str(s),
                        other => {
                            return err(format!("concatStrings: expected a String, got {other}"));
                        }
                    }
                }
                Ok(Value::Str(InternedString::from(out)))
            }
            other => err(format!("concatStrings: expected an Array, got {other}")),
        },
        StringByteLength => Ok(Value::Int(
            text_arg(&args[0], "stringByteLength")?.len() as i64
        )),
        StringByteAt => {
            let s = text_arg(&args[0], "stringByteAt")?;
            let i = as_int(&args[1])?;
            match usize::try_from(i).ok().and_then(|at| s.as_bytes().get(at)) {
                Some(b) => Ok(Value::Word(num::Width::U8, u64::from(*b))),
                None => err(format!(
                    "stringByteAt: index {i} out of bounds (len {})",
                    s.len()
                )),
            }
        }
        StringSlice => {
            let s = text_arg(&args[0], "stringSlice")?;
            let (from, to) = (as_int(&args[1])?, as_int(&args[2])?);
            Ok(Value::Str(InternedString::from(meadow_rt::text::slice(
                s.as_bytes(),
                from,
                to,
            ))))
        }
        StringCompare => {
            let a = text_arg(&args[0], "stringCompare")?;
            let b = text_arg(&args[1], "stringCompare")?;
            Ok(Value::Int(meadow_rt::text::compare(
                a.as_bytes(),
                b.as_bytes(),
            )))
        }
        StringIndexOf => {
            let hay = text_arg(&args[0], "stringIndexOf")?;
            let needle = text_arg(&args[1], "stringIndexOf")?;
            Ok(Value::Int(meadow_rt::text::index_of(
                hay.as_bytes(),
                needle.as_bytes(),
                as_int(&args[2])?,
            )))
        }

        // --- the mutable cell -------------------------------------------------
        NewRef => Ok(Value::Ref(Rc::new(RefCell::new(args[0].clone())))),
        GetRef => match &args[0] {
            Value::Ref(cell) => Ok(cell.borrow().clone()),
            other => err(format!("getRef: expected a Ref, got {other}")),
        },
        SetRef => match &args[0] {
            Value::Ref(cell) => {
                *cell.borrow_mut() = args[1].clone();
                Ok(Value::Unit)
            }
            other => err(format!("setRef: expected a Ref, got {other}")),
        },

        // --- the mutable array ------------------------------------------------
        RunSt => err("runSt reached the machine; lowering applies its body"),
        StNewArray => {
            match &args[0] {
                Value::Int(n) if *n >= 0 => Ok(Value::MutArray(Rc::new(RefCell::new(vec![
                args[1].clone();
                *n as usize
            ])))),
                other => err(format!(
                    "stNewArray: expected a length of zero or more, got {other}"
                )),
            }
        }
        StGetArray => {
            let cells = as_mut_array(&args[0])?;
            let i = as_index(&args[1])?;
            let cells = cells.borrow();
            cells.get(i).cloned().ok_or_else(|| Error {
                msg: format!("stGetArray: index {i} out of bounds (len {})", cells.len()),
            })
        }
        StSetArray => {
            let cells = as_mut_array(&args[0])?;
            let i = as_index(&args[1])?;
            let mut cells = cells.borrow_mut();
            let n = cells.len();
            match cells.get_mut(i) {
                Some(slot) => {
                    *slot = args[2].clone();
                    Ok(Value::Unit)
                }
                None => err(format!("stSetArray: index {i} out of bounds (len {n})")),
            }
        }
        StArrayLen => Ok(Value::Int(as_mut_array(&args[0])?.borrow().len() as i64)),
        StFreeze => Ok(Value::Array(Rc::new(
            as_mut_array(&args[0])?.borrow().clone(),
        ))),
        StThaw => match &args[0] {
            Value::Array(xs) => Ok(Value::MutArray(Rc::new(RefCell::new((**xs).clone())))),
            other => err(format!("stThaw: expected an Array, got {other}")),
        },
        Compact => {
            let region = Rc::new(RefCell::new(Region::default()));
            compact_into(&region, &args[0])?;
            Ok(Value::Compact(Rc::new((args[0].clone(), region))))
        }
        GetCompact => Ok(as_compact(&args[0])?.0.clone()),
        CompactAdd => {
            let region = as_compact(&args[0])?.1.clone();
            compact_into(&region, &args[1])?;
            Ok(Value::Compact(Rc::new((args[1].clone(), region))))
        }
        CompactSize => Ok(Value::Int(
            (as_compact(&args[0])?.1.borrow().slots * meadow_rt::compact::SLOT_BYTES) as i64,
        )),
        // This machine checks the lowering one construct at a time, and has no
        // scheduler. Green threads run on the bytecode VM and the CEK machine,
        // which are checked against each other.
        ThreadSpawn | ThreadAwait | ThreadYield | ChannelNew | ChannelSend | ChannelReceive => {
            err("green threads are not supported by the sequent machine")
        }
        StmNew | StmRead | StmWrite | StmBegin | StmCommit | StmWait | StmNest | StmMerge
        | StmRollback => err("transactions are not supported by the sequent machine"),
        GlobalReady | GlobalGet | GlobalSet => {
            err("a definition cache reached a primitive with no machine")
        }
        SetField => err("a destination-passing write reached a primitive with no machine"),
        Once | TakeOnce => err("a resumption's flag reached a primitive with no machine"),
        Enter | Detach | Reattach => err("the frame stack reached a primitive with no machine"),
        IntAdd | IntSub | IntMul | IntDiv | IntMod | IntEq | IntNe | IntLt | IntLe | IntGt
        | IntGe | FloatAdd | FloatSub | FloatMul | FloatDiv | FloatEq | FloatNe | FloatLt
        | FloatLe | FloatGt | FloatGe => unreachable!("made untyped above"),
    }
}

fn as_compact<'a, 'p>(v: &'a Value<'p>) -> Result<&'a CompactCell<'p>, Error> {
    match v {
        Value::Compact(c) => Ok(c),
        other => err(format!("expected a Compact, got {other}")),
    }
}

/// Put `v` in a region: check that it can go in, and count the VM slots it
/// adds -- each node once, and nothing the region holds already, as the VM
/// shares what is in a region rather than copying it again. A refusal leaves
/// the region as it was. See `meadow_core::compact`.
fn compact_into<'p>(region: &RefCell<Region<'p>>, v: &Value<'p>) -> Result<(), Error> {
    use meadow_rt::compact::uncompactable;
    let mut r = region.borrow_mut();
    let mut new: std::collections::HashSet<*const ()> = std::collections::HashSet::new();
    let mut slots = 0usize;
    let mut stack = vec![v.clone()];
    while let Some(v) = stack.pop() {
        // A node is counted, and its children visited, the first time only.
        let mut first = |p: *const ()| !r.seen.contains(&p) && new.insert(p);
        match &v {
            // A constructor of no fields is a word, not a block: nothing to
            // copy.
            Value::Data(_, _, fields) if fields.is_empty() => {}
            Value::Data(_, _, fields) => {
                if first(Rc::as_ptr(fields) as *const ()) {
                    slots += meadow_rt::compact::object_slots(false, fields.len());
                    stack.extend(fields.iter().cloned());
                }
            }
            Value::Array(xs) => {
                if first(Rc::as_ptr(xs) as *const ()) {
                    slots += meadow_rt::compact::array_slots(xs.len());
                    stack.extend(xs.iter().cloned());
                }
            }
            Value::Record(fields) => {
                if first(Rc::as_ptr(fields) as *const ()) {
                    slots += meadow_rt::compact::object_slots(false, 2 * fields.len());
                    stack.extend(fields.values().cloned());
                }
            }
            Value::BigInt(b) => {
                if first(Rc::as_ptr(b) as *const ()) {
                    slots += meadow_rt::compact::object_slots(true, b.iter_u32_digits().count());
                }
            }
            Value::Compact(c) => {
                if first(Rc::as_ptr(c) as *const ()) {
                    slots += meadow_rt::compact::object_slots(false, 1);
                    stack.push(c.0.clone());
                }
            }
            Value::Ref(_) => return err(uncompactable("a Ref")),
            Value::MutArray(_) => return err(uncompactable("a mutable array")),
            Value::Obj(_) | Value::Halt => {
                return err(uncompactable("a function"));
            }
            Value::Int(_)
            | Value::Float(_)
            | Value::Word(..)
            | Value::Float32(_)
            | Value::Bool(_)
            | Value::Str(_)
            | Value::Char(_)
            | Value::Unit => {}
        }
    }
    r.seen.extend(new);
    r.slots += slots;
    r.roots.push(v.clone());
    Ok(())
}

fn as_mut_array<'a, 'p>(v: &'a Value<'p>) -> Result<&'a Rc<RefCell<Vec<Value<'p>>>>, Error> {
    match v {
        Value::MutArray(a) => Ok(a),
        other => err(format!("expected a mutable array, got {other}")),
    }
}

fn as_array<'a, 'p>(v: &'a Value<'p>) -> Result<&'a Rc<Vec<Value<'p>>>, Error> {
    match v {
        Value::Array(a) => Ok(a),
        other => err(format!("expected an Array, got {other}")),
    }
}

fn to_num(v: &Value) -> Option<Num> {
    Some(match v {
        Value::Int(x) => Num::Int(*x),
        Value::Word(w, b) => Num::Word(*w, *b),
        Value::BigInt(x) => Num::Big((**x).clone()),
        Value::Float(x) => Num::Float(*x),
        Value::Float32(x) => Num::Float32(*x),
        _ => return None,
    })
}

fn from_num<'p>(n: Num) -> Value<'p> {
    match n {
        Num::Int(x) => Value::Int(x),
        Num::Word(w, b) => Value::Word(w, b),
        Num::Big(x) => Value::BigInt(Rc::new(x)),
        Num::Float(x) => Value::Float(x),
        Num::Float32(x) => Value::Float32(x),
    }
}

fn arith(op: Prim) -> Arith {
    match op {
        Prim::Add | Prim::AddF => Arith::Add,
        Prim::Sub | Prim::SubF => Arith::Sub,
        Prim::Mul | Prim::MulF => Arith::Mul,
        Prim::Div | Prim::DivF => Arith::Div,
        Prim::Mod => Arith::Mod,
        _ => Arith::Pow,
    }
}

fn cmp(op: Prim) -> Cmp {
    match op {
        Prim::Lt | Prim::LtF => Cmp::Lt,
        Prim::Gt | Prim::GtF => Cmp::Gt,
        Prim::Le | Prim::LeF => Cmp::Le,
        _ => Cmp::Ge,
    }
}

fn bits(op: Prim) -> Bits {
    match op {
        Prim::Shl => Bits::Shl,
        Prim::Shr => Bits::Shr,
        Prim::Ushr => Bits::Ushr,
        Prim::BitAnd => Bits::And,
        Prim::BitOr => Bits::Or,
        _ => Bits::Xor,
    }
}

/// A string argument of primitive `what`.
fn text_arg(v: &Value, what: &str) -> Result<InternedString, Error> {
    match v {
        Value::Str(s) => Ok(*s),
        other => err(format!("{what}: expected a String, got {other}")),
    }
}

fn as_int(v: &Value) -> Result<i64, Error> {
    match v {
        Value::Int(i) => Ok(*i),
        other => err(format!("expected an Int, got {other}")),
    }
}

fn as_index(v: &Value) -> Result<usize, Error> {
    match as_int(v)? {
        i if i >= 0 => Ok(i as usize),
        i => err(format!("expected a non-negative index, got {i}")),
    }
}

fn bytes_of(v: &Value, what: &str) -> Result<Vec<u8>, Error> {
    let a = as_array(v)?;
    let mut buf = Vec::with_capacity(a.len());
    for x in a.iter() {
        match x {
            Value::Word(Width::U8, b) => buf.push(*b as u8),
            // A literal in code generic over its integer type -- see
            // `meadow_core::num`.
            Value::Int(n) if (0..=255).contains(n) => buf.push(*n as u8),
            other => return err(format!("`{what}`: not a byte: {other}")),
        }
    }
    Ok(buf)
}

impl std::fmt::Display for Value<'_> {
    /// Deliberately the same rendering as the CEK's. Two reasons, and the second
    /// is the load-bearing one: differential tests compare printed results, and
    /// `show` is a primitive, so a program can observe this.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
            Value::BigInt(n) => write!(f, "{n}"),
            Value::Float(x) => f.write_str(&meadow_rt::fmt_float(*x)),
            Value::Word(w, b) => write!(f, "{}", w.value(*b)),
            Value::Float32(x) => f.write_str(&num::fmt_float32(*x)),
            Value::Bool(b) => f.write_str(if *b { "True" } else { "False" }),
            Value::Str(s) => write!(f, "{:?}", &**s),
            Value::Char(c) => write!(f, "{c:?}"),
            Value::Unit => f.write_str("()"),
            Value::Array(items) => {
                f.write_str("#[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str("]")
            }
            Value::Record(map) => {
                f.write_str("{ ")?;
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k} = {v}")?;
                }
                f.write_str(" }")
            }
            Value::Data(name, _, fields) if &**name == TUPLE => {
                f.write_str("(")?;
                for (i, v) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")
            }
            // `Std.Collections.List` prints in its own literal syntax: `[1; 2]`.
            Value::Data(name, _, _) if plays(name, Role::Nil) || plays(name, Role::Cons) => {
                if let Some(xs) = list_items(self) {
                    f.write_str("[")?;
                    for (i, v) in xs.iter().enumerate() {
                        if i > 0 {
                            f.write_str("; ")?;
                        }
                        write!(f, "{v}")?;
                    }
                    return f.write_str("]");
                }
                write!(f, "{}(..)", bare_ctor(name))
            }
            // `Std.Collections.Vector` prints like a list: `[1, 2]`.
            Value::Data(name, _, _) if is_vector_ctor(name) => {
                if let Some(xs) = vector_elems(self) {
                    f.write_str("[")?;
                    for (i, v) in xs.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{v}")?;
                    }
                    return f.write_str("]");
                }
                write!(f, "{}(..)", bare_ctor(name))
            }
            Value::Data(name, _, fields) if fields.is_empty() => {
                write!(f, "{}", bare_ctor(name))
            }
            Value::Data(name, _, fields) => {
                write!(f, "{}(", bare_ctor(name))?;
                for (i, v) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")
            }
            Value::Ref(cell) => write!(f, "ref {}", cell.borrow()),
            Value::MutArray(cells) => {
                f.write_str("mut #[")?;
                for (i, v) in cells.borrow().iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str("]")
            }
            Value::Obj(_) => f.write_str("<closure>"),
            Value::Halt => f.write_str("<halt>"),
            Value::Compact(c) => write!(f, "compact {}", c.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Def, Label, VarId};

    fn v(n: u32) -> Name {
        VarId(n)
    }

    fn program(body: Statement) -> Program {
        Program {
            exports: Default::default(),
            returns: Default::default(),
            frames: Default::default(),
            continuations: Default::default(),
            ctor_fields: Default::default(),
            roles: Default::default(),
            reps: Default::default(),
            origins: Default::default(),
            results: Default::default(),
            threads: Default::default(),
            pure: Default::default(),
            types: Default::default(),
            ctor_types: Default::default(),
            migrations: Default::default(),
            defs: vec![Def {
                label: Label(0),
                name: InternedString::from("main"),
                module: InternedString::default(),
                block: Block {
                    params: vec![v(0)],
                    body,
                },
            }],
            entry: Some(Label(0)),
            tags: Default::default(),
        }
    }

    /// `substitute [k, x] in {(k, x) => invoke k#0}` — the universal return.
    fn ret(k: Name, x: Name) -> Statement {
        Statement::Substitute(
            vec![k, x],
            Box::new(Block {
                params: vec![k, x],
                body: Statement::Invoke(k, 0),
            }),
        )
    }

    #[test]
    fn a_program_ends_by_invoking_the_continuation_it_was_given() {
        // extern lit 7 -> (x, k); substitute [k, x] in { invoke k#0 }
        let p = program(Statement::Extern {
            op: Extern::Lit(Lit::Int(7)),
            args: vec![],
            blocks: vec![Block {
                params: vec![v(1), v(0)],
                body: ret(v(0), v(1)),
            }],
        });
        let out = Machine::run(&p, 100).expect("should finish");
        assert_eq!(out.to_string(), "7");
    }

    #[test]
    fn a_block_that_does_not_cover_the_environment_is_rejected() {
        // The extern prepends its result to an environment of one, so the
        // continuation must take two parameters. Taking one is the exact mistake
        // a lowering with a wrong liveness set makes, and it has to be loud —
        // otherwise the register assignment the IR claims to encode is a
        // fiction.
        let p = program(Statement::Extern {
            op: Extern::Lit(Lit::Int(7)),
            args: vec![],
            blocks: vec![Block {
                params: vec![v(1)],
                body: Statement::Error("unreachable"),
            }],
        });
        let e = Machine::run(&p, 100).expect_err("should be rejected");
        assert!(
            e.msg.contains("1 parameters") && e.msg.contains("2 values"),
            "{}",
            e.msg
        );
    }

    #[test]
    fn switch_dispatches_on_the_tag_and_binds_the_fields() {
        // let d = B#1(x); switch d { #1 (field, d, x, k) => return field, … }
        //
        // The scrutinee's fields go on the front and the scrutinee stays, so the
        // arm sees four values where the environment had three. Writing it out
        // is the clearest statement of what `switch` does — and of why an arm
        // body may still name the thing it matched on.
        let inner = Statement::Switch {
            scrutinee: v(2),
            arms: vec![(
                1,
                Block {
                    params: vec![v(3), v(2), v(1), v(0)],
                    body: ret(v(0), v(3)),
                },
            )],
            default: Box::new(Block {
                params: vec![v(2), v(1), v(0)],
                body: Statement::Error("wrong arm"),
            }),
        };
        let p = program(Statement::Extern {
            op: Extern::Lit(Lit::Int(5)),
            args: vec![],
            blocks: vec![Block {
                params: vec![v(1), v(0)],
                body: Statement::Let {
                    name: v(2),
                    tag: 1,
                    ctor: InternedString::from("B"),
                    fields: vec![v(1)],
                    rest: Box::new(inner),
                },
            }],
        });
        assert_eq!(Machine::run(&p, 100).unwrap().to_string(), "5");
    }

    #[test]
    fn a_switch_with_no_matching_arm_takes_the_default() {
        let inner = Statement::Switch {
            scrutinee: v(2),
            arms: vec![(
                9,
                Block {
                    params: vec![v(3), v(1), v(0)],
                    body: Statement::Error("wrong arm"),
                },
            )],
            default: Box::new(Block {
                params: vec![v(2), v(1), v(0)],
                body: ret(v(0), v(1)),
            }),
        };
        let p = program(Statement::Extern {
            op: Extern::Lit(Lit::Int(5)),
            args: vec![],
            blocks: vec![Block {
                params: vec![v(1), v(0)],
                body: Statement::Let {
                    name: v(2),
                    tag: 1,
                    ctor: InternedString::from("B"),
                    fields: vec![v(1)],
                    rest: Box::new(inner),
                },
            }],
        });
        assert_eq!(Machine::run(&p, 100).unwrap().to_string(), "5");
    }

    #[test]
    fn fuel_bounds_a_loop_instead_of_hanging_a_test() {
        // `jump` leaves the environment alone, so this is a genuine infinite
        // loop with no growth — nothing else would ever stop it.
        let p = program(Statement::Jump(Label(0)));
        let e = Machine::run(&p, 100).expect_err("should run out");
        assert!(e.msg.contains("100 steps"), "{}", e.msg);
    }

    #[test]
    fn data_prints_the_way_the_cek_prints_it() {
        let d = Value::Data(
            InternedString::from("Rect"),
            0,
            Rc::new(Fields::new(vec![Value::Int(3), Value::Int(4)])),
        );
        assert_eq!(d.to_string(), "Rect(3, 4)");
        let t = Value::Data(
            InternedString::from("#tuple"),
            0,
            Rc::new(Fields::new(vec![Value::Int(1), Value::Unit])),
        );
        assert_eq!(t.to_string(), "(1, ())");
        set_roles(&HashMap::from([
            (Role::Nil, InternedString::from("List.Nil")),
            (Role::Cons, InternedString::from("List.Cons")),
        ]));
        // A `List` prints as its elements, not as the chain it is.
        let nil = Value::Data(
            InternedString::from("List.Nil"),
            0,
            Rc::new(Fields::new(vec![])),
        );
        let one = Value::Data(
            InternedString::from("List.Cons"),
            1,
            Rc::new(Fields::new(vec![Value::Int(1), nil])),
        );
        assert_eq!(one.to_string(), "[1]");
    }

    #[test]
    fn dropping_a_long_chain_does_not_recurse() {
        // 200_000 deep — the shape that aborted the CEK before its `Drop` was
        // made iterative.
        let mut xs = Value::Data(
            InternedString::from("List.Nil"),
            0,
            Rc::new(Fields::new(vec![])),
        );
        for i in 0..200_000 {
            xs = Value::Data(
                InternedString::from("List.Cons"),
                1,
                Rc::new(Fields::new(vec![Value::Int(i), xs])),
            );
        }
        assert!(value_eq(&xs, &xs.clone()));
        drop(xs);
    }
}
