//! **Concurrent tracing**: what finds a program's garbage cycles, unless it
//! was built with trial deletion (`crate::cycles`). `docs/SILO.md`,
//! "Concurrent tracing", is the design; this is how it is done.
//!
//! Counting frees everything that dies the moment it dies. What it cannot
//! free is a cycle, and a trace is how those are found: from the roots of one
//! green thread's heap, mark what the thread can reach; what is left unmarked
//! is garbage that only cycles hold. A trace has three parts:
//!
//! 1. **The roots**, in one pause, inside an allocation from compiled code
//!    ([`poll`]): the stacks the thread runs on, scanned conservatively from
//!    where each stopped ([`Level`]), what the context holds of the heap's,
//!    and what is waiting to be erased. Marking starts from these, taken as
//!    they are at that moment -- the snapshot.
//! 2. **Marking**, on the marker thread ([`marker`]), while the program goes
//!    on. A store into a block that already exists logs what it replaces
//!    ([`meadow_satb`]), so whatever the snapshot could reach is marked
//!    however the program rearranges it; a block made since is black, marked
//!    as it is taken ([`blacken`]); and nothing freed is handed out again
//!    until marking ends ([`State::limbo`]), so the marker never reads a block
//!    that has become another.
//! 3. **The sweep**, on the program's thread, a slice per allocation: every
//!    block of the snapshot's heap left unmarked is garbage. Its references
//!    to what is live are given up, what it held outside the heap is let go
//!    of, and its memory comes back once the sweep is through.
//!
//! Marks are bits beside each chunk ([`Chunk`]), never in a block: the
//! program writes a block's header as it builds one. Where each block of a
//! chunk begins is a bit too, set when the block is carved out; since a size
//! class is a word count, a block never moves, splits or merges, so those bits
//! stay true, and they are how a word is known to be a block -- a word on a
//! stack, or one the marker reads from a block that is being rewritten.

use crate::ctx::Ctx;
use crate::heap::{self, Word};
use meadow_rt::desc;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// How many heaps are marking now: what the emitted barrier loads before it
/// calls in, so that outside a trace a store costs a load and a branch.
#[unsafe(no_mangle)]
pub static meadow_marking: AtomicU64 = AtomicU64::new(0);

/// The fewest blocks a heap has before its first trace: below it, a trace
/// would cost more than any cycle there could hold.
const FLOOR: usize = 4096;

/// Logged references the program gathers before it hands them to the marker.
const SATB_BATCH: usize = 4096;

/// Allocations between two looks at whether marking is done.
const CHECK_EVERY: u32 = 64;

/// What one slice of the sweep looks at: words of a chunk's map of block
/// starts, or garbage blocks freed.
const SLICE_WORDS: usize = 256;
const SLICE_BLOCKS: usize = 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// --- where a stack stops -----------------------------------------------------

/// A stack the thread runs on: where it begins (its highest address; stacks
/// grow down) and where it last stopped to run something else -- a segment
/// it resumed, or the handler it suspended to. A trace scans each from where
/// it stopped to where it begins; the innermost, from where the trace is.
#[derive(Clone, Copy, Default, Debug)]
pub struct Level {
    pub base: usize,
    pub stopped: usize,
}

/// Run `f` with where this stack stops: the lowest address of this frame,
/// below the callee-saved registers, which are written into it first. So
/// every reference the code that called this holds -- in its frames, or in a
/// register it expects to find again -- is in memory at or above that
/// address, until `f` returns.
#[inline(never)]
pub(crate) fn stopped<T>(f: impl FnOnce(usize) -> T) -> T {
    let mut regs = [0usize; 12];
    // Safety: writes `regs`, and reads the stack pointer.
    let sp = unsafe { spill(regs.as_mut_ptr()) };
    std::hint::black_box(&mut regs);
    let out = f(sp.min(regs.as_ptr() as usize));
    std::hint::black_box(&mut regs);
    out
}

/// Write the callee-saved registers to `at`, and answer the stack pointer. A
/// register this function's own frame saved, to use for something else, is
/// in the frame, above the stack pointer: either way it is scanned.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn spill(at: *mut usize) -> usize {
    let sp: usize;
    // Safety: the caller's; `at` has room for twelve words.
    unsafe {
        std::arch::asm!(
            "mov [{at}], rbx",
            "mov [{at} + 8], rbp",
            "mov [{at} + 16], r12",
            "mov [{at} + 24], r13",
            "mov [{at} + 32], r14",
            "mov [{at} + 40], r15",
            "mov [{at} + 48], rdi",
            "mov [{at} + 56], rsi",
            "mov {sp}, rsp",
            at = in(reg) at,
            sp = out(reg) sp,
            options(nostack, preserves_flags),
        );
    }
    sp
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn spill(at: *mut usize) -> usize {
    let sp: usize;
    // Safety: as above.
    unsafe {
        std::arch::asm!(
            "stp x19, x20, [{at}]",
            "stp x21, x22, [{at}, #16]",
            "stp x23, x24, [{at}, #32]",
            "stp x25, x26, [{at}, #48]",
            "stp x27, x28, [{at}, #64]",
            "str x29, [{at}, #80]",
            "mov {sp}, sp",
            at = in(reg) at,
            sp = out(reg) sp,
            options(nostack, preserves_flags),
        );
    }
    sp
}

// --- chunks ------------------------------------------------------------------

/// What a trace knows of one chunk of a heap: where its blocks begin, and
/// which of them the trace in progress has marked -- a bit per word each.
pub struct Chunk {
    pub start: usize,
    words: usize,
    starts: Box<[AtomicU64]>,
    marks: Box<[AtomicU64]>,
    /// Whether the chunk is part of the trace in progress: a chunk made
    /// since holds only blocks made since, which are new by where they are.
    traced: AtomicBool,
}

impl Chunk {
    pub fn new(start: usize, words: usize) -> Arc<Chunk> {
        let n = words.div_ceil(64);
        Arc::new(Chunk {
            start,
            words,
            starts: (0..n).map(|_| AtomicU64::new(0)).collect(),
            marks: (0..n).map(|_| AtomicU64::new(0)).collect(),
            traced: AtomicBool::new(false),
        })
    }

    fn end(&self) -> usize {
        self.start + 8 * self.words
    }

    fn place(&self, at: usize) -> (usize, u64) {
        let i = (at - self.start) / 8;
        (i / 64, 1u64 << (i % 64))
    }

    /// A block begins at `at`. Only the heap's own thread writes these.
    pub fn set_start(&self, at: usize) {
        let (w, b) = self.place(at);
        let s = &self.starts[w];
        s.store(s.load(Ordering::Relaxed) | b, Ordering::Release);
    }

    /// No block begins at `at` any more: one poisoned, never handed out again.
    fn clear_start(&self, at: usize) {
        let (w, b) = self.place(at);
        let s = &self.starts[w];
        s.store(s.load(Ordering::Relaxed) & !b, Ordering::Release);
    }

    fn is_start(&self, at: usize) -> bool {
        if at % 8 != 0 {
            return false;
        }
        let (w, b) = self.place(at);
        self.starts[w].load(Ordering::Acquire) & b != 0
    }

    /// Mark the block at `at`: whether this was the first time. Most blocks
    /// a trace comes to are marked already, and asking is cheaper than
    /// writing.
    pub fn mark(&self, at: usize) -> bool {
        let (w, b) = self.place(at);
        let m = &self.marks[w];
        m.load(Ordering::Acquire) & b == 0 && m.fetch_or(b, Ordering::AcqRel) & b == 0
    }

    fn marked(&self, at: usize) -> bool {
        let (w, b) = self.place(at);
        self.marks[w].load(Ordering::Acquire) & b != 0
    }

    pub fn traced(&self) -> bool {
        self.traced.load(Ordering::Acquire)
    }

    /// The start of the block `at` is inside, looking back no further than
    /// the largest block a size class has: a word of the map at a time.
    fn start_at_or_before(&self, at: usize) -> Option<usize> {
        let i = (at - self.start) / 8;
        let lowest = i.saturating_sub(heap::CLASSES);
        let (mut w, b) = (i / 64, i % 64);
        // This word's bits at and below `i`.
        let mut bits = self.starts[w].load(Ordering::Acquire) & (u64::MAX >> (63 - b));
        loop {
            if bits != 0 {
                let j = 64 * w + 63 - bits.leading_zeros() as usize;
                return (j >= lowest).then_some(self.start + 8 * j);
            }
            if w == 0 || 64 * w <= lowest {
                return None;
            }
            w -= 1;
            bits = self.starts[w].load(Ordering::Acquire);
        }
    }

    fn clear_marks(&self) {
        for m in self.marks.iter() {
            m.store(0, Ordering::Relaxed);
        }
        self.traced.store(false, Ordering::Release);
    }
}

/// Word `i` of the block at `v`, read as the marker reads: as it is, while
/// the program may be writing it.
#[inline]
fn load(v: Word, i: usize) -> Word {
    // Safety: inside a chunk or a large block of the heap, which stays
    // mapped for as long as a trace can read it; aligned, as every block is.
    unsafe { (*((v as *const Word).add(i) as *const AtomicU64)).load(Ordering::Relaxed) }
}

// --- the snapshot -------------------------------------------------------------

/// The heap as it was when the roots were taken: its chunks, and its blocks
/// too large for one, each with a mark.
pub struct Snapshot {
    chunks: Vec<Arc<Chunk>>,
    /// Where each of `chunks` begins and ends, beside them: what a search
    /// reads, without going through to each chunk.
    spans: Vec<(usize, usize)>,
    /// The chunk the last search found: a block's fields mostly point near
    /// it, so the next search mostly finds the same one.
    hint: std::sync::atomic::AtomicUsize,
    large: Vec<(usize, usize)>,
    large_marks: Box<[AtomicBool]>,
    /// The lowest address any of it has, and one past the highest: most words
    /// of a stack are outside, and are passed over on that alone.
    lo: usize,
    hi: usize,
}

impl Snapshot {
    fn new(chunks: Vec<Arc<Chunk>>, large: Vec<(usize, usize)>) -> Snapshot {
        let ends = chunks
            .iter()
            .map(|c| (c.start, c.end()))
            .chain(large.iter().map(|&(s, w)| (s, s + 8 * w)));
        let (lo, hi) = ends.fold((usize::MAX, 0), |(lo, hi), (s, e)| (lo.min(s), hi.max(e)));
        Snapshot {
            lo,
            hi,
            spans: chunks.iter().map(|c| (c.start, c.end())).collect(),
            chunks,
            hint: std::sync::atomic::AtomicUsize::new(0),
            large_marks: (0..large.len()).map(|_| AtomicBool::new(false)).collect(),
            large,
        }
    }

    fn chunk_of(&self, at: usize) -> Option<&Chunk> {
        let h = self.hint.load(Ordering::Relaxed);
        if let Some(&(s, e)) = self.spans.get(h)
            && s <= at
            && at < e
        {
            return Some(&self.chunks[h]);
        }
        let i = self
            .spans
            .partition_point(|(s, _)| *s <= at)
            .checked_sub(1)?;
        if at >= self.spans[i].1 {
            return None;
        }
        self.hint.store(i, Ordering::Relaxed);
        Some(&self.chunks[i])
    }

    fn large_at(&self, at: usize) -> Option<usize> {
        let i = self.large.partition_point(|(s, _)| *s <= at);
        let i = i.checked_sub(1)?;
        let (s, words) = self.large[i];
        (at < s + 8 * words).then_some(i)
    }

    /// Where the block `v` is, and how far it may be read: `None` for a word
    /// that is not the start of a block of the snapshot in use -- one made
    /// since, one in a region, a number.
    fn bounds(&self, v: Word) -> Option<usize> {
        if !heap::is_block(v) {
            return None;
        }
        let at = v as usize;
        if let Some(c) = self.chunk_of(at) {
            return (c.is_start(at) && load(v, 0) as u32 != heap::FREE).then_some(c.end());
        }
        let i = self.large_at(at)?;
        let (s, words) = self.large[i];
        (s == at).then_some(s + 8 * words)
    }

    /// Mark `v`, if it is a block of the snapshot: whether it is newly marked,
    /// and so has its fields still to look at.
    fn mark(&self, v: Word) -> bool {
        if !heap::is_block(v) {
            return false;
        }
        let at = v as usize;
        if let Some(c) = self.chunk_of(at) {
            return c.is_start(at) && load(v, 0) as u32 != heap::FREE && c.mark(at);
        }
        match self.large_at(at) {
            Some(i) if self.large[i].0 == at => !self.large_marks[i].swap(true, Ordering::AcqRel),
            _ => false,
        }
    }

    /// Whether `v` is a block of the snapshot that the trace did not mark:
    /// after marking, what the sweep frees.
    fn unmarked(&self, v: Word) -> bool {
        if !heap::is_block(v) {
            return false;
        }
        let at = v as usize;
        if let Some(c) = self.chunk_of(at) {
            return c.is_start(at) && !heap::is_free(v) && !c.marked(at);
        }
        match self.large_at(at) {
            Some(i) if self.large[i].0 == at => !self.large_marks[i].load(Ordering::Acquire),
            _ => false,
        }
    }

    /// The block a word on a stack points into, if any: a frame may hold an
    /// address inside a block as well as its start.
    fn containing(&self, w: Word) -> Option<Word> {
        let at = w as usize;
        if let Some(c) = self.chunk_of(at) {
            let s = c.start_at_or_before(at)?;
            let v = s as Word;
            if heap::is_free(v) || at >= s + 8 * heap::size(v) {
                return None;
            }
            return Some(v);
        }
        let i = self.large_at(at)?;
        Some(self.large[i].0 as Word)
    }
}

/// Look at the fields of the marked block `v`, marking what they reach. Read
/// as they are, which may be half-way through being written -- a dead block
/// being reused, a token whose fields were taken -- so every word followed is
/// checked to be a block first, and nothing past the block is read: a word
/// read wrong can mark too much, and never anything worse.
fn scan(snap: &Snapshot, v: Word, gray: &mut Vec<Word>) {
    scan_from(snap, v, 0, gray);
}

/// [`scan`], from field `from` on: what a block waiting to be erased still
/// holds, its fields before `from` given up already.
fn scan_from(snap: &Snapshot, v: Word, from: usize, gray: &mut Vec<Word>) {
    let Some(end) = snap.bounds(v) else { return };
    let w0 = load(v, 0);
    let w1 = load(v, 1);
    let kind = w1 & 0xFF;
    match kind {
        heap::DATA | heap::CLOSURE | heap::ARRAY | heap::CELL | heap::RECORD | heap::MUT_ARRAY => {}
        // A string or a big integer holds bytes; a channel, a thread, a
        // `TVar`, a one-shot flag or a continuation holds a number; and a
        // compact's region points at nothing outside it.
        _ => return,
    }
    let len = (w0 >> 32) as usize;
    let uniform = w1 & heap::UNIFORM != 0;
    let first = if uniform { 2 } else { 2 + len.div_ceil(16) };
    let room = (end - v as usize) / 8;
    if first >= room {
        return;
    }
    let n = len.min(room - first);
    for i in from..n {
        let d = if uniform {
            ((w1 >> heap::DESC_SHIFT) & 15) as i64
        } else {
            ((load(v, 2 + i / 16) >> (4 * (i % 16))) & 15) as i64
        };
        // A frame built as a closure carries its reuse token as an `Int`:
        // see "Tokens" in the design.
        let follow = d == desc::REF || d == desc::ANY || (kind == heap::CLOSURE && d == desc::INT);
        if !follow {
            continue;
        }
        let x = load(v, first + i);
        if snap.mark(x) {
            gray.push(x);
        }
    }
}

/// Mark from `work` -- references to mark -- `partial` -- marked blocks whose
/// fields from a place on are still to look at -- and `gray` -- marked blocks
/// whose fields are still to look at -- until there is nothing left.
fn mark_all(snap: &Snapshot, work: Vec<Word>, partial: Vec<(Word, usize)>, gray: &mut Vec<Word>) {
    for (v, from) in partial {
        scan_from(snap, v, from, gray);
    }
    for w in work {
        if snap.mark(w) {
            gray.push(w);
        }
    }
    while let Some(v) = gray.pop() {
        scan(snap, v, gray);
    }
}

// --- the marker --------------------------------------------------------------

/// One heap's trace, as the marker and the program share it.
pub struct Mark {
    snap: Snapshot,
    inner: Mutex<Inner>,
    changed: Condvar,
}

struct Inner {
    /// References to mark: the roots, then what the program logs.
    work: Vec<Word>,
    /// Marked blocks still to look at, when the marker was stopped part-way.
    gray: Vec<Word>,
    /// Blocks waiting to be erased, marked, and the field each still holds
    /// from: theirs to look at, as the program goes on.
    partial: Vec<(Word, usize)>,
    state: Run,
    /// The heap is going, or its thread wants the trace finished itself.
    cancel: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Run {
    /// Waiting for the marker.
    Queued,
    /// The marker is at it.
    Active,
    /// The marker has done all it was given.
    Idle,
}

static QUEUE: Mutex<VecDeque<Arc<Mark>>> = Mutex::new(VecDeque::new());
static READY: Condvar = Condvar::new();

/// Hand `m` to the marker, starting it the first time.
fn enqueue(m: Arc<Mark>) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("meadow marker".into())
            .spawn(marker);
        if spawned.is_err() {
            crate::fail("the marker thread could not be started");
        }
    });
    lock(&QUEUE).push_back(m);
    READY.notify_one();
}

/// The marker: one thread for the process, taking heaps in turn. Each is
/// marked until there is nothing left to mark, and goes back in the queue
/// when its program logs more.
fn marker() {
    loop {
        let m = {
            let mut q = lock(&QUEUE);
            loop {
                if let Some(m) = q.pop_front() {
                    break m;
                }
                q = READY.wait(q).unwrap_or_else(|p| p.into_inner());
            }
        };
        run(&m);
    }
}

fn run(m: &Mark) {
    loop {
        let (work, mut gray, partial) = {
            let mut i = lock(&m.inner);
            if i.cancel || (i.work.is_empty() && i.gray.is_empty() && i.partial.is_empty()) {
                i.state = Run::Idle;
                m.changed.notify_all();
                return;
            }
            i.state = Run::Active;
            (
                std::mem::take(&mut i.work),
                std::mem::take(&mut i.gray),
                std::mem::take(&mut i.partial),
            )
        };
        for (v, from) in partial {
            scan_from(&m.snap, v, from, &mut gray);
        }
        for w in work {
            if m.snap.mark(w) {
                gray.push(w);
            }
        }
        let mut n = 0u32;
        while let Some(v) = gray.pop() {
            scan(&m.snap, v, &mut gray);
            n = n.wrapping_add(1);
            if n % 4096 == 0 && lock(&m.inner).cancel {
                let mut i = lock(&m.inner);
                i.gray.append(&mut gray);
                i.state = Run::Idle;
                m.changed.notify_all();
                return;
            }
        }
    }
}

impl Mark {
    /// Stop the marker on this heap and take what it had left: once this
    /// answers, nothing but the caller reads the snapshot's blocks.
    fn stop(&self) -> (Vec<Word>, Vec<Word>, Vec<(Word, usize)>) {
        let mut i = lock(&self.inner);
        i.cancel = true;
        while i.state == Run::Active {
            i = self.changed.wait(i).unwrap_or_else(|p| p.into_inner());
        }
        i.state = Run::Idle;
        (
            std::mem::take(&mut i.work),
            std::mem::take(&mut i.gray),
            std::mem::take(&mut i.partial),
        )
    }
}

// --- a heap's state ----------------------------------------------------------

/// What a heap keeps of its traces.
pub struct State {
    /// Mark blocks as they are taken: while marking, and until the sweep is
    /// through.
    pub(crate) black: bool,
    /// Hold what is freed rather than hand it out again: while marking.
    pub(crate) limbo_on: bool,
    pub(crate) limbo: Vec<(*mut Word, usize)>,
    /// References the program replaced while marking, not yet handed over.
    satb: Vec<Word>,
    phase: Phase,
    /// Live blocks at which the next trace starts; `0` while one is going, so
    /// that every allocation from compiled code looks in.
    at: usize,
    polls: u32,
    /// The chunk the last block made black was in: see [`blacken`].
    pub(crate) hint: usize,
    /// Words of stack the trace's roots were scanned from.
    stack_words: usize,
    /// Allocations a trace may mark through before the program's thread
    /// finishes it itself: see [`check`].
    patience: u32,
    /// When the roots were taken, for the tally.
    began: Option<std::time::Instant>,
}

enum Phase {
    Idle,
    Marking(Arc<Mark>),
    Sweeping(Box<Sweep>),
}

struct Sweep {
    mark: Arc<Mark>,
    step: Step,
    /// The garbage found so far: unmarked blocks of the snapshot.
    doomed: Vec<Word>,
    /// Of those, the reuse tokens only garbage frames hold: freed as memory,
    /// their fields long gone.
    tokens: HashSet<Word>,
    /// Garbage freed, its memory held until the sweep is through: an address
    /// one garbage block names is not handed out while another may name it.
    held: Vec<(*mut Word, usize)>,
    /// Blocks of the snapshot the trace marked: what was live when the roots
    /// were taken.
    kept: usize,
}

enum Step {
    /// Looking through chunk `chunk`'s block starts from bitmap word `word`,
    /// then the large blocks from `large`.
    Find {
        chunk: usize,
        word: usize,
        large: usize,
    },
    /// Freeing `doomed` from `next`.
    Free { next: usize },
}

impl State {
    pub fn new() -> State {
        State {
            black: false,
            limbo_on: false,
            limbo: Vec::new(),
            satb: Vec::new(),
            phase: Phase::Idle,
            at: FLOOR,
            polls: 0,
            hint: 0,
            stack_words: 0,
            patience: 0,
            began: None,
        }
    }

    fn marking(&self) -> Option<&Arc<Mark>> {
        match &self.phase {
            Phase::Marking(m) => Some(m),
            _ => None,
        }
    }
}

impl Default for State {
    fn default() -> State {
        State::new()
    }
}

// --- what the program does ---------------------------------------------------

/// The block at `at` was poisoned (`MEADOW_SILO_POISON`): no block begins
/// there for any trace from now on.
pub(crate) fn unstart(chunks: &[Arc<Chunk>], at: usize) {
    let i = chunks.partition_point(|c| c.start <= at);
    if let Some(c) = i.checked_sub(1).and_then(|i| chunks.get(i))
        && at < c.end()
    {
        c.clear_start(at);
    }
}

thread_local! {
    /// Whether this thread is freeing what a sweep found: what
    /// `MEADOW_SILO_POISON=collector` asks of a block being freed.
    static SWEEPING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Is this thread freeing what a trace found to be garbage?
pub fn sweeping() -> bool {
    SWEEPING.with(std::cell::Cell::get)
}

/// Mark the block at `p`, which was just taken, if it is in a chunk of the
/// trace in progress: new blocks are black. `hint` is the chunk the last
/// block was in, which the next mostly is.
pub(crate) fn blacken(chunks: &[Arc<Chunk>], hint: &mut usize, p: Word) {
    let at = p as usize;
    let near = chunks.get(*hint).filter(|c| c.start <= at && at < c.end());
    let c = match near {
        Some(c) => c,
        None => {
            let Some(i) = chunks.partition_point(|c| c.start <= at).checked_sub(1) else {
                return;
            };
            if at >= chunks[i].end() {
                return;
            }
            *hint = i;
            &chunks[i]
        }
    };
    if c.traced() {
        c.mark(at);
    }
}

/// `old` is about to be overwritten in a block that already exists: log it,
/// if this heap is marking. The emitted barrier calls this once
/// [`meadow_marking`] says some heap is.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_satb(c: *mut Ctx, old: Word) {
    if heap::is_block(old) {
        log_in(crate::ctx::given(c), old);
    }
}

/// The dead block `token` is about to be rebuilt in place: log what its
/// fields still say, and mark it, since it is a new block now.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_satb_reuse(c: *mut Ctx, token: Word) {
    let c = crate::ctx::given(c);
    // Safety: the running thread's heap, borrowed for this alone.
    let h = unsafe { &mut (*c).heap };
    if h.trace.marking().is_none() || !heap::is_block(token) {
        return;
    }
    let kind = heap::kind(token);
    if matches!(
        kind,
        heap::DATA | heap::CLOSURE | heap::ARRAY | heap::CELL | heap::RECORD | heap::MUT_ARRAY
    ) {
        for i in 0..heap::len(token).min(heap::CLASSES) {
            let d = heap::field_desc(token, i);
            if d == desc::REF || d == desc::ANY || (kind == heap::CLOSURE && d == desc::INT) {
                let x = heap::field(token, i);
                if heap::is_block(x) {
                    h.trace.satb.push(x);
                }
            }
        }
    }
    blacken(&h.chunk_meta, &mut h.trace.hint, token);
}

/// The runtime's own barrier: `old` is about to be overwritten in a block
/// that already exists.
#[inline]
pub(crate) fn log(old: Word) {
    if crate::cycles::tracing()
        && meadow_marking.load(Ordering::Relaxed) != 0
        && heap::is_block(old)
    {
        log_in(crate::ctx::get(), old);
    }
}

fn log_in(c: *mut Ctx, old: Word) {
    // Safety: the running thread's heap, borrowed for this alone.
    let h = unsafe { &mut (*c).heap };
    if h.trace.marking().is_some() {
        h.trace.satb.push(old);
    }
}

/// Before an allocation from compiled code: start a trace if one is due, or
/// carry the one going on. A program that cannot make a cycle never calls
/// this, and one that can pays a compare until a trace is due.
#[inline]
pub fn poll(c: *mut Ctx) {
    // Safety: the running thread's heap.
    let h = unsafe { &(*c).heap };
    if h.live_blocks() >= h.trace.at {
        advance(c);
    }
}

#[cold]
fn advance(c: *mut Ctx) {
    // Safety: the running thread's heap, borrowed in turn below.
    let phase = match unsafe { &(*c).heap.trace.phase } {
        Phase::Idle => 0,
        Phase::Marking(_) => 1,
        Phase::Sweeping(_) => 2,
    };
    match phase {
        0 => {
            // No block that a cycle could run through is in use: there is no
            // cycle to find, however many blocks there are.
            let h = unsafe { &mut (*c).heap };
            if h.mutables == 0 {
                h.trace.at = (2 * h.live_blocks()).max(FLOOR);
                return;
            }
            start(c, false)
        }
        1 => {
            let t = unsafe { &mut (*c).heap.trace };
            t.polls = t.polls.wrapping_add(1);
            if t.satb.len() >= SATB_BATCH || t.polls % CHECK_EVERY == 0 {
                check(c);
            }
        }
        _ => {
            sweep(c, SLICE_WORDS, SLICE_BLOCKS);
        }
    }
}

// --- the roots ---------------------------------------------------------------

/// Take the roots and start marking: the one pause. `now` marks here, on this
/// thread, and sweeps, rather than handing the trace to the marker -- what a
/// collection the program waits for does, which is asked for where the
/// program's frames hold nothing of the heap's (a thread's end, the exit), and
/// so takes no roots from the stacks: a stale word there would only keep a
/// dead cycle alive past the end.
fn start(c: *mut Ctx, now: bool) {
    let began = std::time::Instant::now();
    // Safety: the running thread's heap and context, borrowed in turn.
    let h = unsafe { &mut (*c).heap };
    for ch in &h.chunk_meta {
        ch.traced.store(true, Ordering::Release);
    }
    let mut large: Vec<(usize, usize)> = h.large.iter().map(|(a, w)| (*a, *w)).collect();
    large.sort_unstable();
    let snap = Snapshot::new(h.chunk_meta.clone(), large);
    let mut work = Vec::new();
    h.trace.stack_words = 0;
    let t_stacks = std::time::Instant::now();
    let scanned = stopped(|sp| stacks(c, sp, &snap, &mut work, !now));
    let stack_nanos = t_stacks.elapsed().as_nanos() as u64;
    if !scanned {
        // Somewhere the roots cannot all be found: try again later, rather
        // than free what a frame holds.
        for ch in &snap.chunks {
            ch.traced.store(false, Ordering::Release);
        }
        let h = unsafe { &mut (*c).heap };
        h.trace.at = (2 * h.live_blocks()).max(FLOOR);
        tally(|t| t.put_off += 1);
        return;
    }
    // Safety: as above.
    let cx = unsafe { &*c };
    for (v, d) in cx.globals.iter().flatten() {
        if *d == desc::REF {
            work.push(*v);
        }
    }
    work.extend(cx.literals.values().copied());
    work.extend(cx.exits.iter().map(|(_, f)| *f));
    work.extend(
        cx.kept
            .iter()
            .filter(|(_, d)| *d == desc::REF)
            .map(|(v, _)| *v),
    );
    if let Some(t) = &cx.txn {
        for level in &t.writes {
            work.extend(
                level
                    .iter()
                    .filter(|(_, _, d)| *d == desc::REF)
                    .map(|(_, v, _)| *v),
            );
        }
    }
    // The spill area holds the arguments of a call in transit: read as a
    // stack is.
    work.extend(cx.spill.iter().copied().filter(|w| heap::is_block(*w)));
    let h = unsafe { &mut (*c).heap };
    // What is waiting to be erased is dead, and counting frees it; but it
    // still holds what it has not given up yet, which must not be freed
    // under it. Marked here, and its remaining fields left to the marker: a
    // dead block is only ever erased, never written, and one may be an
    // array of millions.
    let t_pending = std::time::Instant::now();
    let pending = h.pending.len() as u64;
    let mut partial = Vec::with_capacity(h.pending.len());
    for &(p, from) in &h.pending {
        let v = p as Word;
        snap.mark(v);
        partial.push((v, from));
    }
    let pending_nanos = t_pending.elapsed().as_nanos() as u64;
    tally(|t| {
        t.stack_nanos = t.stack_nanos.max(stack_nanos);
        t.pending_nanos = t.pending_nanos.max(pending_nanos);
        t.pending_most = t.pending_most.max(pending);
    });
    let m = Arc::new(Mark {
        snap,
        inner: Mutex::new(Inner {
            work: Vec::new(),
            gray: Vec::new(),
            partial: Vec::new(),
            state: if now { Run::Idle } else { Run::Queued },
            cancel: false,
        }),
        changed: Condvar::new(),
    });
    h.trace.phase = Phase::Marking(m.clone());
    h.trace.black = true;
    h.trace.limbo_on = true;
    h.trace.at = 0;
    h.trace.began = Some(began);
    h.trace.polls = 0;
    h.trace.patience = h.live_blocks().max(FLOOR).min(u32::MAX as usize) as u32;
    meadow_marking.fetch_add(1, Ordering::SeqCst);
    tally(|t| {
        t.traces += 1;
        let took = began.elapsed().as_nanos() as u64;
        t.pause_nanos += took;
        t.longest = t.longest.max(took);
    });
    if now {
        let mut gray = Vec::new();
        mark_all(&m.snap, work, partial, &mut gray);
        finish(c);
    } else {
        {
            let mut i = lock(&m.inner);
            i.work = work;
            i.partial = partial;
        }
        enqueue(m);
    }
}

/// Scan every stack the thread has, from where each stopped -- the innermost
/// from `sp` -- for references into the heap: whether they could all be
/// found. Without `running`, the stacks of what is running are left out --
/// what a collection the program waits for asks -- but not those of the
/// continuations and the parked segments, whose frames still hold what they
/// will give back.
fn stacks(c: *mut Ctx, sp: usize, snap: &Snapshot, work: &mut Vec<Word>, running: bool) -> bool {
    // Safety: the running thread's context.
    let cx = unsafe { &*c };
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    if running {
        match (cx.levels.last(), cx.os_level) {
            (Some(l), _) => ranges.push((sp, l.base)),
            (None, Some(o)) => ranges.push((sp, o.base)),
            (None, None) => return false,
        }
        let outer = cx.levels.len().saturating_sub(1);
        ranges.extend(cx.levels[..outer].iter().map(|l| (l.stopped, l.base)));
        if !cx.levels.is_empty()
            && let Some(o) = cx.os_level
        {
            ranges.push((o.stopped, o.base));
        }
    }
    for p in &cx.parked {
        ranges.push((p.level.stopped, p.level.base));
    }
    for s in cx.suspended.iter().flatten() {
        for p in std::iter::once(&s.top).chain(&s.nested) {
            ranges.push((p.level.stopped, p.level.base));
        }
    }
    for (lo, hi) in ranges {
        if lo == 0 || hi <= lo || hi - lo > 1 << 34 {
            return false;
        }
        let mut at = (lo + 7) & !7;
        // Safety: the running thread's heap, for this count alone.
        unsafe { (*c).heap.trace.stack_words += (hi - at) / 8 };
        while at + 8 <= hi {
            // Safety: a word of a stack this thread runs on, between where it
            // stopped and where it begins: in use, and mapped.
            let w = unsafe { std::ptr::read_volatile(at as *const Word) };
            if (w as usize) >= snap.lo
                && (w as usize) < snap.hi
                && let Some(v) = snap.containing(w)
            {
                work.push(v);
            }
            at += 8;
        }
    }
    true
}

// --- the end of marking --------------------------------------------------------

/// Hand the marker what the program has logged; or, once the marker is
/// through with all it was given, mark what is logged here and end the
/// marking.
///
/// Waiting instead for a moment when nothing was logged would wait forever
/// on a program that keeps rewriting its `Ref`s: there is always something
/// logged since the last look. What the program logged since the marker last
/// took its share is a batch at most, and what it reaches that is not marked
/// yet is all that is left -- the short last step of every snapshot trace.
fn check(c: *mut Ctx) {
    // Safety: the running thread's heap.
    let h = unsafe { &mut (*c).heap };
    let Some(m) = h.trace.marking().cloned() else {
        return;
    };
    let mut logged = std::mem::take(&mut h.trace.satb);
    if h.trace.polls >= h.trace.patience {
        // A heap's worth allocated, and the marker still not through -- it is
        // behind, or another heap has it: this thread finishes the marking,
        // so that a trace ends however busy the marker is, and what it holds
        // back meanwhile is bounded.
        let (mut work, mut gray, partial) = m.stop();
        work.append(&mut logged);
        mark_all(&m.snap, work, partial, &mut gray);
        finish(c);
        tally(|t| t.helped += 1);
        return;
    }
    let mut i = lock(&m.inner);
    if i.state == Run::Idle && i.work.is_empty() && i.gray.is_empty() && i.partial.is_empty() {
        // Taken from the marker: only this thread queues it, and this
        // thread is here.
        i.state = Run::Active;
        drop(i);
        let mut gray = Vec::new();
        mark_all(&m.snap, logged, Vec::new(), &mut gray);
        lock(&m.inner).state = Run::Idle;
        finish(c);
        return;
    }
    if !logged.is_empty() {
        i.work.extend(logged);
        if i.state == Run::Idle {
            i.state = Run::Queued;
            drop(i);
            enqueue(m);
        }
    }
}

/// Marking is over: hand out again what was held, and start the sweep.
fn finish(c: *mut Ctx) {
    // Safety: the running thread's heap.
    let h = unsafe { &mut (*c).heap };
    let Phase::Marking(m) = std::mem::replace(&mut h.trace.phase, Phase::Idle) else {
        return;
    };
    meadow_marking.fetch_sub(1, Ordering::SeqCst);
    if let Some(b) = h.trace.began {
        tally(|t| t.mark_nanos += b.elapsed().as_nanos() as u64);
    }
    h.trace.limbo_on = false;
    // A large block goes back to the system, and its address may come back as
    // another: held until the sweep is through, so that the sweep never takes
    // a new one for the old.
    let mut held = Vec::new();
    for (p, words) in std::mem::take(&mut h.trace.limbo) {
        if words >= heap::CLASSES {
            held.push((p, words));
        } else {
            h.clean_block(p, words);
        }
    }
    h.trace.phase = Phase::Sweeping(Box::new(Sweep {
        mark: m,
        step: Step::Find {
            chunk: 0,
            word: 0,
            large: 0,
        },
        doomed: Vec::new(),
        tokens: HashSet::new(),
        held,
        kept: 0,
    }));
}

// --- the sweep ---------------------------------------------------------------

/// A slice of the sweep: looking through up to `words` words of block starts,
/// or freeing up to `blocks` garbage blocks. Answers whether it is through.
fn sweep(c: *mut Ctx, words: usize, blocks: usize) -> bool {
    // Safety: the running thread's heap; what is freed below is reached
    // through it afresh, never while this borrow is held.
    let h = unsafe { &mut (*c).heap };
    let Phase::Sweeping(s) = &mut h.trace.phase else {
        return true;
    };
    match s.step {
        Step::Find {
            mut chunk,
            mut word,
            mut large,
        } => {
            let snap = &s.mark.snap;
            let mut left = words;
            while chunk < snap.chunks.len() && left > 0 {
                let ch = &snap.chunks[chunk];
                if word >= ch.starts.len() {
                    chunk += 1;
                    word = 0;
                    continue;
                }
                let mut bits = ch.starts[word].load(Ordering::Acquire);
                while bits != 0 {
                    let b = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let v = (ch.start + 8 * (64 * word + b)) as Word;
                    if heap::is_free(v) {
                        continue;
                    }
                    if ch.marked(v as usize) {
                        s.kept += 1;
                        continue;
                    }
                    s.doomed.push(v);
                    if heap::kind(v) == heap::CLOSURE {
                        for i in 0..heap::len(v) {
                            let x = heap::field(v, i);
                            if heap::field_desc(v, i) == desc::INT && snap.unmarked(x) {
                                s.tokens.insert(x);
                            }
                        }
                    }
                }
                word += 1;
                left -= 1;
            }
            if chunk >= snap.chunks.len() {
                while large < snap.large.len() && left > 0 {
                    let (at, _) = snap.large[large];
                    if snap.large_marks[large].load(Ordering::Acquire) {
                        s.kept += 1;
                    } else if h.large.contains_key(&at) {
                        s.doomed.push(at as Word);
                    }
                    large += 1;
                    left -= 1;
                }
            }
            s.step = if chunk >= snap.chunks.len() && large >= snap.large.len() {
                Step::Free { next: 0 }
            } else {
                Step::Find { chunk, word, large }
            };
            false
        }
        Step::Free { next } => {
            // Saturating: a sweep finished at once asks for `usize::MAX`
            // blocks, and from part-way through that would wrap to before
            // `next`.
            let to = next.saturating_add(blocks).min(s.doomed.len());
            let batch: Vec<Word> = s.doomed[next..to].to_vec();
            let through = to == s.doomed.len();
            s.step = Step::Free { next: to };
            let mark = s.mark.clone();
            for v in batch {
                free_garbage(c, &mark.snap, v);
            }
            if through {
                end_sweep(c);
                return true;
            }
            false
        }
    }
}

/// Free the garbage block `v`: let go of what it holds outside the heap, give
/// up its references to what is live -- the rest of its garbage goes with it
/// in this sweep -- and hold its memory until the sweep is through.
fn free_garbage(c: *mut Ctx, snap: &Snapshot, v: Word) {
    // Safety: the running thread's heap, borrowed for this test alone.
    let token = unsafe {
        match &(*c).heap.trace.phase {
            Phase::Sweeping(s) => s.tokens.contains(&v),
            _ => false,
        }
    };
    let words = heap::size(v);
    if !token {
        heap::finalize(v);
        let kind = heap::kind(v);
        if kind != heap::STRING && kind != heap::BIGINT {
            for i in 0..heap::len(v) {
                let x = heap::field(v, i);
                if heap::field_desc(v, i) == desc::REF && heap::is_block(x) && !snap.unmarked(x) {
                    heap::erase(x, desc::REF);
                }
            }
        }
    }
    // Safety: as above.
    unsafe {
        if let Phase::Sweeping(s) = &mut (*c).heap.trace.phase {
            s.held.push((v as *mut Word, words));
        }
    }
    tally(|t| t.freed += 1);
}

/// The sweep is through: the garbage's memory goes back, the marks are
/// cleared for the next trace, and when that is due is worked out.
fn end_sweep(c: *mut Ctx) {
    // Safety: the running thread's heap.
    let h = unsafe { &mut (*c).heap };
    let Phase::Sweeping(s) = std::mem::replace(&mut h.trace.phase, Phase::Idle) else {
        return;
    };
    let s = *s;
    for ch in &s.mark.snap.chunks {
        ch.clear_marks();
    }
    h.trace.black = false;
    SWEEPING.with(|w| w.set(true));
    for (p, words) in s.held {
        h.clean_block(p, words);
    }
    SWEEPING.with(|w| w.set(false));
    // The next trace once the heap has grown by as much again as this one
    // found live -- and by an eighth of the stack it scanned, so that a
    // program deep in a recursion does not pay for its stack at every few
    // thousand blocks. Not twice what is in use now: that counts as live the
    // garbage made while this trace ran, which only the next can free, and
    // every trace would wait longer than the last.
    let work = s.kept + h.trace.stack_words / 8;
    h.trace.at = h.live_blocks() + work.max(FLOOR);
    if let Some(b) = h.trace.began.take() {
        tally(|t| t.nanos += b.elapsed().as_nanos() as u64);
    }
    tally(|t| t.sweep_nanos = t.nanos.saturating_sub(t.mark_nanos));
}

// --- collections the program waits for ----------------------------------------

/// Free every garbage cycle there is now, here: the trace going on finished
/// on this thread, and then one whole trace more. What the leak check and a
/// thread's end ask for.
pub fn collect_now(c: *mut Ctx) {
    if c.is_null() || !crate::cycles::tracing() {
        return;
    }
    finish_here(c);
    start(c, true);
    finish_here(c);
}

/// Bring the trace going on, if any, to its end on this thread.
fn finish_here(c: *mut Ctx) {
    loop {
        // Safety: the running thread's heap.
        let h = unsafe { &mut (*c).heap };
        match &h.trace.phase {
            Phase::Idle => return,
            Phase::Marking(m) => {
                let m = m.clone();
                let (mut work, mut gray, partial) = m.stop();
                work.append(&mut h.trace.satb);
                mark_all(&m.snap, work, partial, &mut gray);
                finish(c);
            }
            Phase::Sweeping(_) => while !sweep(c, usize::MAX, usize::MAX) {},
        }
    }
}

/// The heap `h` is going: stop the marker reading it.
pub(crate) fn forget(h: &mut heap::Heap) {
    if let Phase::Marking(m) = std::mem::replace(&mut h.trace.phase, Phase::Idle) {
        m.stop();
        meadow_marking.fetch_sub(1, Ordering::SeqCst);
    }
    h.trace.limbo_on = false;
}

// --- the tally ---------------------------------------------------------------

/// What tracing has done in this process, for `MEADOW_SILO_CYCLES`.
#[derive(Default, Clone, Copy)]
pub struct Tally {
    pub traces: u64,
    pub freed: u64,
    /// The pauses that took the roots, in total and the longest.
    pub pause_nanos: u64,
    pub longest: u64,
    /// From each trace's roots to the end of its sweep.
    pub nanos: u64,
    /// Traces whose marking the program's own thread finished.
    pub helped: u64,
    /// Traces put off because a stack's roots could not all be found.
    pub put_off: u64,
    /// From the roots to the end of marking, and from there to swept.
    pub mark_nanos: u64,
    pub sweep_nanos: u64,
    /// The longest any pause spent on the stacks, and on what waits to be
    /// erased; and the most that waited.
    pub stack_nanos: u64,
    pub pending_nanos: u64,
    pub pending_most: u64,
}

static TALLY: Mutex<Tally> = Mutex::new(Tally {
    traces: 0,
    freed: 0,
    pause_nanos: 0,
    longest: 0,
    nanos: 0,
    helped: 0,
    put_off: 0,
    mark_nanos: 0,
    sweep_nanos: 0,
    stack_nanos: 0,
    pending_nanos: 0,
    pending_most: 0,
});

fn tally(f: impl FnOnce(&mut Tally)) {
    if crate::cycles::counting() {
        f(&mut lock(&TALLY));
    }
}

/// What tracing has done, so far.
pub fn tallied() -> Tally {
    *lock(&TALLY)
}

/// Say what tracing did, under `MEADOW_SILO_CYCLES`.
pub fn report() {
    if !crate::cycles::counting() || !crate::cycles::tracing() {
        return;
    }
    let t = tallied();
    if t.traces == 0 {
        eprintln!("aot: no trace: no heap grew enough to want one");
        return;
    }
    eprintln!(
        "aot: {} traces, {} blocks freed by tracing",
        t.traces, t.freed
    );
    eprintln!(
        "aot: {:.3} ms taking roots, longest pause {:.3} ms; {:.3} ms from roots to swept",
        t.pause_nanos as f64 / 1e6,
        t.longest as f64 / 1e6,
        t.nanos as f64 / 1e6
    );
    eprintln!(
        "aot: of that, {:.3} ms marking and {:.3} ms sweeping",
        t.mark_nanos as f64 / 1e6,
        t.sweep_nanos as f64 / 1e6
    );
    eprintln!(
        "aot: the longest on stacks {:.3} ms, on what waits to be erased {:.3} ms ({} at most)",
        t.stack_nanos as f64 / 1e6,
        t.pending_nanos as f64 / 1e6,
        t.pending_most
    );
    if t.put_off > 0 {
        eprintln!(
            "aot: {} traces put off: a stack's roots could not all be found",
            t.put_off
        );
    }
    if t.helped > 0 {
        eprintln!(
            "aot: {} traces finished marking on the program's own thread",
            t.helped
        );
    }
}
