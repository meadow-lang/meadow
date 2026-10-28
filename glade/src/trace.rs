//! **Tracing**: blocks kept alive by being reached rather than by
//! being counted (`docs/RUNTIME-MERGE.md`, "Tracing on the LLVM path").
//!
//! A program built with `--gc tracing` is emitted with no counting at all
//! (`meadow_llvm::Collect::Trace`): nothing is shared or erased, no block is
//! reused in place, and a block that nobody else holds is never told from one
//! that somebody does, so nothing is written in place on the strength of it.
//! Its memory is collected here instead, by marking what the program can
//! still reach and sweeping the rest.
//!
//! # The blocks
//!
//! The same blocks as counting's (`crate::heap`), with the same header. Its
//! mark (`heap::mark`) holds the number of the collection that last marked
//! the block, so marking needs no clearing pass and a block is live in this
//! collection when its number is this one's. Under Silo's layout the mark is
//! the count, which nothing reads under tracing; Glade's has a field of its
//! own. A block inside a compact region has a mark far above any
//! collection's number (`heap::REGION_FLOOR`), and so is told from a heap's
//! block by the same test counting uses.
//!
//! Blocks come from pages of one size class each, so that a sweep can walk a
//! page from end to end; a free block has a zero kind's word, which no block
//! in use has, and its link word (`LAYOUT.link_word`) links it into its
//! class's free list. A block too big for a class is allocated alone.
//!
//! # Where a collection happens
//!
//! At a safe point: where a function of the program begins, and a collection
//! is due (`meadow_gc_due`). There, everything the program holds is somewhere
//! the collector can see:
//!
//! - in a frame's record on the running segment's chain (`crate::shadow`),
//!   linked by every call that waits for an answer, and by the safe point
//!   itself for the parameters of the function it begins;
//! - in the chains of the segments running further out, which each `drive`
//!   keeps in [`Ctx::outer_chains`] while the segment it runs is in;
//! - in the context: its top-level values, its string literals, what it keeps
//!   for the runtime, and a transaction's writes;
//! - or in a block one of those reaches. A suspended continuation is a block
//!   (`STACK`), and marking it marks what its segments' chains hold; one that
//!   nothing reaches is discarded, as counting discards it.
//!
//! The runtime's own code never collects: allocating there only says that a
//! collection is due. So it may hold a block in a local across anything but
//! a call into the program, and it never makes one of those holding a value
//! it has not put on a chain (`crate::shadow`, "the runtime links records of
//! its own").
//!
//! Nothing moves, so nothing anywhere is read back after a collection, and
//! no store needs a barrier -- unless the young generation is there.
//!
//! # The young generation
//!
//! Under `--gc generational` ([`moving`]), a small block is made in a nursery, a
//! region of the heap's own taken from the front, and most die there and are
//! never looked at. When it fills, the next safe point copies what is still
//! reached out of it ([`minor`]) to where every other block lives, and the
//! nursery is used again from the front.
//!
//! What reaches the nursery is the roots above, and the older blocks a young
//! one was stored into since -- which a store of a reference into a block
//! notes ([`remember`], the write barrier), and which a block made outside
//! the nursery, when it is full, is counted among. Every root is somewhere
//! the copy can rewrite: a record on a chain, the context, an older block's
//! field. The engine reads back what its records held once the safe point,
//! or the call it waited on, is over.
//!
//! A young block that moved says so in its header (`heap::set_forwarded`):
//! Silo's layout marks it in the count, which nothing else reads under
//! tracing, and Glade's in its kind, as Glade does. It keeps where it went in
//! another header word, since its copy has its header whole. A walk over the nursery then
//! steps over a block that moved by its copy's size, and finds the young
//! blocks that died still whole, letting go of what one of them held outside
//! the heap -- a segment, a thread's task, a region.
//!
//! # Compact regions
//!
//! Counting keeps a region alive by counting the pointers into it. Here a
//! collection notes every region a pointer it finds is into, and the heap
//! holds one reference to each region its last collection found -- taken
//! before the sweep lets any `Compact` go, so a region read out of a compact
//! that is itself garbage outlives it, as it must.

use crate::ctx::Ctx;
use crate::heap::{self, Word};
use meadow_rt::desc;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

unsafe extern "C" {
    /// Whether the program's memory is traced: the emitted module says, as it
    /// says whether it spawns. See `meadow_llvm::emit::Collect`.
    static meadow_gc: u8;
}

/// Whether memory is traced rather than counted.
#[inline(always)]
pub fn on() -> bool {
    // Safety: a constant the emitted module defines.
    unsafe { meadow_gc != 0 }
}

/// Whether some heap wants collecting: what every safe point of a traced
/// program reads, so a byte the emitted code loads and compares.
#[unsafe(no_mangle)]
pub static meadow_gc_due: AtomicU8 = AtomicU8::new(0);

/// How many heaps want collecting: the flag is set while any does.
static DUE: AtomicUsize = AtomicUsize::new(0);

/// Word counts with a size class of their own: `crate::heap`'s.
const CLASSES: usize = 128;
/// Words in a page of small blocks, at least.
const PAGE_WORDS: usize = 1 << 13;
/// The least a heap allocates between collections, in words: below it a
/// collection would cost more than the memory it gives back.
const LEAST: usize = 1 << 20;
/// How much a heap allocates between collections, for each word the last one
/// kept. With no generations, every collection marks everything live: at
/// twice, a heap grows to three times what it keeps before it is marked
/// again, and a program building something large pays for marking it a few
/// times over rather than once for each of its size it allocates.
const GROWTH: usize = 2;
/// A collection's number never reaches this, so it is never mistaken for a
/// region block's mark (`heap::REGION_FLOOR` is above it), and it fits the
/// layout's mark.
const EPOCHS: u32 = heap::LAYOUT.epochs() as u32;

/// The word of a free block that links it to the next.
const LINK: usize = heap::LAYOUT.link_word;

/// One heap's traced memory.
pub struct Space {
    /// Each class's pages: where each starts, and how many blocks it has.
    pages: Vec<Vec<(*mut Word, usize)>>,
    /// Each class's free blocks, linked through their first words.
    free: Vec<*mut Word>,
    /// Each class's page being handed out from, and the blocks it has left.
    fresh: Vec<(*mut Word, usize)>,
    /// Blocks too big for a class, by address, and their words.
    large: HashMap<usize, usize>,
    /// The number of the collection marking now, or that last did.
    epoch: u32,
    /// Words allocated since the last collection.
    since: usize,
    /// Words to allocate before the next is due.
    budget: usize,
    due: bool,
    /// The regions this heap holds a reference into: those its last
    /// collection found a pointer into.
    regions: HashSet<usize>,
    /// For `MEADOW_SILO_MEMORY`: words in pages and large blocks.
    words: usize,
    /// Collections run, for a report.
    pub collections: u64,
    /// The young generation, where blocks move: see the module docs.
    nursery: Option<Nursery>,
}

/// A heap's young generation: where small blocks are made, from the front,
/// while there is room.
struct Nursery {
    start: *mut Word,
    end: *mut Word,
    top: *mut Word,
    /// Whether it filled, and a collection of it is due.
    full: bool,
    /// The older blocks that may hold a young one: stored into, or made
    /// while the nursery was full.
    remembered: Vec<Word>,
}

/// Words in a nursery: `MEADOW_GC_NURSERY` says, as Glade's does.
fn nursery_words() -> usize {
    static WORDS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *WORDS.get_or_init(|| {
        std::env::var("MEADOW_GC_NURSERY")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .map_or(1 << 18, |w| w.clamp(1 << 15, 1 << 27))
    })
}

impl Nursery {
    fn new() -> Nursery {
        let words = nursery_words();
        let layout = std::alloc::Layout::array::<Word>(words).expect("a nursery fits memory");
        // Safety: a non-zero size; zeroed, as a block's second word is until
        // its header is written.
        let start = unsafe { std::alloc::alloc_zeroed(layout) } as *mut Word;
        if start.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        crate::memory::HEAPS.fetch_add(words, Ordering::Relaxed);
        Nursery {
            start,
            // Safety: one past its end.
            end: unsafe { start.add(words) },
            top: start,
            full: false,
            remembered: Vec::new(),
        }
    }

    /// `words` from the front, if there is room.
    #[inline]
    fn bump(&mut self, words: usize) -> Option<*mut Word> {
        let left = (self.end as usize - self.top as usize) / std::mem::size_of::<Word>();
        if words > left {
            return None;
        }
        let p = self.top;
        // Safety: within the nursery, as `left` says.
        self.top = unsafe { p.add(words) };
        Some(p)
    }

    #[inline]
    fn holds(&self, v: Word) -> bool {
        v >= self.start as Word && v < self.end as Word
    }
}

impl Drop for Nursery {
    fn drop(&mut self) {
        let words = (self.end as usize - self.start as usize) / std::mem::size_of::<Word>();
        let layout = std::alloc::Layout::array::<Word>(words).expect("a nursery fits memory");
        // Safety: allocated with this layout in `new`.
        unsafe { std::alloc::dealloc(self.start as *mut u8, layout) };
        crate::memory::HEAPS.fetch_sub(words, Ordering::Relaxed);
    }
}

/// Whether blocks move: a young generation, under an engine that holds every
/// reference where the collector can rewrite it. The emitted module, or the
/// interpreter, says, as it says whether memory is traced.
#[inline(always)]
pub fn moving() -> bool {
    // Safety: a constant the emitted module defines.
    unsafe { meadow_gc == 2 }
}

/// [`remember`], for emitted code: what a store into a block written inline
/// calls, where blocks move.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_remember(into: Word, x: Word, d: i64) {
    remember(into, x, d);
}

/// The write barrier: a reference `x`, described by `d`, is being stored in
/// the block at `into`, which may be older than it. An older block that may
/// now hold a young one is remembered for the next collection of the
/// nursery.
#[inline]
pub fn remember(into: Word, x: Word, d: i64) {
    if !moving() || (d != desc::REF && d != desc::ANY) || !heap::is_block(x) {
        return;
    }
    // Safety: the running thread's context.
    let cx = unsafe { &mut *crate::ctx::get() };
    if let Some(n) = cx.heap.space_mut().and_then(|s| s.nursery.as_mut())
        && n.holds(x)
        && !n.holds(into)
    {
        n.remembered.push(into);
    }
}

impl Space {
    pub fn new() -> Space {
        Space {
            pages: (0..CLASSES).map(|_| Vec::new()).collect(),
            free: vec![std::ptr::null_mut(); CLASSES],
            fresh: vec![(std::ptr::null_mut(), 0); CLASSES],
            large: HashMap::new(),
            epoch: 1,
            since: 0,
            budget: LEAST,
            due: false,
            regions: HashSet::new(),
            words: 0,
            collections: 0,
            nursery: moving().then(Nursery::new),
        }
    }

    /// A block of `words` words, its second word zero until the caller writes
    /// its header -- which it does before the program reaches a safe point.
    pub fn take(&mut self, words: usize) -> Word {
        if let Some(n) = self.nursery.as_mut()
            && words < CLASSES
        {
            if let Some(p) = n.bump(words) {
                return p as Word;
            }
            if !n.full {
                n.full = true;
                self.want();
            }
        }
        let v = self.take_old(words);
        // Made outside the nursery, it may be given young fields as it is
        // built: see the module docs.
        if let Some(n) = self.nursery.as_mut() {
            n.remembered.push(v);
        }
        v
    }

    /// Say a collection of this heap is due.
    fn want(&mut self) {
        if !self.due {
            self.due = true;
            DUE.fetch_add(1, Ordering::Relaxed);
            meadow_gc_due.store(1, Ordering::Relaxed);
        }
    }

    /// A block of `words` where the older blocks live: what a block outside
    /// the nursery is made in, and what one copied out of it is copied to.
    fn take_old(&mut self, words: usize) -> Word {
        self.since += words;
        if self.since >= self.budget {
            self.want();
        }
        if words >= CLASSES {
            let layout = std::alloc::Layout::array::<Word>(words).expect("a block fits memory");
            // Safety: a non-zero size.
            let p = unsafe { std::alloc::alloc_zeroed(layout) } as *mut Word;
            if p.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            self.large.insert(p as usize, words);
            self.grew(words);
            return p as Word;
        }
        let head = self.free[words];
        if !head.is_null() {
            // Safety: a free block of this class, whose link word is the
            // next.
            self.free[words] = unsafe { *head.add(LINK) } as *mut Word;
            // Safety: likewise; its kind's word is already zero.
            unsafe { *head.add(LINK) = 0 };
            return head as Word;
        }
        let (at, left) = self.fresh[words];
        if left > 0 {
            // Safety: inside the page being handed out.
            self.fresh[words] = (unsafe { at.add(words) }, left - 1);
            return at as Word;
        }
        let blocks = (PAGE_WORDS / words).max(8);
        let n = blocks * words;
        let layout = std::alloc::Layout::array::<Word>(n).expect("a page fits memory");
        // Safety: a non-zero size; zeroed, so every block in it reads as free.
        let p = unsafe { std::alloc::alloc_zeroed(layout) } as *mut Word;
        if p.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        self.pages[words].push((p, blocks));
        self.grew(n);
        // Safety: the page has `blocks` blocks of `words`.
        self.fresh[words] = (unsafe { p.add(words) }, blocks - 1);
        p as Word
    }

    fn grew(&mut self, words: usize) {
        self.words += words;
        crate::memory::HEAPS.fetch_add(words, Ordering::Relaxed);
    }

    pub fn due(&self) -> bool {
        self.due
    }
}

impl Default for Space {
    fn default() -> Space {
        Space::new()
    }
}

impl Drop for Space {
    fn drop(&mut self) {
        for r in self.regions.drain() {
            // Safety: one of this heap's references, given up with it.
            unsafe { crate::region::release(r as *const crate::region::Region) };
        }
        if self.due {
            settled();
        }
        for (class, pages) in self.pages.iter().enumerate() {
            for &(p, blocks) in pages {
                let layout =
                    std::alloc::Layout::array::<Word>(blocks * class).expect("a page fits memory");
                // Safety: allocated with this layout in `take`.
                unsafe { std::alloc::dealloc(p as *mut u8, layout) };
            }
        }
        for (&p, &words) in &self.large {
            let layout = std::alloc::Layout::array::<Word>(words).expect("a block fits memory");
            // Safety: likewise.
            unsafe { std::alloc::dealloc(p as *mut u8, layout) };
        }
        crate::memory::HEAPS.fetch_sub(self.words, Ordering::Relaxed);
    }
}

/// A heap that wanted collecting does no longer.
fn settled() {
    if DUE.fetch_sub(1, Ordering::Relaxed) == 1 {
        meadow_gc_due.store(0, Ordering::Relaxed);
    }
}

/// A safe point found a collection due: collect the running thread's heap, if
/// it is the one that wants it.
///
/// # Safety
///
/// Called by the emitted code at a safe point, with everything the program
/// holds on a chain: see the module docs.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_gc_poll(c: *mut Ctx) {
    let c = crate::ctx::given(c);
    // Safety: the running thread's context.
    let cx = unsafe { &mut *c };
    if cx.heap.space().is_some_and(Space::due) {
        if moving() {
            minor(cx);
        }
        // The older blocks' budget used up, by what was copied out among
        // the rest: everything is marked.
        if cx.heap.space().is_some_and(|s| s.since >= s.budget) {
            collect(cx);
        } else if let Some(space) = cx.heap.space_mut()
            && space.due
        {
            space.due = false;
            settled();
        }
    }
}

/// Whether the block of a page at `v` is free: its kind's word is zero, as a
/// block on a free list's is, and no live block's ever is.
fn free(v: Word) -> bool {
    heap::word(v, heap::LAYOUT.kind.word) == 0
}

/// Mark what `cx`'s thread can reach, and sweep the rest of its heap.
pub fn collect(cx: &mut Ctx) {
    // What is young is copied out first, so that marking sees only blocks
    // that stay where they are.
    if cx.heap.space().is_some_and(|s| s.nursery.is_some()) {
        minor(cx);
    }
    let started = std::time::Instant::now();
    let Some(space) = cx.heap.space_mut() else {
        return;
    };
    if space.epoch + 1 >= EPOCHS {
        renumber(space);
    }
    space.epoch += 1;
    let epoch = space.epoch;
    let mut m = Marker {
        epoch,
        stack: Vec::new(),
        regions: HashSet::new(),
    };
    // The roots.
    m.chain(crate::shadow::head());
    for &head in &cx.outer_chains {
        m.chain(head);
    }
    for p in &cx.parked {
        m.chain(p.shadow);
    }
    for (w, d) in cx.globals.iter().flatten() {
        m.value(*w, *d);
    }
    for w in cx.literals.values() {
        m.value(*w, desc::REF);
    }
    for (w, d) in &cx.kept {
        m.value(*w, *d);
    }
    if let Some(t) = &cx.txn {
        for level in &t.writes {
            for (_, w, d) in level {
                m.value(*w, *d);
            }
        }
    }
    // And what they reach.
    while let Some(v) = m.stack.pop() {
        m.visit(v, cx);
    }
    let Marker { regions, .. } = m;
    let Some(space) = cx.heap.space_mut() else {
        return;
    };
    // The regions found, held before any `Compact` is let go of below.
    for &r in regions.difference(&space.regions) {
        // Safety: a region a live pointer is into, so not yet freed.
        unsafe { crate::region::retain(r as *const crate::region::Region) };
    }
    let gone: Vec<usize> = space.regions.difference(&regions).copied().collect();
    space.regions = regions;
    // The sweep: what was not marked.
    let mut dying: Vec<(Word, usize)> = Vec::new();
    let mut kept_words = 0usize;
    for class in 1..CLASSES {
        for &(p, blocks) in &space.pages[class] {
            for k in 0..blocks {
                // Safety: block `k` of the page.
                let b = unsafe { p.add(k * class) };
                let v = b as Word;
                if free(v) {
                    continue;
                }
                if heap::mark(v) == epoch {
                    kept_words += class;
                } else {
                    dying.push((v, class));
                }
            }
        }
    }
    let mut large_dying = Vec::new();
    for (&p, &words) in &space.large {
        let v = p as Word;
        if heap::mark(v) == epoch {
            kept_words += words;
        } else {
            large_dying.push((v, words));
        }
    }
    // What a dying block lets go of outside the heap, as counting does when
    // it frees one (`heap::Heap::step`).
    for &(v, _) in dying.iter().chain(&large_dying) {
        finalize(v);
    }
    let Some(space) = cx.heap.space_mut() else {
        return;
    };
    for (v, class) in dying {
        let b = v as *mut Word;
        // Safety: a dead block of this class: its kind's word zero says it
        // is free, its link word links it in.
        unsafe {
            *b.add(heap::LAYOUT.kind.word) = 0;
            *b.add(LINK) = space.free[class] as Word;
        }
        space.free[class] = b;
    }
    for (v, words) in large_dying {
        space.large.remove(&(v as usize));
        let layout = std::alloc::Layout::array::<Word>(words).expect("a block fits memory");
        // Safety: allocated alone, with this layout, in `take`.
        unsafe { std::alloc::dealloc(v as *mut u8, layout) };
        space.words -= words;
        crate::memory::HEAPS.fetch_sub(words, Ordering::Relaxed);
    }
    for r in gone {
        // Safety: the reference this heap held, which nothing it reaches
        // needs any more.
        unsafe { crate::region::release(r as *const crate::region::Region) };
    }
    space.since = 0;
    space.budget = (GROWTH * kept_words).max(LEAST);
    space.collections += 1;
    crate::stats::collected(started.elapsed().as_nanos() as u64);
    if space.due {
        space.due = false;
        settled();
    }
    if std::env::var_os("MEADOW_SILO_GC").is_some() {
        eprintln!(
            "aot: gc {}: {} words live, {:.2} ms",
            space.collections,
            kept_words,
            started.elapsed().as_secs_f64() * 1e3
        );
    }
}

/// Copy what is still reached out of `cx`'s nursery, rewrite everything that
/// pointed at it, and empty it: see the module docs.
pub fn minor(cx: &mut Ctx) {
    let started = std::time::Instant::now();
    let Some(space) = cx.heap.space_mut() else {
        return;
    };
    let Some(nursery) = space.nursery.as_mut() else {
        return;
    };
    let (lo, hi, top) = (nursery.start as Word, nursery.end as Word, nursery.top);
    let remembered = std::mem::take(&mut nursery.remembered);
    let mut c = Copier {
        lo,
        hi,
        copied: 0,
        scan: Vec::new(),
        space: space as *mut Space,
    };
    // The roots, rewritten where they are.
    c.chain(crate::shadow::head());
    for &head in &cx.outer_chains {
        c.chain(head);
    }
    for p in &cx.parked {
        c.chain(p.shadow);
    }
    // Every suspended segment's frames: a continuation that nothing reaches
    // is let go of by a full collection, which this is not.
    for s in cx.suspended.iter().flatten() {
        c.chain(s.top.shadow);
        for p in &s.nested {
            c.chain(p.shadow);
        }
    }
    for (w, d) in cx.globals.iter_mut().flatten() {
        if *d == desc::REF {
            *w = c.forward(*w);
        }
    }
    for w in cx.literals.values_mut() {
        *w = c.forward(*w);
    }
    for (w, d) in cx.kept.iter_mut() {
        if *d == desc::REF {
            *w = c.forward(*w);
        }
    }
    if let Some(t) = cx.txn.as_mut() {
        for level in &mut t.writes {
            for (_, w, d) in level.iter_mut() {
                if *d == desc::REF {
                    *w = c.forward(*w);
                }
            }
        }
    }
    for v in remembered {
        c.fields(v);
    }
    // And what was copied, field by field.
    while let Some(v) = c.scan.pop() {
        c.fields(v);
    }
    let copied = c.copied;
    // The young blocks that died, still whole: what one held outside the
    // heap is let go of, as a sweep does.
    let mut at = lo;
    while at < top as Word {
        let size = if let Some(to) = heap::forwarded(at) {
            heap::size(to)
        } else {
            finalize(at);
            heap::size(at)
        };
        at += (size.max(1) * std::mem::size_of::<Word>()) as Word;
    }
    // Safety: the nursery's used front, which nothing points into now.
    unsafe {
        std::ptr::write_bytes(lo as *mut Word, 0, (top as usize - lo as usize) / 8);
    }
    let Some(space) = cx.heap.space_mut() else {
        return;
    };
    if let Some(n) = space.nursery.as_mut() {
        n.top = n.start;
        n.full = false;
    }
    crate::stats::collected(started.elapsed().as_nanos() as u64);
    if std::env::var_os("MEADOW_SILO_GC").is_some() {
        eprintln!(
            "aot: minor gc: {copied} blocks copied out, {:.2} ms",
            started.elapsed().as_secs_f64() * 1e3
        );
    }
}

/// What copies the nursery's survivors out: the nursery's bounds, how many
/// were copied, and those whose fields are still to be looked at.
struct Copier {
    lo: Word,
    hi: Word,
    copied: usize,
    scan: Vec<Word>,
    space: *mut Space,
}

impl Copier {
    #[inline]
    fn young(&self, v: Word) -> bool {
        heap::is_block(v) && v >= self.lo && v < self.hi
    }

    /// Where the block `v` is now: itself, if it is not young; its copy,
    /// made now if it has none yet.
    fn forward(&mut self, v: Word) -> Word {
        if !self.young(v) {
            return v;
        }
        if let Some(to) = heap::forwarded(v) {
            return to;
        }
        let words = heap::size(v);
        // Safety: the heap's space, which nothing else touches meanwhile.
        let to = unsafe { (*self.space).take_old(words) };
        // Safety: both blocks have `words` words.
        unsafe { std::ptr::copy_nonoverlapping(v as *const Word, to as *mut Word, words) };
        heap::set_mark(to, 0);
        heap::set_forwarded(v, to);
        self.copied += 1;
        self.scan.push(to);
        to
    }

    /// Every record from `head` down: what each holds, rewritten.
    fn chain(&mut self, head: Word) {
        let mut at = head;
        while at != 0 {
            let p = at as *mut i64;
            // Safety: a record on a segment whose stack is intact.
            let (prev, n) = unsafe { (*p, *p.add(1)) };
            for i in 0..n.max(0) as usize {
                // Safety: within the record's `n` pairs.
                unsafe {
                    let d = *p.add(3 + 2 * i);
                    if d == desc::REF || d == crate::shadow::TOKEN {
                        let slot = p.add(2 + 2 * i);
                        *slot = self.forward(*slot as Word) as i64;
                    }
                }
            }
            at = prev as Word;
        }
    }

    /// The fields of the block at `v` that are references, rewritten.
    fn fields(&mut self, v: Word) {
        if !heap::is_block(v) || heap::in_region(v) {
            return;
        }
        match heap::kind(v) {
            heap::STRING | heap::BIGINT | heap::STACK => {}
            _ => {
                let n = heap::len(v);
                let first = heap::first_field(v);
                let uniform = heap::uniform(v);
                if uniform && heap::field_desc(v, 0) != desc::REF {
                    return;
                }
                for i in 0..n {
                    if uniform || heap::field_desc(v, i) == desc::REF {
                        let x = heap::word(v, first + i);
                        if self.young(x) {
                            let to = self.forward(x);
                            heap::set_word(v, first + i, to);
                        }
                    }
                }
            }
        }
    }
}

/// What a block lets go of outside the heap when it dies.
fn finalize(v: Word) {
    match heap::kind(v) {
        // A continuation nobody can resume now: its segments go.
        heap::STACK => crate::segments::discard(heap::meta(v)),
        // One fewer that could await the thread: see `crate::sched::held`.
        heap::TASK => crate::sched::let_go(heap::meta(v)),
        heap::TVAR => crate::sched::let_go_var(heap::field(v, 0) as usize),
        // Its region loses the reference this block held.
        // Safety: a `Compact` block, its fields still in it.
        heap::COMPACT => unsafe { crate::region::forget(v) },
        _ => {}
    }
}

/// Number the collections from the start again, before the numbers reach a
/// region block's mark: every block a collection marked is unmarked.
fn renumber(space: &mut Space) {
    for class in 1..CLASSES {
        for &(p, blocks) in &space.pages[class] {
            for k in 0..blocks {
                // Safety: block `k` of the page.
                let v = unsafe { p.add(k * class) } as Word;
                if !free(v) {
                    heap::set_mark(v, 0);
                }
            }
        }
    }
    for &p in space.large.keys() {
        heap::set_mark(p as Word, 0);
    }
    space.epoch = 1;
}

struct Marker {
    epoch: u32,
    stack: Vec<Word>,
    regions: HashSet<usize>,
}

impl Marker {
    /// A value the program holds, described by `d`.
    fn value(&mut self, w: Word, d: i64) {
        if d == desc::REF && heap::is_block(w) {
            self.stack.push(w);
        }
    }

    /// Every value the records from `head` down hold: see `crate::shadow`.
    fn chain(&mut self, head: Word) {
        let mut at = head;
        while at != 0 {
            let p = at as *const i64;
            // Safety: a record on a segment whose stack is intact.
            let (prev, n) = unsafe { (*p, *p.add(1)) };
            for i in 0..n.max(0) as usize {
                // Safety: within the record's `n` pairs.
                let (v, d) = unsafe { (*p.add(2 + 2 * i) as Word, *p.add(3 + 2 * i)) };
                if d != crate::shadow::TOKEN {
                    self.value(v, d);
                }
            }
            at = prev as Word;
        }
    }

    /// Mark the block at `v`, and queue what it holds.
    fn visit(&mut self, v: Word, cx: &Ctx) {
        let mark = heap::mark(v);
        if mark >= heap::REGION_FLOOR {
            // Inside a compact region, which is never looked inside: what is
            // noted is that the heap still points into it.
            // Safety: the mark says it is a region's block.
            self.regions
                .insert(unsafe { crate::region::owner(v) } as usize);
            return;
        }
        if mark == self.epoch {
            return;
        }
        heap::set_mark(v, self.epoch);
        match heap::kind(v) {
            heap::STRING | heap::BIGINT => {}
            // A suspended continuation: what its segments' frames hold.
            heap::STACK => {
                if let Some(Some(s)) = cx.suspended.get(heap::meta(v) as usize) {
                    self.chain(s.top.shadow);
                    for p in &s.nested {
                        self.chain(p.shadow);
                    }
                }
            }
            _ => {
                let n = heap::len(v);
                let first = heap::first_field(v);
                if heap::uniform(v) {
                    if heap::field_desc(v, 0) == desc::REF {
                        for i in 0..n {
                            let x = heap::word(v, first + i);
                            if heap::is_block(x) {
                                self.stack.push(x);
                            }
                        }
                    }
                } else {
                    for i in 0..n {
                        if heap::field_desc(v, i) == desc::REF {
                            let x = heap::word(v, first + i);
                            if heap::is_block(x) {
                                self.stack.push(x);
                            }
                        }
                    }
                }
            }
        }
    }
}
