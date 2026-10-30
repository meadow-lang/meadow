//! **A green thread's context**: everything the runtime keeps for one thread
//! -- its heap, its stack segments, its literals and top-level values, its
//! transaction -- apart from every other thread's, so that none of it is ever
//! touched by two OS threads at once and counting needs no atomics.
//!
//! A thread moves between OS threads (`crate::sched`), so what is "current"
//! is set by the worker that resumes it, and read through [`get`] -- which is
//! never inlined. That matters: code that waits, and so may resume on another
//! OS thread, must not keep an OS thread's thread-local address from before
//! the wait, and a call LLVM cannot see into, made after the wait, is what
//! stops it.

use crate::heap::{Heap, Word};
use crate::segments::{Parked, Suspended};
use std::cell::Cell;
use std::collections::HashMap;

/// A transaction in progress (`crate::sched`): when it began, the `TVar`s it
/// read and the version of each it saw, and its writes -- values in this
/// thread's heap -- one level per `orElse` being tried.
pub struct Txn {
    pub start: u64,
    pub reads: Vec<(usize, u64)>,
    pub writes: Vec<Vec<(usize, Word, i64)>>,
}

/// What a waiting thread asks of the scheduler, which acts on it once the
/// thread has stopped: see `crate::sched`.
pub enum Request {
    Yield,
    Await(usize),
    Receive(usize),
    Wait(Vec<(usize, u64)>),
    /// The thread failed with this message, and is done.
    Failed(String),
    /// A call into the world that may wait -- an accept, a read -- for an
    /// I/O thread to make, so that the worker runs another thread meanwhile:
    /// see `crate::sched::blocking`.
    Block(Box<dyn FnOnce() + Send>),
    /// A `Time.sleep`, until then, on the scheduler's timer: see
    /// `crate::sched::sleep`.
    Sleep(std::time::Instant),
}

/// What a thread is woken with.
pub enum Wake {
    Nothing,
    Value(crate::parcel::Parcel),
    Fail(String),
}

/// A table keyed by an address. An address is spread over the table by one
/// multiplication: the standard hasher is made to withstand keys chosen to
/// collide, which these are not, and a program's every mention of a string
/// literal looks one up -- a tenth of a compiler's time, hashing addresses.
pub type ByAddress<V> = HashMap<usize, V, std::hash::BuildHasherDefault<AddressHasher>>;

#[derive(Default)]
pub struct AddressHasher(u64);

impl std::hash::Hasher for AddressHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0100_0000_01B3);
        }
    }

    fn write_usize(&mut self, n: usize) {
        // The high bits are the well-mixed ones, and a table takes its
        // place from the low: turned round.
        self.0 = (n as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left(32);
    }
}

/// Laid out as C would, for the first two words' sake: emitted code reads
/// them off the context it is handed (`meadow_llvm::emit`'s `CTX`), so they
/// must be where it looks.
#[repr(C)]
pub struct Ctx {
    /// Word 0: where the spill area starts -- `spill`'s buffer, which never
    /// moves.
    pub spill_at: *mut Word,
    /// Word 1: the running segment's shadow chain: see `crate::shadow`.
    /// Here for a program with threads; one without keeps it in a global.
    pub shadow: Word,
    pub heap: Heap,
    /// This thread's own counts of references into the regions it last
    /// touched: see `crate::region::Tallies`.
    pub regions: crate::region::Tallies,
    /// The segments running, innermost last: see `crate::segments`.
    pub running: Vec<*const ()>,
    /// Where each of `running`'s stacks begins and where it last stopped, in
    /// step with it: what a trace scans for roots (`crate::trace`).
    pub(crate) levels: Vec<crate::trace::Level>,
    /// The same for the OS thread's own stack, where a program that never
    /// spawns runs until a `handle` starts a segment: from where it began.
    pub(crate) os_level: Option<crate::trace::Level>,
    /// The level of the segment that has just suspended, for the `drive`
    /// frame it suspended to: see `crate::segments::suspend`.
    pub(crate) stopped: Option<crate::trace::Level>,
    /// Suspended segments, by the number their stack objects hold, with the
    /// handler each was suspended for and the segments suspended inside it.
    pub(crate) suspended: Vec<Option<Suspended>>,
    /// Segments suspended for a handler further out than their own, while
    /// that suspension lasts, innermost first: see `crate::segments::drive`.
    pub(crate) parked: Vec<Parked>,
    pub next_handler: u32,
    /// String literals, made once per thread, by their bytes' address.
    pub literals: ByAddress<Word>,
    /// Top-level values, once computed on this thread: see `GlobalGet`.
    pub globals: Vec<Option<(Word, i64)>>,
    /// What the leak check does not count: see `crate::prims::roots`.
    pub kept: Vec<(Word, i64)>,
    pub txn: Option<Txn>,
    pub request: Option<Request>,
    pub wake: Wake,
    /// Stacks of segments that have finished, to be used again: making one
    /// reserves memory and sets guard pages, which a program full of
    /// handlers would otherwise do at every `handle`.
    pub(crate) stacks: Vec<corosensei::stack::DefaultStack>,
    /// Where a call puts the arguments that do not fit in registers.
    /// As many words as the emitted module's widest call needs:
    /// `meadow_spill_words`. Never resized: `spill_at` is its address.
    pub spill: Vec<Word>,
    /// The thread's number: 0 is `main`.
    pub tid: usize,
    /// Inside a call C made into a library, on a segment of its own: a
    /// failure ends the call, not the process. See `crate::sched::export_run`.
    pub guarded: bool,
    /// Whether the thread has been asked to stop, and where it waits: shared
    /// with its task, where a canceller finds it. See `crate::sched::Ctl`.
    pub cancel: std::sync::Arc<crate::sched::Ctl>,
    /// The release stack (`Prim::ThreadDefer`): each release's key and the
    /// function, a reference this holds, newest last. See
    /// `docs/CANCELLATION.md`.
    pub exits: Vec<(i64, Word)>,
    /// The key the next release is given.
    pub next_exit: i64,
    /// How the thread's own code ended, once it has, while its releases run:
    /// see `crate::sched::wind_down`.
    pub ending: Option<Result<crate::parcel::Parcel, String>>,
    /// Whose the sockets this thread opens are (`meadow_rt::net::owning`):
    /// closed when it ends.
    pub owner: u64,
}

// Safety: a context is used by one OS thread at a time -- the one running its
// thread -- and handed between them by the scheduler, under its lock.
unsafe impl Send for Ctx {}

impl Ctx {
    pub fn new(tid: usize) -> Box<Ctx> {
        let mut c = Box::new(Ctx {
            spill_at: std::ptr::null_mut(),
            shadow: 0,
            heap: Heap::new(),
            regions: crate::region::Tallies::new(),
            running: Vec::new(),
            levels: Vec::new(),
            os_level: None,
            stopped: None,
            suspended: Vec::new(),
            parked: Vec::new(),
            next_handler: 1,
            literals: ByAddress::default(),
            globals: Vec::new(),
            kept: Vec::new(),
            txn: None,
            request: None,
            wake: Wake::Nothing,
            stacks: Vec::new(),
            // Safety: a constant the emitted module defines.
            spill: vec![0; unsafe { meadow_spill_words }.max(0) as usize],
            tid,
            guarded: false,
            cancel: Default::default(),
            exits: Vec::new(),
            next_exit: 0,
            ending: None,
            owner: meadow_rt::net::new_owner(),
        });
        c.spill_at = c.spill.as_mut_ptr();
        c
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        self.regions.settle();
        // Dropping a suspended segment unwinds it, which a runtime that aborts
        // on a panic cannot do: they are reset instead, never resumed, and
        // their stacks given back. A thread that failed inside a handler --
        // a cancelled one, waiting under it -- leaves its segments here.
        for s in self.suspended.drain(..).flatten() {
            crate::segments::abandon(s.top);
            s.nested.into_iter().for_each(crate::segments::abandon);
        }
        for s in self.parked.drain(..) {
            crate::segments::abandon(s);
        }
    }
}

thread_local! {
    static CURRENT: Cell<*mut Ctx> = const { Cell::new(std::ptr::null_mut()) };
}

/// The one context of a program that cannot spawn: see [`threaded`].
static mut ONLY: *mut Ctx = std::ptr::null_mut();

unsafe extern "C" {
    /// Whether the program spawns a thread anywhere: the emitted module says
    /// so, since it knows what the program does. See `meadow_llvm::emit`.
    static meadow_threaded: u8;
    /// How many words a call passes in the spill area, at most: the
    /// emitted module says, having seen every call.
    static meadow_spill_words: i64;
}

/// Whether the program can ever have a second thread. When it cannot there is
/// nothing to move between OS threads, so the one context is a static, and
/// reading it is a load rather than a thread-local's lookup.
#[inline(always)]
pub fn threaded() -> bool {
    // Safety: a constant the emitted module defines.
    unsafe { meadow_threaded != 0 }
}

/// The running thread's context.
#[inline(always)]
pub fn get() -> *mut Ctx {
    if !threaded() {
        // Safety: set before the program runs, and never changed after.
        return unsafe { ONLY };
    }
    current()
}

/// The running thread's context, when threads move between OS threads. Never
/// inlined: see the module docs.
#[inline(never)]
fn current() -> *mut Ctx {
    let c = CURRENT.with(Cell::get);
    if c.is_null() {
        crate::fail("the native runtime ran outside every thread")
    }
    c
}

/// The running thread's context, for the ways into emitted code from the
/// runtime, which hand it on as every function's first argument.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_ctx() -> *mut Ctx {
    get()
}

/// The context emitted code handed a runtime entry: its own, or -- `null`,
/// from a program that cannot spawn and so passes none -- the one there is.
#[inline(always)]
pub fn given(c: *mut Ctx) -> *mut Ctx {
    if c.is_null() { get() } else { c }
}

/// The running thread's context, or null where none is running on this OS
/// thread. Never inlined when threads move, as `current` is not.
#[inline(always)]
pub fn peek() -> *mut Ctx {
    if !threaded() {
        // Safety: as `get`.
        return unsafe { ONLY };
    }
    current_or_none()
}

#[inline(never)]
fn current_or_none() -> *mut Ctx {
    CURRENT.with(Cell::get)
}

/// Whether there is a running thread's context on this OS thread.
#[inline(never)]
pub fn present() -> bool {
    if !threaded() {
        // Safety: as `get`.
        return !unsafe { ONLY }.is_null();
    }
    !CURRENT.with(Cell::get).is_null()
}

/// Make `c` the running thread's context on this OS thread.
#[inline(never)]
pub fn set(c: *mut Ctx) {
    if !threaded() {
        // Safety: the program has one thread, and this is called before it
        // runs.
        unsafe { ONLY = c };
        return;
    }
    CURRENT.with(|cur| cur.set(c));
}
