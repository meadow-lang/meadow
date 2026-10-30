//! The old generation: Immix blocks and lines, never moved.
//!
//! What survives the nursery twice is promoted here, and stays at the same
//! address until it dies. That is what lets it be marked while the program
//! runs: nothing the marker reads moves under it. See [`crate::mark`] for the
//! marking, and [`crate::heap`] for how the generations fit together.
//!
//! # Blocks and lines
//!
//! The old generation is a table of [`Block`]s of [`BLOCK`] slots, each cut
//! into [`LINES`] lines of [`LINE`] slots. An old address names a block by its
//! high bits and a slot by its low ones, so reading one is two indexings.
//!
//! Space is reclaimed a **line** at a time. Marking marks every line a live
//! object covers; after a cycle, a line nothing marked is free, whatever dead
//! objects are still in it, and allocation bumps a cursor through runs of free
//! lines. No object is freed one at a time, and nothing is copied to reclaim
//! space. An object larger than a line takes the lines it covers; one larger
//! than a block takes consecutive blocks of its own.
//!
//! # Epochs
//!
//! A line records the number of the cycle that last marked it, or that
//! allocated into it -- its **epoch** -- rather than a bit that would have to be
//! cleared across the whole heap before each cycle. Between cycles, allocation
//! stamps lines with the epoch of the cycle that last finished, and a line is
//! in use if it has that stamp. While a cycle is marking, marking and
//! allocation both stamp the new epoch, and a line is in use if it has either.
//! Finishing a cycle is then one assignment: from now on only the new epoch
//! counts, and every line it did not reach is free.
//!
//! Object mark bits work the same way per block: a block's bits belong to the
//! epoch written beside them, and the first mark in a new epoch clears them.
//! So starting a cycle costs nothing per block either.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use crate::value::{Addr, Word};

/// Addresses from here up to [`crate::region::REGION_BASE`] are old; below it,
/// the nursery.
pub const OLD_BASE: Addr = 1 << 30;

/// Slots in a line: 256 bytes, Immix's line.
pub const LINE: usize = 32;
/// Lines in a block.
pub const LINES: usize = 256;
/// Slots in a block: 64 KiB.
pub const BLOCK: usize = LINE * LINES;
const BLOCK_SHIFT: u32 = BLOCK.trailing_zeros();
const WORDS: usize = BLOCK / 64;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The most blocks one heap's old generation can address.
const MAX_BLOCKS: usize = ((crate::region::REGION_BASE - OLD_BASE) as usize) / BLOCK;

/// One block. Its slots are read by the program and by the marker at once;
/// see [`crate::mark`] for why that is safe.
pub struct Block {
    mem: Mem,
    /// Which slots hold an address, one bit each: what a slot's descriptor
    /// says, kept where code that has only the slot's address -- the
    /// remembered set, evacuation's recorded fields -- can read it. Only the
    /// program touches these.
    pointers: Box<[AtomicU64]>,
    /// The epoch each line was last marked or allocated in. 0 is never.
    lines: Box<[AtomicU32]>,
    /// Object mark bits, one per slot, for [`Block::mark_epoch`].
    marks: Box<[AtomicU64]>,
    mark_epoch: AtomicU32,
    /// Held to clear `marks` for a new epoch, so a mark set meanwhile is not
    /// lost.
    clearing: Mutex<()>,
    /// Which slots are in the heap's remembered set. Only the program touches
    /// these.
    remembered: Box<[AtomicU64]>,
    /// Slots in objects marked in `mark_epoch`: how full the block is.
    live: AtomicU32,
    /// Whether it is to be evacuated -- see [`crate::evacuate`] -- and how many
    /// pointers into it have been recorded for that.
    state: AtomicU8,
    refs: AtomicU32,
    /// The fields recorded pointing into it, while it is tracked.
    recorded: Mutex<Vec<Addr>>,
    /// Part of an object bigger than a block, which cannot be moved a block at
    /// a time.
    large: AtomicBool,
}

/// A block's [`Block::state`]: allocated into as usual.
pub const NORMAL: u8 = 0;
/// Chosen for evacuation: nothing is allocated into it, and every pointer
/// into it is recorded.
pub const TRACKED: u8 = 1;
/// Chosen, but too much points into it to record: left alone until the cycle
/// it was chosen for ends.
pub const ABANDONED: u8 = 2;
/// Being evacuated, in this pause.
pub const MOVING: u8 = 3;
/// A chunk of a thread's frame stack: see `crate::heap`'s frame stack. Never
/// allocated into, evacuated or released by the collector; its contents are
/// roots, scanned by the heap that owns it, and the marker skips it.
pub const STACK: u8 = 4;

/// Pointers into one block recorded before it is abandoned. A block sparse
/// enough to choose holds at most a few hundred objects, so more than this
/// means a program rewriting pointers to them over and over, and rewriting
/// all of those would cost more than the block is worth.
const REF_CAP: u32 = 1 << 11;

/// A block's memory: its own, or a share of a **run** of blocks in one
/// allocation, which is what an object bigger than a block is laid out in.
/// The run makes the object contiguous in memory, so that native code, having
/// found where it starts, reaches every element of it by an offset -- across
/// what are, to the heap, several blocks.
///
/// A block that was part of a run goes back to the spares like any other when
/// it is freed, and holds its share of the run alive until it is dropped.
enum Mem {
    /// No memory yet: a table entry with nothing behind it.
    None,
    Own(Box<[UnsafeCell<Word>]>),
    /// Block `k` of the run.
    Part(Arc<[UnsafeCell<Word>]>, usize),
}

// Slots are written only by the heap's own thread; the marker only reads, and
// only slots no one writes while it can read them -- see `crate::mark`.
unsafe impl Sync for Block {}
unsafe impl Send for Block {}

impl Block {
    fn new() -> Block {
        Block::with(Mem::Own((0..BLOCK).map(|_| UnsafeCell::new(0)).collect()))
    }

    /// `n` blocks in one allocation, for an object bigger than a block: see
    /// [`Mem::Part`].
    fn run(n: usize) -> Vec<Block> {
        let run: Arc<[UnsafeCell<Word>]> = (0..n * BLOCK).map(|_| UnsafeCell::new(0)).collect();
        (0..n)
            .map(|k| Block::with(Mem::Part(run.clone(), k)))
            .collect()
    }

    fn with(mem: Mem) -> Block {
        Block {
            mem,
            pointers: (0..WORDS).map(|_| AtomicU64::new(0)).collect(),
            lines: (0..LINES).map(|_| AtomicU32::new(0)).collect(),
            marks: (0..WORDS).map(|_| AtomicU64::new(0)).collect(),
            mark_epoch: AtomicU32::new(0),
            clearing: Mutex::new(()),
            remembered: (0..WORDS).map(|_| AtomicU64::new(0)).collect(),
            live: AtomicU32::new(0),
            state: AtomicU8::new(NORMAL),
            refs: AtomicU32::new(0),
            recorded: Mutex::new(Vec::new()),
            large: AtomicBool::new(false),
        }
    }

    /// A table entry with no memory behind it: a block that was freed, or not
    /// yet needed. No live address reaches one.
    /// The first slot, for native code, which reads a block through the table
    /// [`Old::bases`] publishes rather than through this struct.
    ///
    /// A block with no memory answers [`zero_block`] instead of the dangling
    /// pointer an empty `Box<[_]>` has, so that a table entry is always safe
    /// to load through. What is read there is zero, which is not a valid
    /// object header, so a run of steps gives up on it and the interpreter --
    /// which checks properly -- says what went wrong.
    pub(crate) fn base(&self) -> *mut Word {
        match &self.mem {
            Mem::None => zero_block(),
            Mem::Own(slots) => slots.as_ptr() as *mut Word,
            // Safety: `k` is below the run's length in blocks.
            Mem::Part(run, k) => unsafe { (run.as_ptr() as *mut Word).add(k * BLOCK) },
        }
    }

    fn empty() -> Arc<Block> {
        static EMPTY: LazyLock<Arc<Block>> = LazyLock::new(|| Arc::new(Block::nothing()));
        EMPTY.clone()
    }

    fn nothing() -> Block {
        Block {
            mem: Mem::None,
            pointers: Box::new([]),
            lines: Box::new([]),
            marks: Box::new([]),
            mark_epoch: AtomicU32::new(0),
            clearing: Mutex::new(()),
            remembered: Box::new([]),
            live: AtomicU32::new(0),
            state: AtomicU8::new(NORMAL),
            refs: AtomicU32::new(0),
            recorded: Mutex::new(Vec::new()),
            large: AtomicBool::new(false),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self.mem, Mem::None)
    }

    /// As a new block is, apart from what its slots hold, which nothing reads
    /// before writing.
    fn reset(&mut self) {
        for l in self.lines.iter_mut() {
            *l.get_mut() = 0;
        }
        for w in self.remembered.iter_mut() {
            *w.get_mut() = 0;
        }
        for w in self.pointers.iter_mut() {
            *w.get_mut() = 0;
        }
        *self.mark_epoch.get_mut() = 0;
        *self.live.get_mut() = 0;
        *self.state.get_mut() = NORMAL;
        *self.refs.get_mut() = 0;
        self.recorded
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        *self.large.get_mut() = false;
    }

    #[inline]
    pub fn get(&self, off: usize) -> Word {
        debug_assert!(off < BLOCK);
        // Safety: see the `Sync` impl; `base` is `BLOCK` words, or the zero
        // block, which is as long.
        unsafe { *self.base().add(off) }
    }

    #[inline]
    fn put(&self, off: usize, w: Word, pointer: bool) {
        debug_assert!(off < BLOCK && !self.is_empty());
        // Safety: see the `Sync` impl.
        unsafe { *self.base().add(off) = w }
        let bit = 1u64 << (off % 64);
        let cell = &self.pointers[off / 64];
        let was = cell.load(Ordering::Relaxed);
        let now = if pointer { was | bit } else { was & !bit };
        if now != was {
            cell.store(now, Ordering::Relaxed);
        }
    }

    #[inline]
    fn is_pointer(&self, off: usize) -> bool {
        self.pointers[off / 64].load(Ordering::Relaxed) & (1u64 << (off % 64)) != 0
    }

    pub fn line(&self, i: usize) -> u32 {
        self.lines[i].load(Ordering::Acquire)
    }

    fn stamp(&self, i: usize, epoch: u32) {
        self.lines[i].store(epoch, Ordering::Release);
    }

    fn own_epoch(&self, epoch: u32) {
        if self.mark_epoch.load(Ordering::Acquire) == epoch {
            return;
        }
        let _clearing = self.clearing.lock().unwrap_or_else(|p| p.into_inner());
        if self.mark_epoch.load(Ordering::Acquire) != epoch {
            // Cleared before the epoch is published: whoever sees the new
            // epoch sees cleared bits, and marks nothing into the old ones.
            for w in self.marks.iter() {
                w.store(0, Ordering::Release);
            }
            self.live.store(0, Ordering::Release);
            self.mark_epoch.store(epoch, Ordering::Release);
        }
    }

    /// Mark the object at `off` in `epoch`. True if it was not marked already.
    pub fn mark(&self, off: usize, epoch: u32) -> bool {
        self.own_epoch(epoch);
        let bit = 1u64 << (off % 64);
        self.marks[off / 64].fetch_or(bit, Ordering::AcqRel) & bit == 0
    }

    pub fn is_marked(&self, off: usize, epoch: u32) -> bool {
        self.mark_epoch.load(Ordering::Acquire) == epoch
            && self.marks[off / 64].load(Ordering::Acquire) & (1u64 << (off % 64)) != 0
    }

    /// Count `slots` more alive in the epoch last marked. After a mark.
    pub fn add_live(&self, slots: usize) {
        self.live.fetch_add(slots as u32, Ordering::AcqRel);
    }

    /// Slots alive in `epoch`, as marking counted them.
    pub fn live(&self, epoch: u32) -> u32 {
        if self.mark_epoch.load(Ordering::Acquire) == epoch {
            self.live.load(Ordering::Acquire)
        } else {
            0
        }
    }

    /// Where the objects marked in `epoch` start.
    pub fn marked_offsets(&self, epoch: u32) -> Vec<usize> {
        let mut offs = Vec::new();
        if self.is_empty() || self.mark_epoch.load(Ordering::Acquire) != epoch {
            return offs;
        }
        for (w, word) in self.marks.iter().enumerate() {
            let mut bits = word.load(Ordering::Acquire);
            while bits != 0 {
                offs.push(w * 64 + bits.trailing_zeros() as usize);
                bits &= bits - 1;
            }
        }
        offs
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    pub fn set_state(&self, state: u8) {
        if state == TRACKED || state == NORMAL {
            self.refs.store(0, Ordering::Release);
            lock(&self.recorded).clear();
        }
        self.state.store(state, Ordering::Release);
    }

    /// Record fields pointing into this block. Their count is the caller's to
    /// check, with [`Block::count_ref`].
    pub fn record(&self, slots: impl IntoIterator<Item = Addr>) {
        lock(&self.recorded).extend(slots);
    }

    pub fn take_recorded(&self) -> Vec<Addr> {
        std::mem::take(&mut *lock(&self.recorded))
    }

    pub fn is_tracked(&self) -> bool {
        self.state() == TRACKED
    }

    /// Count one more pointer recorded into this block: false, and the block
    /// abandoned, once there are too many to be worth it.
    pub fn count_ref(&self) -> bool {
        if self.refs.fetch_add(1, Ordering::AcqRel) < REF_CAP {
            return true;
        }
        let _ =
            self.state
                .compare_exchange(TRACKED, ABANDONED, Ordering::AcqRel, Ordering::Acquire);
        false
    }

    pub fn is_large(&self) -> bool {
        self.large.load(Ordering::Acquire)
    }
}

// --- spare blocks ------------------------------------------------------------------
//
// Making a block is a 64 KiB allocation, written through and faulted in, and
// freeing one hands it back to the system: tens of microseconds each, which a
// nursery collection promoting into several new blocks would otherwise spend
// in its pause. So blocks no heap is using are kept here, for any heap to take,
// and the marking threads top the stash up and trim it off the program's time.

static SPARE: Mutex<Vec<Block>> = Mutex::new(Vec::new());
static SPARE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Spare blocks below this, the marking threads make more, up to `SPARE_READY`;
/// above `SPARE_MOST`, they free the excess.
const SPARE_LOW: usize = 16;
const SPARE_READY: usize = 64;
const SPARE_MOST: usize = 256;

fn take_spare() -> Block {
    let spare = {
        let mut s = lock(&SPARE);
        let b = s.pop();
        SPARE_COUNT.store(s.len(), Ordering::Release);
        b
    };
    spare.unwrap_or_else(Block::new)
}

/// Keep `b` for reuse, if nothing else holds it.
fn give_back(b: Arc<Block>) {
    if let Some(mut b) = Arc::into_inner(b).filter(|b| !b.is_empty()) {
        b.reset();
        let mut s = lock(&SPARE);
        s.push(b);
        SPARE_COUNT.store(s.len(), Ordering::Release);
    }
}

/// Do the spares want topping up or trimming?
pub fn spares_want_tending() -> bool {
    let n = SPARE_COUNT.load(Ordering::Acquire);
    !(SPARE_LOW..=SPARE_MOST).contains(&n)
}

/// Top the spares up, or trim them. Off the program's time: for the marking
/// threads.
pub fn tend_spares() {
    while SPARE_COUNT.load(Ordering::Acquire) < SPARE_READY {
        let b = Block::new();
        let mut s = lock(&SPARE);
        s.push(b);
        SPARE_COUNT.store(s.len(), Ordering::Release);
    }
    let excess = {
        let mut s = lock(&SPARE);
        let keep = s.len().min(SPARE_READY);
        let excess = s.split_off(keep);
        SPARE_COUNT.store(s.len(), Ordering::Release);
        excess
    };
    drop(excess);
}

#[inline]
pub fn block_of(a: Addr) -> usize {
    ((a - OLD_BASE) >> BLOCK_SHIFT) as usize
}

#[inline]
pub fn offset_of(a: Addr) -> usize {
    (a - OLD_BASE) as usize & (BLOCK - 1)
}

fn addr(block: usize, off: usize) -> Addr {
    OLD_BASE + (block * BLOCK + off) as Addr
}

/// One block of zeros, shared by every table entry with no block behind it.
///
/// Native code indexes the table without checking, because checking would cost
/// a guard on every heap read for a case that cannot arise in a correct
/// program. This is what makes not checking safe: an entry always points at a
/// whole block that can be read.
fn zero_block() -> *mut Word {
    static ZERO: LazyLock<Box<[Word]>> = LazyLock::new(|| vec![0; BLOCK].into_boxed_slice());
    ZERO.as_ptr() as *mut Word
}

/// Stamp every line the `size` slots at `a` cover with `epoch`, across blocks
/// if they go that far.
pub fn stamp_lines(blocks: &[Arc<Block>], a: Addr, size: usize, epoch: u32) {
    let first = (a - OLD_BASE) as usize / LINE;
    let last = ((a - OLD_BASE) as usize + size.max(1) - 1) / LINE;
    for l in first..=last {
        blocks[l / LINES].stamp(l % LINES, epoch);
    }
}

/// How many slots ahead of where it writes promotion asks for memory: eight
/// cache lines.
const PREFETCH: usize = 64;

/// Ask for the cache line at `p`, to be written soon. Only a hint: nothing
/// happens on a machine without one.
#[inline(always)]
fn prefetch(p: *const Word) {
    #[cfg(target_arch = "x86_64")]
    // Safety: a prefetch has no effect a program can see, at any address.
    unsafe {
        std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(p as *const i8)
    };
    #[cfg(target_arch = "aarch64")]
    // Safety: as above.
    unsafe {
        std::arch::asm!("prfm pstl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags))
    };
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = p;
}

/// The old generation, as its heap sees it.
pub struct Old {
    pub blocks: Vec<Arc<Block>>,
    /// The first slot of each block, for native code -- which cannot follow an
    /// `Arc` or index a `Vec`, and reads a heap word as `bases[block][offset]`.
    ///
    /// In lockstep with `blocks`, which is only changed in three places:
    /// [`Old::fresh_blocks`], [`Old::free_block`] and `Drop`. `Heap` publishes
    /// this array's address after any of them, and native code loads it afresh
    /// at every access, so a `Vec` that reallocates cannot leave a stale
    /// pointer behind.
    bases: Vec<*mut Word>,
    /// Lines stamped with this are in use: the last cycle to finish.
    live_epoch: u32,
    /// What allocation stamps lines with: `live_epoch`, or while marking, the
    /// epoch being marked.
    alloc_epoch: u32,
    /// The run of free lines small objects are bumped into.
    cursor: Addr,
    limit: Addr,
    /// A run for objects bigger than a line that did not fit the one above:
    /// Immix's overflow allocation, so one medium object does not throw away
    /// the rest of a run of small ones.
    overflow: Addr,
    overflow_limit: Addr,
    /// Where the search for free lines has got to since the last cycle.
    sweep_block: usize,
    sweep_line: usize,
    /// Where releasing wholly free blocks has got to.
    release_block: usize,
    /// Slots in objects allocated here since the last cycle finished, in
    /// those allocated since the one in progress began, and in objects the
    /// last cycle found alive.
    pub allocated: u64,
    black: u64,
    pub live: u64,
    /// Blocks with memory behind them, and table entries without.
    pub real_blocks: usize,
    empty_entries: usize,
}

impl Drop for Old {
    fn drop(&mut self) {
        for b in self.blocks.drain(..) {
            give_back(b);
        }
        self.bases.clear();
    }
}

impl Default for Old {
    fn default() -> Old {
        Old {
            blocks: Vec::new(),
            bases: Vec::new(),
            live_epoch: 1,
            alloc_epoch: 1,
            cursor: 0,
            limit: 0,
            overflow: 0,
            overflow_limit: 0,
            sweep_block: 0,
            sweep_line: 0,
            release_block: 0,
            allocated: 0,
            black: 0,
            live: 0,
            real_blocks: 0,
            empty_entries: 0,
        }
    }
}

impl Old {
    /// Nothing promoted here yet. Frame-stack chunks do not count: what is in
    /// them is roots, not old objects, and a collection with only those to
    /// look at still sees everything.
    pub fn is_empty(&self) -> bool {
        self.blocks
            .iter()
            .all(|b| b.is_empty() || b.state() == STACK)
    }

    pub fn capacity(&self) -> usize {
        self.real_blocks * BLOCK
    }

    /// Has the generation used half the addresses it can ever have? Past
    /// here a program allocating faster than the marker frees would run out
    /// -- see `Heap::overdue`.
    pub fn half_full(&self) -> bool {
        2 * self.blocks.len() > MAX_BLOCKS
    }

    pub fn epoch(&self) -> u32 {
        self.alloc_epoch
    }

    /// The epoch of the last cycle to finish.
    pub fn live_epoch(&self) -> u32 {
        self.live_epoch
    }

    #[inline]
    pub fn get(&self, a: Addr) -> Word {
        self.blocks[block_of(a)].get(offset_of(a))
    }

    /// Write slot `a`: a header word, or a field that holds no address.
    #[inline]
    pub fn put(&self, a: Addr, w: Word) {
        self.blocks[block_of(a)].put(offset_of(a), w, false)
    }

    /// Write field slot `a`, which holds an address if `pointer`.
    #[inline]
    pub fn put_field(&self, a: Addr, w: Word, pointer: bool) {
        self.blocks[block_of(a)].put(offset_of(a), w, pointer)
    }

    /// Write a whole object at `a`, which [`Old::alloc`] has just answered:
    /// `words` is its header of `header` slots and then its fields, field `i`
    /// an address if `pointer(i)`. What [`Old::put`] and [`Old::put_field`]
    /// do slot by slot, with the block looked up once for the object and its
    /// pointer bits written a bitmap word at a time -- which is what promoting
    /// an object costs, and a nursery collection that promotes much does
    /// little else.
    pub fn put_object(
        &self,
        a: Addr,
        words: &[Word],
        header: usize,
        pointer: impl Fn(usize) -> bool,
    ) {
        let off = offset_of(a);
        if off + words.len() > BLOCK {
            // Bigger than what is left of the block: a run of blocks, each
            // with bits of its own.
            for (k, &w) in words.iter().enumerate() {
                self.put_field(a + k as Addr, w, k >= header && pointer(k - header));
            }
            return;
        }
        let b = &self.blocks[block_of(a)];
        debug_assert!(!b.is_empty());
        // Safety: `off + words.len() <= BLOCK`, inside the block's memory; see
        // the `Sync` impl for who else may be reading it.
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), b.base().add(off), words.len()) }
        // Promotion bumps through lines that were last written long ago, so
        // nearly every new cache line it reaches is a miss, paid for in the
        // pause. Ask for the line a few ahead each time a new one is begun,
        // and the write gets there to find it.
        let line_end = (off + words.len()).div_ceil(8);
        if line_end != off.div_ceil(8) && off + PREFETCH + words.len() <= BLOCK {
            // Safety: inside the block, as above; a prefetch reads nothing.
            prefetch(unsafe { b.base().add(off + PREFETCH) });
        }
        let mut k = 0;
        while k < words.len() {
            let slot = off + k;
            let first = slot % 64;
            let n = (64 - first).min(words.len() - k);
            let span = if n == 64 {
                u64::MAX
            } else {
                ((1u64 << n) - 1) << first
            };
            let mut set = 0u64;
            for j in 0..n {
                let f = k + j;
                if f >= header && pointer(f - header) {
                    set |= 1 << (first + j);
                }
            }
            let cell = &b.pointers[slot / 64];
            let was = cell.load(Ordering::Relaxed);
            let now = (was & !span) | set;
            if now != was {
                cell.store(now, Ordering::Relaxed);
            }
            k += n;
        }
    }

    /// Did the last write to slot `a` put an address there?
    #[inline]
    pub fn is_pointer(&self, a: Addr) -> bool {
        self.blocks[block_of(a)].is_pointer(offset_of(a))
    }

    /// Could there be an object header at `a`, in a block that exists? For a
    /// debugger: a word that reads as a kind is all it can check, and a dead
    /// object in a free line looks the same as a live one.
    pub fn is_object(&self, a: Addr) -> bool {
        let b = block_of(a);
        b < self.blocks.len()
            && !self.blocks[b].is_empty()
            && !self.is_pointer(a)
            && crate::heap::Kind::try_from_byte(self.get(a) as u8).is_some()
    }

    fn line_free(&self, stamp: u32) -> bool {
        stamp != self.live_epoch && stamp != self.alloc_epoch
    }

    /// Start marking `epoch`: allocation from now on is black, and stamps
    /// lines with it. The runs in hand were stamped with the old epoch, so they
    /// are let go -- an object bumped into one now would be alive but in a
    /// line the cycle never stamps.
    pub fn begin(&mut self, epoch: u32) {
        self.alloc_epoch = epoch;
        self.black = 0;
        self.drop_runs();
    }

    /// The cycle in progress finished marking, finding `marked` slots alive:
    /// only its stamps count now. What was allocated while it marked is
    /// counted alive too.
    pub fn finish(&mut self, marked: u64) {
        self.live_epoch = self.alloc_epoch;
        self.live = marked + self.black;
        self.allocated = 0;
        self.drop_runs();
        self.sweep_block = 0;
        self.sweep_line = 0;
        self.release_block = 0;
    }

    fn drop_runs(&mut self) {
        self.cursor = 0;
        self.limit = 0;
        self.overflow = 0;
        self.overflow_limit = 0;
    }

    /// Room for an object of `size` slots, its lines stamped. `black` marks it
    /// for the cycle in progress, so the marker never traces it.
    pub fn alloc(&mut self, size: usize, black: bool) -> Addr {
        let at = if size > BLOCK {
            self.alloc_large(size)
        } else if self.cursor + size as Addr <= self.limit {
            let at = self.cursor;
            self.cursor += size as Addr;
            at
        } else if size > LINE {
            if self.overflow + size as Addr > self.overflow_limit {
                let b = self.fresh_blocks(1);
                self.overflow = addr(b, 0);
                self.overflow_limit = addr(b, BLOCK);
                self.stamp_run(self.overflow, BLOCK);
            }
            let at = self.overflow;
            self.overflow += size as Addr;
            at
        } else {
            loop {
                if !self.next_run() {
                    let b = self.fresh_blocks(1);
                    self.cursor = addr(b, 0);
                    self.limit = addr(b, BLOCK);
                    self.stamp_run(self.cursor, BLOCK);
                }
                if self.cursor + size as Addr <= self.limit {
                    break;
                }
            }
            let at = self.cursor;
            self.cursor += size as Addr;
            at
        };
        self.allocated += size as u64;
        self.black += size as u64;
        if black {
            let b = &self.blocks[block_of(at)];
            b.mark(offset_of(at), self.alloc_epoch);
            b.add_live(size);
        }
        at
    }

    /// Room for an object being evacuated, marked alive in the last cycle as
    /// the one it was copied from was. Not counted as allocation: nothing grew.
    /// Only between cycles.
    pub fn alloc_moved(&mut self, size: usize) -> Addr {
        let at = self.alloc(size, false);
        self.allocated -= size as u64;
        self.black -= size as u64;
        let b = &self.blocks[block_of(at)];
        b.mark(offset_of(at), self.live_epoch);
        b.add_live(size);
        at
    }

    /// A fresh block for a frame-stack chunk: its first slot's address. In
    /// state [`STACK`], so nothing here allocates into it, moves it or frees
    /// it -- the heap's frame stack owns it from here on.
    pub fn stack_block(&mut self) -> Addr {
        let i = self.fresh_blocks(1);
        self.blocks[i].set_state(STACK);
        // Stamped as in use, so it never looks empty to anything that looks at
        // lines rather than state.
        self.stamp_run(addr(i, 0), BLOCK);
        addr(i, 0)
    }

    /// Whether `a` is in a frame-stack chunk.
    pub fn in_stack(&self, a: Addr) -> bool {
        (OLD_BASE..crate::region::REGION_BASE).contains(&a)
            && self
                .blocks
                .get(block_of(a))
                .is_some_and(|b| b.state() == STACK)
    }

    /// Give block `i` back, empty.
    pub fn free_block(&mut self, i: usize) {
        give_back(std::mem::replace(&mut self.blocks[i], Block::empty()));
        self.bases[i] = self.blocks[i].base();
        self.real_blocks -= 1;
        self.empty_entries += 1;
    }

    /// The first slot of the block holding `a`, as a machine address.
    pub(crate) fn base_ptr(&self, a: Addr) -> *mut Word {
        self.bases[block_of(a)]
    }

    /// Where native code reads the blocks. See [`Old::bases`].
    pub(crate) fn bases_ptr(&self) -> *const *mut Word {
        debug_assert_eq!(
            self.bases.len(),
            self.blocks.len(),
            "the block table and the blocks have come apart"
        );
        debug_assert!(
            self.bases
                .iter()
                .zip(&self.blocks)
                .all(|(p, b)| *p == b.base()),
            "a block table entry points at the wrong block"
        );
        self.bases.as_ptr()
    }

    fn stamp_run(&self, from: Addr, slots: usize) {
        stamp_lines(&self.blocks, from, slots, self.alloc_epoch);
    }

    /// Find the next run of free lines, stamp it, and make it the cursor's.
    fn next_run(&mut self) -> bool {
        while self.sweep_block < self.blocks.len() {
            let b = &self.blocks[self.sweep_block];
            if b.is_empty() || b.state() != NORMAL {
                self.sweep_block += 1;
                self.sweep_line = 0;
                continue;
            }
            while self.sweep_line < LINES {
                if self.line_free(b.line(self.sweep_line)) {
                    let start = self.sweep_line;
                    let mut end = start;
                    while end < LINES && self.line_free(b.line(end)) {
                        end += 1;
                    }
                    self.sweep_line = end;
                    self.cursor = addr(self.sweep_block, start * LINE);
                    self.limit = addr(self.sweep_block, end * LINE);
                    self.stamp_run(self.cursor, (end - start) * LINE);
                    return true;
                }
                self.sweep_line += 1;
            }
            self.sweep_block += 1;
            self.sweep_line = 0;
        }
        false
    }

    /// `n` consecutive blocks with memory behind them and every line free:
    /// the lowest run of empty table entries long enough, or new ones at the
    /// end.
    fn fresh_blocks(&mut self, n: usize) -> usize {
        let mut start = 0;
        let mut run = 0;
        let mut found = None;
        let entries = if self.empty_entries >= n {
            self.blocks.len()
        } else {
            0
        };
        for (i, b) in self.blocks[..entries].iter().enumerate() {
            if b.is_empty() {
                if run == 0 {
                    start = i;
                }
                run += 1;
                if run == n {
                    found = Some(start);
                    break;
                }
            } else {
                run = 0;
            }
        }
        let first = match found {
            Some(s) => s,
            None => {
                // Empty entries at the end count towards the run.
                let tail = self
                    .blocks
                    .iter()
                    .rev()
                    .take_while(|b| b.is_empty())
                    .count();
                let first = self.blocks.len() - tail.min(n);
                let need = first + n;
                assert!(
                    need <= MAX_BLOCKS,
                    "the old generation has used up its address space"
                );
                while self.blocks.len() < need {
                    self.blocks.push(Block::empty());
                    self.bases.push(zero_block());
                    self.empty_entries += 1;
                }
                first
            }
        };
        // One block is a spare; a run of them is one allocation, so that an
        // object spanning them is contiguous -- see [`Mem::Part`].
        let fresh: Vec<Block> = if n == 1 {
            vec![take_spare()]
        } else {
            Block::run(n)
        };
        for (i, b) in (first..first + n).zip(fresh) {
            self.blocks[i] = Arc::new(b);
            self.bases[i] = self.blocks[i].base();
            self.real_blocks += 1;
            self.empty_entries -= 1;
        }
        first
    }

    fn alloc_large(&mut self, size: usize) -> Addr {
        let n = size.div_ceil(BLOCK);
        let first = self.fresh_blocks(n);
        for b in &self.blocks[first..first + n] {
            b.large.store(true, Ordering::Release);
        }
        let at = addr(first, 0);
        self.stamp_run(at, n * BLOCK);
        at
    }

    /// Give back the memory of up to `budget` blocks with nothing in them,
    /// while there is more than half as much again as the last cycle found
    /// alive. Only between cycles: a marker reads blocks by their place in the
    /// table as it was when marking began.
    pub fn release(&mut self, budget: usize) {
        let keep = (self.live as usize * 3 / 2).div_ceil(BLOCK) + 4;
        let mut looked = 0;
        while looked < budget && self.release_block < self.blocks.len() {
            let i = self.release_block;
            self.release_block += 1;
            looked += 1;
            if self.real_blocks <= keep {
                return;
            }
            let b = &self.blocks[i];
            if b.is_empty() || b.state() != NORMAL || self.holds_run(i) {
                continue;
            }
            if (0..LINES).all(|l| self.line_free(b.line(l))) {
                self.free_block(i);
            }
        }
    }

    fn holds_run(&self, i: usize) -> bool {
        let inside = |a: Addr, limit: Addr| limit > a && block_of(limit - 1) == i;
        inside(self.cursor, self.limit) || inside(self.overflow, self.overflow_limit)
    }

    /// Is the line holding slot `a` stamped with the last finished epoch?
    pub fn line_live(&self, a: Addr) -> bool {
        let b = &self.blocks[block_of(a)];
        !b.is_empty() && b.line(offset_of(a) / LINE) == self.live_epoch
    }

    /// Add slot `a` to the remembered set bitmap. True if it was not there.
    pub fn remember(&self, a: Addr) -> bool {
        let b = &self.blocks[block_of(a)];
        let off = offset_of(a);
        let bit = 1u64 << (off % 64);
        let w = &b.remembered[off / 64];
        let was = w.load(Ordering::Relaxed);
        w.store(was | bit, Ordering::Relaxed);
        was & bit == 0
    }

    pub fn forget(&self, a: Addr) {
        let b = &self.blocks[block_of(a)];
        if b.is_empty() {
            return;
        }
        let off = offset_of(a);
        let w = &b.remembered[off / 64];
        w.store(
            w.load(Ordering::Relaxed) & !(1u64 << (off % 64)),
            Ordering::Relaxed,
        );
    }

    /// Is the object at `a` marked in the cycle in progress, or the last one?
    pub fn is_marked(&self, a: Addr, epoch: u32) -> bool {
        self.blocks[block_of(a)].is_marked(offset_of(a), epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Kind;

    /// Write the header of a data object of `len` fields at `a`.
    fn header(old: &Old, a: Addr, len: u32) {
        crate::object::write_header(
            Kind::Data,
            0,
            std::iter::repeat_n(meadow_rt::desc::INT, len as usize),
            |k, w| old.put(a + k as Addr, w),
        );
    }

    #[test]
    fn small_objects_bump_through_a_block() {
        let mut old = Old::default();
        let a = old.alloc(3, false);
        let b = old.alloc(3, false);
        assert_eq!(a, OLD_BASE);
        assert_eq!(b, OLD_BASE + 3);
        assert_eq!(old.real_blocks, 1);
    }

    #[test]
    fn a_finished_cycle_frees_the_lines_it_did_not_mark() {
        let mut old = Old::default();
        let keep = old.alloc(3, false);
        header(&old, keep, 1);
        // Fill the rest of the first line and into the next few.
        for _ in 0..40 {
            let a = old.alloc(3, false);
            header(&old, a, 1);
        }
        old.begin(2);
        // Only `keep` is found alive: its line is stamped with the new epoch.
        assert!(old.blocks[0].mark(offset_of(keep), 2));
        stamp_lines(&old.blocks, keep, 3, 2);
        old.finish(3);
        assert!(old.line_live(keep));
        assert!(!old.line_live(keep + LINE as Addr));
        // The next allocation reuses the line after it, not a new block.
        let next = old.alloc(3, false);
        assert_eq!(next, OLD_BASE + LINE as Addr);
        assert_eq!(old.real_blocks, 1);
    }

    #[test]
    fn marks_belong_to_their_epoch() {
        let mut old = Old::default();
        let a = old.alloc(2, false);
        assert!(old.blocks[0].mark(offset_of(a), 2));
        assert!(
            !old.blocks[0].mark(offset_of(a), 2),
            "marked once per epoch"
        );
        assert!(old.is_marked(a, 2));
        assert!(!old.is_marked(a, 3));
        assert!(
            old.blocks[0].mark(offset_of(a), 3),
            "a new epoch starts clear"
        );
    }

    #[test]
    fn an_object_bigger_than_a_block_takes_blocks_of_its_own() {
        let mut old = Old::default();
        let small = old.alloc(2, false);
        let big = old.alloc(BLOCK * 2 + 5, false);
        assert_eq!(block_of(small), 0);
        assert_eq!(offset_of(big), 0);
        assert_eq!(block_of(big), 1);
        assert_eq!(old.real_blocks, 4);
        // Its last slot is addressable, in the third of its blocks.
        old.put(big + (BLOCK * 2 + 4) as Addr, 0);
    }

    #[test]
    fn free_blocks_are_released_and_reused() {
        let mut old = Old::default();
        let big = old.alloc(BLOCK * 8, false);
        let _ = big;
        assert_eq!(old.real_blocks, 8);
        old.begin(2);
        old.finish(0); // nothing survived
        old.release(100);
        assert!(old.real_blocks <= 4, "down to the reserve");
        let again = old.alloc(BLOCK * 3, false);
        assert!(block_of(again) < 8, "an empty run of the table is reused");
    }
}
