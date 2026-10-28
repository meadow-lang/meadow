//! **Blocks, counted by reference**, the AxCut paper's way (`docs/RUNTIME.md`).
//!
//! A reference is a word: `0` is nothing, an odd word is an object with no
//! block (a nullary constructor, a closure with no captures), and any other
//! word is the address of a block, laid out as [`LAYOUT`] says: Silo's
//! layout, or with the `glade-layout` feature Glade's, which has no count and
//! is only traced (`meadow_rt::layout`).
//!
//! What the paper does, and so what this does:
//!
//! - **share** adds one to the count;
//! - **erase** takes one from it, or -- if it was the last -- puts the block on
//!   the list of blocks _pending_, fields and all, without looking inside;
//! - **clean**: a block whose fields were moved out by the last reference
//!   loading them (the paper's `release`) is reusable at once;
//! - **acquire** takes a clean block of the size wanted, or fresh memory --
//!   and first erases some of what is pending, a bounded amount, so that no
//!   operation ever walks a structure and garbage never waits for long.
//!
//! The paper has one size of block and chains a bigger object through its last
//! field; Meadow has arrays and strings, which want indexing, so blocks come in
//! a size class per word count, with anything bigger allocated on its own.

/// The layout all of this is: `meadow_rt::layout::SILO`, or with the
/// `glade-layout` feature `meadow_rt::layout::GLADE`, which the emitted code
/// and the interpreter read too. The constants below are its, by the names
/// this module has always given them.
#[cfg(not(feature = "glade-layout"))]
pub const LAYOUT: meadow_rt::layout::Layout = meadow_rt::layout::SILO;
#[cfg(feature = "glade-layout")]
pub const LAYOUT: meadow_rt::layout::Layout = meadow_rt::layout::GLADE;

/// The header word the cycle collector's bits are in: the kind's.
pub const FLAGS: usize = LAYOUT.flags_word();

/// What word 1's low byte says a block is.
pub const DATA: u64 = LAYOUT.kinds.data;
pub const CLOSURE: u64 = LAYOUT.kinds.closure;
pub const STRING: u64 = LAYOUT.kinds.string;
pub const ARRAY: u64 = LAYOUT.kinds.array;
pub const BIGINT: u64 = LAYOUT.kinds.bigint;
pub const CELL: u64 = LAYOUT.kinds.cell;
pub const RECORD: u64 = LAYOUT.kinds.record;
pub const MUT_ARRAY: u64 = LAYOUT.kinds.mut_array;
/// A resumption's one-shot flag.
pub const ONCE: u64 = LAYOUT.kinds.once;
pub const COMPACT: u64 = LAYOUT.kinds.compact;
pub const CHANNEL: u64 = LAYOUT.kinds.channel;
pub const TASK: u64 = LAYOUT.kinds.task;
pub const TVAR: u64 = LAYOUT.kinds.tvar;
/// A suspended stack segment: see `crate::segments`.
pub const STACK: u64 = LAYOUT.kinds.stack;
/// What a compact's values live in: see `crate::prims`, `Compact`.
pub const REGION: u64 = LAYOUT.kinds.region;

pub const DESC_SHIFT: u32 = LAYOUT.elem_desc.shift;
pub const UNIFORM: u64 = LAYOUT.uniform.bits_in_place();

/// What the cycle collector keeps in the kind's word ([`FLAGS`]), where a
/// fresh block has zeroes:
/// a colour, whether the block is waiting in the candidate buffer, and
/// whether it died while it was waiting. See [`crate::cycles`].
pub const COLOUR_SHIFT: u32 = LAYOUT.colour.shift;
pub const BLACK: u64 = 0;
pub const GRAY: u64 = 1;
pub const WHITE: u64 = 2;
pub const PURPLE: u64 = 3;
const BUFFERED: u64 = LAYOUT.buffered.bits_in_place();
/// Its count reached zero while it was a candidate: the memory is held back
/// until the collector has finished with it.
const DEAD: u64 = LAYOUT.dead.bits_in_place();
/// Its fields were moved out before it died, so there is nothing in it to
/// erase -- see [`clean`].
const MOVED: u64 = LAYOUT.moved.bits_in_place();
/// The count a block inside a compact region is given on top of the
/// references it has. It does two things at once, and [`crate::region`] has
/// both: it puts the count out of reach of zero, so such a block is never
/// freed on its own and never looks unique to the code that loads its fields;
/// and it puts the count somewhere no ordinary block's ever is, so a share or
/// an erase can tell it is touching a region from the count it has loaded
/// anyway.
pub const STICKY: u32 = LAYOUT.sticky as u32;

/// A mark this big is a block inside a compact region and nothing else. Under
/// counting the mark is the count, and this is half the bias, so no amount of
/// drift in either direction is mistaken either way; `meadow_llvm::emit`'s
/// helpers test the same number inline. A layout with no count gives a
/// region's block this mark, and nothing else has it.
pub const REGION_FLOOR: u32 = LAYOUT.region_floor as u32;

/// Is `v` a block inside a compact region? Ask only of a word a descriptor
/// says is a reference: an `Int` that happens to be even is not an address.
#[inline]
pub fn in_region(v: Word) -> bool {
    is_block(v) && mark(v) >= REGION_FLOOR
}

/// Make the block at `v`, a copy just made in a region, a region's block.
pub fn set_in_region(v: Word) {
    set_mark(v, LAYOUT.sticky as u32);
}

/// Its kind is one a cycle can contain: a `Ref` or a mutable array, which is
/// where a store into a block that already exists can happen. Set where the
/// block is built and never changed, so that the emitted counting helper can
/// ask this and the colour in one test -- `meadow_llvm::emit::helpers`, which
/// has the constant, must agree with these bits.
pub const MUTABLE: u64 = LAYOUT.mutable.bits_in_place();

/// Everything in word 1 that is one heap's collector talking to itself: the
/// colour, whether the block is waiting in that heap's candidate list, and
/// whether it died while it waited. A block copied into another heap
/// ([`crate::parcel`]) keeps none of it. [`MUTABLE`] is not here: that is
/// about the kind, which the copy keeps.
pub const MARKS: u64 = (3 << COLOUR_SHIFT) | BUFFERED | DEAD | MOVED | DEFERRED;

/// Waiting in the oversized list for a walk with no budget (see
/// [`Heap::set_oversized`]): until then, a walk from another candidate that
/// reaches it goes no further in and takes it for alive -- which can only put
/// off freeing a cycle through it, never free what is not garbage. Without
/// this, one big table a program reads over and over is walked in part at
/// every run, from whatever candidate reaches it.
pub const DEFERRED: u64 = LAYOUT.deferred.bits_in_place();

pub fn deferred(v: Word) -> bool {
    word(v, FLAGS) & DEFERRED != 0
}

/// The [`MUTABLE`] bit, for a block of this kind.
const fn mutable(kind: u64) -> u64 {
    if kind == CELL || kind == MUT_ARRAY {
        MUTABLE
    } else {
        0
    }
}

/// The [`MUTABLE`] bit, for a uniform block of this kind whose words are all
/// described by `d`. A mutable array of numbers is mutable, but can hold no
/// reference, and so can be part of no cycle: it is never a candidate, and a
/// program that keeps its tables in such arrays -- a parser's, a compiler's --
/// does not have the collector walk them every time one is let go of.
fn mutable_holding(kind: u64, d: i64) -> u64 {
    if d == meadow_rt::desc::REF || d == meadow_rt::desc::ANY {
        mutable(kind)
    } else {
        0
    }
}

/// A mutable array has had a block stored in it that a cycle could run
/// through -- not a string, a big integer, a number: set where an element is
/// written, and never cleared. One that has only ever held leaves has nothing
/// for the cycle collector to walk, however long it is; which is what a
/// program's column of ten thousand names is, taken and let go of at every
/// read of it and so a candidate again after every run.
pub const CYCLABLE: u64 = LAYOUT.cyclable.bits_in_place();

/// `x`, described by `d`, is being stored in the mutable array at `arr`.
#[inline]
pub fn note_stored(arr: Word, x: Word, d: i64) {
    crate::trace::remember(arr, x, d);
    if (d == meadow_rt::desc::REF || d == meadow_rt::desc::ANY)
        && is_block(x)
        && crate::cycles::may_cycle(x)
    {
        set_word(arr, FLAGS, word(arr, FLAGS) | CYCLABLE);
    }
}

/// Whether the block at `v` can point at anything a cycle could run through:
/// a mutable array only if something that could ever was stored in it.
pub fn may_hold_cycles(v: Word) -> bool {
    if kind(v) == MUT_ARRAY && uniform(v) {
        word(v, FLAGS) & CYCLABLE != 0
    } else {
        holds_refs(v)
    }
}

/// Whether the block at `v` can hold a reference at all: not a uniform one
/// whose words are all described as something else.
pub fn holds_refs(v: Word) -> bool {
    !uniform(v) || {
        let d = LAYOUT.elem_desc.get(word(v, LAYOUT.elem_desc.word)) as i64;
        d == meadow_rt::desc::REF || d == meadow_rt::desc::ANY
    }
}

pub fn colour(v: Word) -> u64 {
    (word(v, FLAGS) >> COLOUR_SHIFT) & 3
}

pub fn set_colour(v: Word, c: u64) {
    let w = word(v, FLAGS) & !(3 << COLOUR_SHIFT);
    set_word(v, FLAGS, w | (c << COLOUR_SHIFT));
}

pub fn buffered(v: Word) -> bool {
    word(v, FLAGS) & BUFFERED != 0
}

pub fn set_buffered(v: Word, yes: bool) {
    let w = word(v, FLAGS) & !BUFFERED;
    set_word(v, FLAGS, if yes { w | BUFFERED } else { w });
}

pub fn dead(v: Word) -> bool {
    word(v, FLAGS) & DEAD != 0
}

/// Keep `v` for the cycle collector: see [`Heap::candidate`].
pub fn keep_candidate(v: Word) {
    with(|h| h.candidate(v));
}

/// Word counts with a size class of their own; bigger blocks are allocated
/// alone and freed at once when clean.
const CLASSES: usize = 128;
/// Memory taken from the system at a time for the size classes, in words: the
/// first chunk is small -- a thread may allocate almost nothing -- and each
/// after it twice the last, up to this.
const CHUNK: usize = 1 << 17;
const FIRST_CHUNK: usize = 1 << 10;
/// Fields erased per step of the pending work.
const STEP: usize = 64;
/// The smallest heap, in blocks, that a doubling is measured from: below it a
/// doubling means nothing and the cycle collector would run constantly.
const FLOOR: usize = 4096;

pub type Word = u64;

/// A heap: one per green thread (`crate::ctx`), freed with it.
pub struct Heap {
    /// The clean blocks of each size class: the first, whose word 0 is the
    /// next, and so on to a null. A list through the blocks themselves is a
    /// load and a store to take one or give one back, and never grows.
    clean: [*mut Word; CLASSES],
    /// Blocks whose last reference was erased, and how far into their fields
    /// erasing has got.
    pending: Vec<(*mut Word, usize)>,
    chunk: *mut Word,
    left: usize,
    /// Blocks acquired and not yet clean: what a leak check counts.
    live: isize,
    /// Blocks acquired at all: what a block built in one the program already
    /// had -- reuse, see `meadow-llvm`'s `linear` -- saves.
    acquired: u64,
    /// Which, when a leak check asks: see [`tracking`].
    blocks: Option<std::collections::HashSet<usize>>,
    /// Blocks whose count went down without reaching zero: where the cycle
    /// collector looks. Empty for a program that cannot make a cycle.
    candidates: Vec<Word>,
    /// Candidates whose walk was too long to do inside a pause: kept until
    /// the heap has grown enough that a long pause beats holding the memory.
    /// See [`crate::cycles::BUDGET`].
    oversized: Vec<Word>,
    /// How many blocks must be live before that is worth doing, or
    /// `usize::MAX` for "nothing is waiting".
    oversized_mark: usize,
    /// Live blocks at which the cycle collector has something to do, which is
    /// the only thing [`acquire`] asks about: one load and one compare on the
    /// path every allocation takes. It is `usize::MAX` -- never -- unless
    /// there is a candidate waiting, `0` once there are [`crate::cycles::
    /// TRIGGER`] of them, and otherwise a doubling of the heap away, which is
    /// what makes a program that ties a handful of very large knots collect
    /// them as it goes rather than waiting for candidates it will never have.
    run_at: usize,
    /// Whether the collector is working through the candidates it has: once
    /// it starts it keeps going, a bounded run per allocation, until the
    /// buffer is empty. Draining a run at a time is what keeps a pause short
    /// without letting cyclic garbage pile up -- the program runs between one
    /// run and the next, since that is where allocation happens.
    draining: bool,
    /// How many candidates must gather before the collector runs: [`crate::
    /// cycles::TRIGGER`] to begin with, and more while its runs find nothing
    /// -- see [`Heap::paced`].
    trigger: usize,
    /// Runs in a row that walked from a candidate and freed nothing.
    futile: u32,
    /// Blocks the collector has freed, for its tally.
    freed: u64,
    /// The memory it has: chunks, and blocks too big for a size class --
    /// what dropping it gives back.
    chunks: Vec<(*mut Word, usize)>,
    large: crate::ctx::ByAddress<usize>,
    /// Under `MEADOW_SILO_MEMORY`: the words in this heap's blocks that are
    /// in use.
    held: usize,
    /// Under `MEADOW_SILO_PRIMS`: how many blocks of each kind this heap has
    /// erased, by what makes them -- a constructor, a closure's code.
    erased: Option<std::collections::HashMap<(u64, u32), u64>>,
    /// Where a traced program's blocks come from: see [`crate::trace`]. Made
    /// at its first allocation; never, for a program that counts.
    space: Option<Box<crate::trace::Space>>,
}

impl Heap {
    pub fn new() -> Heap {
        Heap {
            clean: [std::ptr::null_mut(); CLASSES],
            pending: Vec::new(),
            chunk: std::ptr::null_mut(),
            left: 0,
            live: 0,
            acquired: 0,
            blocks: tracking().then(std::collections::HashSet::new),
            candidates: Vec::new(),
            oversized: Vec::new(),
            oversized_mark: usize::MAX,
            run_at: usize::MAX,
            draining: false,
            trigger: crate::cycles::TRIGGER,
            futile: 0,
            freed: 0,
            chunks: Vec::new(),
            large: crate::ctx::ByAddress::default(),
            held: 0,
            erased: crate::prims::counting().then(std::collections::HashMap::new),
            space: None,
        }
    }

    /// The traced program's memory, if it has allocated any.
    pub fn space(&self) -> Option<&crate::trace::Space> {
        self.space.as_deref()
    }

    pub fn space_mut(&mut self) -> Option<&mut crate::trace::Space> {
        self.space.as_deref_mut()
    }
}

impl Default for Heap {
    fn default() -> Heap {
        Heap::new()
    }
}

/// Under `MEADOW_SILO_PRIMS`: of the blocks let go of whole -- the last
/// reference dropped by code that had not taken the block apart -- one in
/// every [`EVERY`], by what the block was and which function of the program
/// dropped it.
static DROPPED: std::sync::Mutex<Option<std::collections::HashMap<(u64, u32, String), u64>>> =
    std::sync::Mutex::new(None);
static DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const EVERY: u64 = 1024;

#[cold]
fn dropped(v: Word) {
    let n = DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n % EVERY != 0 {
        return;
    }
    // The innermost frame that is the program's own code: emitted functions
    // are named `mw.` and then what they are.
    let trace = std::backtrace::Backtrace::force_capture().to_string();
    let who = trace
        .lines()
        .filter_map(|l| l.trim().split_once(": ").map(|(_, name)| name))
        .find(|name| name.starts_with("mw."))
        .unwrap_or("the runtime")
        .to_string();
    let mut all = DROPPED.lock().unwrap_or_else(|e| e.into_inner());
    *all.get_or_insert_with(std::collections::HashMap::new)
        .entry((kind(v), meta(v), who))
        .or_default() += EVERY;
}

/// Under `MEADOW_SILO_PRIMS`: of the arrays made, one in every [`EVERY`], by
/// how long it was made (up to eight, then "more") and which function of the
/// program made it.
static MADE: std::sync::Mutex<Option<std::collections::HashMap<(usize, String), u64>>> =
    std::sync::Mutex::new(None);
static MAKES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cold]
fn made(n: usize) {
    let k = MAKES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if k % EVERY != 0 {
        return;
    }
    let trace = std::backtrace::Backtrace::force_capture().to_string();
    let who = trace
        .lines()
        .filter_map(|l| l.trim().split_once(": ").map(|(_, name)| name))
        .find(|name| name.starts_with("mw."))
        .unwrap_or("the runtime")
        .to_string();
    let mut all = MADE.lock().unwrap_or_else(|e| e.into_inner());
    *all.get_or_insert_with(std::collections::HashMap::new)
        .entry((n.min(9), who))
        .or_default() += EVERY;
}

/// The arrays made, as sampled: how long, by whom, and about how many.
pub fn made_by() -> (u64, Vec<((usize, String), u64)>) {
    let all = MADE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<((usize, String), u64)> =
        all.iter().flatten().map(|(k, n)| (k.clone(), *n)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    (MAKES.load(std::sync::atomic::Ordering::Relaxed), out)
}

/// The blocks dropped whole, as sampled: what each was, who dropped it, and
/// about how many, most first.
pub fn dropped_by() -> Vec<((u64, u32, String), u64)> {
    let all = DROPPED.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<((u64, u32, String), u64)> =
        all.iter().flatten().map(|(k, n)| (k.clone(), *n)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// Under `MEADOW_SILO_LEAKS`: the blocks threads ended still holding, by
/// kind and what made them, over every thread that has ended.
static ENDED_WITH: std::sync::Mutex<Option<std::collections::HashMap<(u64, u32), u64>>> =
    std::sync::Mutex::new(None);

/// What threads that have ended were still holding, most first.
pub fn ended_with() -> Vec<((u64, u32), u64)> {
    let all = ENDED_WITH.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<((u64, u32), u64)> = all.iter().flatten().map(|(k, n)| (*k, *n)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// What every heap that has gone erased, by kind: see [`Heap::erased`].
static ERASED: std::sync::Mutex<Option<std::collections::HashMap<(u64, u32), u64>>> =
    std::sync::Mutex::new(None);

/// The blocks erased so far, by kind and what made them, most first: the
/// running thread's and every finished one's.
pub fn erased() -> Vec<((u64, u32), u64)> {
    with(|h| h.tell());
    let all = ERASED.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<((u64, u32), u64)> = all.iter().flatten().map(|(k, n)| (*k, *n)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

impl Heap {
    /// This heap's counts into the program's.
    fn tell(&mut self) {
        if let Some(mine) = &mut self.erased {
            let mut all = ERASED.lock().unwrap_or_else(|e| e.into_inner());
            let all = all.get_or_insert_with(std::collections::HashMap::new);
            for (k, n) in mine.drain() {
                *all.entry(k).or_default() += n;
            }
        }
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        self.tell();
        // Under the leak check: what this heap's thread ended still holding,
        // by kind -- blocks that go with the heap unerased, and so without
        // giving up what they held outside it.
        if let Some(blocks) = &self.blocks {
            if !blocks.is_empty() {
                let mut all = ENDED_WITH.lock().unwrap_or_else(|e| e.into_inner());
                let all = all.get_or_insert_with(std::collections::HashMap::new);
                for b in blocks {
                    let v = *b as Word;
                    let key = (
                        kind(v),
                        if kind(v) == DATA || kind(v) == CLOSURE {
                            meta(v)
                        } else {
                            0
                        },
                    );
                    *all.entry(key).or_default() += 1;
                }
            }
        }
        // What this heap still held goes with it.
        crate::memory::LIVE.fetch_sub(self.held, std::sync::atomic::Ordering::Relaxed);
        let words = |n: usize| std::alloc::Layout::array::<Word>(n).expect("a block fits memory");
        for (p, n) in self.chunks.drain(..) {
            crate::memory::HEAPS.fetch_sub(n, std::sync::atomic::Ordering::Relaxed);
            // Safety: allocated with this layout in `take`.
            unsafe { std::alloc::dealloc(p as *mut u8, words(n)) };
        }
        for (p, n) in self.large.drain() {
            crate::memory::HEAPS.fetch_sub(n, std::sync::atomic::Ordering::Relaxed);
            // Safety: likewise.
            unsafe { std::alloc::dealloc(p as *mut u8, words(n)) };
        }
    }
}

fn with<T>(f: impl FnOnce(&mut Heap) -> T) -> T {
    with_in(crate::ctx::get(), f)
}

/// `f` of the heap of context `c`: the running thread's, which emitted code
/// hands the runtime's hottest entries rather than have them look it up.
#[inline(always)]
fn with_in<T>(c: *mut crate::ctx::Ctx, f: impl FnOnce(&mut Heap) -> T) -> T {
    // Safety: the running green thread's heap, and nothing here re-enters
    // it while it is borrowed -- erasing pushes onto `pending` rather than
    // recursing.
    f(unsafe { &mut (*c).heap })
}

/// Is `v` the address of a block?
#[inline]
pub fn is_block(v: Word) -> bool {
    LAYOUT.is_block(v)
}

/// Is `v` a value with no block: a constructor with no fields, a closure
/// with no captures?
#[inline]
pub fn is_immediate(v: Word) -> bool {
    v & LAYOUT.imm_mask == LAYOUT.imm_tag
}

/// The value with no block that carries `payload` -- a tag, a method base.
#[inline]
pub const fn immediate(payload: u64) -> Word {
    LAYOUT.immediate(payload)
}

/// What the value with no block `v` carries.
#[inline]
pub const fn payload(v: Word) -> u64 {
    LAYOUT.payload(v)
}

/// The tag of a constructor, or the method base of a closure, whether it has
/// a block or not: what a `switch` and an `invoke` read.
#[inline]
pub fn meta_of(v: Word) -> u32 {
    if is_immediate(v) {
        payload(v) as u32
    } else {
        meta(v)
    }
}

#[inline]
fn ptr(v: Word) -> *mut Word {
    v as *mut Word
}

/// Word `i` of the block at `v`.
#[inline]
pub fn word(v: Word, i: usize) -> Word {
    // Safety: callers ask for words inside a live block.
    unsafe { *ptr(v).add(i) }
}

#[inline]
pub fn set_word(v: Word, i: usize, w: Word) {
    // Safety: as `word`.
    unsafe { *ptr(v).add(i) = w }
}

pub fn len(v: Word) -> usize {
    LAYOUT.len.get(word(v, LAYOUT.len.word)) as usize
}

pub fn kind(v: Word) -> u64 {
    LAYOUT.kind.get(word(v, LAYOUT.kind.word))
}

pub fn meta(v: Word) -> u32 {
    LAYOUT.meta.get(word(v, LAYOUT.meta.word)) as u32
}

pub(crate) fn uniform(v: Word) -> bool {
    LAYOUT.uniform.get(word(v, LAYOUT.uniform.word)) != 0
}

/// The references to `v` besides one. Only a layout that counts has them.
#[inline]
pub fn count(v: Word) -> u32 {
    debug_assert!(LAYOUT.counted, "the count of a block of a layout with none");
    LAYOUT.count.get(word(v, LAYOUT.count.word)) as u32
}

#[inline]
pub fn set_count(v: Word, rc: u32) {
    let at = LAYOUT.count.word;
    set_word(v, at, LAYOUT.count.set(word(v, at), u64::from(rc)));
}

/// The number of the collection that last marked `v` (see
/// [`crate::trace`]): its count, under a layout that has one.
#[inline]
pub fn mark(v: Word) -> u32 {
    LAYOUT.mark.get(word(v, LAYOUT.mark.word)) as u32
}

#[inline]
pub fn set_mark(v: Word, m: u32) {
    let at = LAYOUT.mark.word;
    set_word(v, at, LAYOUT.mark.set(word(v, at), u64::from(m)));
}

/// Where the block at `v` went, if it moved (see [`crate::trace`]).
#[inline]
pub fn forwarded(v: Word) -> Option<Word> {
    let f = LAYOUT.forward;
    (f.get(word(v, f.word)) == LAYOUT.forwarded).then(|| word(v, LAYOUT.forward_to))
}

/// Say that the block at `v` moved to `to`.
#[inline]
pub fn set_forwarded(v: Word, to: Word) {
    let f = LAYOUT.forward;
    set_word(v, f.word, f.set(word(v, f.word), LAYOUT.forwarded));
    set_word(v, LAYOUT.forward_to, to);
}

/// Copy the header of the block at `from` into the new block at `to`, of
/// another heap or a region: its length, kind, `meta` and what else its
/// header words hold, but none of what one heap's collectors keep -- no
/// count beyond the one, no mark, none of the cycle collector's bits
/// ([`MARKS`]).
pub fn copy_header(from: Word, to: Word) {
    for i in 0..LAYOUT.header_words {
        set_word(to, i, word(from, i));
    }
    set_word(to, FLAGS, word(to, FLAGS) & !MARKS);
    set_count(to, 0);
    set_mark(to, 0);
}

/// Where a uniform block's elements begin -- an array's, a big integer's
/// limbs: right after its header, with no descriptors before them.
pub const ELEMS: usize = LAYOUT.first_field(0, true);

/// Set `v`'s length to `n`, keeping the rest of its header.
pub fn set_len(v: Word, n: usize) {
    let at = LAYOUT.len.word;
    set_word(v, at, LAYOUT.len.set(word(v, at), n as u64));
}

/// Set the `meta` of the block at `v` -- a constructor's tag, a segment's
/// number -- keeping the rest of its header.
pub fn set_meta(v: Word, m: u32) {
    let at = LAYOUT.meta.word;
    set_word(v, at, LAYOUT.meta.set(word(v, at), u64::from(m)));
}

/// Set the one descriptor of the uniform block at `v`'s elements.
pub fn set_elem_desc(v: Word, d: i64) {
    let at = LAYOUT.elem_desc.word;
    set_word(v, at, LAYOUT.elem_desc.set(word(v, at), d as u64));
}

/// Where the fields start.
pub fn first_field(v: Word) -> usize {
    LAYOUT.first_field(len(v), uniform(v))
}

/// Words the block takes.
pub fn size(v: Word) -> usize {
    first_field(v)
        + if kind(v) == ARRAY {
            array_room(len(v))
        } else {
            len(v)
        }
}

/// The element slots an array of `n` elements is given: `n` for a small one,
/// and for a bigger one the next of 12, 16, 24, 32, 48, ... -- each a power of
/// two or three quarters of one -- so that at most a third is spare.
///
/// The room is a function of the length and nothing else, so no block records
/// it: [`size`] works it out, as the allocator's free path needs. It is what
/// makes `arrayPush` constant time on an array nobody else holds -- the element
/// goes into the room while there is some (`meadow_array_push`), and only
/// crossing to the next size copies.
pub fn array_room(n: usize) -> usize {
    if n <= 8 {
        return n;
    }
    let p = n.next_power_of_two();
    let q = p / 4 * 3;
    if q >= n { q } else { p }
}

/// Field `i`'s descriptor.
pub fn field_desc(v: Word, i: usize) -> i64 {
    if uniform(v) {
        LAYOUT.elem_desc.get(word(v, LAYOUT.elem_desc.word)) as i64
    } else {
        let (at, shift) = LAYOUT.desc_at(i);
        ((word(v, at) >> shift) & LAYOUT.desc_mask()) as i64
    }
}

pub fn field(v: Word, i: usize) -> Word {
    word(v, first_field(v) + i)
}

/// Write field `i` of the non-uniform block at `v`, and its descriptor --
/// noted, where blocks move, if `v` is older than `x` (`crate::trace`).
pub fn set_field(v: Word, i: usize, x: Word, d: i64) {
    crate::trace::remember(v, x, d);
    set_word(v, first_field(v) + i, x);
    if !uniform(v) {
        let (at, shift) = LAYOUT.desc_at(i);
        let mask = LAYOUT.desc_mask();
        let w = (word(v, at) & !(mask << shift)) | (((d as u64) & mask) << shift);
        set_word(v, at, w);
    }
}

/// `n` references more to `v`, described by `d`: one count written, unless
/// the block is in a region, which counts each.
pub fn share_n(v: Word, d: i64, n: usize) {
    if n == 0 || d != meadow_rt::desc::REF || !is_block(v) || crate::trace::on() {
        return;
    }
    if in_region(v) {
        for _ in 0..n {
            share(v, d);
        }
    } else {
        // Safety: a live block's count.
        let rc = unsafe { &mut *(v as *mut u32) };
        *rc += n as u32;
    }
}

/// One reference more to `v`, described by `d`.
#[inline]
pub fn share(v: Word, d: i64) {
    // Traced, nothing is counted: see `crate::trace`.
    if d == meadow_rt::desc::REF && is_block(v) && !crate::trace::on() {
        // Safety: a live block's count.
        let rc = unsafe { &mut *(v as *mut u32) };
        *rc += 1;
        if *rc > REGION_FLOOR {
            // One more reference into the region this block is in, which is
            // what keeps the region alive: see [`crate::region`].
            // Safety: the count says it is in one.
            unsafe { crate::region::shared(v) };
        }
    }
}

/// One reference fewer.
#[inline]
pub fn erase(v: Word, d: i64) {
    if d == meadow_rt::desc::REF && is_block(v) && !crate::trace::on() {
        // Safety: a live block's count.
        let rc = unsafe { &mut *(v as *mut u32) };
        if *rc == 0 {
            died(v);
        } else {
            let region = *rc > REGION_FLOOR;
            *rc -= 1;
            if region {
                // Safety: the count says it is in a region. The region cannot
                // go under us: this is one of its own references.
                unsafe { crate::region::erased(v) };
            } else if crate::cycles::possible() {
                crate::cycles::meadow_candidate(v);
            }
        }
    }
}

/// The last reference to `v` is gone: its fields are erased and its memory
/// comes back. A block waiting in the candidate buffer is only marked, since
/// the collector still has a pointer to it -- see [`Heap::free_dead`].
#[inline]
fn died(v: Word) {
    died_in(crate::ctx::get(), v);
}

/// [`died`], in context `c`'s heap.
#[inline]
pub fn died_in(c: *mut crate::ctx::Ctx, v: Word) {
    if crate::trace::on() {
        return;
    }
    if crate::prims::counting() {
        dropped(v);
    }
    if crate::cycles::possible() && buffered(v) {
        // Black and dead: what the collector must not mistake for a
        // candidate with a reference left, since the count it reads is the
        // references besides one.
        set_word(v, FLAGS, (word(v, FLAGS) | DEAD) & !(3 << COLOUR_SHIFT));
        return;
    }
    with_in(c, |h| h.pending.push((ptr(v), 0)));
}

/// A block of `words` words, count 0, the rest for the caller to write: what
/// the runtime's own allocations go through, in a counted program or a
/// traced one.
pub fn acquire(words: usize) -> Word {
    let c = crate::ctx::get();
    if crate::trace::on() {
        acquire_traced(c, words)
    } else {
        acquire_in(c, words)
    }
}

/// [`acquire`] under tracing, from context `c`'s: what a traced program's
/// code allocates through (`meadow_acquire_traced`), apart from counting's,
/// whose path every allocation of a counted program takes and which is as it
/// was before tracing was there.
pub fn acquire_traced(c: *mut crate::ctx::Ctx, words: usize) -> Word {
    with_in(c, |h| {
        h.space
            .get_or_insert_with(|| Box::new(crate::trace::Space::new()))
            .take(words)
    })
}

/// [`acquire`] in a counted program, from context `c`'s heap.
pub fn acquire_in(c: *mut crate::ctx::Ctx, words: usize) -> Word {
    with_in(c, |h| {
        // Work off what is pending: one step always, more while nothing clean
        // of this size is to be had.
        h.step();
        let want = words < CLASSES;
        while want && h.clean[words].is_null() && !h.pending.is_empty() {
            h.step();
        }
        // A program that cannot make a cycle never sets this, so what it pays
        // for the collector is this compare and nothing else.
        if h.live as usize >= h.run_at {
            h.draining = true;
            if h.oversized_due() {
                crate::cycles::collect_oversized(h);
            } else {
                crate::cycles::collect(h);
            }
        }
        h.live += 1;
        h.acquired += 1;
        let p = h.take(words);
        if let Some(b) = &mut h.blocks {
            b.insert(p as usize);
        }
        p
    })
}

/// Is the leak check on? `MEADOW_SILO_LEAKS`.
pub fn tracking() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MEADOW_SILO_LEAKS").is_some())
}

impl Heap {
    fn take(&mut self, words: usize) -> Word {
        let h = self;
        if crate::memory::on() {
            h.held += words;
            crate::memory::LIVE.fetch_add(words, std::sync::atomic::Ordering::Relaxed);
        }
        let want = words < CLASSES;
        if !want {
            let layout = std::alloc::Layout::array::<Word>(words).expect("a block fits memory");
            // Safety: a non-zero size.
            let p = unsafe { std::alloc::alloc(layout) } as *mut Word;
            if p.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            crate::memory::HEAPS.fetch_add(words, std::sync::atomic::Ordering::Relaxed);
            h.large.insert(p as usize, words);
            return p as Word;
        }
        let p = h.clean[words];
        if !p.is_null() {
            // Safety: a clean block, whose word 0 is the next.
            h.clean[words] = unsafe { *p } as *mut Word;
            return p as Word;
        }
        if h.left < words {
            // Before more is asked of the system: what can be given back.
            if crate::sched::vars_waiting() {
                crate::sched::forget_vars();
            }
            let size = h
                .chunks
                .last()
                .map_or(FIRST_CHUNK, |(_, n)| (2 * n).min(CHUNK));
            let layout = std::alloc::Layout::array::<Word>(size).expect("a chunk fits memory");
            // Safety: a non-zero size.
            let p = unsafe { std::alloc::alloc(layout) } as *mut Word;
            if p.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            crate::memory::HEAPS.fetch_add(size, std::sync::atomic::Ordering::Relaxed);
            h.chunks.push((p, size));
            h.chunk = p;
            h.left = size;
        }
        let p = h.chunk;
        // Safety: inside the chunk, which has `left` words from `chunk`.
        h.chunk = unsafe { p.add(words) };
        h.left -= words;
        p as Word
    }
}

// --- MEADOW_SILO_POISON -------------------------------------------------------
//
// For a block used after it was freed. Every freed block is filled with
// junk and never handed out again, and at exit each one written to since is
// said; `collector` in the variable keeps only what the cycle collector
// frees, to tell its mistakes from counting's, and `ptr` makes the junk an
// address nothing is mapped at, so that a reference read out of a freed
// block faults where it is followed -- and on Windows the fault is said, with
// its address in the executable, for `llvm-symbolizer` (a build with `-g`,
// through `MEADOW_CLANG`). It found the collector freeing what a later run
// would still walk: `cycles::collect_white`.

fn poisoning() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MEADOW_SILO_POISON").is_some())
}

fn poison_says(what: &str) -> bool {
    std::env::var("MEADOW_SILO_POISON").is_ok_and(|v| v.split(',').any(|p| p == what))
}

fn poison_collector_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| poison_says("collector"))
}

/// What a freed block is filled with: odd, so no block and no small number;
/// or, with `ptr`, even, and an address nothing is mapped at.
fn poison() -> Word {
    static P: std::sync::OnceLock<Word> = std::sync::OnceLock::new();
    *P.get_or_init(|| {
        if poison_says("ptr") {
            0x0000_7ead_beef_0000
        } else {
            0xdead_beef_dead_beef
        }
    })
}

/// Every block freed under poisoning: where, and how many words.
static QUARANTINE: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

fn quarantine(p: *mut Word, words: usize) {
    for i in 0..words {
        // Safety: the block's own words, which nothing should read any more.
        unsafe { *p.add(i) = poison() };
    }
    QUARANTINE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((p as usize, words));
}

/// Under poisoning, at exit: each freed block written to since, and each
/// freed twice.
pub fn quarantined() {
    if !poisoning() {
        return;
    }
    let q = QUARANTINE.lock().unwrap_or_else(|e| e.into_inner());
    let mut written = 0;
    let mut seen = std::collections::HashSet::new();
    let mut twice = 0;
    for &(p, words) in q.iter() {
        twice += usize::from(!seen.insert(p));
        let p = p as *const Word;
        // Safety: never handed out again under poisoning.
        let changed: Vec<(usize, Word)> = (0..words)
            .map(|i| (i, unsafe { *p.add(i) }))
            .filter(|(_, w)| *w != poison())
            .collect();
        if !changed.is_empty() {
            written += 1;
            if written <= 20 {
                eprintln!("aot: freed block {p:p} of {words} words written since: {changed:x?}");
            }
        }
    }
    eprintln!(
        "aot: {} blocks freed; {written} written to after, {twice} freed twice",
        q.len()
    );
}

/// Under poisoning, on Windows: an access violation said before the process
/// goes -- where, the offset into the executable, and the address touched.
pub fn catch_faults() {
    if !poisoning() {
        return;
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        struct Record {
            code: u32,
            flags: u32,
            next: *mut Record,
            address: *mut std::ffi::c_void,
            count: u32,
            info: [usize; 15],
        }
        #[repr(C)]
        struct Pointers {
            record: *mut Record,
            context: *mut std::ffi::c_void,
        }
        unsafe extern "system" {
            fn AddVectoredExceptionHandler(
                first: u32,
                handler: extern "system" fn(*mut Pointers) -> i32,
            ) -> *mut std::ffi::c_void;
            fn GetModuleHandleW(name: *const u16) -> *mut std::ffi::c_void;
        }
        extern "system" fn said(p: *mut Pointers) -> i32 {
            // Safety: what Windows hands a vectored handler.
            let r = unsafe { &*(*p).record };
            if r.code == 0xc000_0005 {
                let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
                eprintln!(
                    "aot: access violation at {:p} (offset {:#x} in the executable), {} {:#x}",
                    r.address,
                    (r.address as usize).wrapping_sub(base),
                    if r.info[0] == 1 { "writing" } else { "reading" },
                    r.info[1]
                );
                std::process::exit(99);
            }
            0
        }
        // Safety: a handler that lives as long as the process.
        unsafe { AddVectoredExceptionHandler(1, said) };
    }
}

/// The block at `v` is done with: its fields were moved out, or erased.
pub fn clean(v: Word) {
    clean_in(crate::ctx::get(), v);
}

/// [`clean`], into context `c`'s heap.
pub fn clean_in(c: *mut crate::ctx::Ctx, v: Word) {
    // Traced, a block is freed by the sweep and nothing else.
    if crate::trace::on() {
        return;
    }
    if crate::cycles::possible() && buffered(v) {
        // Waiting in the candidate buffer: hold the memory back, and
        // remember that there is nothing left inside to erase.
        set_word(
            v,
            FLAGS,
            (word(v, FLAGS) | DEAD | MOVED) & !(3 << COLOUR_SHIFT),
        );
        return;
    }
    let words = size(v);
    with_in(c, |h| h.clean_block(ptr(v), words));
}

impl Heap {
    /// Keep `v` for the cycle collector to look at, unless it is kept
    /// already. See [`crate::cycles`].
    pub(crate) fn candidate(&mut self, v: Word) {
        if colour(v) == PURPLE {
            return;
        }
        set_colour(v, PURPLE);
        if !buffered(v) {
            set_buffered(v, true);
            self.candidates.push(v);
            self.schedule();
        }
    }

    /// A candidate has just been buffered: bring the next run no further off
    /// than it already was. Never further -- a mark that moved with the heap
    /// would never be reached.
    fn schedule(&mut self) {
        if self.candidates.len() >= self.trigger {
            self.run_at = 0;
        } else if self.run_at == usize::MAX {
            self.run_at = (2 * self.live.max(0) as usize).max(FLOOR);
        }
    }

    /// A run has just finished: when the next one is due, from what it left.
    /// Once the collector is going it keeps going -- the next allocation
    /// starts another run -- until there is nothing buffered; then it waits
    /// for a doubling of the heap, or for [`crate::cycles::TRIGGER`]
    /// candidates, whichever comes first. See [`Heap::run_at`].
    pub(crate) fn reschedule(&mut self) {
        if self.candidates.is_empty() {
            self.draining = false;
        }
        let want = if self.candidates.is_empty() {
            usize::MAX
        } else if self.draining || self.candidates.len() >= self.trigger {
            0
        } else {
            (2 * self.live.max(0) as usize).max(FLOOR)
        };
        self.run_at = want.min(self.oversized_mark);
    }

    /// A run has just finished, having walked from `walked` candidates and
    /// freed `freed` blocks of garbage: how many candidates the next round of
    /// runs waits for.
    ///
    /// A program that makes candidates by the million and cycles never -- an
    /// interpreter, whose every scope is a map of cells shared with the
    /// scopes made from it -- would otherwise spend most of its time here:
    /// each candidate walked is everything its cell reaches, and proves
    /// nothing. So while the runs free nothing the collector waits for twice
    /// as many candidates as before, up to [`crate::cycles::TRIGGER_MOST`],
    /// and by then most of them have died where they waited and are freed
    /// without a walk. The first run that does free a cycle puts it back. A
    /// doubling of the heap still starts a run whatever this says, so what
    /// waiting costs is bounded as it always was.
    pub(crate) fn paced(&mut self, walked: usize, freed: u64) {
        if freed > 0 {
            self.trigger = crate::cycles::TRIGGER;
            self.futile = 0;
        } else if walked > 0 {
            self.futile += 1;
            if self.futile >= crate::cycles::FUTILE {
                self.futile = 0;
                self.trigger = (self.trigger * 2).min(crate::cycles::TRIGGER_MOST);
            }
        }
    }

    pub(crate) fn has_candidates(&self) -> bool {
        !self.candidates.is_empty()
    }

    /// Up to `n` candidates, for a run of the collector: see [`crate::cycles`].
    /// What is left waits for the next run, still buffered.
    pub(crate) fn take_candidates(&mut self, n: usize) -> Vec<Word> {
        if self.candidates.len() <= n {
            return std::mem::take(&mut self.candidates);
        }
        self.candidates.split_off(self.candidates.len() - n)
    }

    /// Candidates a run did not get to: they are still buffered, so they go
    /// straight back to wait for the next one.
    pub(crate) fn return_candidates(&mut self, vs: &[Word]) {
        self.candidates.extend_from_slice(vs);
    }

    /// `v` reaches more than a run may walk. Keep it, and remember how big
    /// the heap may get before walking it anyway -- a doubling, so that the
    /// one long pause it costs is paid over as much allocation as it saves.
    pub(crate) fn set_oversized(&mut self, v: Word) {
        set_word(v, FLAGS, word(v, FLAGS) | DEFERRED);
        self.oversized.push(v);
        if self.oversized_mark == usize::MAX {
            self.oversized_mark = (2 * self.live.max(0) as usize).max(FLOOR);
        }
    }

    /// Has the heap grown enough to be worth that pause?
    pub(crate) fn oversized_due(&self) -> bool {
        self.live as usize >= self.oversized_mark
    }

    /// Look at them after all, with no budget: the candidates waiting go back
    /// on the end of the list, and this says how many.
    pub(crate) fn retry_oversized(&mut self) -> usize {
        self.oversized_mark = usize::MAX;
        let waiting = std::mem::take(&mut self.oversized);
        for &v in &waiting {
            set_word(v, FLAGS, word(v, FLAGS) & !DEFERRED);
        }
        self.candidates.extend_from_slice(&waiting);
        waiting.len()
    }

    /// A block that died while it was a candidate, now that the collector has
    /// finished with it: erase what is in it, if anything, and take the
    /// memory back.
    pub(crate) fn free_dead(&mut self, v: Word) {
        let moved = word(v, FLAGS) & MOVED != 0;
        set_word(v, FLAGS, word(v, FLAGS) & !(DEAD | MOVED));
        if moved {
            let words = size(v);
            self.clean_block(ptr(v), words);
        } else {
            self.pending.push((ptr(v), 0));
        }
    }

    /// One reference fewer to the block at `x`, from inside the collector,
    /// which has this heap in hand already: `erase`, without looking the heap
    /// up again. For a leaf -- a string, a big integer, a compact -- which is
    /// never a candidate.
    pub(crate) fn let_go(&mut self, x: Word) {
        // Safety: a live block's count.
        let rc = unsafe { &mut *(x as *mut u32) };
        if *rc == 0 {
            self.pending.push((ptr(x), 0));
        } else {
            let region = *rc > REGION_FLOOR;
            *rc -= 1;
            if region {
                // Safety: the count says it is in a region.
                unsafe { crate::region::erased(x) };
            }
        }
    }

    /// How many blocks the collector has freed from this heap.
    pub(crate) fn freed(&self) -> u64 {
        self.freed
    }

    /// A block the collector found in a garbage cycle. Its edges are
    /// accounted for already -- the mark pass took them away and the scan
    /// pass did not put them back -- so only the memory comes back here.
    /// A block of a garbage cycle that is waiting in the candidate buffer:
    /// collected, its edges gone with the rest of the cycle's, and its memory
    /// held back until its turn frees it -- dead, with nothing left in it to
    /// erase. See `cycles::collect_white`.
    pub(crate) fn hold_collected(&mut self, v: Word) {
        self.freed += 1;
        set_word(
            v,
            FLAGS,
            (word(v, FLAGS) | DEAD | MOVED) & !(3 << COLOUR_SHIFT),
        );
    }

    /// Under `MEADOW_SILO_LEAKS`, after a run of the collector: no candidate
    /// still waiting is white. One that is was left, proved garbage, for a
    /// later run to collect, still pointing at what this one freed.
    pub(crate) fn check_none_white(&self) {
        for &v in self.candidates.iter().chain(&self.oversized) {
            if !dead(v) && colour(v) == WHITE {
                crate::fail("the cycle collector left a garbage block waiting for a later run");
            }
        }
    }

    pub(crate) fn free_cycle(&mut self, v: Word) {
        self.freed += 1;
        let words = size(v);
        self.clean_block(ptr(v), words);
    }

    fn clean_block(&mut self, p: *mut Word, words: usize) {
        self.live -= 1;
        if poisoning() && (!poison_collector_only() || crate::cycles::running()) {
            quarantine(p, words);
            return;
        }
        if crate::memory::on() {
            self.held = self.held.saturating_sub(words);
            crate::memory::LIVE.fetch_sub(words, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(b) = &mut self.blocks {
            b.remove(&(p as usize));
        }
        if words >= CLASSES {
            let layout = std::alloc::Layout::array::<Word>(words).expect("a block fits memory");
            self.large.remove(&(p as usize));
            crate::memory::HEAPS.fetch_sub(words, std::sync::atomic::Ordering::Relaxed);
            // Safety: allocated alone, with this layout, in `acquire`.
            unsafe { std::alloc::dealloc(p as *mut u8, layout) };
            return;
        }
        // Safety: a block of `words` words, which nothing reads any more.
        unsafe { *p = self.clean[words] as Word };
        self.clean[words] = p;
    }

    /// Erase up to [`STEP`] fields of the block pending longest.
    fn step(&mut self) {
        let Some((p, from)) = self.pending.pop() else {
            return;
        };
        let v = p as Word;
        let n = len(v);
        let to = (from + STEP).min(n);
        let first = first_field(v);
        let refs = kind(v) != STRING && kind(v) != BIGINT;
        let cycles = crate::cycles::possible();
        if refs {
            for i in from..to {
                let d = field_desc(v, i);
                let x = word(v, first + i);
                if d == meadow_rt::desc::REF && is_block(x) {
                    // Safety: a live block's count.
                    let rc = unsafe { &mut *(x as *mut u32) };
                    if *rc == 0 {
                        if cycles && buffered(x) {
                            set_word(x, FLAGS, word(x, FLAGS) | DEAD);
                        } else {
                            self.pending.push((ptr(x), 0));
                        }
                    } else {
                        let region = *rc > REGION_FLOOR;
                        *rc -= 1;
                        if region {
                            // Safety: the count says it is inside a region.
                            unsafe { crate::region::erased(x) };
                        } else if cycles && matches!(kind(x), CELL | MUT_ARRAY) {
                            self.candidate(x);
                        }
                    }
                }
            }
        }
        if to < n && refs {
            self.pending.push((p, to));
        } else {
            // A continuation nobody can resume now: its segments go.
            if kind(v) == STACK {
                crate::segments::discard(meta(v));
            }
            // One fewer that could await the thread: see `crate::sched::held`.
            if kind(v) == TASK {
                crate::sched::let_go(meta(v));
            }
            if kind(v) == TVAR {
                crate::sched::let_go_var(field(v, 0) as usize);
            }
            // The last reference to a compact: its region loses the one this
            // block held, and goes with it if that was the last. Everything
            // inside it is freed there, all at once, and never here.
            if kind(v) == COMPACT {
                // Safety: a `Compact` block whose fields are still in it.
                unsafe { crate::region::forget(v) };
            }
            if let Some(erased) = &mut self.erased {
                *erased.entry((kind(v), meta(v))).or_default() += 1;
            }
            let words = size(v);
            self.clean_block(p, words);
        }
    }
}

/// Look for every cycle there is now, and erase everything pending: what is
/// live after this is what the program really holds. See [`crate::cycles`].
pub fn settle() {
    with(|h| crate::cycles::collect_all(h));
    with(|h| {
        while !h.pending.is_empty() {
            h.step();
        }
    });
    // Erasing what a cycle held can make candidates of its own.
    with(|h| crate::cycles::collect_all(h));
}

/// Erase everything this thread has let go of, which is ending: its heap
/// goes all at once, but what its dead blocks hold outside it -- a reference
/// into a compact's region -- is given up only by erasing them. Left, each
/// such region outlived the program.
///
/// That is the blocks waiting to be erased, and also the ones that died
/// while the cycle collector had them down as candidates: those are only
/// marked, for the collector to free when it next runs -- which, for a
/// thread that ends first, was never. A `Ref` written under a handler is
/// one, and what a query's thread keeps its answers in.
pub fn finish() {
    settle();
    with(|h| {
        while !h.pending.is_empty() {
            h.step();
        }
    });
}

/// Blocks acquired on this thread so far.
pub fn acquired() -> u64 {
    with(|h| h.acquired)
}

/// Blocks acquired and not yet clean, on this thread -- after everything
/// pending has been erased.
pub fn live() -> isize {
    with(|h| {
        while !h.pending.is_empty() {
            h.step();
        }
        h.live
    })
}

// --- building -------------------------------------------------------------

/// A non-uniform block of `kind` and `meta` holding `fields`, described by
/// `descs`. No fields is no block.
pub fn build(kind: u64, meta: u32, fields: &[Word], descs: &[i64]) -> Word {
    let n = fields.len();
    if n == 0 {
        return immediate(u64::from(meta));
    }
    let first = LAYOUT.first_field(n, false);
    let v = acquire(first + n);
    let mut head = LAYOUT.header(kind, n as u64, u64::from(meta));
    head[FLAGS] |= mutable(kind);
    LAYOUT.pack_descs(descs, |w, bits| {
        if w < LAYOUT.header_words {
            head[w] |= bits;
        } else {
            set_word(v, w, bits);
        }
    });
    for (i, h) in head.iter().enumerate() {
        set_word(v, i, *h);
    }
    for (i, x) in fields.iter().enumerate() {
        set_word(v, first + i, *x);
    }
    v
}

/// A uniform block of `kind` and `meta` of `n` words, every one described by
/// `d`, the words for the caller to write.
pub fn build_uniform(kind: u64, meta: u32, n: usize, d: i64) -> Word {
    if kind == ARRAY && crate::prims::counting() {
        made(n);
    }
    let v = acquire(ELEMS + if kind == ARRAY { array_room(n) } else { n });
    let mut head = LAYOUT.header(kind, n as u64, u64::from(meta));
    head[FLAGS] |= mutable_holding(kind, d);
    let e = LAYOUT.elem_desc;
    head[e.word] |= e.put(d as u64);
    head[LAYOUT.uniform.word] |= UNIFORM;
    for (i, h) in head.iter().enumerate() {
        set_word(v, i, *h);
    }
    v
}

/// A string of `bytes`, eight to a word.
pub fn string(bytes: &[u8]) -> Word {
    let n = bytes.len().div_ceil(8);
    let v = build_uniform(STRING, bytes.len() as u32, n, meadow_rt::desc::INT);
    for i in 0..n {
        let chunk = &bytes[8 * i..bytes.len().min(8 * i + 8)];
        let mut w = [0u8; 8];
        w[..chunk.len()].copy_from_slice(chunk);
        set_word(v, ELEMS + i, Word::from_le_bytes(w));
    }
    v
}

/// The bytes of the string at `v`, copied out.
pub fn bytes(v: Word) -> Vec<u8> {
    str_bytes(v).to_vec()
}

/// The bytes of the string at `v`, where they are: eight to a word,
/// little-endian, from word 2 -- which is to say one run of bytes in memory.
/// Valid as long as the string is.
pub fn str_bytes<'a>(v: Word) -> &'a [u8] {
    // Safety: a live string block, whose words from 2 hold `meta` bytes.
    unsafe { std::slice::from_raw_parts((v as *const u8).add(16), meta(v) as usize) }
}

/// The blocks reachable from `roots`.
pub fn reachable(roots: &[(Word, i64)]) -> std::collections::HashSet<usize> {
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<(Word, i64)> = roots.to_vec();
    while let Some((w, d)) = stack.pop() {
        if d != meadow_rt::desc::REF || !is_block(w) || !seen.insert(w as usize) {
            continue;
        }
        if kind(w) != STRING && kind(w) != BIGINT {
            for i in 0..len(w) {
                stack.push((field(w, i), field_desc(w, i)));
            }
        }
    }
    seen
}

/// Live blocks not reachable from `roots` -- what the run leaked -- by kind
/// and constructor tag, most first; and how many there are.
pub fn leaked(roots: &[(Word, i64)]) -> (usize, Vec<(usize, u64, u32)>) {
    let kept = reachable(roots);
    with(|h| {
        while !h.pending.is_empty() {
            h.step();
        }
        let mut counts: std::collections::HashMap<(u64, u32), usize> =
            std::collections::HashMap::new();
        let mut total = 0;
        for b in h.blocks.iter().flatten() {
            if kept.contains(b) {
                continue;
            }
            total += 1;
            let v = *b as Word;
            let key = (
                kind(v),
                if kind(v) == DATA || kind(v) == CLOSURE {
                    meta(v)
                } else {
                    0
                },
            );
            *counts.entry(key).or_default() += 1;
        }
        let mut out: Vec<(usize, u64, u32)> =
            counts.into_iter().map(|((k, m), n)| (n, k, m)).collect();
        out.sort_by(|a, b| b.0.cmp(&a.0));
        (total, out)
    })
}
