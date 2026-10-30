//! The scheduler: green threads, each with a heap of its own, run M:N on a pool
//! of OS threads.
//!
//! # What a green thread is
//!
//! A [`Vm`]: a register file, a program counter and a heap.
//! Its call stack is the frame stack, which is chunks of that heap and not the
//! OS thread's stack, so that is all of it -- a thread that is not
//! running is a value that can be put in a queue, and any OS thread can pick
//! it up and carry on. The OS threads are workers and nothing else; a program
//! never sees one.
//!
//! # The rule: a heap belongs to one thread
//!
//! Only the OS thread currently running a green thread touches that thread's
//! heap. Values reach another thread in three ways -- the function it is
//! spawned with, a message on a channel, the result `await` returns -- and each
//! is exported to a [`Parcel`] by the thread that has the value, and imported by
//! the thread that receives it, when it next runs. A parcel waiting in a channel
//! belongs to no heap at all. So there is no shared mutable memory between
//! threads, no locking in the machine, and each heap is collected on its own
//! with nothing else stopping.
//!
//! # Work stealing
//!
//! Every worker has a run queue of its own. A thread it spawns, a thread it
//! wakes -- by sending to a channel someone waits on, or by finishing a thread
//! someone awaits -- and a thread of its own that used up its slice all go on
//! *its* queue, so a thread keeps to the core it is on and its heap stays in
//! that core's cache. A worker with nothing to do takes half of another
//! worker's queue: work spreads to idle cores, and only to idle cores.
//!
//! A thread woken by a message or a result goes one better: into the worker's
//! *next* slot, which is not stealable and runs as soon as the current thread
//! stops. A send and the receive it answers then happen on one core, back to
//! back, the way a pipeline wants -- where waking a sleeping core to carry every
//! message across would cost more than the message. A worker is woken to steal
//! only when a queue holds more than its owner will run next.
//!
//! One shared queue remains for fairness: a worker looks at it first every
//! [`GLOBAL_EVERY`] turns. The locks are per queue, per channel and per thread
//! handle, so workers that are not touching the same things do not wait on each
//! other.
//!
//! # How a thread operation happens
//!
//! A primitive cannot carry one out: the channels and the other threads are
//! here. It leaves a [`Request`] in the machine and returns, and the worker
//! running the machine does what was asked, then either carries on with the
//! same thread -- a send, a spawn, a new channel -- or parks it with whatever
//! it is waiting for: a receive on an empty channel, an `await` on a thread
//! still running. Whoever wakes it attaches the answer as a [`Wake`], and it is
//! delivered into the thread's registers when it runs again.
//!
//! # Cancellation
//!
//! A thread asked to stop (see `meadow_core::thread`) fails at its next
//! operation the runtime answers: a thread operation, which is a request, so
//! [`Worker::settle`] looks at the thread's [`Ctl`] before it does what was
//! asked; or one of the world's, where the machine looks itself
//! (`Vm::cancel`). One that makes neither is ended when it is preempted the
//! second time after the cancel -- a slice's grace -- unless it has asked and
//! been told. A thread already waiting has to be found and woken. It said
//! where it waits in its `Ctl` when it parked, then looked at the flag, under
//! the lock of what it waits on; the canceller sets the flag and then reads
//! where. So either the canceller finds it there -- taking the same lock to
//! take it out -- or the thread saw the flag and did not wait at all. The
//! place is only a hint: a thread woken some other way leaves it behind, and
//! a canceller that looks there finds nobody, which is right, since that
//! thread is running and will see the flag itself.
//!
//! # Fairness, deadlock and the end
//!
//! A thread runs for [`SLICE`] instructions and then goes to the back of its
//! worker's queue, so a loop that never waits cannot starve the others. The
//! program ends when its first thread -- `main` -- does, as in Go; threads still
//! running then are dropped.
//!
//! Only a running thread can wake a waiting one. So the scheduler counts the
//! threads that are running or queued, and if that count reaches zero with
//! `main` unfinished, every thread is waiting on another and none ever will
//! stop: a deadlock, reported as an error.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::Duration;

use meadow_bytecode::{Pc, Program, Reg};

use crate::heap::{Heap, Kind, Parcel, THREAD_INITIAL};
use crate::vm::{Error, Request, Vm};

/// Instructions a thread runs before another gets a turn.
pub const SLICE: u64 = 2048;

/// A worker looks at the shared queue before its own every this many turns, so
/// what is there is never starved by a busy local queue.
pub const GLOBAL_EVERY: u64 = 61;

/// How a run ended, and what the collectors did along the way.
pub struct Outcome {
    /// The main thread's result, rendered, or why it failed.
    pub result: Result<String, Error>,
    /// The main thread's answer itself, where it is not an object: what a C
    /// function a library exports answers (see [`run_export`]).
    pub answer: Option<crate::value::Value>,
    pub stats: Stats,
    /// Every thread's samples, merged. `None` unless the run was sampled.
    pub profile: Option<crate::profile::Profile>,
}

/// The collectors' work, summed over every thread that finished.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub threads: u64,
    pub collections: u64,
    pub allocated: u64,
    pub copied: u64,
    pub gc_nanos: u64,
    /// The main thread's heap and regions when it finished.
    pub heap_slots: usize,
    pub region_slots: usize,
    /// Threads taken from another worker's queue.
    pub stolen: u64,
    pub pauses: crate::pauses::Pauses,
    /// What each instruction allocated, summed over every thread. Only under
    /// `--features profile-alloc` -- see [`crate::profile::Sites`].
    #[cfg(feature = "profile-alloc")]
    pub sites: crate::profile::Sites,
    /// How many of each instruction the run retired.
    #[cfg(feature = "profile-alloc")]
    pub ops: crate::profile::Ops,
    /// Where each `invoke` went and which way each tag test did.
    #[cfg(feature = "profile-alloc")]
    pub feedback: crate::profile::Feedback,
    /// Slots promoted to old generations, marking cycles, and time spent
    /// marking on any thread.
    pub promoted: u64,
    pub cycles: u64,
    pub mark_nanos: u64,
    /// Slots and blocks evacuated out of sparse old blocks.
    pub evacuated: u64,
    pub evacuated_blocks: u64,
    /// The main thread's old generation when it finished: slots in its blocks,
    /// and slots its last marking cycle found alive. The difference is what
    /// fragmentation and the headroom before the next cycle cost.
    pub old_slots: usize,
    pub old_live: usize,
}

impl Stats {
    fn add(&mut self, heap: &Heap) {
        self.threads += 1;
        self.collections += heap.collections;
        self.allocated += heap.allocated;
        self.copied += heap.copied;
        self.gc_nanos += heap.gc_nanos;
        self.pauses.merge(&heap.pauses);
        self.promoted += heap.promoted;
        self.cycles += heap.cycles;
        self.mark_nanos += heap.mark_nanos;
        self.evacuated += heap.evacuated;
        self.evacuated_blocks += heap.evacuated_blocks;
        #[cfg(feature = "profile-alloc")]
        self.sites.merge(&heap.sites);
    }
}

/// How many OS threads run green threads: `MEADOW_THREADS` if it is set, or one
/// per core.
pub fn workers() -> usize {
    std::env::var("MEADOW_THREADS")
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// Run `program` from `entry` as its main thread, with [`workers`] OS threads,
/// until it finishes or `fuel` instructions have run across every thread.
pub fn run(program: &Program, entry: Pc, fuel: u64) -> Outcome {
    run_with(program, entry, fuel, workers())
}

/// [`run`], with the number of OS threads given.
pub fn run_with(program: &Program, entry: Pc, fuel: u64, workers: usize) -> Outcome {
    run_native(program, None, entry, fuel, workers)
}

/// How often to sample a run, and how deep -- see [`crate::profile`].
///
/// Every green thread gets its own [`crate::profile::Profile`], because every
/// green thread is its own machine with its own stack of continuations, and a
/// run's profile is all of them merged.
#[derive(Debug, Clone, Copy)]
pub struct Sampling {
    /// Block entries between samples for a profile of work, or `None` for a
    /// profile of time -- see [`crate::profile::When`] by way of
    /// [`crate::profile::Ticker`].
    pub every: Option<u64>,
    /// Samples a second, when `every` is `None`.
    pub hz: u64,
    pub depth: usize,
}

/// [`run_with`], with native code for some or all of the program's blocks --
/// every thread runs it where there is some, and the bytecode where not.
pub fn run_native(
    program: &Program,
    native: Option<&crate::jit::Native>,
    entry: Pc,
    fuel: u64,
    workers: usize,
) -> Outcome {
    run_sampled(program, native, entry, fuel, workers, None)
}

/// Where a run's output goes instead of the process's: what `print` writes on
/// any of its threads, a piece at a time. Shared, because every green thread
/// of the run writes to the one place.
pub type Sink = Arc<dyn Fn(&str) + Send + Sync>;

/// [`run_native`], with what the program prints handed to `sink` rather than
/// written out -- which is what lets many runs share a process without
/// sharing a terminal: a test runner running tests side by side.
pub fn run_captured(
    program: &Program,
    native: Option<&crate::jit::Native>,
    entry: Pc,
    fuel: u64,
    workers: usize,
    sink: Sink,
) -> Outcome {
    run_all(
        program,
        native,
        Wake::Entry(entry),
        fuel,
        workers,
        None,
        Some(sink),
    )
}

/// [`run_native`], sampled.
pub fn run_sampled(
    program: &Program,
    native: Option<&crate::jit::Native>,
    entry: Pc,
    fuel: u64,
    workers: usize,
    sampling: Option<Sampling>,
) -> Outcome {
    run_all(
        program,
        native,
        Wake::Entry(entry),
        fuel,
        workers,
        sampling,
        None,
    )
}

/// Run the definition at `entry` with `args`, as a call C made into a library
/// does: a run of its own, whose main thread is the call, which answers what
/// the definition answers -- a number, a `Bool`, a `Char` or `()` -- or why
/// it failed.
pub fn run_export(
    program: &Program,
    native: Option<&crate::jit::Native>,
    entry: Pc,
    args: Vec<crate::value::Value>,
) -> Result<crate::value::Value, Error> {
    let outcome = run_all(
        program,
        native,
        Wake::Call(entry, args),
        u64::MAX,
        workers(),
        None,
        None,
    );
    outcome.result?;
    outcome.answer.ok_or_else(|| Error {
        msg: "the call answered an object, which C cannot take".into(),
    })
}

/// A machine's output, sent to `sink`.
fn capture(vm: &mut Vm, sink: &Option<Sink>) {
    if let Some(sink) = sink {
        let sink = sink.clone();
        vm.io.output = Some(Box::new(move |s| sink(s)));
    }
}

fn run_all(
    program: &Program,
    native: Option<&crate::jit::Native>,
    start: Wake,
    fuel: u64,
    workers: usize,
    sampling: Option<Sampling>,
    sink: Option<Sink>,
) -> Outcome {
    let workers = workers.max(1);
    // One clock for the run. Every machine samples on it, so a thread that is
    // running contributes a sample per tick and a thread that is waiting
    // contributes none -- which is what makes the counts come out proportional
    // to processor time.
    let ticker = sampling
        .filter(|s| s.every.is_none())
        .map(|s| crate::profile::Ticker::at(s.hz));
    let world = Arc::new(crate::stm::World::default());
    // A call into a library starts small, as a spawned thread does: most
    // make little, and every call makes its heap anew.
    let vm = match start {
        Wake::Call(..) => Vm::with_heap(program, Heap::with_capacity(THREAD_INITIAL)),
        _ => Vm::new(program),
    };
    let mut main = Fiber::new(vm, 0, start);
    let main_ctl = main.ctl.clone();
    main.vm.scheduled = true;
    main.vm.world = Some(world.clone());
    main.vm.use_native(native);
    capture(&mut main.vm, &sink);
    if let Some(s) = sampling {
        main.vm.profile = Some(Box::new(match (&ticker, s.every) {
            (Some(t), _) => t.profile(s.depth),
            (None, every) => crate::profile::Profile::new(every.unwrap_or(1), s.depth),
        }));
    }
    let shared = Shared {
        program,
        native,
        fuel,
        steps: AtomicU64::new(0),
        locals: (0..workers).map(|_| Mutex::new(VecDeque::new())).collect(),
        global: Mutex::new(VecDeque::new()),
        channels: RwLock::new(Vec::new()),
        tasks: RwLock::new(vec![Arc::new(Mutex::new(Task {
            ctl: main_ctl,
            ..Task::default()
        }))]),
        world,
        stm_waiters: Mutex::new(std::collections::HashMap::new()),
        active: AtomicUsize::new(1),
        sleeping: AtomicUsize::new(0),
        sleep: Mutex::new(()),
        wake: Condvar::new(),
        done: AtomicBool::new(false),
        finished: Mutex::new(None),
        answer: Mutex::new(None),
        started: AtomicBool::new(false),
        stats: Mutex::new(Stats::default()),
        sampling,
        ticker,
        profile: Mutex::new(None),
        sink,
        waits: Arc::new(meadow_rt::net::Waits::default()),
        io: Mutex::new(IoQueue::default()),
        io_ready: Condvar::new(),
        io_pending: AtomicUsize::new(0),
    };
    lock(&shared.locals[0]).push_back(main);
    // The calling thread is worker 0. The others start with the first `spawn`,
    // so a program that never spawns never makes an OS thread.
    std::thread::scope(|scope| Worker::new(0, &shared).run(scope));
    let result = lock(&shared.finished).take().unwrap_or_else(|| {
        Err(Error {
            msg: "the scheduler stopped".into(),
        })
    });
    let stats = lock(&shared.stats).clone();
    let profile = lock(&shared.profile).take();
    let answer = lock(&shared.answer).take();
    Outcome {
        result,
        answer,
        stats,
        profile,
    }
}

/// A green thread, and what it gets when it next runs.
struct Fiber<'p> {
    vm: Vm<'p>,
    /// Its number, which is where `await` finds its result. `0` is `main`.
    task: u32,
    wake: Wake,
    /// Whether it has been asked to stop, and where it waits: shared with
    /// its [`Task`], where a canceller finds it.
    ctl: Arc<Ctl<'p>>,
    /// It asked whether it had been cancelled, and was told so: it stops of
    /// its own accord, and is not ended at a preemption point.
    seen: bool,
    /// It has been preempted once since it was cancelled: the next ends it.
    armed: bool,
}

impl<'p> Fiber<'p> {
    fn new(vm: Vm<'p>, task: u32, wake: Wake) -> Box<Fiber<'p>> {
        let ctl: Arc<Ctl> = Arc::default();
        let mut f = Box::new(Fiber {
            vm,
            task,
            wake,
            ctl,
            seen: false,
            armed: false,
        });
        f.vm.cancel = Some(f.ctl.cancelled.clone());
        f
    }
}

/// A thread's cancellation: see the module docs.
#[derive(Default)]
struct Ctl<'p> {
    /// Its machine reads it too, at every operation the runtime answers.
    cancelled: Arc<AtomicBool>,
    parked: Mutex<Parked<'p>>,
}

/// Where a thread last waited -- a hint, since nothing clears it when the
/// thread is woken.
#[derive(Default)]
enum Parked<'p> {
    #[default]
    Nowhere,
    /// On the thread with this number, in `await`.
    Await(u32),
    /// On the channel with this number.
    Receive(u32),
    /// In `retry`. Weak, or a thread left waiting when the run ends would
    /// hold itself, through its `Ctl`, and its heap would never be let go.
    Stm(std::sync::Weak<Mutex<Option<(Box<Fiber<'p>>, Reg)>>>),
    /// On a call into the world, which an I/O thread makes.
    Io(std::sync::Weak<Mutex<Option<(Box<Fiber<'p>>, Reg)>>>),
}

impl<'p> Ctl<'p> {
    /// The thread is about to wait `at`: say so, then answer whether it has
    /// been cancelled, in which case it must not wait. Called with what it
    /// waits on locked.
    fn park(&self, at: Parked<'p>) -> bool {
        *lock(&self.parked) = at;
        self.cancelled.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// What a thread is given when it runs again.
enum Wake {
    /// Nothing: carry on from where it stopped.
    Go,
    /// Start at a definition -- the main thread.
    Entry(Pc),
    /// Start at a definition with these arguments -- the main thread of a
    /// call C made into a library.
    Call(Pc, Vec<crate::value::Value>),
    /// Start by calling a function with `()` -- a spawned thread.
    Start(Parcel, meadow_rt::desc::Desc),
    /// The answer to what it was waiting for, into a register.
    Deliver(Reg, Parcel),
    /// A new channel or thread handle, into a register.
    Handle(Reg, Kind, u32),
    /// What a call into the world answered, into a register.
    Built(Reg, crate::native::Build),
    /// An immediate -- a commit's `Bool`, a wait's `()` -- into a register.
    Set(Reg, crate::value::Value),
    /// What it was waiting for failed, with this message: the thread it
    /// awaited did.
    Fail(String),
}

#[derive(Default)]
struct Task<'p> {
    /// Its result exported for whoever awaits it -- any number of times, so
    /// kept -- or the message it failed with.
    outcome: Option<Result<Parcel, String>>,
    waiters: Vec<(Box<Fiber<'p>>, Reg)>,
    /// The thread's, while it runs: see [`Fiber::ctl`].
    ctl: Arc<Ctl<'p>>,
}

#[derive(Default)]
struct Channel<'p> {
    messages: VecDeque<Parcel>,
    receivers: VecDeque<(Box<Fiber<'p>>, Reg)>,
}

type Queue<'p> = Mutex<VecDeque<Box<Fiber<'p>>>>;

/// A thread parked by `retry`: shared between every `TVar` it waits on, and
/// taken out by whichever is written first.
type WaitSlot<'p> = Arc<Mutex<Option<(Box<Fiber<'p>>, Reg)>>>;

struct Shared<'p> {
    /// Where the run's output goes, if not to the process's -- see [`Sink`].
    sink: Option<Sink>,
    program: &'p Program,
    native: Option<&'p crate::jit::Native<'p>>,
    fuel: u64,
    steps: AtomicU64,
    /// One run queue per worker, stealable by the others.
    locals: Vec<Queue<'p>>,
    /// The queue every worker looks at now and then.
    global: Queue<'p>,
    channels: RwLock<Vec<Arc<Mutex<Channel<'p>>>>>,
    tasks: RwLock<Vec<Arc<Mutex<Task<'p>>>>>,
    /// Every `TVar`, and the threads waiting on each. A commit publishes its
    /// writes first and then looks here; a thread checks nothing it read has
    /// changed and parks itself, under this same lock -- so no write can fall
    /// between the check and the parking.
    world: Arc<crate::stm::World>,
    stm_waiters: Mutex<std::collections::HashMap<u32, Vec<WaitSlot<'p>>>>,
    /// Threads running or queued. Waiting ones are not counted -- see the
    /// module docs on deadlock.
    active: AtomicUsize,
    /// Workers asleep, and what wakes them.
    sleeping: AtomicUsize,
    sleep: Mutex<()>,
    wake: Condvar,
    done: AtomicBool,
    finished: Mutex<Option<Result<String, Error>>>,
    /// The main thread's answer, where it is not an object: see
    /// [`Outcome::answer`].
    answer: Mutex<Option<crate::value::Value>>,
    /// Have the workers past the first been started?
    started: AtomicBool,
    stats: Mutex<Stats>,
    /// What a new thread's machine is given, and where every finished thread's
    /// samples are added up.
    sampling: Option<Sampling>,
    /// The run's clock, when it is being profiled for time. Held here so it
    /// outlives every thread that samples on it, and stops when the run does.
    ticker: Option<crate::profile::Ticker>,
    profile: Mutex<Option<crate::profile::Profile>>,
    /// The socket calls the run's threads are waiting in, stopped when it ends.
    waits: Arc<meadow_rt::net::Waits>,
    /// Threads waiting on a call into the world (`Request::Block`), each for
    /// an I/O thread to make it.
    io: Mutex<IoQueue<'p>>,
    io_ready: Condvar,
    /// Calls made or to be made: a thread waiting on one is not deadlocked,
    /// whatever else is waiting.
    io_pending: AtomicUsize,
}

#[derive(Default)]
struct IoQueue<'p> {
    /// Each call, and the thread waiting on it -- which a cancel may take out
    /// first, leaving the slot empty.
    calls: VecDeque<(WaitSlot<'p>, crate::vm::Work)>,
    /// I/O threads waiting for a call.
    idle: usize,
}

/// A lock, carrying on past a panic elsewhere: the data behind these locks is
/// queues and counters, which a panicking worker leaves consistent.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl<'p> Shared<'p> {
    fn channel(&self, id: u32) -> Option<Arc<Mutex<Channel<'p>>>> {
        self.channels
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(id as usize)
            .cloned()
    }

    fn task(&self, id: u32) -> Option<Arc<Mutex<Task<'p>>>> {
        self.tasks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(id as usize)
            .cloned()
    }

    /// End the run with `result`, unless something already has.
    fn finish(&self, result: Result<String, Error>) {
        let mut finished = lock(&self.finished);
        if finished.is_none() {
            *finished = Some(result);
        }
        drop(finished);
        self.done.store(true, Ordering::SeqCst);
        // A thread waiting on a socket -- a server's, accepting -- would keep
        // its worker, and so the run, from ever ending.
        self.waits.stop();
        {
            let _io = lock(&self.io);
            self.io_ready.notify_all();
        }
        let _guard = lock(&self.sleep);
        self.wake.notify_all();
    }

    /// A thread stopped counting as active: it finished, or it is waiting now.
    /// Called with whatever it waits on still locked, so nothing can wake it
    /// before it is counted out.
    fn release(&self) {
        // A call into the world counts until its thread is active again,
        // and it is counted active before the call stops counting: so this
        // never sees neither.
        if self.active.fetch_sub(1, Ordering::SeqCst) == 1
            && self.io_pending.load(Ordering::SeqCst) == 0
        {
            // Nothing running and nothing queued: nobody is left who could
            // wake a waiting thread.
            self.finish(Err(Error {
                msg: meadow_rt::thread::DEADLOCK.into(),
            }));
        }
    }

    /// Wake a sleeping worker, if there is one, to look for work.
    fn nudge(&self) {
        if self.sleeping.load(Ordering::SeqCst) > 0 {
            let _guard = lock(&self.sleep);
            self.wake.notify_one();
        }
    }

    /// Is there anything in any queue?
    fn any_work(&self) -> bool {
        !lock(&self.global).is_empty() || self.locals.iter().any(|q| !lock(q).is_empty())
    }
}

/// Ends the run if the worker holding it panics.
///
/// A panic is the runtime's own bug -- a program's failures are `Err`s -- but it
/// must not hang the run. The panicking worker's thread dies with it, still
/// counted as active, so without this the other workers would wait for it
/// forever and the scope would wait for them. Stopping everyone lets the scope
/// finish, and it passes the panic on.
struct StopOnPanic<'s, 'p>(&'s Shared<'p>);

impl Drop for StopOnPanic<'_, '_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.finish(Err(Error {
                msg: "the runtime panicked".into(),
            }));
        }
    }
}

/// The worker's socket calls as the run's (`meadow_rt::net::Waits`), until
/// it stops.
struct JoinWaits;

impl JoinWaits {
    fn new(waits: &Arc<meadow_rt::net::Waits>) -> Self {
        meadow_rt::net::join(waits);
        JoinWaits
    }
}

impl Drop for JoinWaits {
    fn drop(&mut self) {
        meadow_rt::net::leave();
    }
}

/// One OS thread's view of the scheduler.
struct Worker<'s, 'p> {
    index: usize,
    shared: &'s Shared<'p>,
    /// The thread to run as soon as the current one stops: the last one this
    /// worker woke. Only this worker sees it, so it cannot be stolen.
    next: Option<Box<Fiber<'p>>>,
    turns: u64,
    /// Where the next search for a victim starts, varied so that idle workers
    /// do not all pile onto the same one.
    rng: u64,
}

/// Why a thread stopped running.
enum Stop {
    Halted(crate::value::Value),
    Failed(Error),
    Requested(Request),
    Preempted,
    OutOfFuel,
}

impl<'s, 'p: 's> Worker<'s, 'p> {
    fn new(index: usize, shared: &'s Shared<'p>) -> Self {
        Worker {
            index,
            shared,
            next: None,
            turns: 0,
            rng: 0x9E37_79B9_7F4A_7C15 ^ (index as u64 + 1),
        }
    }

    /// Take a thread, run it until it stops, deal with why, repeat.
    fn run(mut self, scope: &'s std::thread::Scope<'s, '_>) {
        let sh = self.shared;
        let _stop_everyone = StopOnPanic(sh);
        let _waits = JoinWaits::new(&sh.waits);
        loop {
            if sh.done.load(Ordering::SeqCst) {
                return;
            }
            let Some(mut fiber) = self.find() else {
                self.idle();
                continue;
            };
            // Run it for as long as it keeps going. A request that does not make
            // it wait comes straight back here.
            loop {
                let stop = run_slice(sh, &mut fiber);
                match self.settle(scope, fiber, stop) {
                    Some(f) => fiber = f,
                    None => break,
                }
            }
        }
    }

    /// The next thread to run: this worker's own, the shared queue's, or
    /// another worker's.
    fn find(&mut self) -> Option<Box<Fiber<'p>>> {
        let sh = self.shared;
        self.turns += 1;
        if let Some(f) = self.next.take() {
            return Some(f);
        }
        if self.turns % GLOBAL_EVERY == 0 {
            if let Some(f) = lock(&sh.global).pop_front() {
                return Some(f);
            }
        }
        if let Some(f) = lock(&sh.locals[self.index]).pop_front() {
            return Some(f);
        }
        if let Some(f) = lock(&sh.global).pop_front() {
            return Some(f);
        }
        self.steal()
    }

    /// Take half of some other worker's queue, from the end it runs last.
    fn steal(&mut self) -> Option<Box<Fiber<'p>>> {
        let sh = self.shared;
        let n = sh.locals.len();
        if n == 1 {
            return None;
        }
        // xorshift: enough to spread the victims, and no dependency.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let start = (self.rng % n as u64) as usize;
        for k in 0..n {
            let victim = (start + k) % n;
            if victim == self.index {
                continue;
            }
            let mut taken = {
                let mut q = lock(&sh.locals[victim]);
                let half = q.len().div_ceil(2);
                let at = q.len() - half;
                q.split_off(at)
            };
            if let Some(first) = taken.pop_front() {
                if !taken.is_empty() {
                    lock(&sh.locals[self.index]).extend(taken);
                }
                lock(&sh.stats).stolen += 1;
                return Some(first);
            }
        }
        None
    }

    /// Nothing to do: sleep until woken, or briefly, in case.
    fn idle(&self) {
        let sh = self.shared;
        let guard = lock(&sh.sleep);
        // Counted as asleep *before* the last look, so a thread queued after
        // that look finds `sleeping` set and wakes this worker.
        sh.sleeping.fetch_add(1, Ordering::SeqCst);
        if !sh.done.load(Ordering::SeqCst) && !sh.any_work() {
            let _ = sh.wake.wait_timeout(guard, Duration::from_millis(50));
        }
        sh.sleeping.fetch_sub(1, Ordering::SeqCst);
    }

    /// Put a new thread on this worker's queue, counted as active, where an
    /// idle worker may take it.
    fn schedule(&self, fiber: Box<Fiber<'p>>) {
        self.shared.active.fetch_add(1, Ordering::SeqCst);
        self.requeue(fiber);
    }

    /// Run a thread this worker just woke as soon as the current one stops,
    /// counted as active. Whatever held the slot before goes on the queue.
    fn schedule_next(&mut self, fiber: Box<Fiber<'p>>) {
        self.shared.active.fetch_add(1, Ordering::SeqCst);
        if let Some(bumped) = self.next.replace(fiber) {
            self.requeue(bumped);
        }
    }

    /// Put an already active thread back on this worker's queue, and wake a
    /// worker to take it if this one has more than it can run.
    fn requeue(&self, fiber: Box<Fiber<'p>>) {
        let waiting = {
            let mut q = lock(&self.shared.locals[self.index]);
            q.push_back(fiber);
            q.len()
        };
        if waiting > 1 || self.next.is_some() {
            self.shared.nudge();
        }
    }

    /// Start the other workers, the first time a thread is spawned.
    fn start_workers(&self, scope: &'s std::thread::Scope<'s, '_>) {
        let sh = self.shared;
        if sh.started.swap(true, Ordering::SeqCst) {
            return;
        }
        for index in 1..sh.locals.len() {
            scope.spawn(move || Worker::new(index, sh).run(scope));
        }
    }

    /// Take a finished thread's samples into the run's.
    ///
    /// Every thread that stops for good, not only `main`: a green thread is its
    /// own machine with its own chain of continuations, so a run's profile is
    /// all of them. Missing this is how a profile of a program that does its
    /// work on spawned threads comes back empty.
    fn collect(sh: &Shared<'p>, fiber: &mut Fiber<'p>) {
        #[cfg(feature = "profile-alloc")]
        {
            let mut stats = lock(&sh.stats);
            stats.ops.merge(&fiber.vm.ops);
            stats.feedback.merge(&fiber.vm.feedback);
        }
        let Some(p) = fiber.vm.profile.take() else {
            return;
        };
        let mut into = lock(&sh.profile);
        match &mut *into {
            Some(all) => all.merge(&p),
            None => *into = Some(*p),
        }
    }

    /// Act on why a thread stopped. `Some` hands it back to keep running.
    fn settle(
        &mut self,
        scope: &'s std::thread::Scope<'s, '_>,
        mut fiber: Box<Fiber<'p>>,
        stop: Stop,
    ) -> Option<Box<Fiber<'p>>> {
        let sh = self.shared;
        match stop {
            Stop::Preempted => {
                // A cancelled thread that makes no operation is ended at its
                // second preemption point after the cancel, unless it asked:
                // at least a slice's grace to stop of its own accord.
                if fiber.ctl.cancelled() && !fiber.seen && fiber.task != 0 {
                    if fiber.armed {
                        let msg = meadow_rt::thread::CANCELLED.into();
                        return self.settle(scope, fiber, Stop::Failed(Error { msg }));
                    }
                    fiber.armed = true;
                }
                self.requeue(fiber);
                None
            }
            Stop::OutOfFuel => {
                sh.finish(Err(Error {
                    msg: format!("ran for {} instructions without finishing", sh.fuel),
                }));
                None
            }
            Stop::Halted(v) => {
                Self::collect(sh, &mut fiber);
                if fiber.task == 0 {
                    if !matches!(v, crate::value::Value::Obj(_)) {
                        *lock(&sh.answer) = Some(v);
                    }
                    let shown = fiber.vm.show(v);
                    {
                        let mut stats = lock(&sh.stats);
                        stats.add(&fiber.vm.heap);
                        stats.heap_slots = fiber.vm.heap.capacity();
                        stats.region_slots = fiber.vm.heap.region_slots();
                        stats.old_slots = fiber.vm.heap.old_slots();
                        stats.old_live = fiber.vm.heap.old_live();
                    }
                    sh.finish(Ok(shown));
                } else {
                    // Exported with the thread's own heap, which goes away with it.
                    let outcome = fiber
                        .vm
                        .heap
                        .export(v)
                        .map_err(|why| meadow_rt::thread::unsendable(why.describe()));
                    lock(&sh.stats).add(&fiber.vm.heap);
                    self.complete(fiber.task, outcome);
                    sh.release();
                }
                None
            }
            Stop::Failed(e) => {
                Self::collect(sh, &mut fiber);
                lock(&sh.stats).add(&fiber.vm.heap);
                if fiber.task == 0 {
                    sh.finish(Err(e));
                } else {
                    self.complete(fiber.task, Err(e.msg));
                    sh.release();
                }
                None
            }
            Stop::Requested(request) if request.notices_cancel() && fiber.ctl.cancelled() => {
                fiber.wake = Wake::Fail(meadow_rt::thread::CANCELLED.into());
                Some(fiber)
            }
            Stop::Requested(request) => match request {
                Request::Cancel { task } => {
                    let Some(slot) = sh.task(task) else {
                        fiber.wake = Wake::Fail(format!("cancel: there is no thread {task}"));
                        return Some(fiber);
                    };
                    let ctl = {
                        let t = lock(&slot);
                        t.outcome.is_none().then(|| t.ctl.clone())
                    };
                    // Finished, or asked already: nothing to do.
                    if let Some(ctl) = ctl.filter(|c| !c.cancelled.swap(true, Ordering::SeqCst)) {
                        let at = std::mem::take(&mut *lock(&ctl.parked));
                        if let Some(mut w) = self.unpark(task, at) {
                            w.wake = Wake::Fail(meadow_rt::thread::CANCELLED.into());
                            self.schedule_next(w);
                        }
                    }
                    Some(fiber)
                }
                Request::Cancelled { dst } => {
                    let cancelled = fiber.ctl.cancelled();
                    fiber.seen |= cancelled;
                    fiber.wake = Wake::Set(dst, crate::value::Value::Bool(cancelled));
                    Some(fiber)
                }
                Request::Block { job, dst } => {
                    let slot: WaitSlot<'p> = Arc::new(Mutex::new(Some((fiber, dst))));
                    let cancelled = {
                        let held = lock(&slot);
                        let (f, _) = held.as_ref().expect("just put there");
                        f.ctl.park(Parked::Io(Arc::downgrade(&slot)))
                    };
                    if cancelled {
                        let (mut fiber, _) = lock(&slot).take().expect("not filed yet");
                        fiber.wake = Wake::Fail(meadow_rt::thread::CANCELLED.into());
                        return Some(fiber);
                    }
                    // Counted as pending before the thread is counted out --
                    // see `Shared::release`.
                    sh.io_pending.fetch_add(1, Ordering::SeqCst);
                    let more = {
                        let mut io = lock(&sh.io);
                        io.calls.push_back((slot, job.0));
                        io.calls.len() > io.idle
                    };
                    if more {
                        // Every I/O thread is busy waiting on a call of its own.
                        scope.spawn(move || io_thread(sh));
                    } else {
                        sh.io_ready.notify_one();
                    }
                    sh.release();
                    None
                }
                Request::Spawn { body, answer, dst } => {
                    let mut child = Fiber::new(
                        Vm::with_heap(sh.program, Heap::with_capacity(THREAD_INITIAL)),
                        0,
                        Wake::Start(body, answer),
                    );
                    child.vm.scheduled = true;
                    child.vm.use_native(sh.native);
                    child.vm.world = Some(sh.world.clone());
                    capture(&mut child.vm, &sh.sink);
                    if let Some(s) = sh.sampling {
                        child.vm.profile = Some(Box::new(match (&sh.ticker, s.every) {
                            (Some(t), _) => t.profile(s.depth),
                            (None, every) => {
                                crate::profile::Profile::new(every.unwrap_or(1), s.depth)
                            }
                        }));
                    }
                    let id = {
                        let mut tasks = sh.tasks.write().unwrap_or_else(|p| p.into_inner());
                        tasks.push(Arc::new(Mutex::new(Task {
                            ctl: child.ctl.clone(),
                            ..Task::default()
                        })));
                        (tasks.len() - 1) as u32
                    };
                    child.task = id;
                    self.start_workers(scope);
                    self.schedule(child);
                    fiber.wake = Wake::Handle(dst, Kind::Task, id);
                    Some(fiber)
                }
                Request::Await { task, dst } => {
                    let Some(slot) = sh.task(task) else {
                        fiber.wake = Wake::Fail(format!("await: there is no thread {task}"));
                        return Some(fiber);
                    };
                    let mut t = lock(&slot);
                    match &t.outcome {
                        Some(Ok(p)) => {
                            fiber.wake = Wake::Deliver(dst, p.clone());
                            Some(fiber)
                        }
                        Some(Err(msg)) => {
                            fiber.wake = Wake::Fail(msg.clone());
                            Some(fiber)
                        }
                        None if fiber.ctl.park(Parked::Await(task)) => {
                            fiber.wake = Wake::Fail(meadow_rt::thread::CANCELLED.into());
                            Some(fiber)
                        }
                        None => {
                            t.waiters.push((fiber, dst));
                            sh.release();
                            None
                        }
                    }
                }
                Request::Yield => {
                    self.requeue(fiber);
                    None
                }
                Request::NewChannel { dst } => {
                    let id = {
                        let mut channels = sh.channels.write().unwrap_or_else(|p| p.into_inner());
                        channels.push(Arc::new(Mutex::new(Channel::default())));
                        (channels.len() - 1) as u32
                    };
                    fiber.wake = Wake::Handle(dst, Kind::Channel, id);
                    Some(fiber)
                }
                Request::Send { channel, message } => {
                    let Some(slot) = sh.channel(channel) else {
                        fiber.wake = Wake::Fail(format!("send: there is no channel {channel}"));
                        return Some(fiber);
                    };
                    let receiver = {
                        let mut ch = lock(&slot);
                        match ch.receivers.pop_front() {
                            Some(r) => Some((r, message)),
                            None => {
                                ch.messages.push_back(message);
                                None
                            }
                        }
                    };
                    if let Some(((mut r, dst), message)) = receiver {
                        r.wake = Wake::Deliver(dst, message);
                        self.schedule_next(r);
                    }
                    Some(fiber)
                }
                Request::StmWake { written, dst } => {
                    let mut woken = Vec::new();
                    {
                        let mut waiters = lock(&sh.stm_waiters);
                        for id in written {
                            for slot in waiters.remove(&id).unwrap_or_default() {
                                if let Some(w) = lock(&slot).take() {
                                    sh.world.wait_ends();
                                    woken.push(w);
                                }
                            }
                        }
                    }
                    for (mut w, wdst) in woken {
                        w.wake = Wake::Set(wdst, crate::value::Value::Unit);
                        self.schedule_next(w);
                    }
                    fiber.wake = Wake::Set(dst, crate::value::Value::Bool(true));
                    Some(fiber)
                }
                Request::StmWait { dst } => {
                    let Some(txn) = fiber.vm.txn.take() else {
                        fiber.wake = Wake::Fail(meadow_rt::stm::outside("retry"));
                        return Some(fiber);
                    };
                    // Counted before the check -- see `World::wait_begins`.
                    sh.world.wait_begins();
                    let mut waiters = lock(&sh.stm_waiters);
                    if sh.world.changed(&txn.reads) {
                        drop(waiters);
                        sh.world.wait_ends();
                        fiber.wake = Wake::Set(dst, crate::value::Value::Unit);
                        return Some(fiber);
                    }
                    let slot: WaitSlot<'p> = Arc::new(Mutex::new(Some((fiber, dst))));
                    let cancelled = {
                        let held = lock(&slot);
                        let (f, _) = held.as_ref().expect("just put there");
                        f.ctl.park(Parked::Stm(Arc::downgrade(&slot)))
                    };
                    if cancelled {
                        drop(waiters);
                        sh.world.wait_ends();
                        let (mut fiber, _) = lock(&slot).take().expect("not filed yet");
                        fiber.wake = Wake::Fail(meadow_rt::thread::CANCELLED.into());
                        return Some(fiber);
                    }
                    for (id, _) in &txn.reads {
                        let list = waiters.entry(*id).or_default();
                        list.retain(|w| lock(w).is_some());
                        list.push(slot.clone());
                    }
                    // Counted out while still holding the lock a commit needs
                    // to wake it -- see `Shared::release`.
                    sh.release();
                    None
                }
                Request::Receive { channel, dst } => {
                    let Some(slot) = sh.channel(channel) else {
                        fiber.wake = Wake::Fail(format!("receive: there is no channel {channel}"));
                        return Some(fiber);
                    };
                    let mut ch = lock(&slot);
                    match ch.messages.pop_front() {
                        Some(message) => {
                            fiber.wake = Wake::Deliver(dst, message);
                            Some(fiber)
                        }
                        None if fiber.ctl.park(Parked::Receive(channel)) => {
                            fiber.wake = Wake::Fail(meadow_rt::thread::CANCELLED.into());
                            Some(fiber)
                        }
                        None => {
                            ch.receivers.push_back((fiber, dst));
                            sh.release();
                            None
                        }
                    }
                }
            },
        }
    }

    /// Take thread `task` out of where it waits, `at`, if it is still there.
    fn unpark(&self, task: u32, at: Parked<'p>) -> Option<Box<Fiber<'p>>> {
        let sh = self.shared;
        match at {
            Parked::Nowhere => None,
            Parked::Await(on) => {
                let slot = sh.task(on)?;
                let mut t = lock(&slot);
                let i = t.waiters.iter().position(|(f, _)| f.task == task)?;
                Some(t.waiters.remove(i).0)
            }
            Parked::Receive(on) => {
                let slot = sh.channel(on)?;
                let mut ch = lock(&slot);
                let i = ch.receivers.iter().position(|(f, _)| f.task == task)?;
                ch.receivers.remove(i).map(|(f, _)| f)
            }
            Parked::Stm(slot) => {
                // Under the lock a commit wakes waiters with, as `StmWake`
                // takes them; the empty slot it leaves is dropped from the
                // lists the next time one is filed.
                let _waiters = lock(&sh.stm_waiters);
                let slot = slot.upgrade()?;
                let (f, _) = lock(&slot).take()?;
                sh.world.wait_ends();
                Some(f)
            }
            // The call is still made, or being made, on its I/O thread, and
            // still counted as pending until it is; what it answers is dropped.
            Parked::Io(slot) => {
                let slot = slot.upgrade()?;
                let (f, _) = lock(&slot).take()?;
                Some(f)
            }
        }
    }

    /// Record a spawned thread's result, and put whoever awaits it on this
    /// worker's queue.
    fn complete(&mut self, task: u32, outcome: Result<Parcel, String>) {
        let Some(slot) = self.shared.task(task) else {
            return;
        };
        let waiters = {
            let mut t = lock(&slot);
            t.outcome = Some(outcome.clone());
            std::mem::take(&mut t.waiters)
        };
        for (mut w, dst) in waiters {
            w.wake = match &outcome {
                Ok(p) => Wake::Deliver(dst, p.clone()),
                Err(msg) => Wake::Fail(msg.clone()),
            };
            self.schedule_next(w);
        }
    }
}

/// Make the calls into the world that threads wait on, one at a time, each
/// thread's answer handed back to the workers to run it again; until the run
/// ends. One is started whenever a call finds none of them waiting for one,
/// so a thread never waits for another's call.
fn io_thread(sh: &Shared) {
    let _waits = JoinWaits::new(&sh.waits);
    loop {
        let (slot, work) = {
            let mut io = lock(&sh.io);
            loop {
                if sh.done.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(call) = io.calls.pop_front() {
                    break call;
                }
                io.idle += 1;
                io = sh.io_ready.wait(io).unwrap_or_else(|p| p.into_inner());
                io.idle -= 1;
            }
        };
        // A thread cancelled before its call was made: not made at all; and
        // one cancelled while it was being made does not get what it answered.
        let built = if lock(&slot).is_some() {
            Some(work())
        } else {
            None
        };
        let fiber = built.and_then(|b| {
            let (mut f, dst) = lock(&slot).take()?;
            f.wake = Wake::Built(dst, b);
            Some(f)
        });
        if let Some(fiber) = fiber {
            sh.active.fetch_add(1, Ordering::SeqCst);
            lock(&sh.global).push_back(fiber);
        }
        sh.io_pending.fetch_sub(1, Ordering::SeqCst);
        sh.nudge();
    }
}

/// Deliver what the thread was woken with, then run it for a slice.
fn run_slice(sh: &Shared, fiber: &mut Fiber) -> Stop {
    let vm = &mut fiber.vm;
    match std::mem::replace(&mut fiber.wake, Wake::Go) {
        Wake::Go => {}
        Wake::Entry(pc) => vm.start(pc),
        Wake::Call(pc, args) => vm.start_with(pc, &args),
        Wake::Start(body, answer) => {
            if let Err(e) = vm.start_call(&body, answer) {
                return Stop::Failed(e);
            }
        }
        Wake::Deliver(dst, parcel) => vm.deliver_parcel(dst, &parcel),
        Wake::Handle(dst, kind, id) => vm.deliver_handle(dst, kind, id),
        Wake::Set(dst, v) => vm.set(dst, v),
        Wake::Built(dst, b) => {
            let v = vm.build(b);
            vm.set(dst, v);
        }
        Wake::Fail(msg) => return Stop::Failed(Error { msg }),
    }
    for n in 0..SLICE {
        match vm.advance() {
            Err(e) => return Stop::Failed(e),
            Ok(Some(v)) => return Stop::Halted(v),
            Ok(None) => {
                if let Some(r) = vm.request.take() {
                    sh.steps.fetch_add(n + 1, Ordering::Relaxed);
                    return Stop::Requested(r);
                }
            }
        }
    }
    if sh.steps.fetch_add(SLICE, Ordering::Relaxed) + SLICE > sh.fuel {
        return Stop::OutOfFuel;
    }
    Stop::Preempted
}
