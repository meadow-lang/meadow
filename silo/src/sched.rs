//! **Green threads, channels and transactions**, M:N over OS threads. See
//! `docs/SILO.md`, "Threads".
//!
//! A thread is a coroutine -- a stack segment, as a `handle` is -- with a
//! context of its own (`crate::ctx`): its own heap, so counting needs no
//! atomics, and what crosses between threads is copied ([`Parcel`]). A worker
//! per core takes ready threads off one queue and runs each until it waits: on
//! a thread it awaits, on an empty channel, in a transaction's `retry`, or by
//! yielding. `main` is a thread too, and the program ends when it does;
//! threads still running then are stopped. A thread that neither waits nor
//! finishes gives way at a safe point once its turn is up, so one that never
//! waits cannot keep a core to itself.
//!
//! None of this is here for a program that cannot spawn: it runs on the
//! thread that called it, with no scheduler, no workers and no safe points
//! (`crate::ctx`, and `meadow_llvm::emit`).
//!
//! A thread that waits says why in its context and suspends -- every segment
//! of it, out to the worker, as performing an operation further out does
//! ([`crate::segments`]). The worker files it, under the scheduler's lock, with
//! what it waits on, or puts it straight back if that is already there: no
//! wake-up can fall between the thread deciding to wait and its waiting.
//!
//! A thread asked to stop (`meadow_core::thread`) fails at its next thread
//! operation, which looks at its [`Ctl`] first, or at its next operation of
//! the world's (`crate::native`). One that makes neither is ended at a safe
//! point: while any thread is cancelled and not yet ended, the flag the safe
//! points read stays up, so that the thread comes to [`meadow_preempted`]
//! even with nothing else to run; the first time marks it, and one at least a
//! tick of the timer later ends it, unless it has asked and been told. A
//! thread already waiting is found and woken to fail. Filing it, the worker
//! says in its `Ctl` where it waits and then looks at the flag, under the
//! lock of what it waits on; a canceller sets the flag and then reads where.
//! So either the canceller finds it there, under that same lock, or the thread
//! saw the flag and was not filed. Where is only a hint: nothing clears it
//! when the thread is woken some other way, and a canceller looking there
//! then finds nobody -- rightly, since that thread is running.
//!
//! A `Time.sleep` parks too, with the time it wakes at, and is filed on the
//! timer ([`sleep`]) rather than with anything another thread does: one timer
//! thread puts each sleeper back to run when its time comes.
//!
//! A thread whose code returns or fails runs the releases left on its
//! release stack (`Ctx::exits`) first, where it is ([`wind_down`]): on its own
//! coroutine as it returns, or on top of the frames that failed, which
//! nothing resumes, before it parks to fail. A cancel is held off meanwhile
//! (`Ctl`'s count), and a release still running [`RELEASE_TICKS`] ticks after
//! a safe point first found the thread cancelled in it is ended there. See
//! `docs/CANCELLATION.md`.
//!
//! A transaction keeps its writes in a log, one level per `orElse`, and
//! reads a `TVar` only if nothing has written it since the transaction began.
//! Its commit checks that everything it read is as it was, and publishes its
//! writes, under the lock; either may find a conflict, and the transaction
//! runs again.

use crate::ctx::{self, Ctx, Request, Txn, Wake};
use crate::heap::{self, Word};
use crate::parcel::Parcel;
use crate::segments::{self, Down, SendSegment, Up};
use crate::value::{self, Val};
use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};
use meadow_rt::{Prim, desc};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};

/// A thread's stack: reserved, and committed as it is touched.
const THREAD: usize = 256 << 20;
/// `main`'s: non-tail recursion over a long list is ordinary Meadow.
const MAIN: usize = 1 << 30;

/// A thread that is not running.
struct Green {
    ctx: Box<Ctx>,
    co: SendSegment,
}

/// A thread's cancellation: whether it has been asked to stop, and where it
/// last waited. See the module docs.
#[derive(Default)]
pub struct Ctl {
    cancelled: AtomicBool,
    parked: Mutex<Parked>,
    /// It asked whether it had been cancelled, and was told so: it stops of
    /// its own accord, and is not ended at a safe point. Its own alone.
    seen: AtomicBool,
    /// The timer's [`TICKS`], plus one, when a safe point first found it
    /// cancelled -- or, cancelled, in the release it runs; `0` until then.
    /// Its own alone.
    armed: AtomicU64,
    /// Releases it runs in place, and one more once it is on its way out:
    /// while there are any, a cancel is held off -- its operations answer and
    /// its waits are not broken into. See `docs/CANCELLATION.md`.
    held_off: std::sync::atomic::AtomicU32,
}

/// Where a thread last waited.
#[derive(Default, Clone)]
enum Parked {
    #[default]
    Nowhere,
    /// On the thread with this number, in `await`.
    Await(usize),
    /// On the channel at this address.
    Receive(usize),
    /// In `retry`, under this key of the scheduler's `waiting`.
    Wait(u64),
    /// On a call into the world, which an I/O thread makes.
    Io(std::sync::Weak<Mutex<Option<Box<Green>>>>),
    /// In `Time.sleep`, on the timer.
    Sleep(std::sync::Weak<Mutex<Option<Box<Green>>>>),
}

impl Ctl {
    /// The thread is about to wait `at`: say so, then answer whether it has
    /// been cancelled, in which case it must not wait. Called with what it
    /// waits on locked.
    fn park(&self, at: Parked) -> bool {
        *self.parked.lock().unwrap_or_else(|p| p.into_inner()) = at;
        self.stops()
    }

    fn held_off(&self) -> bool {
        self.held_off.load(Ordering::SeqCst) > 0
    }

    /// Does a cancel stop it at an operation, or wake it from a wait? Not
    /// while it is held off.
    fn stops(&self) -> bool {
        self.cancelled() && !self.held_off()
    }

    /// A release begins: in a cancelled thread, its own grace.
    fn release_begins(&self) {
        self.armed.store(0, Ordering::Relaxed);
        self.seen.store(false, Ordering::Relaxed);
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Fail the running thread if it has been asked to stop: what every thread
/// operation does first, and every operation the runtime answers
/// (`crate::native`).
pub(crate) fn notice_cancel() {
    if ctx::present() && cx().cancel.stops() {
        crate::fail(meadow_rt::thread::CANCELLED)
    }
}

/// Bits of [`meadow_preempt`]: the timer asks the running threads to give way
/// ...
const GIVE_WAY: u8 = 1;
/// ... or a thread has been cancelled and has not ended, and may be one that
/// makes no operation: every safe point looks, until it has.
const ENDING: u8 = 2;

/// Threads cancelled and not yet ended. Changed under the scheduler's lock.
static CANCELLED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Times the timer has ticked: what a cancelled thread's grace is counted in.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// A safe point: end the running thread if it was cancelled and has neither
/// noticed nor asked -- a thread that makes no operation, which nothing else
/// would stop. Not at the first safe point after the cancel, which only marks
/// it, but at one at least a whole tick of the timer later: a thread that is
/// about to perform an operation, or to ask, does so and stops softly. Not
/// where the runtime's own call into the program has nowhere to suspend to,
/// either: it is ended at a safe point after that call returns.
fn end_if_cancelled() {
    if !ctx::present() || segments::innermost().is_none() {
        return;
    }
    let c = &cx().cancel;
    if cx().tid == 0 || !c.cancelled() || c.seen.load(Ordering::Relaxed) {
        return;
    }
    let now = TICKS.load(Ordering::Relaxed);
    // A release it runs, held off, gets longer: see `docs/CANCELLATION.md`.
    let grace = if c.held_off() { RELEASE_TICKS } else { 1 };
    match c.armed.load(Ordering::Relaxed) {
        0 => c.armed.store(now + 1, Ordering::Relaxed),
        at if now >= at + grace => crate::fail(meadow_rt::thread::CANCELLED),
        _ => {}
    }
}

/// Ticks of the timer a release in a cancelled thread is given before it is
/// ended at a safe point, where other code is given one.
const RELEASE_TICKS: u64 = 16;

/// Take thread `tid` out of where it waits, `at`, and wake it to fail; if it
/// is still there.
fn unpark(tid: usize, at: Parked) {
    let fail = || Wake::Fail(meadow_rt::thread::CANCELLED.into());
    match at {
        Parked::Nowhere => {}
        Parked::Await(on) => {
            let mut w = world();
            let Some(task) = w.tasks.get_mut(on) else {
                return;
            };
            if let Some(i) = task.waiters.iter().position(|g| g.ctx.tid == tid) {
                let g = task.waiters.remove(i);
                wake(&mut w, g, fail());
            }
        }
        Parked::Receive(c) => {
            // Safety: the address of a channel that lives for the whole run.
            let c = unsafe { &*(c as *const Chan) };
            let g = {
                let mut state = chan_state(c);
                let i = state.receivers.iter().position(|g| g.ctx.tid == tid);
                i.and_then(|i| state.receivers.remove(i))
            };
            // The channel's lock let go first: it comes before the scheduler's.
            if let Some(g) = g {
                ready(g, fail());
            }
        }
        Parked::Wait(key) => {
            // Its key stays on the `TVar`s it read, and a commit that finds
            // it gone wakes nobody.
            let mut w = world();
            if let Some(g) = w.waiting.remove(&key) {
                wake(&mut w, g, fail());
            }
        }
        // The call is made all the same, if it has begun; what it answers
        // finds nobody waiting, and is dropped.
        Parked::Io(slot) => {
            let g = slot
                .upgrade()
                .and_then(|s| s.lock().unwrap_or_else(|p| p.into_inner()).take());
            if let Some(g) = g {
                ready(g, fail());
            }
        }
        // Left on the timer, which wakes nobody when it comes due; counted
        // off here, as the timer would have.
        Parked::Sleep(slot) => {
            let g = slot
                .upgrade()
                .and_then(|s| s.lock().unwrap_or_else(|p| p.into_inner()).take());
            if let Some(g) = g {
                let mut w = world();
                w.calls -= 1;
                wake(&mut w, g, fail());
            }
        }
    }
}

#[derive(Default)]
struct Task {
    outcome: Option<Result<Arc<Parcel>, String>>,
    waiters: Vec<Box<Green>>,
    /// The thread's, while it runs: see [`Ctx::cancel`].
    ctl: Option<Arc<Ctl>>,
    /// Nothing can `await` it any more -- every `Task` of it has gone -- so
    /// what it answers is not kept: see [`held`].
    nobody: bool,
}

// --- who can still await a thread ------------------------------------------
//
// What a thread answered is kept for `await`, and may be awaited more than
// once, so it is kept while anything could ask: while there is a `Task` of
// the thread in some heap, or in a parcel on its way to one. Kept for good,
// as it was, a thread's answer -- and every compact region in it -- outlived
// the program: a compiler that asked each file's tree of a thread held every
// tree it had ever made.
//
// The count has a lock of its own, taken alone: a `Task` is erased, and a
// parcel dropped, from places that hold the scheduler's. When it reaches
// nothing the thread is put on a list, and the scheduler lets go of its
// answer where it next takes its own lock afresh ([`forget`]).
//
// A `Task` in a heap that goes whole -- its thread ended still holding it --
// is never counted off, and its thread's answer stays, as before.

static HANDLES: Mutex<Option<HashMap<u32, usize>>> = Mutex::new(None);
static FORGOTTEN: Mutex<Vec<u32>> = Mutex::new(Vec::new());
// The same for a `TVar`, by its cell's address: while there is a handle to
// it -- in a heap, in a parcel, or inside a region, where a `TVar`'s value
// may hold others -- its value is kept, and when the last has gone the value
// is let go of, and its region with it. The cell itself stays: a transaction
// under way may still have its address.
static VARS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);
static FORGOTTEN_VARS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Whether that list has anything on it: read where a heap asks the system
/// for more, so that a thread working on alone -- no other is scheduled, and
/// the scheduler does not look -- still lets go of what it can before it
/// takes more.
static VARS_WAITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a `TVar` nothing can reach is waiting to let its value go.
#[inline]
pub fn vars_waiting() -> bool {
    VARS_WAITING.load(Ordering::Relaxed)
}

/// One more handle to the `TVar` whose cell is at `at`.
pub fn held_var(at: usize) {
    let mut h = VARS.lock().unwrap_or_else(|p| p.into_inner());
    *h.get_or_insert_with(HashMap::new).entry(at).or_default() += 1;
}

/// One fewer.
pub fn let_go_var(at: usize) {
    let last = {
        let mut h = VARS.lock().unwrap_or_else(|p| p.into_inner());
        let h = h.get_or_insert_with(HashMap::new);
        match h.get_mut(&at) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                h.remove(&at);
                true
            }
            None => false,
        }
    };
    if last {
        FORGOTTEN_VARS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(at);
        VARS_WAITING.store(true, Ordering::Relaxed);
    }
}

/// Let go of the values of the `TVar`s nothing can reach. Takes no lock but
/// each cell's own, so it may be called from wherever no cell's is held.
pub fn forget_vars() {
    // A value let go of may hold the last handles to other `TVar`s, which
    // puts them on the list in turn: until it is empty.
    loop {
        VARS_WAITING.store(false, Ordering::Relaxed);
        let vars = std::mem::take(&mut *FORGOTTEN_VARS.lock().unwrap_or_else(|p| p.into_inner()));
        if vars.is_empty() {
            break;
        }
        for at in vars {
            // Safety: a cell's address, and a cell lives for the whole run.
            let cell = unsafe { &*(at as *const TVar) };
            let was = {
                let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
                std::mem::replace(&mut state.0, Held::Imm(0, desc::INT))
            };
            drop(was);
        }
    }
}

/// One more `Task` of thread `tid`: made by `spawn`, copied into a parcel,
/// or arrived out of one.
pub fn held(tid: u32) {
    let mut h = HANDLES.lock().unwrap_or_else(|p| p.into_inner());
    *h.get_or_insert_with(HashMap::new).entry(tid).or_default() += 1;
}

/// One fewer: a `Task` erased, or a parcel holding one dropped.
pub fn let_go(tid: u32) {
    let last = {
        let mut h = HANDLES.lock().unwrap_or_else(|p| p.into_inner());
        let h = h.get_or_insert_with(HashMap::new);
        match h.get_mut(&tid) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                h.remove(&tid);
                true
            }
            None => false,
        }
    };
    if last {
        FORGOTTEN
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(tid);
    }
}

/// Let go of what the threads nobody can await answered. Called with the
/// scheduler's lock not held: the answers are dropped once it is released.
pub fn forget() {
    forget_vars();
    let gone = std::mem::take(&mut *FORGOTTEN.lock().unwrap_or_else(|p| p.into_inner()));
    if gone.is_empty() {
        return;
    }
    let answers: Vec<_> = {
        let mut w = world();
        gone.iter()
            .filter_map(|t| {
                let task = w.tasks.get_mut(*t as usize)?;
                task.nobody = true;
                task.outcome.take()
            })
            .collect()
    };
    drop(answers);
}

/// A channel: what has been sent and not yet taken, and the threads waiting
/// to take something.
///
/// A lock of its own, not the scheduler's: a send and a receive that pass
/// each other touch this and nothing else, and channels that are busy at the
/// same time never meet. Taken before the scheduler's lock, never after --
/// which is the whole of the ordering that keeps the two apart.
#[derive(Default)]
struct Chan {
    state: Mutex<ChanState>,
}

#[derive(Default)]
struct ChanState {
    messages: VecDeque<Parcel>,
    receivers: VecDeque<Box<Green>>,
}

/// The channel a handle names: its block holds the address, as a `TVar`'s
/// does. One lives as long as the program.
fn chan(v: Val, op: &str) -> &'static Chan {
    let at = match v {
        Val::Ref(w) if heap::is_block(w) && heap::kind(w) == heap::CHANNEL => heap::field(w, 0),
        other => {
            let (w, d) = other.bits();
            crate::fail(&format!(
                "{op}: expected a channel, got {}",
                crate::show::show(w, d)
            ))
        }
    };
    // Safety: the address of a channel made by `ChannelNew`, which lives for
    // the whole run.
    unsafe { &*(at as *const Chan) }
}

fn chan_state(c: &Chan) -> std::sync::MutexGuard<'_, ChanState> {
    c.state.lock().unwrap_or_else(|p| p.into_inner())
}

/// What a `TVar` holds: a value that is a word and nothing more -- an `Int`,
/// a `Bool` -- kept as it is, and anything else in a compact region, where
/// every thread reads it in place. So what a region cannot hold -- a value
/// reaching a function, a `Ref`, a mutable array or a continuation -- a `TVar`
/// cannot either, as on `meadow-glade` (`meadow_core::stm`).
///
/// Reading a `TVar` that holds a large map costs a pointer, not a copy of the
/// map -- as it does on `meadow-glade`, whose `stm` module this follows. A
/// `TVar` used to hold a parcel, and every read copied all of it into the
/// reader's heap and every write all of it out: a query database keeping its
/// answers in one `TVar` spent nine tenths of its time copying them.
#[derive(Clone)]
enum Held {
    Imm(Word, i64),
    Shared(Arc<InRegion>),
}

/// A value inside a region, and a reference to the region for as long as a
/// `TVar` or a transaction holds it.
struct InRegion {
    region: *const crate::region::Region,
    word: Word,
    desc: i64,
    /// Words the region held when it was last started afresh -- about what a
    /// copy of the value costs, for deciding when it has grown too much.
    fresh: usize,
}

// Safety: a region is closed and immutable once published, and its count is
// atomic: see `crate::region`.
unsafe impl Send for InRegion {}
unsafe impl Sync for InRegion {}

impl Drop for InRegion {
    fn drop(&mut self) {
        // Safety: the reference this holds, given up.
        unsafe { crate::region::release(self.region) };
    }
}

/// Words below which a region is always added to rather than started afresh:
/// a small value's region is cheap to keep, and a fresh copy of it is too.
const REGION_FLOOR: usize = 1 << 12;

impl Held {
    /// `v` to hold, in place of `was`. A write copies the new value into the
    /// region the old one is in, so whatever the two have in common -- a map
    /// with one entry more shares all the rest -- is not copied again. That
    /// region only grows, so once it has grown to four times a fresh copy a
    /// write starts a new one, and the old is freed when nothing reads it.
    fn of(v: Val, was: Option<&Held>) -> Held {
        let (w, d) = v.bits();
        if d != desc::REF || !heap::is_block(w) {
            return Held::Imm(w, d);
        }
        let grown = |s: &InRegion| {
            // Safety: `s` holds a reference to its region.
            let words = unsafe { crate::region::words(s.region) };
            words > 4 * s.fresh.max(REGION_FLOOR)
        };
        let into = match was {
            Some(Held::Shared(s)) if !grown(s) => Some((s.region, s.fresh)),
            _ => None,
        };
        // Safety: `was` holds a reference to the region added to.
        match unsafe { crate::region::share_into(into.map(|i| i.0), w, d) } {
            Ok((region, word)) => {
                let fresh = match into {
                    Some((_, f)) => f,
                    // Safety: the reference `share_into` answered.
                    None => unsafe { crate::region::words(region) },
                };
                Held::Shared(Arc::new(InRegion {
                    region,
                    word,
                    desc: d,
                    fresh,
                }))
            }
            Err(why) => {
                let what = why.strip_prefix("a compact cannot hold ").unwrap_or(&why);
                crate::fail(&meadow_rt::stm::unstorable(what))
            }
        }
    }

    /// The value, in the running thread's heap and owned by the caller.
    fn open(&self) -> (Word, i64) {
        match self {
            Held::Imm(w, d) => (*w, *d),
            Held::Shared(s) => {
                // Safety: `s` holds a reference to the region; the caller
                // gets one of its own, which erasing the value gives up.
                unsafe { crate::region::retain(s.region) };
                (s.word, s.desc)
            }
        }
    }
}

/// A `TVar`: what it holds, the clock when it was last written, and the
/// threads waiting in `retry` for it to be written (keys of the scheduler's
/// `waiting`).
///
/// A lock of its own, rather than one lock over all of them: a transaction
/// holds it for as long as a word takes to copy, and threads touching
/// different `TVar`s never meet. What touches several locks them in address
/// order, so two of them cannot hold each other up.
///
/// One lives as long as the program: a `TVar` is a handle any thread may
/// hold, and nothing says when the last of them is gone.
struct TVar {
    state: Mutex<(Held, u64)>,
    waiters: Mutex<Vec<u64>>,
}

/// Counts commits that wrote something: what a `TVar`'s version is, and what
/// a transaction remembers as its beginning.
static CLOCK: AtomicU64 = AtomicU64::new(0);
/// Numbers `TVar`s and channels, for printing.
static NEXT_TVAR: AtomicU64 = AtomicU64::new(0);
static NEXT_CHAN: AtomicU64 = AtomicU64::new(0);

/// The cell a handle names. The block holds its address, so reading or
/// writing one is no lookup at all -- and a handle copied to another thread
/// carries the address, which is the same everywhere.
fn cell(v: Val, op: &str) -> &'static TVar {
    let at = match v {
        Val::Ref(w) if heap::is_block(w) && heap::kind(w) == heap::TVAR => heap::field(w, 0),
        other => {
            let (w, d) = other.bits();
            crate::fail(&format!(
                "{op}: expected a TVar, got {}",
                crate::show::show(w, d)
            ))
        }
    };
    // Safety: the address of a cell that lives for the whole run, put in the
    // block by `StmNew`.
    unsafe { &*(at as *const TVar) }
}

fn at(c: &TVar) -> usize {
    c as *const TVar as usize
}

/// Lock `cells`, each once, in address order: two transactions locking any of
/// the same cells take them in the same order, so neither waits on the other.
/// Answers the cells locked, in that order, with their state.
fn lock_all<'a>(cells: &[&'a TVar]) -> (Vec<usize>, Vec<std::sync::MutexGuard<'a, (Held, u64)>>) {
    let mut order: Vec<&'a TVar> = cells.to_vec();
    order.sort_unstable_by_key(|c| at(c));
    order.dedup_by_key(|c| at(c));
    let ats = order.iter().map(|c| at(c)).collect();
    let held = order
        .into_iter()
        .map(|c| c.state.lock().unwrap_or_else(|p| p.into_inner()))
        .collect();
    (ats, held)
}

#[derive(Default)]
struct World {
    ready: VecDeque<Box<Green>>,
    tasks: Vec<Task>,
    /// Threads waiting in `retry`, by a number each `TVar` they read lists.
    waiting: HashMap<u64, Box<Green>>,
    next_wait: u64,
    /// Threads being run by a worker now.
    running: usize,
    /// Threads waiting on a call into the world: not deadlocked, whatever
    /// else is waiting.
    calls: usize,
    /// Whether the workers have started.
    started: bool,
    /// Whether `main` has finished: the program is over, and the workers
    /// stop wherever the threads they are running have got to.
    done: bool,
    /// What `main` answered, and its context, for [`main`] to take.
    answer: Option<(Box<Ctx>, Word)>,
}

static WORLD: LazyLock<Mutex<World>> = LazyLock::new(|| Mutex::new(World::default()));
static CHANGED: Condvar = Condvar::new();
/// Told when `main` has finished, for the thread waiting on it.
static FINISHED: Condvar = Condvar::new();

fn world() -> MutexGuard<'static, World> {
    WORLD.lock().unwrap_or_else(|p| p.into_inner())
}

unsafe extern "C" {
    /// Call the function value `f` with `arg`, under `ev`, and answer what it
    /// returns natively: defined by the emitted module.
    fn meadow_apply(f: Word, arg: Word, ev: Word) -> Word;
}

/// No handlers: what a thread starts with.
fn no_evidence() -> Word {
    (u64::from(value::tag(meadow_rt::roles::evidence::NONE)) << 1) | 1
}

fn cx() -> &'static mut Ctx {
    // Safety: the running thread's context, used by this OS thread alone
    // while the thread runs. Read afresh each time: see `crate::ctx`.
    unsafe { &mut *ctx::get() }
}

/// Stacks of threads that have finished, kept to start another with:
/// reserving one and setting its guard pages is what a `spawn` would
/// otherwise spend most of its time on.
static SPARE: Mutex<Vec<DefaultStack>> = Mutex::new(Vec::new());

fn spare() -> MutexGuard<'static, Vec<DefaultStack>> {
    SPARE.lock().unwrap_or_else(|p| p.into_inner())
}

fn coroutine(size: usize, body: impl FnOnce() -> Word + 'static) -> SendSegment {
    let kept = (size == THREAD).then(|| spare().pop()).flatten();
    let stack = kept.unwrap_or_else(|| {
        DefaultStack::new(size)
            .unwrap_or_else(|e| crate::fail(&format!("could not make a thread's stack: {e}")))
    });
    let base = corosensei::stack::Stack::base(&stack).get();
    SendSegment(Coroutine::with_stack(
        stack,
        move |y: &Yielder<Down, Up>, _: Down| {
            segments::enter_thread(y, base);
            let v = body();
            segments::leave_thread();
            v
        },
    ))
}

/// Keep a finished thread's stack to start another with, if there is room:
/// twice a worker each, so that a burst of threads does not hold a great deal
/// of address space for the rest of the run.
fn keep_stack(co: SendSegment) {
    let mut spare = spare();
    if spare.len() < 2 * workers() {
        spare.push(co.0.into_stack());
    }
}

/// Run `entry` as the program's `main` thread, with the scheduler; answer
/// `main`'s context and what it returned, in that context's heap.
pub fn main(entry: impl FnOnce() -> Word + 'static) -> (Box<Ctx>, Word) {
    let co = coroutine(MAIN, entry);
    let mut w = world();
    w.tasks.push(Task::default());
    w.ready.push_back(Box::new(Green {
        ctx: Ctx::new(0),
        co,
    }));
    start_workers(&mut w);
    // This thread waits rather than running threads itself: whichever worker
    // `main` finishes on, the program is over then, and a worker still inside
    // a thread that never waits must not hold the answer up.
    while w.answer.is_none() {
        w = FINISHED.wait(w).unwrap_or_else(|p| p.into_inner());
    }
    w.answer.take().expect("main finished")
}

/// A worker: run ready threads until `main` has finished.
fn work() {
    loop {
        forget();
        let mut g = {
            let mut w = world();
            loop {
                if w.done {
                    return;
                }
                if let Some(g) = w.ready.pop_front() {
                    w.running += 1;
                    break g;
                }
                if w.running == 0 && w.calls == 0 {
                    crate::fail(
                        "deadlock: every thread is waiting, on a channel nobody will send to \
                         or on another waiting thread",
                    )
                }
                w = CHANGED.wait(w).unwrap_or_else(|p| p.into_inner());
            }
        };
        ctx::set(&mut *g.ctx);
        let went = g.co.0.resume(Down::Wake);
        match went {
            CoroutineResult::Return(v) if g.ctx.tid == 0 => {
                ctx::set(std::ptr::null_mut());
                let mut w = world();
                w.running -= 1;
                w.done = true;
                w.answer = Some((g.ctx, v));
                CHANGED.notify_all();
                FINISHED.notify_all();
                return;
            }
            CoroutineResult::Return(v) => {
                ctx::set(std::ptr::null_mut());
                // Safety: what a thread's body returns -- see `ThreadSpawn`.
                let out = *unsafe { Box::from_raw(v as *mut Result<Parcel, String>) };
                let tid = g.ctx.tid;
                keep_stack(g.co);
                drop(g.ctx);
                let mut w = world();
                w.running -= 1;
                finish(&mut w, tid, out);
            }
            CoroutineResult::Yield(Up::Park) => {
                let request = g.ctx.request.take();
                ctx::set(std::ptr::null_mut());
                // Not under the scheduler's lock: filing may have to take a
                // channel's or a `TVar`'s, and those come first.
                file(g, request);
                let mut w = world();
                w.running -= 1;
                CHANGED.notify_all();
            }
            CoroutineResult::Yield(Up::Detach { .. }) => segments::escaped(),
        }
    }
}

/// How many OS threads run green threads: `MEADOW_THREADS` if it is set, or
/// one per core -- as `meadow-glade` reads it.
fn workers() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(count_workers)
}

fn count_workers() -> usize {
    std::env::var("MEADOW_THREADS")
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// Start the workers and the timer, once: the thread that called [`main`]
/// waits for the answer instead of running threads itself.
fn start_workers(w: &mut World) {
    if w.started {
        return;
    }
    w.started = true;
    STARTED.store(true, Ordering::SeqCst);
    for i in 0..workers() {
        let _ = std::thread::Builder::new()
            .name(format!("meadow worker {i}"))
            .stack_size(8 << 20)
            .spawn(work);
    }
    let _ = std::thread::Builder::new()
        .name("meadow timer".into())
        .spawn(tick);
}

/// How long a thread may keep a worker while others wait.
const SLICE: std::time::Duration = std::time::Duration::from_millis(5);

/// The flag the emitted code reads at its safe points: see
/// `meadow_llvm::emit::safe_point`. Set only while a thread is waiting for a
/// core, so that a program whose threads are all running reads a byte and
/// carries on.
#[unsafe(no_mangle)]
pub static meadow_preempt: AtomicU8 = AtomicU8::new(0);

/// The timer: while the program runs, ask the running threads to give way
/// whenever another is ready and waiting for a core.
fn tick() {
    loop {
        std::thread::sleep(SLICE);
        let w = world();
        if w.done {
            return;
        }
        TICKS.fetch_add(1, Ordering::Relaxed);
        if !w.ready.is_empty() {
            meadow_preempt.fetch_or(GIVE_WAY, Ordering::Relaxed);
        }
    }
}

/// A safe point, with the flag set: the thread has had its turn. It goes to
/// the back of the queue, and the next one runs.
///
/// The flag is cleared here rather than by the timer, so that one thread
/// giving way answers one timer tick: the others carry on until the next.
///
/// A thread cancelled and not yet ended keeps the flag up meanwhile, and so
/// every thread comes here at every safe point, where the cancelled one is
/// ended ([`end_if_cancelled`]) and the others carry straight on.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_preempted() {
    end_if_cancelled();
    // Read before it is written, so that threads passing safe points while
    // only `ENDING` is up do not all write the one byte.
    if meadow_preempt.load(Ordering::Relaxed) & GIVE_WAY == 0 {
        return;
    }
    meadow_preempt.fetch_and(!GIVE_WAY, Ordering::Relaxed);
    // Nothing waiting, or nowhere to suspend to (the runtime's own calls into
    // the program, which run to the end): carry on.
    if world().ready.is_empty() || segments::innermost().is_none() {
        return;
    }
    park(Request::Yield);
}

fn wake(w: &mut World, mut g: Box<Green>, with: Wake) {
    g.ctx.wake = with;
    w.ready.push_back(g);
    CHANGED.notify_one();
}

/// Thread `tid` is done: keep its outcome for `await`, and wake whoever
/// awaits it.
fn finish(w: &mut World, tid: usize, out: Result<Parcel, String>) {
    let out = out.map(Arc::new);
    if let Some(ctl) = w.tasks[tid].ctl.take()
        && ctl.cancelled()
        && CANCELLED.fetch_sub(1, Ordering::SeqCst) == 1
    {
        meadow_preempt.fetch_and(!ENDING, Ordering::SeqCst);
    }
    let waiters = std::mem::take(&mut w.tasks[tid].waiters);
    if !w.tasks[tid].nobody {
        w.tasks[tid].outcome = Some(out.clone());
    }
    for g in waiters {
        let with = match &out {
            Ok(p) => Wake::Value((**p).clone()),
            Err(m) => Wake::Fail(m.clone()),
        };
        wake(w, g, with);
    }
    CHANGED.notify_all();
}

/// A thread stopped to wait for `request`: put it where what it waits for
/// will find it -- or back to run, if that is already there.
fn file(mut g: Box<Green>, request: Option<Request>) {
    match request {
        None | Some(Request::Yield) => ready(g, Wake::Nothing),
        Some(Request::Await(t)) => {
            let mut w = world();
            match &w.tasks[t].outcome {
                Some(Ok(p)) => {
                    let p = (**p).clone();
                    wake(&mut w, g, Wake::Value(p));
                }
                Some(Err(m)) => {
                    let m = m.clone();
                    wake(&mut w, g, Wake::Fail(m));
                }
                None if g.ctx.cancel.park(Parked::Await(t)) => {
                    wake(&mut w, g, Wake::Fail(meadow_rt::thread::CANCELLED.into()));
                }
                None => w.tasks[t].waiters.push(g),
            }
        }
        Some(Request::Receive(c)) => {
            // Safety: the address of a channel that lives for the whole run.
            let at = c;
            let c = unsafe { &*(c as *const Chan) };
            let mut state = chan_state(c);
            match state.messages.pop_front() {
                Some(p) => {
                    drop(state);
                    ready(g, Wake::Value(p));
                }
                None if g.ctx.cancel.park(Parked::Receive(at)) => {
                    drop(state);
                    ready(g, Wake::Fail(meadow_rt::thread::CANCELLED.into()));
                }
                None => state.receivers.push_back(g),
            }
        }
        Some(Request::Wait(reads)) => {
            // The cells this read are locked while it is checked and the
            // thread written down, so a commit either changes one of them
            // before the check or finds the thread among the waiters.
            let cells: Vec<&TVar> = reads
                .iter()
                // Safety: addresses of cells that live for the whole run.
                .map(|(id, _)| unsafe { &*(*id as *const TVar) })
                .collect();
            let (ats, held) = lock_all(&cells);
            let changed = reads
                .iter()
                .any(|(id, seen)| held[ats.binary_search(id).expect("locked")].1 != *seen);
            if changed {
                drop(held);
                ready(g, Wake::Nothing);
                return;
            }
            let mut w = world();
            let key = w.next_wait;
            w.next_wait += 1;
            if g.ctx.cancel.park(Parked::Wait(key)) {
                drop(held);
                wake(&mut w, g, Wake::Fail(meadow_rt::thread::CANCELLED.into()));
                return;
            }
            for c in &cells {
                c.waiters
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(key);
            }
            drop(held);
            w.waiting.insert(key, g);
        }
        Some(Request::Block(call)) => {
            let slot = Arc::new(Mutex::new(Some(g)));
            let cancelled = {
                let held = slot.lock().unwrap_or_else(|p| p.into_inner());
                let g = held.as_ref().expect("just put there");
                g.ctx.cancel.park(Parked::Io(Arc::downgrade(&slot)))
            };
            if cancelled {
                let g = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
                ready(
                    g.expect("not filed yet"),
                    Wake::Fail(meadow_rt::thread::CANCELLED.into()),
                );
                return;
            }
            // Counted before the worker counts the thread out: see `work`.
            world().calls += 1;
            let mut io = io();
            io.calls.push_back((slot, call));
            if io.calls.len() > io.idle {
                // Every I/O thread is busy waiting on a call of its own.
                drop(io);
                let _ = std::thread::Builder::new()
                    .name("meadow io".into())
                    .spawn(io_thread);
            } else {
                IO_READY.notify_one();
            }
        }
        Some(Request::Sleep(until)) => {
            // Pending before it is filed where a canceller can take it out,
            // since whoever takes it out counts it off.
            world().calls += 1;
            let slot = Arc::new(Mutex::new(Some(g)));
            let cancelled = {
                let held = slot.lock().unwrap_or_else(|p| p.into_inner());
                let g = held.as_ref().expect("just put there");
                g.ctx.cancel.park(Parked::Sleep(Arc::downgrade(&slot)))
            };
            if cancelled {
                let g = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
                let mut w = world();
                w.calls -= 1;
                wake(
                    &mut w,
                    g.expect("not filed yet"),
                    Wake::Fail(meadow_rt::thread::CANCELLED.into()),
                );
                return;
            }
            let mut t = timers();
            let seq = t.next;
            t.next += 1;
            t.due.push(Due { until, seq, slot });
            TIMER_READY.notify_one();
            if !TIMING.swap(true, Ordering::SeqCst) {
                let _ = std::thread::Builder::new()
                    .name("meadow sleeps".into())
                    .spawn(timer_thread);
            }
        }
        Some(Request::Failed(msg)) => {
            let tid = g.ctx.tid;
            meadow_rt::net::close_owned(g.ctx.owner);
            // Never resumed, and not unwound.
            // Safety: nothing will resume it.
            unsafe { g.co.0.force_reset() };
            keep_stack(g.co);
            drop(g.ctx);
            let mut w = world();
            finish(&mut w, tid, Err(msg));
        }
    }
}

// --- calls into the world ----------------------------------------------------
//
// A socket's accept or read waits on the world, and a worker that made it
// would wait with it: enough of them -- a server's connections, each reading
// -- and no worker is left to run the thread that would answer. So a thread
// on the scheduler hands such a call to an I/O thread and parks; the I/O
// thread makes it and puts the thread back to run. One is started whenever a
// call finds none waiting for one, so no call waits for another's.

/// Whether the scheduler is running: without it, a call is made in place.
static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Default)]
struct IoQueue {
    /// Each call, and the thread waiting on it -- which a cancel may take
    /// out first, leaving the slot empty.
    calls: VecDeque<(Arc<Mutex<Option<Box<Green>>>>, Box<dyn FnOnce() + Send>)>,
    /// I/O threads waiting for a call.
    idle: usize,
}

static IO: LazyLock<Mutex<IoQueue>> = LazyLock::new(|| Mutex::new(IoQueue::default()));
static IO_READY: Condvar = Condvar::new();

fn io() -> MutexGuard<'static, IoQueue> {
    IO.lock().unwrap_or_else(|p| p.into_inner())
}

fn io_thread() {
    loop {
        let (slot, call) = {
            let mut io = io();
            loop {
                if let Some(c) = io.calls.pop_front() {
                    break c;
                }
                io.idle += 1;
                io = IO_READY.wait(io).unwrap_or_else(|p| p.into_inner());
                io.idle -= 1;
            }
        };
        let waiting =
            |s: &Mutex<Option<Box<Green>>>| s.lock().unwrap_or_else(|p| p.into_inner()).take();
        // A thread cancelled before its call was made: not made at all; and
        // one cancelled while it was being made does not get what it answered.
        if slot.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
            call();
        }
        let g = waiting(&slot);
        let mut w = world();
        w.calls -= 1;
        if let Some(g) = g {
            wake(&mut w, g, Wake::Nothing);
        }
    }
}

// --- sleeping ------------------------------------------------------------------
//
// A `Time.sleep` on the scheduler parks its thread on a timer instead of
// sleeping on the worker, which runs other threads meanwhile: ten thousand
// sleeping threads hold up no worker, and a cancel wakes one at once. A
// sleeping thread is counted with the calls into the world (`World::calls`),
// as a thread that will wake. One timer thread for the process, started by
// the first sleep.

/// The sleeping threads, soonest first.
#[derive(Default)]
struct Timers {
    due: std::collections::BinaryHeap<Due>,
    /// Breaks ties between threads that wake at the same moment: in the
    /// order they went to sleep.
    next: u64,
}

/// A thread asleep until `until` -- unless a cancel takes it out of its slot
/// first, and the timer finds the slot empty.
struct Due {
    until: std::time::Instant,
    seq: u64,
    slot: Arc<Mutex<Option<Box<Green>>>>,
}

impl PartialEq for Due {
    fn eq(&self, other: &Self) -> bool {
        (self.until, self.seq) == (other.until, other.seq)
    }
}

impl Eq for Due {}

impl PartialOrd for Due {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Due {
    /// Reversed, so that the heap's greatest is the soonest.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (other.until, other.seq).cmp(&(self.until, self.seq))
    }
}

static TIMERS: LazyLock<Mutex<Timers>> = LazyLock::new(|| Mutex::new(Timers::default()));
static TIMER_READY: Condvar = Condvar::new();
/// Whether the timer thread has been started.
static TIMING: AtomicBool = AtomicBool::new(false);

fn timers() -> MutexGuard<'static, Timers> {
    TIMERS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Wake each sleeping thread when its time comes.
fn timer_thread() {
    let mut t = timers();
    loop {
        let now = std::time::Instant::now();
        let until = match t.due.peek() {
            None => None,
            Some(d) if d.until > now => Some(d.until - now),
            Some(_) => {
                let due = t.due.pop().expect("just peeked");
                drop(t);
                // A thread cancelled in its sleep has been taken out already,
                // and counted off by whoever took it.
                let g = due.slot.lock().unwrap_or_else(|p| p.into_inner()).take();
                if let Some(g) = g {
                    let mut w = world();
                    w.calls -= 1;
                    wake(&mut w, g, Wake::Nothing);
                }
                t = timers();
                continue;
            }
        };
        t = match until {
            None => TIMER_READY.wait(t).unwrap_or_else(|p| p.into_inner()),
            Some(d) => {
                TIMER_READY
                    .wait_timeout(t, d)
                    .unwrap_or_else(|p| p.into_inner())
                    .0
            }
        };
    }
}

/// `Time.sleep` for `ms` milliseconds: on the scheduler's timer, where this
/// is one of its threads, or here and now.
pub fn sleep(ms: u64) {
    let d = std::time::Duration::from_millis(ms);
    if !STARTED.load(Ordering::SeqCst) || !ctx::present() || segments::innermost().is_none() {
        std::thread::sleep(d);
        return;
    }
    // Woken when the time comes, or to fail: cancelled while it slept.
    let _ = delivered(park(Request::Sleep(std::time::Instant::now() + d)));
}

/// `work`'s answer: made by an I/O thread while this thread waits, where it
/// is one of the scheduler's, or here and now.
pub fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    if !STARTED.load(Ordering::SeqCst) || !ctx::present() || segments::innermost().is_none() {
        return work();
    }
    // Made on an I/O thread, for this thread: what it opens is this one's.
    let owner = owner();
    let work = move || meadow_rt::net::owning(owner, work);
    let slot: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
    let into = slot.clone();
    let woken = park(Request::Block(Box::new(move || {
        let answer = work();
        *into.lock().unwrap_or_else(|p| p.into_inner()) = Some(answer);
    })));
    let _ = delivered(woken);
    let answer = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
    answer.unwrap_or_else(|| crate::fail("a call into the world came back without its answer"))
}

/// Whose the sockets the running thread opens are: see `meadow_rt::net`.
pub(crate) fn owner() -> u64 {
    if ctx::present() { cx().owner } else { 0 }
}

/// Put a thread back on the queue, with what it was woken with: the
/// scheduler's lock taken here and nowhere above it.
fn ready(g: Box<Green>, with: Wake) {
    let mut w = world();
    wake(&mut w, g, with);
}

/// Suspend the running thread -- every segment of it -- having said why in
/// its context; answer what it is woken with.
fn park(request: Request) -> Wake {
    cx().request = Some(request);
    let Some(y) = segments::innermost() else {
        crate::fail("a thread operation ran outside every thread")
    };
    match segments::suspend(y, Up::Park) {
        Down::Wake => std::mem::replace(&mut cx().wake, Wake::Nothing),
        Down::Start | Down::Resume(_) => unreachable!("a parked thread is woken"),
    }
}

/// A failure in a thread other than `main` fails that thread: `await` fails
/// with the same message, and nothing else is affected; so does one in a call
/// C made into a library, which ends that call ([`export_run`]). Answers
/// whether it was one; for `main`, or outside every thread, the program stops.
pub fn fail_thread(msg: &str) -> bool {
    if !ctx::present() || (cx().tid == 0 && !cx().guarded) || segments::innermost().is_none() {
        return false;
    }
    // Its releases first, here on top of what failed, which nothing will go
    // back to: the thread fails with the first failure.
    let msg = match wind_down(Err(msg.to_string())) {
        Err(m) => m,
        Ok(_) => unreachable!("a failure is a failure"),
    };
    loop {
        park(Request::Failed(msg.clone()));
    }
}

/// The running thread's code is over, ending with `out` -- or a release it
/// runs on the way out has failed: run the releases left on its stack,
/// newest first, under no handlers, and answer how the thread ends. That is
/// how its own code ended, unless a release failed where it had succeeded:
/// the first failure, then. A release that fails comes back here, through
/// [`fail_thread`] or `crate::fail`, and the rest run from there. See
/// `docs/CANCELLATION.md`.
pub(crate) fn wind_down(out: Result<Parcel, String>) -> Result<Parcel, String> {
    let c = cx();
    match (&c.ending, out) {
        (None, out) => {
            // On its way out: a cancel is held off for good, and a
            // transaction the thread was in is over.
            c.cancel.held_off.fetch_add(1, Ordering::SeqCst);
            c.txn = None;
            c.ending = Some(out);
        }
        (Some(Ok(_)), Err(e)) => c.ending = Some(Err(e)),
        _ => {}
    }
    while let Some((_, f)) = cx().exits.pop() {
        cx().cancel.release_begins();
        // Safety: a function value of the emitted module, owned by the call.
        let _ = unsafe { meadow_apply(f, 0, no_evidence()) };
    }
    cx().ending.take().expect("the thread has ended")
}

/// `main` has returned, or the program's last thread failed: its releases,
/// before the program ends. What the program then fails with, if a release
/// failed or it already had. Nothing where the failure is in the runtime's
/// own call into the program, which has nowhere to run more.
pub(crate) fn last_rites(failed: Option<&str>) -> Option<String> {
    if !ctx::present() || cx().tid != 0 || (ctx::threaded() && segments::innermost().is_none()) {
        return failed.map(str::to_string);
    }
    let out = match failed {
        Some(m) => Err(m.to_string()),
        None => Parcel::of(0, desc::UNIT),
    };
    wind_down(out).err()
}

fn handle(v: Val, kind: u64, what: &str, op: &str) -> usize {
    match v {
        Val::Ref(w) if heap::is_block(w) && heap::kind(w) == kind => heap::meta(w) as usize,
        other => {
            let (w, d) = other.bits();
            crate::fail(&format!(
                "{op}: expected {what}, got {}",
                crate::show::show(w, d)
            ))
        }
    }
}

fn outside(op: &str) -> ! {
    crate::fail(&meadow_rt::stm::outside(op))
}

fn parcel(v: Val) -> Parcel {
    let (w, d) = v.bits();
    Parcel::of(w, d).unwrap_or_else(|e| crate::fail(&e))
}

/// What a waiting operation was woken with, as a value in this heap.
fn delivered(wake: Wake) -> Word {
    match wake {
        Wake::Value(p) => p.open(),
        Wake::Fail(m) => crate::fail(&m),
        Wake::Nothing => 0,
    }
}

/// The thread and transaction primitives. Arguments are borrowed, the
/// result owned.
pub fn prim(p: Prim, args: &[Val]) -> Word {
    use Prim::*;
    let arg = |i: usize| args[i];
    if matches!(
        p,
        ThreadSpawn
            | ThreadAwait
            | ThreadYield
            | ChannelNew
            | ChannelSend
            | ChannelReceive
            | ThreadCancel
            | StmWait
    ) {
        notice_cancel();
    }
    match p {
        ThreadCancel => {
            let t = handle(arg(0), heap::TASK, "a thread", "cancel");
            // Finished, the thread has no `Ctl` any more: nothing to do; and
            // asked already, once is enough. Under the scheduler's lock, as
            // the thread's end is, so that it is counted as ending exactly
            // when it is. The lock is let go before anything can fail.
            let found = world().tasks.get(t).map(|task| {
                task.ctl.clone().filter(|c| {
                    let first = !c.cancelled.swap(true, Ordering::SeqCst);
                    if first {
                        CANCELLED.fetch_add(1, Ordering::SeqCst);
                        meadow_preempt.fetch_or(ENDING, Ordering::SeqCst);
                    }
                    first
                })
            });
            let Some(ctl) = found else {
                crate::fail(&format!("cancel: there is no thread {t}"))
            };
            // Held off -- running a release -- it is not woken: it stops at
            // its first operation once it is not.
            if let Some(ctl) = ctl.filter(|c| !c.held_off()) {
                let at = std::mem::take(&mut *ctl.parked.lock().unwrap_or_else(|p| p.into_inner()));
                unpark(t, at);
            }
            0
        }
        // The release stack is the thread's own: no scheduler is asked. See
        // `docs/CANCELLATION.md`.
        ThreadDefer => {
            let f = match arg(0) {
                Val::Ref(f) => f,
                other => {
                    let (w, d) = other.bits();
                    crate::fail(&format!(
                        "threadDefer: expected a function, got {}",
                        crate::show::show(w, d)
                    ))
                }
            };
            // Kept: the argument is only borrowed.
            heap::share(f, desc::REF);
            let c = cx();
            let key = c.next_exit;
            c.next_exit += 1;
            c.exits.push((key, f));
            key as Word
        }
        ThreadTake => {
            let key = match arg(0) {
                Val::Int(k) => k,
                other => {
                    let (w, d) = other.bits();
                    crate::fail(&format!(
                        "threadTake: expected an Int, got {}",
                        crate::show::show(w, d)
                    ))
                }
            };
            let c = cx();
            if matches!(arg(1), Val::Bool(true)) {
                let _ = c
                    .cancel
                    .held_off
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
            }
            match c.exits.last() {
                Some((k, _)) if *k >= key => {
                    let (_, f) = c.exits.pop().expect("just looked");
                    c.cancel.held_off.fetch_add(1, Ordering::SeqCst);
                    // Its own grace; a thread that has taken the cancel on
                    // itself keeps it so.
                    c.cancel.armed.store(0, Ordering::Relaxed);
                    // What the stack held is handed on.
                    value::data(
                        crate::show::role(meadow_rt::roles::Role::Some),
                        &[Val::Ref(f)],
                    )
                }
                _ => value::data(crate::show::role(meadow_rt::roles::Role::None), &[]),
            }
        }
        ThreadCancelled => {
            let c = &cx().cancel;
            let cancelled = c.cancelled();
            if cancelled {
                c.seen.store(true, Ordering::Relaxed);
            }
            u64::from(cancelled)
        }
        ThreadSpawn => {
            let body = parcel(arg(0));
            let answer = match args.get(1) {
                Some(Val::Int(d)) => *d,
                _ => desc::ANY,
            };
            let tid = {
                let mut w = world();
                w.tasks.push(Task::default());
                w.tasks.len() - 1
            };
            let co = coroutine(THREAD, move || {
                let f = body.open();
                // Safety: a function value of the emitted module, owned by
                // the call.
                let v = unsafe { meadow_apply(f, 0, no_evidence()) };
                let out = Parcel::of(v, answer);
                heap::erase(v, answer);
                // A release that fails does not come back.
                let out = wind_down(out);
                // What it answers was handed on as it was lifted; the sockets
                // it still holds go with it.
                meadow_rt::net::close_owned(cx().owner);
                heap::finish();
                Box::into_raw(Box::new(out)) as Word
            });
            let mut w = world();
            let g = Box::new(Green {
                ctx: Ctx::new(tid),
                co,
            });
            w.tasks[tid].ctl = Some(g.ctx.cancel.clone());
            wake(&mut w, g, Wake::Nothing);
            held(tid as u32);
            heap::build(heap::TASK, tid as u32, &[0], &[desc::INT])
        }
        ThreadAwait => {
            let t = handle(arg(0), heap::TASK, "a thread", "await");
            let ready = {
                let w = world();
                match w.tasks.get(t).map(|t| &t.outcome) {
                    None => Some(Wake::Fail(format!("await: there is no thread {t}"))),
                    Some(Some(Ok(p))) => Some(Wake::Value((**p).clone())),
                    Some(Some(Err(m))) => Some(Wake::Fail(m.clone())),
                    Some(None) => None,
                }
            };
            delivered(ready.unwrap_or_else(|| park(Request::Await(t))))
        }
        ThreadYield => {
            park(Request::Yield);
            0
        }
        ChannelNew => {
            let c: &'static Chan = Box::leak(Box::new(Chan::default()));
            let id = NEXT_CHAN.fetch_add(1, Ordering::Relaxed) as u32;
            heap::build(heap::CHANNEL, id, &[c as *const Chan as Word], &[desc::INT])
        }
        ChannelSend => {
            let c = chan(arg(0), "send");
            let message = parcel(arg(1));
            // The channel's lock, and the scheduler's only if somebody was
            // waiting -- and never both at once.
            let waiting = {
                let mut state = chan_state(c);
                match state.receivers.pop_front() {
                    Some(g) => Some(g),
                    None => {
                        state.messages.push_back(message);
                        return 0;
                    }
                }
            };
            if let Some(g) = waiting {
                ready(g, Wake::Value(message));
            }
            0
        }
        ChannelReceive => {
            let c = chan(arg(0), "receive");
            let got = chan_state(c).messages.pop_front();
            match got {
                Some(p) => p.open(),
                None => delivered(park(Request::Receive(c as *const Chan as usize))),
            }
        }

        StmNew => {
            // The cell lives for the whole run; the handle carries its
            // address, so no thread has to look it up.
            let held = Held::of(arg(0), None);
            let cell: &'static TVar = Box::leak(Box::new(TVar {
                state: Mutex::new((held, 0)),
                waiters: Mutex::new(Vec::new()),
            }));
            let id = NEXT_TVAR.fetch_add(1, Ordering::Relaxed) as u32;
            held_var(at(cell));
            heap::build(heap::TVAR, id, &[at(cell) as Word], &[desc::INT])
        }
        StmRead => {
            let c = cell(arg(0), "readTVar");
            let Some(txn) = cx().txn.as_mut() else {
                outside("readTVar")
            };
            let written = txn
                .writes
                .iter()
                .rev()
                .flat_map(|level| level.iter().rev())
                .find(|(t, _, _)| *t == at(c))
                .map(|(_, x, d)| (*x, *d));
            if let Some((x, d)) = written {
                heap::share(x, d);
                return value::data(
                    crate::show::role(meadow_rt::roles::Role::Some),
                    &[value::val(x, d)],
                );
            }
            let start = txn.start;
            let seen = {
                let state = c.state.lock().unwrap_or_else(|p| p.into_inner());
                (state.1 <= start).then(|| (state.0.clone(), state.1))
            };
            // Written since this transaction began: it runs again.
            let Some((held, version)) = seen else {
                return value::data(crate::show::role(meadow_rt::roles::Role::None), &[]);
            };
            let txn = cx().txn.as_mut().expect("a transaction");
            if !txn.reads.iter().any(|(t, _)| *t == at(c)) {
                txn.reads.push((at(c), version));
            }
            let (x, d) = held.open();
            value::data(
                crate::show::role(meadow_rt::roles::Role::Some),
                &[value::val(x, d)],
            )
        }
        StmWrite => {
            let c = cell(arg(0), "writeTVar");
            let (x, d) = arg(1).bits();
            let Some(level) = cx().txn.as_mut().and_then(|t| t.writes.last_mut()) else {
                outside("writeTVar")
            };
            heap::share(x, d);
            let old = match level.iter_mut().find(|(t, _, _)| *t == at(c)) {
                Some(slot) => Some(std::mem::replace(slot, (at(c), x, d))),
                None => {
                    level.push((at(c), x, d));
                    None
                }
            };
            if let Some((_, ox, od)) = old {
                heap::erase(ox, od);
            }
            0
        }
        StmBegin => {
            let start = CLOCK.load(Ordering::Acquire);
            let last = cx().txn.replace(Txn {
                start,
                reads: Vec::new(),
                writes: vec![Vec::new()],
            });
            if let Some(t) = last {
                drop_writes(t.writes);
            }
            0
        }
        StmNest => {
            let Some(t) = cx().txn.as_mut() else {
                outside("orElse")
            };
            t.writes.push(Vec::new());
            0
        }
        StmMerge => {
            let Some(t) = cx().txn.as_mut() else {
                outside("orElse")
            };
            let (Some(top), true) = (t.writes.pop(), !t.writes.is_empty()) else {
                outside("orElse")
            };
            let below = t.writes.last_mut().expect("a level below");
            let mut replaced = Vec::new();
            for (id, x, d) in top {
                match below.iter_mut().find(|(t, _, _)| *t == id) {
                    Some(slot) => replaced.push(std::mem::replace(slot, (id, x, d))),
                    None => below.push((id, x, d)),
                }
            }
            drop_writes(vec![replaced]);
            0
        }
        StmRollback => {
            let Some(top) = cx().txn.as_mut().and_then(|t| t.writes.pop()) else {
                outside("orElse")
            };
            drop_writes(vec![top]);
            0
        }
        StmCommit => {
            let Some(txn) = cx().txn.take() else {
                outside("atomically")
            };
            // The last write to each, lifted out of this thread's heap before
            // anything is locked -- into the region of the value it replaces,
            // looked at under the cell's lock and let go at once. Another
            // commit may replace that value meanwhile; then this one conflicts,
            // or its value went into a region that is no longer the latest,
            // which costs sharing and nothing else.
            let mut writes: Vec<(usize, Word, i64)> = Vec::new();
            for level in &txn.writes {
                for &(id, x, d) in level {
                    match writes.iter_mut().find(|(t, _, _)| *t == id) {
                        Some(slot) => *slot = (id, x, d),
                        None => writes.push((id, x, d)),
                    }
                }
            }
            let last: Vec<(usize, Held)> = writes
                .into_iter()
                .map(|(id, x, d)| {
                    // Safety: the address of a cell that lives for the whole run.
                    let c = unsafe { &*(id as *const TVar) };
                    let was = c.state.lock().unwrap_or_else(|p| p.into_inner()).0.clone();
                    (id, Held::of(value::val(x, d), Some(&was)))
                })
                .collect();
            // What it read must be as it was, and what it wrote goes out, as
            // one step: the cells it touched are locked for it, in address
            // order, and nothing else can be committing them meanwhile.
            let touched: Vec<&TVar> = txn
                .reads
                .iter()
                .map(|(id, _)| *id)
                .chain(last.iter().map(|(id, _)| *id))
                // Safety: addresses of cells that live for the whole run.
                .map(|id| unsafe { &*(id as *const TVar) })
                .collect();
            let (ats, mut held) = lock_all(&touched);
            let slot = |id: usize| ats.binary_search(&id).expect("locked");
            let valid = txn
                .reads
                .iter()
                .all(|(id, seen)| held[slot(*id)].1 == *seen);
            let mut keys = Vec::new();
            if valid && !last.is_empty() {
                let now = CLOCK.fetch_add(1, Ordering::AcqRel) + 1;
                for (id, value) in last {
                    let state = &mut held[slot(id)];
                    **state = (value, now);
                }
            }
            drop(held);
            if valid {
                for c in &touched {
                    let mut waiters = c.waiters.lock().unwrap_or_else(|p| p.into_inner());
                    keys.append(&mut waiters);
                }
            }
            wake_waiting(keys);
            drop_writes(txn.writes);
            u64::from(valid)
        }
        StmWait => {
            let Some(txn) = cx().txn.take() else {
                outside("retry")
            };
            drop_writes(txn.writes);
            if txn.reads.is_empty() {
                crate::fail("deadlock: a transaction retried having read nothing")
            }
            // Woken by a write, or to fail: cancelled while it waited.
            let _ = delivered(park(Request::Wait(txn.reads)));
            0
        }
        _ => unreachable!("{p:?} is not a thread primitive"),
    }
}

/// Wake the threads whose `retry` was waiting on a `TVar` just written.
fn wake_waiting(keys: Vec<u64>) {
    if keys.is_empty() {
        return;
    }
    let mut w = world();
    for k in keys {
        if let Some(g) = w.waiting.remove(&k) {
            wake(&mut w, g, Wake::Nothing);
        }
    }
}

fn drop_writes(levels: Vec<Vec<(usize, Word, i64)>>) {
    for (_, x, d) in levels.into_iter().flatten() {
        heap::erase(x, d);
    }
}

// --- calls C makes into a library ------------------------------------------

thread_local! {
    /// Stacks of calls into the library that have finished, to run the next
    /// on.
    static EXPORT_STACKS: std::cell::RefCell<Vec<DefaultStack>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// What the last call into the library that failed on this OS thread
    /// failed with, until `Std.Ffi.failure` asks.
    static FAILURE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Run `body` of `env` -- a call C made into a library, its arguments in
/// `env` -- on a segment of its own, so that a failure in it ends the call
/// rather than the process: as a thread's does, it parks the segment, which
/// is let go without unwinding. Answers 0 when it returned, and 1 when it
/// failed, its message said on stderr and kept for [`take_failure`]. What the
/// call's frames held is given up; the context is as it was before the call.
pub fn export_run(body: extern "C" fn(*mut std::ffi::c_void), env: *mut std::ffi::c_void) -> i32 {
    let c = cx();
    let depth = c.running.len();
    let levels = c.levels.len();
    let shadow = crate::shadow::head();
    let in_txn = c.txn.is_some();
    let guarded = std::mem::replace(&mut c.guarded, true);
    let stack = EXPORT_STACKS
        .with(|s| s.borrow_mut().pop())
        .unwrap_or_else(|| {
            DefaultStack::new(THREAD).unwrap_or_else(|e| {
                crate::fail(&format!(
                    "could not make a stack for a call into the library: {e}"
                ))
            })
        });
    let env = env as usize;
    let base = corosensei::stack::Stack::base(&stack).get();
    let mut co: segments::Segment =
        Coroutine::with_stack(stack, move |y: &Yielder<Down, Up>, _: Down| {
            segments::enter_thread(y, base);
            body(env as *mut std::ffi::c_void);
            // What the call registered and did not release, before it
            // returns: nothing of one call is left for the next.
            let _ = wind_down(Parcel::of(0, desc::UNIT));
            segments::leave_thread();
            0
        });
    let failed = match segments::resume(&mut co, Down::Start) {
        CoroutineResult::Return(_) => None,
        CoroutineResult::Yield(up) => {
            let why = match (up, cx().request.take()) {
                (Up::Park, Some(Request::Failed(m))) => m,
                (Up::Park, _) => "a thread waited in a call C made into a library, where there                                   is no scheduler to run another"
                    .to_string(),
                (Up::Detach { .. }, _) => "an effect was performed after its handler had                                            finished, in a call C made into a library"
                    .to_string(),
            };
            // Never resumed, and not unwound.
            // Safety: nothing will resume it.
            unsafe { co.force_reset() };
            Some(why)
        }
    };
    EXPORT_STACKS.with(|s| {
        let mut s = s.borrow_mut();
        if s.len() < 2 {
            s.push(co.into_stack());
        }
    });
    let c = cx();
    c.running.truncate(depth);
    c.levels.truncate(levels);
    c.guarded = guarded;
    // The context lives on for the next call, which starts afresh.
    c.ending = None;
    c.cancel.held_off.store(0, Ordering::SeqCst);
    if !in_txn {
        c.txn = None;
    }
    crate::shadow::set(shadow);
    match failed {
        None => 0,
        Some(why) => {
            use std::io::Write;
            let _ = std::io::stdout().flush();
            eprintln!("{why}");
            FAILURE.with(|f| *f.borrow_mut() = Some(why));
            1
        }
    }
}

/// What the last call into the library that failed on this OS thread failed
/// with, once: see [`export_run`].
pub fn take_failure() -> Option<String> {
    FAILURE.with(|f| f.borrow_mut().take())
}
