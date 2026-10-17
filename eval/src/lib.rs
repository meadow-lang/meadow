//! A **CEK abstract machine** for `meadow_core`.
//!
//! The evaluator is an explicit `(Control, Environment, Kontinuation)` loop rather
//! than a recursive tree-walk. The continuation is a `Vec<K>` of stack frames —
//! one per "hole" in a partly-evaluated term — which is what makes algebraic
//! effects implementable:
//!
//! * `perform E.op arg` scans the kontinuation top-down for the nearest matching
//!   `HandleMark`, **splits the stack there**, and hands the sliced-off prefix to
//!   the handler clause as a resumption ([`Value::Cont`]).
//! * resuming (calling that `Value::Cont`) splices the captured frames back on.
//!
//! Handlers are **deep** (the captured slice includes the `HandleMark`, so a
//! resumption re-enters under the same handler) and **one-shot** (each `Cont` may
//! be resumed at most once — enforced by a `take`n `Option`).

use meadow_core as core;
use meadow_core::fmt_float;
use meadow_core::num;
use meadow_intern::InternedString;
use num_bigint::BigInt;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

type Term = core::Term;
type Var = core::Var;

#[derive(Debug, Clone)]
pub enum Value {
    /// Fixed-width integer (`Int`, i.e. i64).
    Int(i64),
    /// Arbitrary-precision integer (`BigInt`) — produced by `toBigInt` and the
    /// `~`-suffixed operators, never by a literal.
    BigInt(BigInt),
    Float(f64),
    /// A sized integer: its width, and its bits masked to it -- immediate, like
    /// an `Int`.
    Word(num::Width, u64),
    Float32(f32),
    Bool(bool),
    Str(InternedString),
    /// A single Unicode scalar. `String` is a byte sequence, so the two are
    /// bridged explicitly by `stringToChars` / `charsToString`.
    Char(char),
    Unit,
    Tuple(Vec<Value>),
    /// The one builtin collection: a persistent, `Rc`-shared contiguous buffer.
    /// `Rc` gives O(1) clone and structural sharing.
    ///
    /// **`arraySet` / `arrayPush` / `arrayPop` copy the whole buffer, always.**
    /// They call `Rc::make_mut`, but it never finds the buffer uniquely held:
    /// the argument vector holds one reference for the duration of the call and
    /// the caller's environment usually holds another. Writing an `n`-element
    /// array one push at a time is therefore O(n²) — `Std.Collections.Vector`
    /// only gets away with it because its chunks are 32 elements. Anything
    /// building a full-length array should go through `Vector` (chunked, so
    /// linear) rather than accumulating with `arrayPush`.
    ///
    /// Making the in-place case actually happen needs last-use information the
    /// evaluator does not have — moving out of `args` alone is not enough while
    /// the environment still holds the binding.
    ///
    /// `List` and `Vector` are *not* here — they are ordinary `Std` data types
    /// and reach the runtime as [`Value::Ctor`] chains.
    Array(Rc<Vec<Value>>),
    Record(BTreeMap<InternedString, Value>),
    /// A data constructor and its fields.
    ///
    /// The payload is shared, like [`Value::Array`]'s, and for the same reason
    /// twice over. `List` is a `Cons` chain, so matching `| Cons x rest ->` binds
    /// `rest` on every step: with a plain `Vec` that copied the whole remaining
    /// list, making a walk O(n²) and — because `Value::clone` recurses once per
    /// element — overflowing the *Rust* stack at about 2000 elements, aborting
    /// the process rather than raising an error. With an `Rc` both go away:
    /// binding a tail is a refcount bump.
    ///
    /// Use [`Value::ctor`] to build one.
    Ctor(InternedString, Rc<Fields>),
    Closure {
        param: Var,
        body: Arc<Term>,
        env: Env,
    },
    /// A partially applied primitive.
    Builtin {
        op: core::Prim,
        args: Vec<Value>,
    },
    /// A captured (one-shot, deep) continuation — a slice of stack frames.
    Cont(Rc<RefCell<Option<Vec<K>>>>),
    /// The one mutable cell, from `newRef`.
    ///
    /// Unlike every other value here this one has *identity*: two `Ref`s are the
    /// same cell only if they are the same allocation. That is what makes
    /// equality on them compare pointers rather than contents — `newRef 1` twice
    /// gives two cells that hold the same thing and are not the same cell.
    Ref(Rc<RefCell<Value>>),
    /// A mutable array, from `stNewArray`. Identity, like a [`Value::Ref`]: it
    /// is changed in place, and two of them are equal only if they are one.
    MutArray(Rc<RefCell<Vec<Value>>>),
    /// A value in a compact region, and the region. Reference counting has no
    /// collector to spare the work, so nothing is copied: a region is only the
    /// bookkeeping that makes `compactSize` agree with the VM's. See
    /// `meadow_core::compact`.
    Compact(CompactCell),
    /// A channel between green threads, by its number in the scheduler.
    Channel(u32),
    /// A green thread, as `threadSpawn` answers it.
    Task(u32),
    /// A `TVar`: one variable every thread shares, changed only by a commit.
    TVar(Rc<TVarCell>),
    /// A top-level value not evaluated yet: its definition. Only ever in the
    /// root environment -- looking a name up forces it, and the value replaces
    /// it there -- so a program never holds one. See `core::globals`.
    Lazy(Arc<Term>),
}

/// A `TVar`'s state: the clock value of the commit that last wrote it, and
/// what it holds. Values here are immutable and shared, so a commit only has to
/// swap which one it is. See `meadow_core::stm`.
#[derive(Debug)]
pub struct TVarCell {
    version: std::cell::Cell<u64>,
    value: RefCell<Value>,
}

/// A transaction in progress: when it started, what it read and what it wrote,
/// innermost `orElse` last.
struct Txn {
    start: u64,
    reads: Vec<(Rc<TVarCell>, u64)>,
    writes: Vec<Vec<(Rc<TVarCell>, Value)>>,
}

impl Txn {
    fn write(&mut self, tv: &Rc<TVarCell>, v: Value) {
        let frame = self.writes.last_mut().expect("a frame");
        match frame.iter_mut().find(|(t, _)| Rc::ptr_eq(t, tv)) {
            Some(slot) => slot.1 = v,
            None => frame.push((tv.clone(), v)),
        }
    }
}

thread_local! {
    /// Commits so far. This machine runs every thread on one OS thread, so the
    /// clock can be this OS thread's.
    static STM_CLOCK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A `Compact`: the value, and the region it was put in.
pub type CompactCell = Rc<(Value, Rc<RefCell<Region>>)>;

/// What a compact region holds, as far as sizing it goes: every value put in
/// (which keeps each node alive, so an address counted is never reused), the
/// nodes already counted, and the VM slots they add up to.
#[derive(Debug, Default)]
pub struct Region {
    roots: Vec<Value>,
    seen: std::collections::HashSet<*const ()>,
    slots: usize,
}

impl Value {
    /// Build a constructor value.
    ///
    /// The one place `Value::Ctor` is assembled, so the sharing is not something
    /// a caller has to remember.
    pub fn ctor(name: InternedString, args: Vec<Value>) -> Value {
        Value::Ctor(name, Rc::new(Fields(args)))
    }
}

/// A constructor's fields.
///
/// A newtype only so that [`Drop`] can be implemented on it. Dropping a `List`
/// is dropping a `Cons` whose second field is another `Cons`, so the derived
/// drop glue recurses once per element and overflows the stack on a list of a
/// few thousand — the same shape of bug as the recursive `clone` that sharing
/// fixed, and not fixed by sharing.
///
/// It cannot go on `Value` itself: a type that implements `Drop` cannot be
/// destructured by move, and the evaluator does that everywhere.
#[derive(Debug, Clone)]
pub struct Fields(Vec<Value>);

/// So `args.len()`, `args[0]` and `args.iter()` keep working on a `Fields`.
impl std::ops::Deref for Fields {
    type Target = Vec<Value>;
    fn deref(&self) -> &Vec<Value> {
        &self.0
    }
}

impl Drop for Fields {
    fn drop(&mut self) {
        // Dismantle iteratively: move each child out, and if it is the last
        // reference to another node, move *its* children onto the same worklist
        // rather than letting the drop nest.
        let mut stack: Vec<Value> = std::mem::take(&mut self.0);
        while let Some(v) = stack.pop() {
            if let Value::Ctor(_, rc) = v
                && let Ok(mut fields) = Rc::try_unwrap(rc)
            {
                // Emptied before it drops, so its own `drop` finds nothing to do.
                stack.append(&mut fields.0);
            }
        }
    }
}

#[derive(Debug)]
pub struct RuntimeError {
    pub msg: String,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "runtime error: {}", self.msg)
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, RuntimeError> {
    Err(RuntimeError { msg: msg.into() })
}

// --- environments ----------------------------------------------------------

#[derive(Debug)]
pub struct Frame {
    slots: RefCell<Vec<(Var, Value)>>,
    parent: Option<Env>,
}

pub type Env = Rc<Frame>;

fn root_env() -> Env {
    Rc::new(Frame {
        slots: RefCell::new(Vec::new()),
        parent: None,
    })
}

fn child(parent: &Env) -> Env {
    Rc::new(Frame {
        slots: RefCell::new(Vec::new()),
        parent: Some(parent.clone()),
    })
}

fn define(env: &Env, var: Var, val: Value) {
    env.slots.borrow_mut().push((var, val));
}

/// Replace `var`'s value in whichever frame binds it.
fn redefine(env: &Env, var: Var, val: Value) {
    let mut cur = Some(env.clone());
    while let Some(frame) = cur {
        if let Some(slot) = frame
            .slots
            .borrow_mut()
            .iter_mut()
            .rev()
            .find(|(k, _)| *k == var)
        {
            slot.1 = val;
            return;
        }
        cur = frame.parent.clone();
    }
}

/// The outermost frame: where top-level definitions live.
fn root_of(env: &Env) -> Env {
    let mut cur = env.clone();
    while let Some(parent) = cur.parent.clone() {
        cur = parent;
    }
    cur
}

fn lookup(env: &Env, var: Var) -> Option<Value> {
    let mut cur = Some(env.clone());
    while let Some(frame) = cur {
        if let Some((_, v)) = frame.slots.borrow().iter().rev().find(|(k, _)| *k == var) {
            return Some(v.clone());
        }
        cur = frame.parent.clone();
    }
    None
}

// --- the machine ---------------------------------------------------------

pub type FieldTable = std::collections::HashMap<InternedString, Vec<InternedString>>;

#[derive(Debug, Clone)]
pub struct HandlerData {
    clauses: Vec<core::HClause>,
    ret: Option<(Var, core::Ty, Arc<Term>)>,
}

/// One continuation frame: "given the value of the sub-expression currently being
/// evaluated, here is what to do next".
#[derive(Debug, Clone)]
pub enum K {
    /// `App`: the function is evaluated; evaluate this argument next.
    EvalArg {
        arg: Arc<Term>,
        env: Env,
    },
    /// `App`: the argument is evaluated; apply the saved function to it.
    ApplyTo {
        func: Value,
    },
    If {
        then: Arc<Term>,
        els: Arc<Term>,
        env: Env,
    },
    /// `Let` / `Lam` application: bind `var` to the incoming value, run `body`.
    Bind {
        var: Var,
        body: Arc<Term>,
        env: Env,
    },
    LetRec {
        scope: Env,
        pending: Vec<(Var, Term)>,
        body: Arc<Term>,
    },
    BuildTuple {
        done: Vec<Value>,
        pending: Vec<Term>,
        env: Env,
    },
    BuildArray {
        done: Vec<Value>,
        pending: Vec<Term>,
        env: Env,
    },
    BuildCtor {
        name: InternedString,
        done: Vec<Value>,
        pending: Vec<Term>,
        env: Env,
    },
    BuildPrim {
        op: core::Prim,
        done: Vec<Value>,
        pending: Vec<Term>,
        env: Env,
    },
    BuildRecord {
        done: Vec<(InternedString, Value)>,
        pending: Vec<(InternedString, Term)>,
        env: Env,
    },
    Proj(usize),
    Sel(InternedString),
    /// `Extend`: the record is evaluated; evaluate the new field value.
    ExtendVal {
        label: InternedString,
        val: Arc<Term>,
        env: Env,
    },
    /// `Extend`: the field value is evaluated; insert it into the saved record.
    ExtendWith {
        label: InternedString,
        rec: Value,
    },
    Match {
        arms: Arms,
        env: Env,
    },
    /// An arm's pattern matched and its guard is being evaluated, in `scope`:
    /// its body next if the guard holds, and arm `next` on `scrutinee` if not.
    Guard {
        arms: Arms,
        env: Env,
        next: usize,
        scrutinee: Value,
        scope: Env,
    },
    /// `Perform`: the operation argument is evaluated; unwind to a handler.
    PerformWith {
        effect: InternedString,
        op: InternedString,
    },
    /// A handler boundary sitting on the stack.
    HandleMark(Rc<HandlerData>, Env),
    /// A top-level value is being evaluated for the first time; keep it.
    Cache {
        var: Var,
        root: Env,
    },
}

enum Control {
    Eval(Arc<Term>, Env),
    Ret(Value),
}

struct Machine<'a> {
    ctrl: Control,
    kont: Vec<K>,
    fields: &'a FieldTable,
    /// A thread operation just made, for the scheduler to carry out. The
    /// machine stops as soon as one is here -- see [`schedule`].
    request: Option<Request>,
    /// What this thread was waiting on failed with this message, and so does
    /// the thread, when it next runs.
    failed: Option<String>,
    /// The transaction this thread is in, if any.
    txn: Option<Txn>,
}

/// What a thread operation asks of the scheduler.
enum Request {
    Spawn(Value),
    Await(u32),
    Yield,
    NewChannel,
    Send(u32, Value),
    Receive(u32),
    StmCommit,
    StmWait,
}

impl<'a> Machine<'a> {
    fn new(ctrl: Control, kont: Vec<K>, fields: &'a FieldTable) -> Machine<'a> {
        Machine {
            ctrl,
            kont,
            fields,
            request: None,
            failed: None,
            txn: None,
        }
    }
}

pub fn run(program: &core::Program) -> Result<Value, RuntimeError> {
    let env = load(program)?;
    match program.entry {
        // Through a mention, so the entry point is evaluated the way any use
        // of a top-level value is.
        Some(e) => Machine::new(
            Control::Eval(Arc::new(Term::Var(e)), env),
            Vec::new(),
            &program.ctor_fields,
        )
        .run(),
        None => Ok(Value::Unit),
    }
}

/// Evaluate `term` against `program`'s definitions, giving up after `fuel`
/// steps.
///
/// What a procedural macro is run with. A macro runs while its caller is being
/// compiled, so two things have to be true of it that are not true of a
/// program: it must stop, and it must not start anything -- a thread or a
/// transaction is refused rather than scheduled, since there is no program
/// here for one to belong to.
pub fn eval_with_fuel(
    program: &core::Program,
    term: Arc<Term>,
    fuel: u64,
) -> Result<Value, RuntimeError> {
    let env = load(program)?;
    let mut m = Machine::new(Control::Eval(term, env), Vec::new(), &program.ctor_fields);
    let mut used: u64 = 0;
    loop {
        match m.run_slice() {
            Stop::Done(v) => return Ok(v),
            Stop::Failed(e) => return Err(e),
            Stop::Preempted => {
                used = used.saturating_add(SLICE as u64);
                if used >= fuel {
                    return Err(RuntimeError {
                        msg: format!("did not finish within {fuel} steps"),
                    });
                }
            }
            Stop::Requested(_) => {
                return Err(RuntimeError {
                    msg: "started a thread or a transaction, which it may not".into(),
                });
            }
        }
    }
}

/// Load the program, then call each of `tests` with `()`, in order.
///
/// One load for the whole run, so top-level definitions are evaluated once and
/// the tests share them — the same thing `run` does before reading its entry
/// point. Each test gets its own result: a failing one does not stop the rest.
pub fn run_tests(
    program: &core::Program,
    tests: &[core::Var],
) -> Result<Vec<Result<Value, RuntimeError>>, RuntimeError> {
    run_tests_watched(program, tests, &mut |_, _| {})
}

/// [`run_tests`], telling `each` about a result as it lands.
///
/// A run of a thousand tests says nothing for a minute otherwise, and what it
/// is doing is exactly what someone waiting wants to know.
pub fn run_tests_watched(
    program: &core::Program,
    tests: &[core::Var],
    each: &mut dyn FnMut(usize, &Result<Value, RuntimeError>),
) -> Result<Vec<Result<Value, RuntimeError>>, RuntimeError> {
    let env = load(program)?;
    Ok(tests
        .iter()
        .enumerate()
        .map(|(i, &var)| {
            let Some(f) = lookup(&env, var) else {
                return Err(RuntimeError {
                    msg: "test not found".into(),
                });
            };
            // Start with the function already evaluated and `()` queued as its
            // argument — the same state the machine reaches part-way through an
            // `App`, so applying it needs no new machinery.
            let m = Machine::new(
                Control::Ret(f),
                vec![K::EvalArg {
                    arg: Arc::new(core::Term::Lit(core::Lit::Unit)),
                    env: env.clone(),
                }],
                &program.ctor_fields,
            );
            let out = m.run();
            each(i, &out);
            out
        })
        .collect())
}

/// How a test went, away from the machine that ran it: its value rendered, or
/// its failure, and what it printed if that was being kept.
pub struct Told {
    pub result: Result<String, String>,
    pub output: String,
}

/// [`run_tests_watched`] on `threads` OS threads, each test's output kept --
/// or written as it comes, if `capture` is false.
///
/// A value of this machine is reference-counted and cannot leave the thread
/// that made it, so each thread loads the program for itself and hands back
/// renderings. `each` is called from whichever thread finished a test.
pub fn run_tests_parallel(
    program: &core::Program,
    tests: &[core::Var],
    threads: usize,
    capture: bool,
    each: &(dyn Fn(usize, &Told) + Sync),
) -> Result<Vec<Told>, RuntimeError> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<Told>>> =
        tests.iter().map(|_| std::sync::Mutex::new(None)).collect();
    let failed: std::sync::Mutex<Option<RuntimeError>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..threads.clamp(1, tests.len().max(1)) {
            // As much stack as the process's main thread has, which is what
            // the machine's recursion was written against.
            let spawned = std::thread::Builder::new()
                .stack_size(64 << 20)
                .spawn_scoped(scope, || {
                    let env = match load(program) {
                        Ok(env) => env,
                        Err(e) => {
                            *failed.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                            return;
                        }
                    };
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&var) = tests.get(i) else { return };
                        CAPTURED.with(|c| *c.borrow_mut() = capture.then(String::new));
                        let result = match lookup(&env, var) {
                            None => Err("test not found".to_string()),
                            Some(f) => Machine::new(
                                Control::Ret(f),
                                vec![K::EvalArg {
                                    arg: Arc::new(core::Term::Lit(core::Lit::Unit)),
                                    env: env.clone(),
                                }],
                                &program.ctor_fields,
                            )
                            .run()
                            .map(|v| v.to_string())
                            .map_err(|e| e.msg),
                        };
                        let output = CAPTURED.with(|c| c.borrow_mut().take()).unwrap_or_default();
                        let told = Told { result, output };
                        each(i, &told);
                        *slots[i].lock().unwrap_or_else(|p| p.into_inner()) = Some(told);
                    }
                });
            if let Err(e) = spawned {
                *failed.lock().unwrap_or_else(|p| p.into_inner()) = Some(RuntimeError {
                    msg: format!("could not start a test thread: {e}"),
                });
            }
        }
    });
    if let Some(e) = failed.into_inner().unwrap_or_else(|p| p.into_inner()) {
        return Err(e);
    }
    Ok(slots
        .into_iter()
        .map(|s| {
            s.into_inner()
                .unwrap_or_else(|p| p.into_inner())
                .unwrap_or_else(|| Told {
                    result: Err("the test did not run".into()),
                    output: String::new(),
                })
        })
        .collect())
}

/// Every top-level definition, in a fresh environment.
///
/// Nothing is evaluated here but functions, whose values are only closures. A
/// top-level value is left [`Value::Lazy`] and evaluated the first time it is
/// used, once -- which is what the other engines do, and what makes a
/// definition nobody uses cost nothing and fail nothing. It is also why loading
/// the program for `meadow test` does not run `main`.
fn load(program: &core::Program) -> Result<Env, RuntimeError> {
    // This machine is untyped, like every other backend: core's type
    // abstractions come off first. One pass, once per program load.
    // Number-generic definitions are copied per number type first, while the
    // types that say which are still there -- see `core::specialize`.
    let program = &core::erase::program(&core::specialize::program(program));
    let env = root_env();
    for def in &program.defs {
        let value = match peel(&def.term) {
            Term::Lam(param, _, body) => Value::Closure {
                param: *param,
                body: body.clone(),
                env: env.clone(),
            },
            _ => Value::Lazy(Arc::new(def.term.clone())),
        };
        define(&env, def.var, value);
    }
    Ok(env)
}

/// A definition's term without the positions a debug build wraps it in.
fn peel(t: &Term) -> &Term {
    match t {
        Term::Loc(_, inner) => peel(inner),
        other => other,
    }
}

/// Instructions -- steps, here -- a thread takes before another gets a turn.
const SLICE: usize = 2048;

/// Why a thread stopped running.
enum Stop {
    Done(Value),
    Failed(RuntimeError),
    Requested(Request),
    Preempted,
}

impl<'a> Machine<'a> {
    /// Run to the end as the main thread, with any threads it starts.
    fn run(self) -> Result<Value, RuntimeError> {
        schedule(self)
    }

    /// Run for a slice, or until the thread finishes, fails or asks for
    /// something only the scheduler can do.
    fn run_slice(&mut self) -> Stop {
        if let Some(msg) = self.failed.take() {
            return Stop::Failed(RuntimeError { msg });
        }
        for _ in 0..SLICE {
            if self.kont.is_empty() {
                if let Control::Ret(_) = &self.ctrl {
                    match std::mem::replace(&mut self.ctrl, Control::Ret(Value::Unit)) {
                        Control::Ret(v) => return Stop::Done(v),
                        Control::Eval(..) => unreachable!(),
                    }
                }
            }
            if let Err(e) = self.step() {
                return Stop::Failed(e);
            }
            if let Some(r) = self.request.take() {
                return Stop::Requested(r);
            }
        }
        Stop::Preempted
    }

    /// A saturated primitive. The thread operations only ask: see [`schedule`].
    fn prim(&mut self, op: core::Prim, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use core::Prim::*;
        let handle = |v: &Value, what: &str, want: fn(&Value) -> Option<u32>| match want(v) {
            Some(id) => Ok(id),
            None => err(format!("expected {what}, got {v}")),
        };
        let request = match op {
            ThreadSpawn => {
                sendable(&args[0])?;
                Request::Spawn(args[0].clone())
            }
            ThreadAwait => Request::Await(handle(&args[0], "a thread", |v| match v {
                Value::Task(id) => Some(*id),
                _ => None,
            })?),
            ThreadYield => Request::Yield,
            ChannelNew => Request::NewChannel,
            ChannelSend => {
                let id = handle(&args[0], "a channel", |v| match v {
                    Value::Channel(id) => Some(*id),
                    _ => None,
                })?;
                sendable(&args[1])?;
                Request::Send(id, args[1].clone())
            }
            ChannelReceive => Request::Receive(handle(&args[0], "a channel", |v| match v {
                Value::Channel(id) => Some(*id),
                _ => None,
            })?),
            StmNew => {
                storable(&args[0])?;
                return Ok(Value::TVar(Rc::new(TVarCell {
                    version: std::cell::Cell::new(0),
                    value: RefCell::new(args[0].clone()),
                })));
            }
            StmRead => {
                let tv = as_tvar(&args[0])?;
                let txn = self.txn.as_mut().ok_or_else(|| RuntimeError {
                    msg: core::stm::outside("readTVar"),
                })?;
                for frame in txn.writes.iter().rev() {
                    if let Some((_, v)) = frame.iter().find(|(t, _)| Rc::ptr_eq(t, &tv)) {
                        return Ok(Value::ctor("Maybe.Just".into(), vec![v.clone()]));
                    }
                }
                if tv.version.get() > txn.start {
                    return Ok(Value::ctor("Maybe.None".into(), vec![]));
                }
                if !txn.reads.iter().any(|(t, _)| Rc::ptr_eq(t, &tv)) {
                    txn.reads.push((tv.clone(), tv.version.get()));
                }
                let v = tv.value.borrow().clone();
                return Ok(Value::ctor("Maybe.Just".into(), vec![v]));
            }
            StmWrite => {
                let tv = as_tvar(&args[0])?;
                storable(&args[1])?;
                let txn = self.txn.as_mut().ok_or_else(|| RuntimeError {
                    msg: core::stm::outside("writeTVar"),
                })?;
                txn.write(&tv, args[1].clone());
                return Ok(Value::Unit);
            }
            StmBegin => {
                let start = STM_CLOCK.with(|c| c.get());
                self.txn = Some(Txn {
                    start,
                    reads: Vec::new(),
                    writes: vec![Vec::new()],
                });
                return Ok(Value::Unit);
            }
            StmNest | StmMerge | StmRollback => {
                let txn = self.txn.as_mut().ok_or_else(|| RuntimeError {
                    msg: core::stm::outside("orElse"),
                })?;
                match op {
                    StmNest => txn.writes.push(Vec::new()),
                    StmMerge if txn.writes.len() > 1 => {
                        let inner = txn.writes.pop().expect("a nested frame");
                        for (tv, v) in inner {
                            txn.write(&tv, v);
                        }
                    }
                    StmRollback if txn.writes.len() > 1 => {
                        txn.writes.pop();
                    }
                    _ => {}
                }
                return Ok(Value::Unit);
            }
            StmCommit => {
                if self.txn.is_none() {
                    return err(core::stm::outside("atomically"));
                }
                Request::StmCommit
            }
            StmWait => {
                if self.txn.is_none() {
                    return err(core::stm::outside("retry"));
                }
                Request::StmWait
            }
            _ => return run_prim(op, args),
        };
        self.request = Some(request);
        // A placeholder: the scheduler replaces it with the answer.
        Ok(Value::Unit)
    }

    fn step(&mut self) -> Result<(), RuntimeError> {
        match std::mem::replace(&mut self.ctrl, Control::Ret(Value::Unit)) {
            Control::Eval(term, env) => self.eval(term, env),
            Control::Ret(v) => self.ret(v),
        }
    }

    /// Decompose a term: push frames for its sub-expressions, then start on the
    /// first one; or, for a value form, return it directly.
    fn eval(&mut self, term: Arc<Term>, env: Env) -> Result<(), RuntimeError> {
        use core::Term as T;
        match &*term {
            T::Var(v) => {
                let val = lookup(&env, *v).ok_or_else(|| RuntimeError {
                    msg: format!("unbound variable {v:?}"),
                })?;
                match val {
                    // The first use of a top-level value: evaluate it where
                    // top-level definitions live, and keep the result.
                    Value::Lazy(term) => {
                        let root = root_of(&env);
                        self.kont.push(K::Cache {
                            var: *v,
                            root: root.clone(),
                        });
                        self.ctrl = Control::Eval(term, root);
                    }
                    val => self.ctrl = Control::Ret(val),
                }
            }
            T::Lit(l) => self.ctrl = Control::Ret(lit_value(l)),
            // A position means nothing to a machine that cannot stop.
            T::Loc(_, inner) => return self.eval(inner.clone(), env),
            // Erased by `load_except`; this machine never sees a type.
            T::TyLam(..) | T::TyApp(..) => {
                unreachable!("a type abstraction reached the evaluator")
            }
            T::Lam(param, _, body) => {
                self.ctrl = Control::Ret(Value::Closure {
                    param: *param,
                    body: body.clone(),
                    env,
                });
            }
            T::App(f, a) => {
                self.kont.push(K::EvalArg {
                    arg: a.clone(),
                    env: env.clone(),
                });
                self.ctrl = Control::Eval(f.clone(), env);
            }
            T::Let(v, _, rhs, body) => {
                self.kont.push(K::Bind {
                    var: *v,
                    body: body.clone(),
                    env: env.clone(),
                });
                self.ctrl = Control::Eval(rhs.clone(), env);
            }
            // A join point is a `letrec` binding that happens to be entered only
            // by tail calls, and a jump is one of those calls. The CEK is the
            // reference semantics, so it runs them as exactly that rather than
            // refusing: a program that has been through `joins` means what it
            // meant before, and this is where that is checked.
            //
            // Nothing the machine does with the closure it makes here depends
            // on the promise a join point carries. The promise is for the back
            // end, which turns one into a label instead of an object.
            T::Join {
                var,
                params,
                ty,
                rhs,
                body,
            } => {
                let lam = params.iter().rev().fold((**rhs).clone(), |b, (v, t)| {
                    T::Lam(*v, t.clone(), Arc::new(b))
                });
                let bind = (*var, core::Poly::mono(ty.clone()), lam);
                let as_letrec = T::LetRec(vec![bind], body.clone());
                self.eval(Arc::new(as_letrec), env)?;
            }
            T::Jump(j, args, _) => {
                let call = args
                    .iter()
                    .fold(T::Var(*j), |f, a| T::App(Arc::new(f), Arc::new(a.clone())));
                self.eval(Arc::new(call), env)?;
            }
            T::LetRec(binds, body) => {
                let scope = child(&env);
                for (v, _, _) in binds {
                    define(&scope, *v, Value::Unit);
                }
                // `pending` holds the not-yet-evaluated binds, innermost last; the
                // last entry is always the one currently being evaluated (its slot
                // gets filled when its value comes back — see `K::LetRec` in `ret`).
                let pending: Vec<(Var, Term)> = binds
                    .iter()
                    .rev()
                    .map(|(v, _, t)| (*v, t.clone()))
                    .collect();
                match pending.last().cloned() {
                    Some((_, rhs)) => {
                        self.kont.push(K::LetRec {
                            scope: scope.clone(),
                            pending,
                            body: body.clone(),
                        });
                        self.ctrl = Control::Eval(Arc::new(rhs), scope);
                    }
                    None => {
                        let _ = pending;
                        self.ctrl = Control::Eval(body.clone(), scope);
                    }
                }
            }
            T::If(c, t, e) => {
                self.kont.push(K::If {
                    then: t.clone(),
                    els: e.clone(),
                    env: env.clone(),
                });
                self.ctrl = Control::Eval(c.clone(), env);
            }
            T::Tuple(items) => self.start_seq(items, env, SeqKind::Tuple),
            T::Array(items, _) => self.start_seq(items, env, SeqKind::Array),
            T::Ctor(name, _, args) => self.start_seq(args, env, SeqKind::Ctor(*name)),
            T::Prim(op, args, _) => self.start_seq(args, env, SeqKind::Prim(*op)),
            T::Record(fields) => {
                let pending: Vec<(InternedString, Term)> =
                    fields.iter().rev().map(|(l, t)| (*l, t.clone())).collect();
                match pending.last().cloned() {
                    Some((_, first)) => {
                        self.kont.push(K::BuildRecord {
                            done: vec![],
                            pending,
                            env: env.clone(),
                        });
                        self.ctrl = Control::Eval(Arc::new(first), env);
                    }
                    None => self.ctrl = Control::Ret(Value::Record(BTreeMap::new())),
                }
            }
            T::Proj(t, i) => {
                self.kont.push(K::Proj(*i));
                self.ctrl = Control::Eval(t.clone(), env);
            }
            T::Sel(t, label, _) => {
                self.kont.push(K::Sel(*label));
                self.ctrl = Control::Eval(t.clone(), env);
            }
            T::Extend(rec, label, val) => {
                self.kont.push(K::ExtendVal {
                    label: *label,
                    val: val.clone(),
                    env: env.clone(),
                });
                self.ctrl = Control::Eval(rec.clone(), env);
            }
            T::Case(scrut, arms, _) => {
                self.kont.push(K::Match {
                    arms: Rc::new(arms.clone()),
                    env: env.clone(),
                });
                self.ctrl = Control::Eval(scrut.clone(), env);
            }
            T::Perform(effect, op, arg, _) => {
                self.kont.push(K::PerformWith {
                    effect: *effect,
                    op: *op,
                });
                self.ctrl = Control::Eval(arg.clone(), env);
            }
            T::Handle {
                body, clauses, ret, ..
            } => {
                let data = Rc::new(HandlerData {
                    clauses: clauses.clone(),
                    ret: ret.clone(),
                });
                self.kont.push(K::HandleMark(data, env.clone()));
                self.ctrl = Control::Eval(body.clone(), env);
            }
            T::Error => return err("evaluating an ill-formed expression"),
        }
        Ok(())
    }

    fn start_seq(&mut self, items: &[Term], env: Env, kind: SeqKind) {
        let mut pending: Vec<Term> = items.iter().rev().cloned().collect();
        match pending.pop() {
            Some(first) => {
                let done = Vec::new();
                self.kont.push(match kind {
                    SeqKind::Tuple => K::BuildTuple {
                        done,
                        pending,
                        env: env.clone(),
                    },
                    SeqKind::Array => K::BuildArray {
                        done,
                        pending,
                        env: env.clone(),
                    },
                    SeqKind::Ctor(name) => K::BuildCtor {
                        name,
                        done,
                        pending,
                        env: env.clone(),
                    },
                    SeqKind::Prim(op) => K::BuildPrim {
                        op,
                        done,
                        pending,
                        env: env.clone(),
                    },
                });
                self.ctrl = Control::Eval(Arc::new(first), env);
            }
            None => {
                self.ctrl = Control::Ret(match kind {
                    SeqKind::Tuple => Value::Tuple(vec![]),
                    SeqKind::Array => Value::Array(Rc::new(vec![])),
                    SeqKind::Ctor(name) => Value::ctor(name, vec![]),
                    SeqKind::Prim(_) => Value::Unit, // prims always have args
                });
            }
        }
    }

    /// A value came back; pop the top frame and combine.
    /// Match `v` against the arms from `from` on: the first whose pattern
    /// matches, and whose guard -- evaluated next, if it has one -- holds.
    fn try_arms(
        &mut self,
        arms: Arms,
        env: Env,
        from: usize,
        v: Value,
    ) -> Result<(), RuntimeError> {
        for i in from..arms.len() {
            let (pat, guard, body) = &arms[i];
            let scope = child(&env);
            if !match_pat(pat, &v, &scope) {
                continue;
            }
            match guard {
                None => self.ctrl = Control::Eval(Arc::new(body.clone()), scope),
                Some(g) => {
                    let g = Arc::new(g.clone());
                    self.kont.push(K::Guard {
                        arms: arms.clone(),
                        env,
                        next: i + 1,
                        scrutinee: v,
                        scope: scope.clone(),
                    });
                    self.ctrl = Control::Eval(g, scope);
                }
            }
            return Ok(());
        }
        err("non-exhaustive pattern match")
    }

    fn ret(&mut self, v: Value) -> Result<(), RuntimeError> {
        let Some(frame) = self.kont.pop() else {
            self.ctrl = Control::Ret(v);
            return Ok(());
        };
        match frame {
            K::EvalArg { arg, env } => {
                self.kont.push(K::ApplyTo { func: v });
                self.ctrl = Control::Eval(arg, env);
            }
            K::ApplyTo { func } => self.apply(func, v)?,
            K::If { then, els, env } => match v {
                Value::Bool(true) => self.ctrl = Control::Eval(then, env),
                Value::Bool(false) => self.ctrl = Control::Eval(els, env),
                other => return err(format!("`if` condition is not a Bool: {other}")),
            },
            K::Bind { var, body, env } => {
                let scope = child(&env);
                define(&scope, var, v);
                self.ctrl = Control::Eval(body, scope);
            }
            K::LetRec {
                scope,
                mut pending,
                body,
            } => {
                // the just-evaluated bind is the last `pending` entry
                let (var, _) = pending.pop().expect("letrec marker");
                define(&scope, var, v);
                match pending.last().cloned() {
                    Some((_, next_rhs)) => {
                        self.kont.push(K::LetRec {
                            scope: scope.clone(),
                            pending,
                            body,
                        });
                        self.ctrl = Control::Eval(Arc::new(next_rhs), scope);
                    }
                    None => self.ctrl = Control::Eval(body, scope),
                }
            }
            K::BuildTuple {
                mut done,
                mut pending,
                env,
            } => {
                done.push(v);
                match pending.pop() {
                    Some(next) => {
                        self.kont.push(K::BuildTuple {
                            done,
                            pending,
                            env: env.clone(),
                        });
                        self.ctrl = Control::Eval(Arc::new(next), env);
                    }
                    None => self.ctrl = Control::Ret(Value::Tuple(done)),
                }
            }
            K::BuildArray {
                mut done,
                mut pending,
                env,
            } => {
                done.push(v);
                match pending.pop() {
                    Some(next) => {
                        self.kont.push(K::BuildArray {
                            done,
                            pending,
                            env: env.clone(),
                        });
                        self.ctrl = Control::Eval(Arc::new(next), env);
                    }
                    None => self.ctrl = Control::Ret(Value::Array(Rc::new(done))),
                }
            }
            K::BuildCtor {
                name,
                mut done,
                mut pending,
                env,
            } => {
                done.push(v);
                match pending.pop() {
                    Some(next) => {
                        self.kont.push(K::BuildCtor {
                            name,
                            done,
                            pending,
                            env: env.clone(),
                        });
                        self.ctrl = Control::Eval(Arc::new(next), env);
                    }
                    None => self.ctrl = Control::Ret(Value::ctor(name, done)),
                }
            }
            K::BuildPrim {
                op,
                mut done,
                mut pending,
                env,
            } => {
                done.push(v);
                match pending.pop() {
                    Some(next) => {
                        self.kont.push(K::BuildPrim {
                            op,
                            done,
                            pending,
                            env: env.clone(),
                        });
                        self.ctrl = Control::Eval(Arc::new(next), env);
                    }
                    None => self.ctrl = Control::Ret(self.prim(op, done)?),
                }
            }
            K::BuildRecord {
                mut done,
                mut pending,
                env,
            } => {
                let (label, _) = pending.pop().expect("record marker");
                done.push((label, v));
                match pending.last().cloned() {
                    Some((_, next)) => {
                        self.kont.push(K::BuildRecord {
                            done,
                            pending,
                            env: env.clone(),
                        });
                        self.ctrl = Control::Eval(Arc::new(next), env);
                    }
                    None => {
                        self.ctrl = Control::Ret(Value::Record(done.into_iter().collect()));
                    }
                }
            }
            K::Proj(i) => match v {
                Value::Tuple(items) => {
                    let val = items.into_iter().nth(i).ok_or_else(|| RuntimeError {
                        msg: format!("tuple projection {i} out of range"),
                    })?;
                    self.ctrl = Control::Ret(val);
                }
                other => return err(format!("cannot project field {i} out of {other}")),
            },
            K::Sel(label) => {
                let val = match v {
                    Value::Record(map) => map.get(&label).cloned().ok_or_else(|| RuntimeError {
                        msg: format!("record has no field `{label}`"),
                    })?,
                    Value::Ctor(cname, vals) => self
                        .fields
                        .get(&cname)
                        .and_then(|fs| fs.iter().position(|f| *f == label))
                        // The payload is shared, so take a copy of the one field
                        // rather than moving the whole thing out.
                        .and_then(|i| vals.get(i).cloned())
                        .ok_or_else(|| RuntimeError {
                            msg: format!(
                                "`{}` has no field `{label}`",
                                meadow_core::ctor_spelling(&cname)
                            ),
                        })?,
                    other => return err(format!("cannot select `.{label}` from {other}")),
                };
                self.ctrl = Control::Ret(val);
            }
            K::ExtendVal { label, val, env } => {
                self.kont.push(K::ExtendWith { label, rec: v });
                self.ctrl = Control::Eval(val, env);
            }
            K::ExtendWith { label, rec } => match rec {
                Value::Record(mut map) => {
                    map.insert(label, v);
                    self.ctrl = Control::Ret(Value::Record(map));
                }
                other => return err(format!("cannot extend non-record {other}")),
            },
            K::Match { arms, env } => self.try_arms(arms, env, 0, v)?,
            K::Guard {
                arms,
                env,
                next,
                scrutinee,
                scope,
            } => match v {
                Value::Bool(true) => {
                    let body = arms[next - 1].2.clone();
                    self.ctrl = Control::Eval(Arc::new(body), scope);
                }
                Value::Bool(false) => self.try_arms(arms, env, next, scrutinee)?,
                other => return err(format!("a `match` guard is not a Bool: {other}")),
            },
            K::PerformWith { effect, op } => self.perform(effect, op, v)?,
            K::Cache { var, root } => {
                redefine(&root, var, v.clone());
                self.ctrl = Control::Ret(v);
            }
            K::HandleMark(data, henv) => {
                // body returned normally — run the `return` clause (or identity)
                match &data.ret {
                    Some((param, _, body)) => {
                        let scope = child(&henv);
                        define(&scope, *param, v);
                        self.ctrl = Control::Eval(body.clone(), scope);
                    }
                    None => self.ctrl = Control::Ret(v),
                }
            }
        }
        Ok(())
    }

    fn apply(&mut self, func: Value, arg: Value) -> Result<(), RuntimeError> {
        match func {
            Value::Closure { param, body, env } => {
                let scope = child(&env);
                define(&scope, param, arg);
                self.ctrl = Control::Eval(body, scope);
            }
            Value::Builtin { op, mut args } => {
                args.push(arg);
                self.ctrl = if args.len() >= op.arity() {
                    Control::Ret(self.prim(op, args)?)
                } else {
                    Control::Ret(Value::Builtin { op, args })
                };
            }
            Value::Cont(frames) => {
                let Some(saved) = frames.borrow_mut().take() else {
                    return err("continuation resumed more than once");
                };
                self.kont.extend(saved);
                self.ctrl = Control::Ret(arg);
            }
            other => return err(format!("{other} is not a function")),
        }
        Ok(())
    }

    /// Handle a `perform`: unwind the kontinuation to the nearest matching handler,
    /// capture the sliced-off prefix as a resumption, and run the handler clause.
    fn perform(
        &mut self,
        effect: InternedString,
        op: InternedString,
        arg: Value,
    ) -> Result<(), RuntimeError> {
        // find the nearest HandleMark (from the top) with a matching clause
        let idx = self.kont.iter().rposition(|k| {
            matches!(k, K::HandleMark(data, _)
                if data.clauses.iter().any(|c| c.effect == effect && c.op == op))
        });
        let Some(idx) = idx else {
            // No user handler: the runtime discharges a few built-in effects
            // itself. `Std.Fs` operations hit the real filesystem here (a `handle`
            // would have matched above and taken precedence).
            if &*effect == "Fs" {
                let result = native_fs(&op, arg)?;
                self.ctrl = Control::Ret(result);
                return Ok(());
            }
            if &*effect == "Process" {
                let result = native_process(&op, arg)?;
                self.ctrl = Control::Ret(result);
                return Ok(());
            }
            // `Std.Test.fail` with nobody listening is a failed assertion: stop
            // the test and hand its message to the runner. A `handle` still takes
            // precedence, which is how a test asserts that something *does* fail.
            if &*effect == "Test" && &*op == "fail" {
                return err(match arg {
                    Value::Str(s) => s.to_string(),
                    other => other.to_string(),
                });
            }
            if &*effect == "Random" {
                let result = native_random(&op, arg)?;
                self.ctrl = Control::Ret(result);
                return Ok(());
            }
            if &*effect == "Time" {
                let result = native_time(&op, arg)?;
                self.ctrl = Control::Ret(result);
                return Ok(());
            }
            if &*effect == "Console" {
                let result = native_console(&op, arg)?;
                self.ctrl = Control::Ret(result);
                return Ok(());
            }
            if &*effect == "Net" || &*effect == "Std.Net.Net" {
                let result = native_net(&op, arg)?;
                self.ctrl = Control::Ret(result);
                return Ok(());
            }
            if &*effect == "Log" || &*effect == "Std.Log.Log" {
                let result = native_log(&op, arg)?;
                self.ctrl = Control::Ret(result);
                return Ok(());
            }
            let effect = meadow_core::spelling(&effect);
            return err(format!("unhandled effect {effect}.{op}"));
        };

        // frames [idx..] — including the HandleMark itself, so a resumption
        // re-enters under the same handler (deep handlers)
        let captured = self.kont.split_off(idx);
        let (data, henv) = match &captured[0] {
            K::HandleMark(d, e) => (d.clone(), e.clone()),
            _ => unreachable!(),
        };
        let clause = data
            .clauses
            .iter()
            .find(|c| c.effect == effect && c.op == op)
            .expect("matched above")
            .clone();

        let cont = Value::Cont(Rc::new(RefCell::new(Some(captured))));
        let scope = child(&henv);
        define(&scope, clause.param, arg);
        define(&scope, clause.resume, cont);
        self.ctrl = Control::Eval(Arc::new(clause.body), scope);
        Ok(())
    }
}

enum SeqKind {
    Tuple,
    Array,
    Ctor(InternedString),
    Prim(core::Prim),
}

fn lit_value(lit: &core::Lit) -> Value {
    match lit {
        core::Lit::Int(i) | core::Lit::AnyInt(i, _) => Value::Int(*i),
        core::Lit::BigInt(i) => Value::BigInt(BigInt::from(*i)),
        core::Lit::Float(x) | core::Lit::AnyFloat(x, _) => Value::Float(*x),
        core::Lit::Word(w, b) => Value::Word(*w, *b),
        core::Lit::Float32(x) => Value::Float32(*x),
        core::Lit::Str(s) | core::Lit::Sym(s) => Value::Str(*s),
        core::Lit::Char(c) => Value::Char(*c),
        core::Lit::Bool(b) => Value::Bool(*b),
        core::Lit::Unit => Value::Unit,
    }
}

// --- pattern matching ------------------------------------------------------

/// A `case`'s arms: pattern, guard, body.
type Arms = Rc<Vec<(core::Pat, Option<Term>, Term)>>;

fn match_pat(pat: &core::Pat, value: &Value, scope: &Env) -> bool {
    use core::Pat as P;
    match (pat, value) {
        (P::Wild, _) => true,
        (P::Var(v, _), _) => {
            define(scope, *v, value.clone());
            true
        }
        (P::As(v, _, sub), _) => {
            define(scope, *v, value.clone());
            match_pat(sub, value, scope)
        }
        // A number pattern by value, which is what lets a literal in generic
        // code match whatever integer type arrives -- see `meadow_core::num`.
        (
            P::Lit(
                l @ (core::Lit::Int(_)
                | core::Lit::BigInt(_)
                | core::Lit::Float(_)
                | core::Lit::AnyInt(..)
                | core::Lit::AnyFloat(..)
                | core::Lit::Word(..)
                | core::Lit::Float32(_)),
            ),
            v,
        ) => match to_num(v) {
            Some(n) => num::num_eq(&to_num(&lit_value(l)).expect("a number"), &n),
            None => false,
        },
        (P::Lit(core::Lit::Str(a)), Value::Str(b)) => a == b,
        (P::Lit(core::Lit::Char(a)), Value::Char(b)) => a == b,
        (P::Lit(core::Lit::Bool(a)), Value::Bool(b)) => a == b,
        (P::Lit(core::Lit::Unit), Value::Unit) => true,
        (P::Tuple(ps), Value::Tuple(vs)) if ps.len() == vs.len() => ps
            .iter()
            .zip(vs.iter())
            .all(|(p, v)| match_pat(p, v, scope)),
        (P::Array(ps), Value::Array(vs)) if ps.len() == vs.len() => ps
            .iter()
            .zip(vs.iter())
            .all(|(p, v)| match_pat(p, v, scope)),
        (P::Ctor(name, ps), Value::Ctor(vname, vs)) if name == vname && ps.len() == vs.len() => ps
            .iter()
            .zip(vs.iter())
            .all(|(p, v)| match_pat(p, v, scope)),
        (P::Record(fields), Value::Record(map)) => fields
            .iter()
            .all(|(label, p)| map.get(label).is_some_and(|v| match_pat(p, v, scope))),
        _ => false,
    }
}

// --- primitives ----------------------------------------------------------

/// How `print` and `println` render a value.
///
/// A `String` prints as its text. Everything else prints as [`Value`]'s own
/// rendering, which is what `show` gives and what the REPL displays — so
/// `println 42` is `42` and `println (1, "a")` is `(1, "a")`, with the quotes
/// kept where a string is *data inside* a structure rather than the message.
///
/// Without this, `println "hello"` wrote `"hello"`, quotes and all, because
/// these are polymorphic and fell through to `Display`. That made them unusable
/// for anything a person reads, and it is where the claim that Meadow string
/// literals "include their surrounding quotes" came from — they never did.
fn displayed(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        other => other.to_string(),
    }
}

/// `hash`, fed to [`meadow_core::hash::Hasher`] in the order every engine uses:
/// a value's head, then its parts left to right.
fn hash_value(v: &Value) -> Result<i64, RuntimeError> {
    use meadow_core::hash::{Hasher, unhashable};
    enum Work {
        Val(Value),
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
            Value::Tuple(xs) => {
                h.data("#tuple", xs.len());
                stack.extend(xs.iter().rev().cloned().map(Work::Val));
            }
            Value::Ctor(name, _) if is_vector_ctor(name) => {
                let Some(xs) = vector_elems(&v) else {
                    return err(format!("hash: a malformed vector ({name})"));
                };
                h.vector(xs.len());
                stack.extend(xs.into_iter().rev().map(Work::Val));
            }
            Value::Ctor(name, fields) => {
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
            Value::Closure { .. } | Value::Builtin { .. } | Value::Cont(_) => {
                return err(unhashable("a function"));
            }
            Value::Compact(c) => {
                h.compact();
                stack.push(Work::Val(c.0.clone()));
            }
            Value::Channel(_) => return err(unhashable("a channel")),
            Value::TVar(_) => return err(unhashable("a TVar")),
            Value::Task(_) => return err(unhashable("a thread")),
            Value::Lazy(_) => return err("hash: a top-level value that was never evaluated"),
        }
    }
    Ok(h.finish())
}

fn run_prim(op: core::Prim, args: Vec<Value>) -> Result<Value, RuntimeError> {
    use core::Prim::*;

    let n = |i: usize| {
        to_num(&args[i]).ok_or_else(|| RuntimeError {
            msg: format!("expected a number, got {}", args[i]),
        })
    };
    let done = |r: Result<num::Num, String>| r.map(from_num).map_err(|msg| RuntimeError { msg });
    let truth = |r: Result<bool, String>| r.map(Value::Bool).map_err(|msg| RuntimeError { msg });

    match op {
        // --- numbers: see `meadow_core::num` ---------------------------------
        Add | Sub | Mul | Div | Mod | Pow => done(num::int_arith(arith(op), n(0)?, n(1)?)),
        Lt | Gt | Le | Ge => truth(num::int_cmp(cmp(op), n(0)?, n(1)?)),
        AddF | SubF | MulF | DivF => done(num::float_arith(arith(op), n(0)?, n(1)?)),
        LtF | GtF | LeF | GeF => truth(num::float_cmp(cmp(op), n(0)?, n(1)?)),
        ToFloat => done(num::to_float(n(0)?).map(num::Num::Float)),
        ToFloat32 => done(num::to_float32(n(0)?).map(num::Num::Float32)),
        Floor => done(num::floor(n(0)?).map(num::Num::Int)),
        ToBig => done(num::to_int(num::IntTarget::Big, n(0)?)),
        ToInt => done(num::to_int(num::IntTarget::Int, n(0)?)),
        ToWord(w) => done(num::to_int(num::IntTarget::Word(w), n(0)?)),
        Eq => Ok(Value::Bool(value_eq(&args[0], &args[1]))),
        Ne => Ok(Value::Bool(!value_eq(&args[0], &args[1]))),
        Neg => done(num::int_neg(n(0)?)),
        Display => Ok(Value::Str(InternedString::from(displayed(&args[0])))),
        Hash => hash_value(&args[0]).map(Value::Int),

        // --- builtin `Array` -------------------------------------------------
        ArrayLen => Ok(Value::Int(as_array(&args[0])?.len() as i64)),
        ArrayGet => {
            let a = as_array(&args[0])?;
            let i = as_index(&args[1])?;
            a.get(i).cloned().ok_or_else(|| RuntimeError {
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

        // --- bitwise `Int` ops --------------------------------------------------
        Shl | Shr | Ushr | BitAnd | BitOr | BitXor => done(num::int_bits(bits(op), n(0)?, n(1)?)),
        BitNot => done(num::int_not(n(0)?)),
        PopCount => done(num::pop_count(n(0)?).map(num::Num::Int)),
        BitWidth => done(num::bit_width(&n(0)?).map(num::Num::Int)),

        // --- bytes -----------------------------------------------------------
        StringToBytes => match &args[0] {
            Value::Str(s) => Ok(Value::Array(Rc::new(
                s.bytes()
                    .map(|b| Value::Word(num::Width::U8, b as u64))
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
        // The `Display` the REPL prints with, exposed to the language. Structural,
        // so it works at every type without a class or a derive.
        Show => Ok(Value::Str(InternedString::from(args[0].to_string()))),

        // --- the mutable cell ---
        //
        // The only place in the runtime where a value a program can still see is
        // changed in place. `Rc::make_mut` on an `Array` looks similar but is
        // copy-on-write: it only writes when nobody else is holding the buffer,
        // and the caller gets a new array either way.
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

        // --- the mutable array -----------------------------------------------
        RunSt => err("runSt reached the evaluator; lowering applies its body"),
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
            cells.get(i).cloned().ok_or_else(|| RuntimeError {
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
            compact_into(&region, &args[0], meadow_core::compact::uncompactable)?;
            Ok(Value::Compact(Rc::new((args[0].clone(), region))))
        }
        GetCompact => Ok(as_compact(&args[0])?.0.clone()),
        CompactAdd => {
            let region = as_compact(&args[0])?.1.clone();
            compact_into(&region, &args[1], meadow_core::compact::uncompactable)?;
            Ok(Value::Compact(Rc::new((args[1].clone(), region))))
        }
        CompactSize => Ok(Value::Int(
            (as_compact(&args[0])?.1.borrow().slots * meadow_core::compact::SLOT_BYTES) as i64,
        )),
        // Only the machine can carry these out -- see `Machine::prim`.
        ThreadSpawn | ThreadAwait | ThreadYield | ChannelNew | ChannelSend | ChannelReceive => {
            err("a thread operation reached a primitive with no machine to run it")
        }
        StmNew | StmRead | StmWrite | StmBegin | StmCommit | StmWait | StmNest | StmMerge
        | StmRollback => {
            err("a transaction operation reached a primitive with no machine to run it")
        }
        // This machine keeps top-level values lazily in its environment and
        // never runs the caching pass that produces these.
        GlobalReady | GlobalGet | GlobalSet => err("a definition cache reached the CEK machine"),
        Once | TakeOnce => err("a resumption's flag reached the CEK machine"),
        Enter | Detach | Reattach => err("the frame stack reached the CEK machine"),
        SetField => err("a destination-passing write reached the CEK machine"),
        IntAdd | IntSub | IntMul | IntDiv | IntMod | IntEq | IntNe | IntLt | IntLe | IntGt
        | IntGe | FloatAdd | FloatSub | FloatMul | FloatDiv | FloatEq | FloatNe | FloatLt
        | FloatLe | FloatGt | FloatGe => err("a typed primitive reached the CEK machine"),
        CharCode => match &args[0] {
            Value::Char(c) => Ok(Value::Int(*c as i64)),
            other => err(format!("charCode: expected a Char, got {other}")),
        },
        // Not every `Int` is a character: surrogates and anything above
        // U+10FFFF are not scalar values, and there is no `Char` to return.
        CharFromCode => match &args[0] {
            Value::Int(n) => u32::try_from(*n)
                .ok()
                .and_then(char::from_u32)
                .map(Value::Char)
                .ok_or_else(|| RuntimeError {
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
            Ok(Value::Str(InternedString::from(meadow_core::text::slice(
                s.as_bytes(),
                from,
                to,
            ))))
        }
        StringCompare => {
            let a = text_arg(&args[0], "stringCompare")?;
            let b = text_arg(&args[1], "stringCompare")?;
            Ok(Value::Int(meadow_core::text::compare(
                a.as_bytes(),
                b.as_bytes(),
            )))
        }
        StringIndexOf => {
            let hay = text_arg(&args[0], "stringIndexOf")?;
            let needle = text_arg(&args[1], "stringIndexOf")?;
            Ok(Value::Int(meadow_core::text::index_of(
                hay.as_bytes(),
                needle.as_bytes(),
                as_int(&args[2])?,
            )))
        }
        BytesToHex => {
            let buf = bytes_of(&args[0], "bytesToHex")?;
            let mut s = String::with_capacity(buf.len() * 2);
            for b in buf {
                s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
                s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
            }
            Ok(Value::Str(InternedString::from(s)))
        }
        BytesFromHex => {
            let s = match &args[0] {
                Value::Str(s) => *s,
                other => return err(format!("`bytesFromHex` expects a String, got {other}")),
            };
            let none = || Value::ctor(InternedString::from("Maybe.None"), vec![]);
            let bytes = s.as_bytes();
            if bytes.len() % 2 != 0 {
                return Ok(none());
            }
            let mut out = Vec::with_capacity(bytes.len() / 2);
            for pair in bytes.chunks_exact(2) {
                let hi = (pair[0] as char).to_digit(16);
                let lo = (pair[1] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push(Value::Word(num::Width::U8, ((h << 4) | l) as u64))
                    }
                    _ => return Ok(none()),
                }
            }
            Ok(Value::ctor(
                InternedString::from("Maybe.Just"),
                vec![Value::Array(Rc::new(out))],
            ))
        }
    }
}

/// Read a `Value::Array` of `UInt8`s into a byte buffer. `what` names the caller
/// for the error message.
fn bytes_of(v: &Value, what: &str) -> Result<Vec<u8>, RuntimeError> {
    let a = as_array(v)?;
    let mut buf = Vec::with_capacity(a.len());
    for x in a.iter() {
        match x {
            Value::Word(num::Width::U8, b) => buf.push(*b as u8),
            // A literal in code generic over its integer type -- see
            // `meadow_core::num`.
            Value::Int(n) if (0..=255).contains(n) => buf.push(*n as u8),
            other => return err(format!("`{what}`: not a byte: {other}")),
        }
    }
    Ok(buf)
}

fn to_num(v: &Value) -> Option<num::Num> {
    Some(match v {
        Value::Int(x) => num::Num::Int(*x),
        Value::Word(w, b) => num::Num::Word(*w, *b),
        Value::BigInt(x) => num::Num::Big(x.clone()),
        Value::Float(x) => num::Num::Float(*x),
        Value::Float32(x) => num::Num::Float32(*x),
        _ => return None,
    })
}

fn from_num(n: num::Num) -> Value {
    match n {
        num::Num::Int(x) => Value::Int(x),
        num::Num::Word(w, b) => Value::Word(w, b),
        num::Num::Big(x) => Value::BigInt(x),
        num::Num::Float(x) => Value::Float(x),
        num::Num::Float32(x) => Value::Float32(x),
    }
}

fn arith(op: core::Prim) -> num::Arith {
    use core::Prim::*;
    match op {
        Add | AddF => num::Arith::Add,
        Sub | SubF => num::Arith::Sub,
        Mul | MulF => num::Arith::Mul,
        Div | DivF => num::Arith::Div,
        Mod => num::Arith::Mod,
        _ => num::Arith::Pow,
    }
}

fn cmp(op: core::Prim) -> num::Cmp {
    use core::Prim::*;
    match op {
        Lt | LtF => num::Cmp::Lt,
        Gt | GtF => num::Cmp::Gt,
        Le | LeF => num::Cmp::Le,
        _ => num::Cmp::Ge,
    }
}

fn bits(op: core::Prim) -> num::Bits {
    use core::Prim::*;
    match op {
        Shl => num::Bits::Shl,
        Shr => num::Bits::Shr,
        Ushr => num::Bits::Ushr,
        BitAnd => num::Bits::And,
        BitOr => num::Bits::Or,
        _ => num::Bits::Xor,
    }
}

fn as_array<'a>(v: &'a Value) -> Result<&'a Rc<Vec<Value>>, RuntimeError> {
    match v {
        Value::Array(a) => Ok(a),
        other => err(format!("expected an Array, got {other}")),
    }
}

/// A string argument of primitive `what`.
fn text_arg(v: &Value, what: &str) -> Result<InternedString, RuntimeError> {
    match v {
        Value::Str(s) => Ok(*s),
        other => err(format!("{what}: expected a String, got {other}")),
    }
}

fn as_int(v: &Value) -> Result<i64, RuntimeError> {
    match v {
        Value::Int(i) => Ok(*i),
        other => err(format!("expected an Int, got {other}")),
    }
}

fn as_mut_array(v: &Value) -> Result<&Rc<RefCell<Vec<Value>>>, RuntimeError> {
    match v {
        Value::MutArray(a) => Ok(a),
        other => err(format!("expected a mutable array, got {other}")),
    }
}

fn as_index(v: &Value) -> Result<usize, RuntimeError> {
    let i = as_int(v)?;
    if i < 0 {
        return err(format!("negative array index {i}"));
    }
    Ok(i as usize)
}

/// The runtime's default handler for the `Std.Fs` effect: perform the real
/// filesystem operation and return its result. Read/write ops yield
/// `Result String a` (`Err` carries the OS message); `exists` / `isFile` /
/// `isDir` yield `Bool`.
fn native_fs(op: &str, arg: Value) -> Result<Value, RuntimeError> {
    use std::fs;
    use std::path::Path;

    let sv = |s: String| Value::Str(InternedString::from(s));
    let ok = |v: Value| Value::ctor(InternedString::from("Result.Ok"), vec![v]);
    let ioerr = |e: std::io::Error| {
        Value::ctor(
            InternedString::from("Result.Err"),
            vec![Value::Str(InternedString::from(e.to_string()))],
        )
    };
    let unit = |r: std::io::Result<()>| match r {
        Ok(()) => ok(Value::Unit),
        Err(e) => ioerr(e),
    };
    let one = |v: &Value| -> Result<InternedString, RuntimeError> {
        match v {
            Value::Str(s) => Ok(*s),
            other => err(format!("Fs.{op}: expected a String, got {other}")),
        }
    };
    let two = |v: &Value| -> Result<(InternedString, InternedString), RuntimeError> {
        match v {
            Value::Tuple(xs) if xs.len() == 2 => Ok((one(&xs[0])?, one(&xs[1])?)),
            other => err(format!(
                "Fs.{op}: expected a (String, String) pair, got {other}"
            )),
        }
    };

    Ok(match op {
        "readToString" => match fs::read_to_string(&*one(&arg)?) {
            Ok(c) => ok(sv(c)),
            Err(e) => ioerr(e),
        },
        "readBytes" => match fs::read(&*one(&arg)?) {
            Ok(b) => ok(Value::Array(Rc::new(
                b.into_iter()
                    .map(|x| Value::Word(num::Width::U8, u64::from(x)))
                    .collect(),
            ))),
            Err(e) => ioerr(e),
        },
        "writeString" => {
            let (p, c) = two(&arg)?;
            unit(fs::write(&*p, c.as_bytes()))
        }
        "writeBytes" => match &arg {
            Value::Tuple(xs) if xs.len() == 2 => {
                let path = one(&xs[0])?;
                let Value::Array(bytes) = &xs[1] else {
                    return err(format!(
                        "Fs.writeBytes: expected an array of bytes, got {}",
                        xs[1]
                    ));
                };
                let mut out = Vec::with_capacity(bytes.len());
                for b in bytes.iter() {
                    match b {
                        Value::Word(num::Width::U8, x) => out.push(*x as u8),
                        Value::Int(n) if (0..=255).contains(n) => out.push(*n as u8),
                        other => return err(format!("Fs.writeBytes: not a byte: {other}")),
                    }
                }
                unit(fs::write(&*path, out))
            }
            other => {
                return err(format!(
                    "Fs.writeBytes: expected a (String, #[UInt8]) pair, got {other}"
                ));
            }
        },
        "appendString" => {
            use std::io::Write;
            let (p, c) = two(&arg)?;
            let r = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&*p)
                .and_then(|mut f| f.write_all(c.as_bytes()));
            unit(r)
        }
        "removeFile" => unit(fs::remove_file(&*one(&arg)?)),
        "createDir" => unit(fs::create_dir(&*one(&arg)?)),
        "createDirAll" => unit(fs::create_dir_all(&*one(&arg)?)),
        "removeDir" => unit(fs::remove_dir(&*one(&arg)?)),
        "removeDirAll" => unit(fs::remove_dir_all(&*one(&arg)?)),
        "rename" => {
            let (a, b) = two(&arg)?;
            unit(fs::rename(&*a, &*b))
        }
        "copy" => {
            let (a, b) = two(&arg)?;
            match fs::copy(&*a, &*b) {
                Ok(n) => ok(Value::Int(n as i64)),
                Err(e) => ioerr(e),
            }
        }
        "readDir" => match fs::read_dir(&*one(&arg)?) {
            Ok(entries) => {
                let mut names = Vec::new();
                for e in entries {
                    match e {
                        Ok(en) => names.push(sv(en.file_name().to_string_lossy().into_owned())),
                        Err(e) => return Ok(ioerr(e)),
                    }
                }
                ok(vector_value(names))
            }
            Err(e) => ioerr(e),
        },
        "metadata" => match fs::metadata(&*one(&arg)?) {
            // `Std.Fs.FileMeta`, a record the language knows by name: its
            // fields in the order it declares them.
            Ok(md) => ok(Value::ctor(
                InternedString::from("FileMeta.FileMeta"),
                vec![
                    Value::Bool(md.is_file()),
                    Value::Bool(md.is_dir()),
                    Value::Int(md.len() as i64),
                    Value::Bool(md.permissions().readonly()),
                    Value::Int(meadow_core::args::modified_millis(&md)),
                ],
            )),
            Err(e) => ioerr(e),
        },
        "exists" => Value::Bool(Path::new(&*one(&arg)?).exists()),
        "isFile" => Value::Bool(Path::new(&*one(&arg)?).is_file()),
        "isDir" => Value::Bool(Path::new(&*one(&arg)?).is_dir()),
        other => return err(format!("unhandled effect Fs.{other}")),
    })
}

/// The runtime's default handler for the `Std.Process` effect: spawn real child
/// processes / touch the real environment. A `Command` (from `Std.Process`) is
/// the tuple `(program, args, cwd, envVars) : (String, List String,
/// Maybe String, List (String, String))`; an `Output` is `(status, stdout,
/// stderr) : (Int, String, String)`.
fn native_process(op: &str, arg: Value) -> Result<Value, RuntimeError> {
    use meadow_core::ffi;
    use std::process::Command as Proc;

    let sv = |s: String| Value::Str(InternedString::from(s));
    let ok = |v: Value| Value::ctor(InternedString::from("Result.Ok"), vec![v]);
    let errv = |m: String| Value::ctor(InternedString::from("Result.Err"), vec![sv(m)]);
    let just = |v: Value| Value::ctor(InternedString::from("Maybe.Just"), vec![v]);
    let none = || Value::ctor(InternedString::from("Maybe.None"), vec![]);
    let answered = |r: Result<Value, String>| match r {
        Ok(v) => ok(v),
        Err(e) => errv(e),
    };
    let tuple = |v: &Value, n: usize| -> Result<Vec<Value>, RuntimeError> {
        match v {
            Value::Tuple(t) if t.len() == n => Ok(t.to_vec()),
            other => err(format!("Process.{op}: expected {n} things, got {other}")),
        }
    };
    let as_float = |v: &Value| -> Result<f64, RuntimeError> {
        match v {
            Value::Float(x) => Ok(*x),
            other => err(format!("Process.{op}: expected a Float, got {other}")),
        }
    };

    let as_str = |v: &Value| -> Result<InternedString, RuntimeError> {
        match v {
            Value::Str(s) => Ok(*s),
            other => err(format!("Process.{op}: expected a String, got {other}")),
        }
    };
    fn as_vector(op: &str, v: &Value) -> Result<Vec<Value>, RuntimeError> {
        match vector_elems(v) {
            Some(xs) => Ok(xs),
            None => err(format!("Process.{op}: expected a Vector, got {v}")),
        }
    }
    let as_cwd = |v: &Value| -> Result<Option<InternedString>, RuntimeError> {
        match v {
            Value::Ctor(n, args) if &**n == "Maybe.None" && args.is_empty() => Ok(None),
            Value::Ctor(n, args) if &**n == "Maybe.Just" && args.len() == 1 => {
                Ok(Some(as_str(&args[0])?))
            }
            other => err(format!(
                "Process.{op}: expected a Maybe String, got {other}"
            )),
        }
    };
    let build = |v: &Value| -> Result<Proc, RuntimeError> {
        let t = match v {
            Value::Tuple(t) if t.len() == 4 => t,
            other => return err(format!("Process.{op}: expected a Command, got {other}")),
        };
        let program = as_str(&t[0])?;
        let mut cmd = Proc::new(&*program);
        for a in as_vector(op, &t[1])? {
            cmd.arg(&*as_str(&a)?);
        }
        if let Some(dir) = as_cwd(&t[2])? {
            cmd.current_dir(&*dir);
        }
        for e in as_vector(op, &t[3])? {
            match e {
                Value::Tuple(kv) if kv.len() == 2 => {
                    cmd.env(&*as_str(&kv[0])?, &*as_str(&kv[1])?);
                }
                other => {
                    return err(format!(
                        "Process.{op}: expected a (String, String) pair, got {other}"
                    ));
                }
            }
        }
        Ok(cmd)
    };

    Ok(match op {
        "spawn" => match build(&arg)?.output() {
            Ok(out) => ok(Value::Tuple(vec![
                Value::Int(out.status.code().unwrap_or(-1) as i64),
                sv(String::from_utf8_lossy(&out.stdout).into_owned()),
                sv(String::from_utf8_lossy(&out.stderr).into_owned()),
            ])),
            Err(e) => errv(e.to_string()),
        },
        "status" => match build(&arg)?.status() {
            Ok(st) => ok(Value::Int(st.code().unwrap_or(-1) as i64)),
            Err(e) => errv(e.to_string()),
        },
        // The reference machine notes nothing of what it reaches: a fuzzer
        // that asks is told nothing is new.
        "covered" => Value::Int(0),
        "exit" => {
            let code = as_int(&arg)?;
            std::process::exit(code as i32);
        }
        "currentPid" => Value::Int(std::process::id() as i64),
        "currentExe" => sv(meadow_core::args::current_exe()),
        "isTerminal" => Value::Bool(is_terminal(as_int(&arg)?)),
        "argv" => vector_value(meadow_core::args::get().into_iter().map(sv).collect()),
        "getEnv" => match std::env::var(&*as_str(&arg)?) {
            Ok(v) => just(sv(v)),
            Err(_) => none(),
        },
        "setEnv" => {
            let t = match &arg {
                Value::Tuple(t) if t.len() == 2 => t,
                other => {
                    return err(format!(
                        "Process.setEnv: expected a (String, String) pair, got {other}"
                    ));
                }
            };
            unsafe {
                std::env::set_var(&*as_str(&t[0])?, &*as_str(&t[1])?);
            }
            Value::Unit
        }
        "removeEnv" => {
            unsafe {
                std::env::remove_var(&*as_str(&arg)?);
            }
            Value::Unit
        }
        // Calling C, for `Std.Ffi`: see `meadow_core::ffi`.
        "ffiOpen" => answered(ffi::open(&as_str(&arg)?).map(Value::Int)),
        "ffiSymbol" => {
            let t = tuple(&arg, 2)?;
            answered(ffi::symbol(as_int(&t[0])?, &as_str(&t[1])?).map(Value::Int))
        }
        "ffiCall" => {
            let t = tuple(&arg, 3)?;
            let Some(ret) = ffi::Ret::from_code(as_int(&t[1])?) else {
                return err("Process.ffiCall: no such kind of answer".to_string());
            };
            let mut args = Vec::new();
            for a in as_vector(op, &t[2])? {
                let a = tuple(&a, 4)?;
                args.push(match as_int(&a[0])? {
                    0 => ffi::Arg::Int(as_int(&a[1])?),
                    1 => ffi::Arg::Float(as_float(&a[2])?),
                    2 => ffi::Arg::Str(as_str(&a[3])?.to_string()),
                    3 => ffi::Arg::Float32(as_float(&a[2])?),
                    _ => return err("Process.ffiCall: no such kind of argument".to_string()),
                });
            }
            answered(
                ffi::call(as_int(&t[0])?, ret, &args)
                    .map(|(n, x, s)| Value::Tuple(vec![Value::Int(n), Value::Float(x), sv(s)])),
            )
        }
        "ffiAlloc" => Value::Int(ffi::alloc(as_int(&arg)?)),
        "ffiFree" => {
            ffi::free(as_int(&arg)?);
            Value::Unit
        }
        "ffiWriteInts" => {
            let t = tuple(&arg, 3)?;
            let mut values = Vec::new();
            for v in as_vector(op, &t[2])? {
                values.push(as_int(&v)?);
            }
            // Safety: the program's word that the memory is its own.
            answered(
                unsafe { ffi::write_ints(as_int(&t[0])?, as_int(&t[1])?, &values) }
                    .map(|()| Value::Unit),
            )
        }
        "ffiReadInts" => {
            let t = tuple(&arg, 4)?;
            let signed = matches!(t[2], Value::Bool(true));
            // Safety: as above.
            let read =
                unsafe { ffi::read_ints(as_int(&t[0])?, as_int(&t[1])?, signed, as_int(&t[3])?) };
            answered(read.map(|ns| vector_value(ns.into_iter().map(Value::Int).collect())))
        }
        "ffiWriteFloats" => {
            let t = tuple(&arg, 3)?;
            let mut values = Vec::new();
            for v in as_vector(op, &t[2])? {
                values.push(as_float(&v)?);
            }
            // Safety: as above.
            answered(
                unsafe { ffi::write_floats(as_int(&t[0])?, as_int(&t[1])?, &values) }
                    .map(|()| Value::Unit),
            )
        }
        "ffiReadFloats" => {
            let t = tuple(&arg, 3)?;
            // Safety: as above.
            let read = unsafe { ffi::read_floats(as_int(&t[0])?, as_int(&t[1])?, as_int(&t[2])?) };
            answered(read.map(|xs| vector_value(xs.into_iter().map(Value::Float).collect())))
        }
        "ffiReadText" => {
            let t = tuple(&arg, 2)?;
            // Safety: as above.
            sv(unsafe { ffi::read_text(as_int(&t[0])?, as_int(&t[1])?) })
        }
        // Only a library Silo builds is called by C.
        "ffiFailure" => sv(String::new()),
        other => return err(format!("unhandled effect Process.{other}")),
    })
}

// --- `Std.Collections` marshalling ------------------------------------------
//
// `List` and `Vector` are ordinary `Std` data types, so at runtime they are
// nothing but `Value::Ctor`s -- the runtime has no sequence of its own beyond
// `Array`. Native operations whose declared signature mentions `Vector` build
// and read that structure here, the same way `Bool` crosses the boundary as
// `Value::Bool`.

/// `items` laid out as `Vector.fromArray` lays them out: `Empty`, one `Single`
/// chunk, or a radix-balanced tree of 32-element chunks under a `Full` with
/// empty side buffers. That module's code takes the result apart, so this has
/// to stay `vBuildTree`.
fn vector_value(items: Vec<Value>) -> Value {
    const WIDTH: usize = 32;
    let array = |xs: &[Value]| Value::Array(Rc::new(xs.to_vec()));
    let ctor = |name: &str, fields: Vec<Value>| Value::ctor(InternedString::from(name), fields);
    if items.is_empty() {
        return ctor("Vector.Empty", vec![]);
    }
    if items.len() <= WIDTH {
        return ctor("Vector.Single", vec![array(&items)]);
    }
    let mut nodes: Vec<Value> = items
        .chunks(WIDTH)
        .map(|chunk| ctor("VNode.Leaf", vec![array(chunk)]))
        .collect();
    let mut shift = 5;
    let branch = |kids: &[Value]| {
        ctor(
            "VNode.Branch",
            vec![ctor("Maybe.None", vec![]), array(kids)],
        )
    };
    while nodes.len() > WIDTH {
        nodes = nodes.chunks(WIDTH).map(branch).collect();
        shift += 5;
    }
    ctor(
        "Vector.Full",
        vec![
            Value::Int(items.len() as i64),
            Value::Int(shift),
            array(&[]),
            array(&[]),
            branch(&nodes),
            array(&[]),
            array(&[]),
        ],
    )
}

/// Flatten a `Cons`/`Nil` chain to its elements. `None` if `v` is not a list.
fn list_items(v: &Value) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    let mut cur = v;
    loop {
        match cur {
            Value::Ctor(n, args) if &**n == "List.Nil" && args.is_empty() => return Some(out),
            Value::Ctor(n, args) if &**n == "List.Cons" && args.len() == 2 => {
                out.push(args[0].clone());
                cur = &args[1];
            }
            _ => return None,
        }
    }
}

/// Names of the outer `Std.Collections.Vector` constructors — the values `[…]`
/// literal syntax produces. Their internal shape varies with how the vector was
/// built, so [`Display`] and [`value_eq`] flatten them to their element sequence.
/// A constructor as a reader wants to see it: `Maybe.Just` prints as `Just`.
///
/// The canonical name exists so that two types may each own a `Leaf`; a person
/// reading output already has the context that disambiguates, so the qualifier
/// would be noise. `meadow_glade::show` strips it the same way, and the
/// differential tests compare the two machines' output, so they have to agree.
fn bare_ctor(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((_, c)) => c,
        None => name,
    }
}

fn is_vector_ctor(name: &str) -> bool {
    matches!(name, "Vector.Empty" | "Vector.Single" | "Vector.Full")
}

/// Flatten a `Vector` value to its elements, in order. `None` if `v` is not a
/// recognizable vector (wrong ctor / arity / field types).
fn vector_elems(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::Ctor(n, args) if &**n == "Vector.Empty" && args.is_empty() => Some(Vec::new()),
        Value::Ctor(n, args) if &**n == "Vector.Single" && args.len() == 1 => match &args[0] {
            Value::Array(xs) => Some(xs.iter().cloned().collect()),
            _ => None,
        },
        Value::Ctor(n, args) if &**n == "Vector.Full" && args.len() == 7 => {
            let mut out = Vec::new();
            for i in [2usize, 3] {
                match &args[i] {
                    Value::Array(xs) => out.extend(xs.iter().cloned()),
                    _ => return None,
                }
            }
            vector_node_elems(&args[4], &mut out)?;
            for i in [5usize, 6] {
                match &args[i] {
                    Value::Array(xs) => out.extend(xs.iter().cloned()),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn vector_node_elems(n: &Value, out: &mut Vec<Value>) -> Option<()> {
    match n {
        Value::Ctor(name, args) if &**name == "VNode.Leaf" && args.len() == 1 => match &args[0] {
            Value::Array(xs) => {
                out.extend(xs.iter().cloned());
                Some(())
            }
            _ => None,
        },
        Value::Ctor(name, args) if &**name == "VNode.Branch" && args.len() == 2 => match &args[1] {
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

/// Structural equality, iteratively.
///
/// A `List` is a `Cons` chain as deep as it is long, so comparing two of them
/// recursively overflows the stack at a few thousand elements. The worklist here
/// is what keeps `==` usable on the sizes the language can now build.
///
/// Pairs are owned rather than borrowed because the `Vector` case has to extract
/// its elements into a fresh `Vec`; cloning is cheap for exactly the values that
/// can be deep, since `Ctor` and `Array` payloads are shared.
fn value_eq(a: &Value, b: &Value) -> bool {
    let mut stack: Vec<(Value, Value)> = vec![(a.clone(), b.clone())];
    while let Some((a, b)) = stack.pop() {
        match (&a, &b) {
            // A `Vector` is a tree, so two equal ones need not have equal shapes.
            // Compare the sequences they denote instead.
            (Value::Ctor(na, _), Value::Ctor(nb, _))
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
            (Value::Tuple(x), Value::Tuple(y)) if x.len() == y.len() => {
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
            (Value::Ctor(n1, x), Value::Ctor(n2, y)) => {
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
            // Identity, not contents. Everything else here is a value and two of
            // them are equal when they look alike; a `Ref` is a *place*, and two
            // cells that happen to hold the same thing are still two cells.
            (Value::Ref(x), Value::Ref(y)) if Rc::ptr_eq(x, y) => {}
            (Value::MutArray(x), Value::MutArray(y)) if Rc::ptr_eq(x, y) => {}
            // Immutable, so two are equal when what they hold is.
            (Value::Compact(x), Value::Compact(y)) => stack.push((x.0.clone(), y.0.clone())),
            (Value::Channel(x), Value::Channel(y)) | (Value::Task(x), Value::Task(y)) if x == y => {
            }
            (Value::TVar(x), Value::TVar(y)) if Rc::ptr_eq(x, y) => {}
            _ => return false,
        }
    }
    true
}

// --- display -------------------------------------------------------------

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(i) => write!(f, "{i}"),
            Value::BigInt(i) => write!(f, "{i}"),
            Value::Float(x) => f.write_str(&fmt_float(*x)),
            Value::Word(w, b) => write!(f, "{}", w.value(*b)),
            Value::Float32(x) => f.write_str(&num::fmt_float32(*x)),
            // Printed as the constructors a program names them by.
            Value::Bool(b) => f.write_str(if *b { "True" } else { "False" }),
            // quote + escape, so a string is visually distinct from a bare ident
            Value::Str(s) => write!(f, "{:?}", &**s),
            Value::Char(c) => write!(f, "{c:?}"),
            Value::Unit => f.write_str("()"),
            Value::Tuple(items) => {
                f.write_str("(")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")
            }
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
            // By label, as every machine prints one: the map's own order is
            // the order the labels happened to be interned in, which is the
            // process's and not the program's.
            Value::Record(map) => {
                f.write_str("{ ")?;
                let mut fields: Vec<_> = map.iter().collect();
                fields.sort_by(|a, b| (**a.0).cmp(&**b.0));
                for (i, (k, v)) in fields.into_iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k} = {v}")?;
                }
                f.write_str(" }")
            }
            // `Std.Collections.List` values print in their own literal syntax:
            // `[1; 2; 3]`. The comma form `[1, 2, 3]` is a `Vector`, and
            // `#[1, 2, 3]` an `Array`.
            Value::Ctor(name, _) if matches!(&**name, "List.Nil" | "List.Cons") => {
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
            // `Std.Collections.Vector` values print like a list: `[1, 2, 3]`.
            Value::Ctor(name, _) if is_vector_ctor(name) => {
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
            Value::Ctor(name, args) if args.is_empty() => write!(f, "{}", bare_ctor(name)),
            Value::Ctor(name, args) => {
                write!(f, "{}(", bare_ctor(name))?;
                for (i, v) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")
            }
            Value::Closure { .. } => f.write_str("<closure>"),
            Value::Builtin { op, .. } => write!(f, "<builtin {op:?}>"),
            Value::Cont(_) => f.write_str("<continuation>"),
            // The contents, not the address: a `Ref` prints as what it holds,
            // which is what a person debugging one wants to see.
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
            Value::Compact(c) => write!(f, "compact {}", c.0),
            Value::Channel(_) => f.write_str("<channel>"),
            Value::TVar(_) => f.write_str("<tvar>"),
            Value::Task(_) => f.write_str("<thread>"),
            Value::Lazy(_) => f.write_str("<unevaluated>"),
        }
    }
}

fn as_compact(v: &Value) -> Result<&CompactCell, RuntimeError> {
    match v {
        Value::Compact(c) => Ok(c),
        other => err(format!("expected a Compact, got {other}")),
    }
}

/// Put `v` in a region: check that it can go in, and count the VM slots it
/// adds -- each node once, and nothing the region holds already, as the VM
/// shares what is in a region rather than copying it again. A refusal leaves
/// the region as it was.
///
/// A tuple or a record here is not shared behind an `Rc`, so it is counted each
/// time it is reached; the VM would share it. The figure is an estimate.
fn compact_into(
    region: &RefCell<Region>,
    v: &Value,
    uncompactable: fn(&str) -> String,
) -> Result<(), RuntimeError> {
    let mut r = region.borrow_mut();
    let mut new: std::collections::HashSet<*const ()> = std::collections::HashSet::new();
    let mut slots = 0usize;
    let mut stack = vec![v.clone()];
    while let Some(v) = stack.pop() {
        // A node is counted, and its children visited, the first time only.
        let mut first = |p: *const ()| !r.seen.contains(&p) && new.insert(p);
        match &v {
            Value::Ctor(_, fields) => {
                if first(Rc::as_ptr(fields) as *const ()) {
                    slots += meadow_core::compact::object_slots(false, fields.len());
                    stack.extend(fields.iter().cloned());
                }
            }
            Value::Tuple(xs) => {
                slots += meadow_core::compact::object_slots(false, xs.len());
                stack.extend(xs.iter().cloned());
            }
            Value::Array(xs) => {
                if first(Rc::as_ptr(xs) as *const ()) {
                    slots += meadow_core::compact::object_slots(true, xs.len());
                    stack.extend(xs.iter().cloned());
                }
            }
            Value::Record(fields) => {
                slots += meadow_core::compact::object_slots(false, 2 * fields.len());
                stack.extend(fields.values().cloned());
            }
            Value::BigInt(b) => {
                slots += meadow_core::compact::object_slots(true, b.iter_u32_digits().count())
            }
            Value::Compact(c) => {
                if first(Rc::as_ptr(c) as *const ()) {
                    slots += meadow_core::compact::object_slots(false, 1);
                    stack.push(c.0.clone());
                }
            }
            Value::Ref(_) => return err(uncompactable("a Ref")),
            Value::MutArray(_) => return err(uncompactable("a mutable array")),
            Value::Closure { .. } | Value::Builtin { .. } | Value::Cont(_) => {
                return err(uncompactable("a function"));
            }
            // A handle: a header on the VM, and nothing inside.
            Value::Channel(_) | Value::Task(_) | Value::TVar(_) => {
                slots += meadow_core::compact::object_slots(false, 0)
            }
            Value::Lazy(_) => return err("compact: a top-level value that was never evaluated"),
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

/// The runtime's default handler for the `Std.Random` effect.
///
/// SplitMix64, seeded once per process from the clock. It is not cryptographic
/// and makes no promise of reproducibility — a program that wants repeatable
/// numbers should `handle` the effect with `Std.Random`'s own pure generator,
/// which is the whole reason that generator is in the library.
fn native_random(op: &str, arg: Value) -> Result<Value, RuntimeError> {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new(0);
    }

    let next = || {
        STATE.with(|s| {
            let mut x = s.get();
            if x == 0 {
                x = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x9E3779B97F4A7C15)
                    | 1;
            }
            x = x.wrapping_add(0x9E3779B97F4A7C15);
            s.set(x);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        })
    };

    match op {
        "nextInt" | "nextSeed" => Ok(Value::Int(next() as i64)),
        // [0, 1) from the top 53 bits, which is what a f64 can hold exactly.
        "nextFloat" => Ok(Value::Float((next() >> 11) as f64 / (1u64 << 53) as f64)),
        "intBetween" => match arg {
            Value::Tuple(ref t) if t.len() == 2 => match (&t[0], &t[1]) {
                (Value::Int(lo), Value::Int(hi)) => {
                    if hi <= lo {
                        return Ok(Value::Int(*lo));
                    }
                    let span = (hi - lo) as u64;
                    Ok(Value::Int(lo + (next() % span) as i64))
                }
                _ => err(format!("Random.{op}: expected two Ints, got {arg}")),
            },
            other => err(format!(
                "Random.{op}: expected an (Int, Int) pair, got {other}"
            )),
        },
        other => err(format!("unhandled effect Random.{other}")),
    }
}

thread_local! {
    /// What the program on this thread has printed, while it is being kept
    /// rather than written -- see [`run_tests_parallel`].
    static CAPTURED: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The runtime's default handler for the `Std.Console` effect: real standard
/// output and input.
///
/// `readLine` strips the line terminator, including a `\r\n` pair, so a program
/// reading a file piped in on Windows sees the same lines as one reading a
/// terminal. End of input is `None` rather than an error: a loop over stdin ends
/// by matching it, which is not an exceptional thing to do.
fn native_console(op: &str, arg: Value) -> Result<Value, RuntimeError> {
    use std::io::BufRead;

    match op {
        "writeOutput" => match arg {
            Value::Str(s) => {
                use std::io::Write;
                let captured = CAPTURED.with(|c| match &mut *c.borrow_mut() {
                    Some(text) => {
                        text.push_str(&s);
                        true
                    }
                    None => false,
                });
                if !captured {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(s.as_bytes());
                    let _ = out.flush();
                }
                Ok(Value::Unit)
            }
            other => err(format!(
                "Console.writeOutput: expected a String, got {other}"
            )),
        },
        "writeError" => match arg {
            Value::Str(s) => {
                use std::io::Write;
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().lock().write_all(s.as_bytes());
                Ok(Value::Unit)
            }
            other => err(format!(
                "Console.writeError: expected a String, got {other}"
            )),
        },
        "readLine" => {
            let _ = arg;
            let mut line = String::new();
            match std::io::stdin().lock().read_line(&mut line) {
                // Zero bytes is end of input, not an empty line: an empty line
                // still carries its terminator.
                Ok(0) => Ok(Value::ctor(InternedString::from("Maybe.None"), vec![])),
                Ok(_) => {
                    let line = line.strip_suffix('\n').unwrap_or(&line);
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    Ok(Value::ctor(
                        InternedString::from("Maybe.Just"),
                        vec![Value::Str(InternedString::from(line))],
                    ))
                }
                Err(e) => err(format!("Console.readLine: {e}")),
            }
        }
        "readExact" => {
            let Value::Int(n) = arg else {
                return err(format!("Console.readExact: expected an Int, got {arg}"));
            };
            match read_bytes(n) {
                Ok(Some(text)) => Ok(Value::ctor(
                    InternedString::from("Maybe.Just"),
                    vec![Value::Str(InternedString::from(text.as_str()))],
                )),
                Ok(None) => Ok(Value::ctor(InternedString::from("Maybe.None"), vec![])),
                Err(e) => err(format!("Console.readExact: {e}")),
            }
        }
        // The terminal, for `Std.Terminal`: see `meadow_core::terminal`.
        "rawMode" => match arg {
            Value::Bool(on) => Ok(Value::Bool(meadow_core::terminal::raw(on))),
            other => err(format!("Console.rawMode: expected a Bool, got {other}")),
        },
        "readKey" => {
            let (code, text, mods) = meadow_core::terminal::read_key();
            Ok(Value::Tuple(vec![
                Value::Int(code),
                Value::Str(InternedString::from(text.as_str())),
                Value::Int(mods),
            ]))
        }
        "terminalSize" => {
            let (columns, rows) = meadow_core::terminal::size();
            Ok(Value::Tuple(vec![Value::Int(columns), Value::Int(rows)]))
        }
        other => err(format!("unhandled effect Console.{other}")),
    }
}

/// Exactly `n` bytes of standard input, as text -- `None` if it ends first.
/// What a protocol that frames its messages by length reads a message with,
/// the Language Server Protocol's `Content-Length` among them: its body ends
/// in no newline for `readLine` to stop at. It reads through the same buffer
/// `readLine` does, so the two can take turns.
pub fn read_bytes(n: i64) -> std::io::Result<Option<String>> {
    read_exact_bounded(&mut std::io::stdin().lock(), n)
}

/// Read exactly `n` bytes of `input`, growing the buffer as bytes arrive, or
/// `None` if it ends first. The length is not trusted: reading in chunks means
/// a message that claims to be enormous costs nothing until the bytes actually
/// come, rather than a preallocation of `n` that aborts the process.
pub fn read_exact_bounded<R: std::io::Read>(
    input: &mut R,
    n: i64,
) -> std::io::Result<Option<String>> {
    let mut remaining = n.max(0) as usize;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        match input.read(&mut chunk[..want]) {
            Ok(0) => return Ok(None),
            Ok(got) => {
                buf.extend_from_slice(&chunk[..got]);
                remaining -= got;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// The runtime's default handler for the `Std.Time` effect: the real clock.
/// `Std.Net`'s sockets and `Std.Tls`'s sessions: see `meadow_rt::net`.
fn native_net(op: &str, arg: Value) -> Result<Value, RuntimeError> {
    use meadow_core::net;
    let sv = |s: String| Value::Str(InternedString::from(s));
    let ok = |v: Value| Value::ctor(InternedString::from("Result.Ok"), vec![v]);
    let errv = |m: String| Value::ctor(InternedString::from("Result.Err"), vec![sv(m)]);
    let answered = |r: Result<Value, String>| match r {
        Ok(v) => ok(v),
        Err(e) => errv(e),
    };
    let wrong = |arg: &Value| err(format!("Net.{op}: unexpected argument {arg}"));
    let int = |v: &Value| match v {
        Value::Int(n) => Some(*n),
        _ => None,
    };
    let text = |v: &Value| match v {
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    };
    let parts = |v: &Value| match v {
        Value::Tuple(t) => t.to_vec(),
        _ => Vec::new(),
    };
    let t = parts(&arg);
    Ok(match op {
        "tcpListen" | "tcpConnect" => match (t.first().and_then(text), t.get(1).and_then(int)) {
            (Some(host), Some(port)) => answered(
                if op == "tcpListen" {
                    net::listen(&host, port)
                } else {
                    net::connect(&host, port)
                }
                .map(Value::Int),
            ),
            _ => return wrong(&arg),
        },
        "tcpAccept" => match int(&arg) {
            Some(id) => answered(
                net::accept(id).map(|(s, peer)| Value::Tuple(vec![Value::Int(s), sv(peer)])),
            ),
            None => return wrong(&arg),
        },
        "tcpRead" => match (t.first().and_then(int), t.get(1).and_then(int)) {
            (Some(id), Some(max)) => answered(net::read(id, max).map(|b| {
                Value::Array(Rc::new(
                    b.into_iter()
                        .map(|x| Value::Word(num::Width::U8, u64::from(x)))
                        .collect(),
                ))
            })),
            _ => return wrong(&arg),
        },
        "tcpWrite" => match (t.first().and_then(int), t.get(1)) {
            (Some(id), Some(Value::Array(xs))) => {
                let bytes: Vec<u8> = xs
                    .iter()
                    .map(|x| match x {
                        Value::Word(_, w) => *w as u8,
                        Value::Int(n) => *n as u8,
                        _ => 0,
                    })
                    .collect();
                answered(net::write(id, &bytes).map(Value::Int))
            }
            _ => return wrong(&arg),
        },
        "tcpClose" => match int(&arg) {
            Some(id) => {
                net::close(id);
                Value::Unit
            }
            None => return wrong(&arg),
        },
        "tcpTimeout" => match (t.first().and_then(int), t.get(1).and_then(int)) {
            (Some(id), Some(ms)) => answered(net::set_timeout(id, ms).map(|()| Value::Unit)),
            _ => return wrong(&arg),
        },
        "tcpLocalPort" => match int(&arg) {
            Some(id) => answered(net::local_port(id).map(Value::Int)),
            None => return wrong(&arg),
        },
        "resolve" => match text(&arg) {
            Some(host) => answered(
                net::resolve(&host).map(|ips| vector_value(ips.into_iter().map(sv).collect())),
            ),
            None => return wrong(&arg),
        },
        "tlsConnect" | "tlsAccept" => match (
            t.first().and_then(int),
            t.get(1).and_then(text),
            t.get(2).and_then(text),
        ) {
            (Some(id), Some(a), Some(b)) => answered(
                if op == "tlsConnect" {
                    net::tls_connect(id, &a, &b)
                } else {
                    net::tls_accept(id, &a, &b)
                }
                .map(Value::Int),
            ),
            _ => return wrong(&arg),
        },
        _ => return err(format!("unhandled effect Net.{op}")),
    })
}

/// `Std.Log`'s line, where no handler took it: see `meadow_rt::log`.
fn native_log(op: &str, arg: Value) -> Result<Value, RuntimeError> {
    match (op, &arg) {
        ("logAt", Value::Tuple(t)) if t.len() == 2 => match (&t[0], &t[1]) {
            (Value::Int(n), Value::Str(s)) => {
                meadow_core::log::line(*n, s);
                Ok(Value::Unit)
            }
            _ => err(format!("Log.logAt: expected (Int, String), got {arg}")),
        },
        ("logAt", other) => err(format!("Log.logAt: expected (Int, String), got {other}")),
        _ => err(format!("unhandled effect Log.{op}")),
    }
}

fn native_time(op: &str, arg: Value) -> Result<Value, RuntimeError> {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    // A process-wide origin, so `monotonic` is a small number that fits an `Int`
    // and counts from the program's own start rather than from an unspecified past.
    thread_local! {
        static ORIGIN: Instant = Instant::now();
    }

    match op {
        // Wall clock, milliseconds since the Unix epoch.
        "now" => Ok(Value::Int(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        )),
        // Monotonic, for measuring a duration: unaffected by the clock changing.
        "monotonic" => Ok(Value::Int(ORIGIN.with(|o| o.elapsed().as_nanos() as i64))),
        "sleep" => match arg {
            Value::Int(ms) if ms > 0 => {
                std::thread::sleep(Duration::from_millis(ms as u64));
                Ok(Value::Unit)
            }
            Value::Int(_) => Ok(Value::Unit),
            other => err(format!("Time.sleep: expected an Int, got {other}")),
        },
        other => err(format!("unhandled effect Time.{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meadow_core::{HClause, Lit, Prim, Program, Term};

    // --- Console.readExact reads in bounded chunks -------------------------

    #[test]
    fn read_exact_returns_the_requested_bytes() {
        let mut cur = std::io::Cursor::new(b"hello world".to_vec());
        assert_eq!(
            read_exact_bounded(&mut cur, 5).unwrap().as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn read_exact_is_none_when_input_ends_first() {
        let mut cur = std::io::Cursor::new(b"hi".to_vec());
        // Asks for more than there is: `None`, not a partial read.
        assert_eq!(read_exact_bounded(&mut cur, 100).unwrap(), None);
    }

    #[test]
    fn a_huge_claimed_length_allocates_nothing_up_front() {
        // The whole point: a peer that claims an enormous length but sends a
        // few bytes must not make the runtime preallocate that length and
        // abort. With a bounded reader it just reaches end of input and
        // answers `None`, in an instant.
        let mut cur = std::io::Cursor::new(b"tiny".to_vec());
        let started = std::time::Instant::now();
        assert_eq!(read_exact_bounded(&mut cur, i64::MAX).unwrap(), None);
        assert!(
            started.elapsed().as_secs() < 5,
            "should not have tried to allocate i64::MAX bytes"
        );
    }

    #[test]
    fn a_negative_length_reads_nothing() {
        let mut cur = std::io::Cursor::new(b"data".to_vec());
        assert_eq!(
            read_exact_bounded(&mut cur, -1).unwrap().as_deref(),
            Some("")
        );
    }

    /// A fresh variable for a hand-built test term.
    ///
    /// Synthetic because that is what these are: terms written by hand, not
    /// minted by a compilation unit. Counted here rather than globally, which
    /// is all these tests ever wanted from the old `VarId::fresh`.
    fn v() -> Var {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        Var::synthetic(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Evaluate a single term (as `def main = term`, the entry: a value, as the
    /// REPL's `it` is).
    fn eval_term(term: Term) -> Result<Value, RuntimeError> {
        let m = v();
        run(&Program {
            defs: vec![core::Def::untyped(m, "main", term)],
            entry: Some(m),
            ..Default::default()
        })
    }

    fn int(i: i64) -> Arc<Term> {
        Arc::new(Term::Lit(Lit::Int(i)))
    }

    #[test]
    fn arithmetic_and_application() {
        // (\x -> x + 1) 41
        let x = v();
        let body = Term::prim(Prim::Add, vec![Term::Var(x), Term::Lit(Lit::Int(1))]);
        let term = Term::App(Arc::new(Term::lam(x, body)), int(41));
        assert_eq!(eval_term(term).unwrap().to_string(), "42");
    }

    #[test]
    fn if_and_let() {
        let x = v();
        let term = Term::let_(
            x,
            Term::Lit(Lit::Int(10)),
            Term::If(
                Arc::new(Term::prim(
                    Prim::Lt,
                    vec![Term::Var(x), Term::Lit(Lit::Int(20))],
                )),
                int(1),
                int(2),
            ),
        );
        assert_eq!(eval_term(term).unwrap().to_string(), "1");
    }

    #[test]
    fn list_cons_prepends() {
        // Lists are plain `Std` constructors: `Cons 0 (Cons 1 (Cons 2 Nil))`.
        let cons = |h: Term, t: Term| Term::ctor("List.Cons", vec![h, t]);
        let nil = Term::ctor("List.Nil", vec![]);
        let term = cons(
            Term::Lit(Lit::Int(0)),
            cons(Term::Lit(Lit::Int(1)), cons(Term::Lit(Lit::Int(2)), nil)),
        );
        assert_eq!(eval_term(term).unwrap().to_string(), "[0; 1; 2]");
    }

    #[test]
    fn recursion_via_letrec() {
        // letrec f = \n -> if n == 0 then 0 else n + f (n - 1) in f 5   => 15
        let f = v();
        let n = v();
        let lam = Term::lam(
            n,
            Term::If(
                Arc::new(Term::prim(
                    Prim::Eq,
                    vec![Term::Var(n), Term::Lit(Lit::Int(0))],
                )),
                int(0),
                Arc::new(Term::prim(
                    Prim::Add,
                    vec![
                        Term::Var(n),
                        Term::App(
                            Arc::new(Term::Var(f)),
                            Arc::new(Term::prim(
                                Prim::Sub,
                                vec![Term::Var(n), Term::Lit(Lit::Int(1))],
                            )),
                        ),
                    ],
                )),
            ),
        );
        let term = Term::letrec(vec![(f, lam)], Term::App(Arc::new(Term::Var(f)), int(5)));
        assert_eq!(eval_term(term).unwrap().to_string(), "15");
    }

    #[test]
    fn handler_state_like() {
        // handle (perform E.get () ; perform E.get ())  with
        //   get _ k -> k 7
        //   return x -> x
        // ==> 7  (each `get` resumes with 7; the body's value is the 2nd get)
        let k = v();
        let p = v();
        let x = v();
        let get = |_arg: Arc<Term>| Term::perform("E", "get", Term::Lit(Lit::Unit));
        let discard = v();
        let body = Term::let_(discard, get(int(0)), get(int(0)));
        let term = Term::Handle {
            body: Arc::new(body),
            clauses: vec![HClause {
                effect: "E".into(),
                op: "get".into(),
                param: p,
                param_ty: core::unknown(),
                resume: k,
                resume_ty: core::unknown(),
                body: Term::App(Arc::new(Term::Var(k)), int(7)),
            }],
            ret: Some((x, core::unknown(), Arc::new(Term::Var(x)))),
            ty: core::unknown(),
        };
        assert_eq!(eval_term(term).unwrap().to_string(), "7");
    }

    #[test]
    fn unhandled_effect_errors() {
        let term = Term::perform("E", "boom", Term::Lit(Lit::Unit));
        let e = eval_term(term).unwrap_err();
        assert!(e.msg.contains("unhandled effect E.boom"), "{}", e.msg);
    }

    #[test]
    fn one_shot_resume_twice_errors() {
        // clause resumes k, then tries to resume k again
        let k = v();
        let p = v();
        let term = Term::Handle {
            body: Arc::new(Term::perform("E", "op", Term::Lit(Lit::Unit))),
            clauses: vec![HClause {
                effect: "E".into(),
                op: "op".into(),
                param: p,
                param_ty: core::unknown(),
                resume: k,
                resume_ty: core::unknown(),
                // k 1 ; k 2
                body: Term::let_(
                    v(),
                    Term::App(Arc::new(Term::Var(k)), int(1)),
                    Term::App(Arc::new(Term::Var(k)), int(2)),
                ),
            }],
            ret: None,
            ty: core::unknown(),
        };
        let e = eval_term(term).unwrap_err();
        assert!(e.msg.contains("resumed more than once"), "{}", e.msg);
    }

    #[test]
    fn value_display_forms() {
        assert_eq!(Value::Unit.to_string(), "()");
        assert_eq!(Value::Bool(true).to_string(), "True");
        assert_eq!(
            Value::ctor("Maybe.Just".into(), vec![Value::Int(3)]).to_string(),
            "Just(3)"
        );
    }

    #[test]
    fn running_tests_does_not_run_the_program() {
        // `def main = ...` is evaluated at load, which is right for `run` and
        // wrong for `test`: a `main` that reads input would block the suite, and
        // one with any other effect would perform it. Here `main` divides by
        // zero, so loading it at all would fail the whole run.
        let m = v();
        let t = v();
        let program = Program {
            defs: vec![
                core::Def::untyped(
                    m,
                    "main",
                    Term::prim(
                        Prim::Div,
                        vec![Term::Lit(Lit::Int(1)), Term::Lit(Lit::Int(0))],
                    ),
                ),
                core::Def::untyped(t, "t", Term::lam(v(), Term::Lit(Lit::Int(7)))),
            ],
            entry: Some(m),
            ..Default::default()
        };
        let results = run_tests(&program, &[t]).expect("the runner should not itself fail");
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().map(|v| v.to_string()).ok(),
            Some("7".to_string()),
            "the test runs and `main` never does"
        );
    }

    #[test]
    fn prim_arithmetic_direct() {
        assert_eq!(
            run_prim(Prim::Mul, vec![Value::Int(4), Value::Int(5)])
                .unwrap()
                .to_string(),
            "20"
        );
        assert!(run_prim(Prim::Div, vec![Value::Int(1), Value::Int(0)]).is_err());
    }

    #[test]
    fn structural_equality() {
        let a = Value::Tuple(vec![Value::Int(1), vector_value(vec![Value::Int(2)])]);
        let b = Value::Tuple(vec![Value::Int(1), vector_value(vec![Value::Int(2)])]);
        assert!(value_eq(&a, &b));
        assert!(!value_eq(&a, &Value::Int(1)));
    }
}

// --- green threads ------------------------------------------------------------

/// A thread waiting for an answer, and the thread's number.
type Waiting<'a> = (Machine<'a>, u32);

#[derive(Default)]
struct TaskSlot<'a> {
    outcome: Option<Result<Value, String>>,
    waiters: Vec<Waiting<'a>>,
}

#[derive(Default)]
struct ChannelSlot<'a> {
    messages: VecDeque<Value>,
    receivers: VecDeque<Waiting<'a>>,
}

/// Run `main` and every thread it starts, one at a time, until `main` finishes.
///
/// The specification of what the bytecode VM's scheduler does, minus the
/// parallelism: threads take turns of [`SLICE`] steps in a fixed order, so a
/// run here is always the same run. A value passed between threads is shared
/// rather than copied -- values here are immutable and reference-counted, and
/// [`sendable`] has already refused anything a copy would not be equal to.
/// See `meadow_core::thread` for what the engines agree on.
fn schedule(main: Machine<'_>) -> Result<Value, RuntimeError> {
    let mut ready: VecDeque<(Machine, u32)> = VecDeque::from([(main, 0)]);
    let mut tasks: Vec<TaskSlot> = vec![TaskSlot::default()];
    let mut channels: Vec<ChannelSlot> = Vec::new();
    // Threads parked by `retry`, and which `TVar`s -- by address -- each waits on.
    let mut parked: Vec<Option<(Machine, u32)>> = Vec::new();
    let mut stm_waiters: HashMap<usize, Vec<usize>> = HashMap::new();

    while let Some((mut m, task)) = ready.pop_front() {
        loop {
            match m.run_slice() {
                Stop::Preempted => {
                    ready.push_back((m, task));
                    break;
                }
                Stop::Done(v) => {
                    if task == 0 {
                        return Ok(v);
                    }
                    finish(&mut tasks, &mut ready, task, Ok(v));
                    break;
                }
                Stop::Failed(e) => {
                    if task == 0 {
                        return Err(e);
                    }
                    finish(&mut tasks, &mut ready, task, Err(e.msg));
                    break;
                }
                Stop::Requested(r) => match r {
                    Request::Spawn(f) => {
                        let id = tasks.len() as u32;
                        tasks.push(TaskSlot::default());
                        let child = Machine::new(
                            Control::Ret(f),
                            vec![K::EvalArg {
                                arg: Arc::new(core::Term::Lit(core::Lit::Unit)),
                                env: root_env(),
                            }],
                            m.fields,
                        );
                        ready.push_back((child, id));
                        m.ctrl = Control::Ret(Value::Task(id));
                    }
                    Request::Await(t) => match tasks.get_mut(t as usize) {
                        None => m.failed = Some(format!("await: there is no thread {t}")),
                        Some(slot) => match &slot.outcome {
                            Some(Ok(v)) => m.ctrl = Control::Ret(v.clone()),
                            Some(Err(msg)) => m.failed = Some(msg.clone()),
                            None => {
                                slot.waiters.push((m, task));
                                break;
                            }
                        },
                    },
                    Request::Yield => {
                        m.ctrl = Control::Ret(Value::Unit);
                        ready.push_back((m, task));
                        break;
                    }
                    Request::NewChannel => {
                        let id = channels.len() as u32;
                        channels.push(ChannelSlot::default());
                        m.ctrl = Control::Ret(Value::Channel(id));
                    }
                    Request::Send(c, v) => match channels.get_mut(c as usize) {
                        None => m.failed = Some(format!("send: there is no channel {c}")),
                        Some(ch) => {
                            match ch.receivers.pop_front() {
                                Some((mut r, rt)) => {
                                    r.ctrl = Control::Ret(v);
                                    ready.push_back((r, rt));
                                }
                                None => ch.messages.push_back(v),
                            }
                            m.ctrl = Control::Ret(Value::Unit);
                        }
                    },
                    Request::StmCommit => {
                        let txn = m.txn.take().expect("checked by the primitive");
                        if txn
                            .reads
                            .iter()
                            .any(|(tv, version)| tv.version.get() != *version)
                        {
                            m.ctrl = Control::Ret(Value::Bool(false));
                            continue;
                        }
                        let writes = txn.writes.into_iter().next().unwrap_or_default();
                        if !writes.is_empty() {
                            let version = STM_CLOCK.with(|c| {
                                c.set(c.get() + 1);
                                c.get()
                            });
                            for (tv, v) in writes {
                                tv.version.set(version);
                                *tv.value.borrow_mut() = v;
                                for i in stm_waiters
                                    .remove(&(Rc::as_ptr(&tv) as usize))
                                    .unwrap_or_default()
                                {
                                    if let Some((mut w, wt)) = parked[i].take() {
                                        w.ctrl = Control::Ret(Value::Unit);
                                        ready.push_back((w, wt));
                                    }
                                }
                            }
                        }
                        m.ctrl = Control::Ret(Value::Bool(true));
                    }
                    Request::StmWait => {
                        let txn = m.txn.take().expect("checked by the primitive");
                        if txn
                            .reads
                            .iter()
                            .any(|(tv, version)| tv.version.get() != *version)
                        {
                            m.ctrl = Control::Ret(Value::Unit);
                            continue;
                        }
                        let slot = parked.len();
                        for (tv, _) in &txn.reads {
                            stm_waiters
                                .entry(Rc::as_ptr(tv) as usize)
                                .or_default()
                                .push(slot);
                        }
                        parked.push(Some((m, task)));
                        break;
                    }
                    Request::Receive(c) => match channels.get_mut(c as usize) {
                        None => m.failed = Some(format!("receive: there is no channel {c}")),
                        Some(ch) => match ch.messages.pop_front() {
                            Some(v) => m.ctrl = Control::Ret(v),
                            None => {
                                ch.receivers.push_back((m, task));
                                break;
                            }
                        },
                    },
                },
            }
        }
    }
    // The queue ran dry with `main` still waiting.
    err(core::thread::DEADLOCK)
}

/// Record a thread's result, and wake whoever awaits it.
fn finish<'a>(
    tasks: &mut [TaskSlot<'a>],
    ready: &mut VecDeque<(Machine<'a>, u32)>,
    task: u32,
    outcome: Result<Value, String>,
) {
    let slot = &mut tasks[task as usize];
    for (mut w, wt) in slot.waiters.drain(..) {
        match &outcome {
            Ok(v) => w.ctrl = Control::Ret(v.clone()),
            Err(msg) => w.failed = Some(msg.clone()),
        }
        ready.push_back((w, wt));
    }
    slot.outcome = Some(outcome);
}

/// Refuse a value another thread must not see: see `meadow_core::thread`.
///
/// A closure is judged by what its body uses, not by its whole environment,
/// which here holds everything in scope where it was made. The bytecode VM's
/// closures capture exactly what they use, so this is what makes the two
/// engines refuse the same functions.
fn sendable(v: &Value) -> Result<(), RuntimeError> {
    use core::thread::unsendable;
    let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let mut stack = vec![v.clone()];
    while let Some(v) = stack.pop() {
        match &v {
            Value::Ref(_) => return err(unsendable("a Ref")),
            Value::MutArray(_) => return err(unsendable("a mutable array")),
            Value::Cont(_) => return err(unsendable("a continuation")),
            // Immutable and closed: shared as it is, as the VM shares a region.
            Value::Compact(_) => {}
            Value::Closure { param, body, env } => {
                let key = (Arc::as_ptr(body) as usize, Rc::as_ptr(env) as usize);
                if seen.insert(key) {
                    for &var in free_vars(body).iter() {
                        if var != *param {
                            if let Some(x) = lookup(env, var) {
                                stack.push(x);
                            }
                        }
                    }
                }
            }
            Value::Builtin { args, .. } => stack.extend(args.iter().cloned()),
            Value::Ctor(_, fields) => {
                if seen.insert((Rc::as_ptr(fields) as *const () as usize, 0)) {
                    stack.extend(fields.iter().cloned());
                }
            }
            Value::Array(xs) => {
                if seen.insert((Rc::as_ptr(xs) as *const () as usize, 0)) {
                    stack.extend(xs.iter().cloned());
                }
            }
            Value::Tuple(xs) => stack.extend(xs.iter().cloned()),
            Value::Record(fields) => stack.extend(fields.values().cloned()),
            Value::Int(_)
            | Value::BigInt(_)
            | Value::Float(_)
            | Value::Word(..)
            | Value::Float32(_)
            | Value::Bool(_)
            | Value::Str(_)
            | Value::Char(_)
            | Value::Unit
            | Value::Channel(_)
            | Value::Task(_)
            | Value::TVar(_) => {}
            // A top-level value a closure mentions. It is pure, and the other
            // thread evaluates its own copy when it needs it.
            Value::Lazy(_) => {}
        }
    }
    Ok(())
}

/// The free variables of a function body, computed once per body.
fn free_vars(body: &Arc<Term>) -> Rc<Vec<Var>> {
    thread_local! {
        static CACHE: RefCell<HashMap<usize, (Arc<Term>, Rc<Vec<Var>>)>> = RefCell::new(HashMap::new());
    }
    let key = Arc::as_ptr(body) as usize;
    if let Some(found) = CACHE.with(|c| c.borrow().get(&key).map(|(_, v)| v.clone())) {
        return found;
    }
    let mut out = std::collections::HashSet::new();
    core::free_vars_into(body, &mut out);
    let vars = Rc::new(out.into_iter().collect::<Vec<_>>());
    // The body is kept alongside, so its address cannot be reused by another.
    CACHE.with(|c| c.borrow_mut().insert(key, (body.clone(), vars.clone())));
    vars
}

fn as_tvar(v: &Value) -> Result<Rc<TVarCell>, RuntimeError> {
    match v {
        Value::TVar(tv) => Ok(tv.clone()),
        other => err(format!("expected a TVar, got {other}")),
    }
}

/// Refuse what a `TVar` cannot hold -- the same as what a `Compact` cannot, so
/// that on the VM its value can live in a shared region.
fn storable(v: &Value) -> Result<(), RuntimeError> {
    compact_into(&RefCell::new(Region::default()), v, core::stm::unstorable)
}

/// Whether standard input (0), output (1) or error (2) is a terminal:
/// `Process.isTerminal`, for a program choosing whether to colour what it
/// writes: for output, a terminal that draws colour -- which a Windows console
/// does once asked to (`meadow_core::console::ansi`).
fn is_terminal(fd: i64) -> bool {
    use std::io::IsTerminal;
    match fd {
        0 => std::io::stdin().is_terminal(),
        1 => std::io::stdout().is_terminal() && meadow_core::console::ansi(),
        2 => std::io::stderr().is_terminal() && meadow_core::console::ansi(),
        _ => false,
    }
}
