//! The heap: a nursery, an old generation marked while the program runs, and
//! the regions it refers to.
//!
//! Every green thread has a heap of its own, so everything here is one
//! thread's business, and a pause stops only that thread. What this module is
//! built around is keeping those pauses short however much the thread keeps
//! alive -- well under a millisecond -- rather than making the total time spent
//! collecting as small as it can be.
//!
//! # The nursery
//!
//! New objects are bumped into the nursery, and when it fills it is collected
//! the Cheney way: the roots are copied into the other half, the copies are
//! scanned as a queue and what they reach copied after them, and what is left
//! behind is garbage in bulk. That costs what survives, and almost nothing
//! does. What has survived one nursery collection already is **promoted** into
//! the old generation at the next, so nothing is copied back and forth for long,
//! and the nursery has a fixed largest size -- [`GcConfig::nursery`] -- so a
//! nursery collection has a fixed longest pause. While what is kept young keeps
//! surviving to be promoted anyway, survivors are promoted at once instead:
//! see `Heap::tenure`.
//!
//! Until a heap fills its largest nursery, nothing is promoted and the nursery
//! just grows: most green threads never keep enough alive to need an old
//! generation at all, and pay nothing for one.
//!
//! # The old generation
//!
//! Immix blocks and lines -- see [`crate::old`] -- where objects do not move
//! while a cycle marks. It is collected by marking what is reachable, on another OS thread while the
//! program runs -- see [`crate::mark`] -- and then treating every line nothing
//! marked as free. A cycle costs the program two pauses: one to empty the
//! nursery and hand the roots to the marker, and one when marking is done, to
//! adopt its result. Neither does work that grows with the old generation.
//!
//! A cycle begins when the old generation has grown by as much as the last
//! cycle found alive (or [`GcConfig::min_trigger`], if that is more): memory
//! is traded for fewer cycles, and a program that keeps more alive is marked
//! less often for it.
//!
//! Between cycles, the sparsest blocks are **evacuated** a block or two per
//! pause: their survivors copied somewhere denser and the blocks given back.
//! Finding every pointer to a moved object without tracing the heap takes
//! lists the marker and the barriers keep -- see [`crate::evacuate`].
//! [`GcConfig::evacuate`] turns it off: less memory given back, and a slightly
//! shorter tail of pauses.
//!
//! # The remembered set
//!
//! A nursery collection does not look at the old generation, so it has to be
//! told where the old generation points into the nursery: those fields are its
//! remembered set, and it treats them as roots. Such a pointer can only be made
//! three ways -- promoting an object whose field points at something that stays
//! young, allocating a large object straight into the old generation, and
//! overwriting a field of a `Ref`, a mutable array or a resumption -- and each
//! of those checks and records. Data in Meadow is immutable, so the barrier on
//! an ordinary field write, which a generational collector for Java pays on
//! every assignment, is not there at all.
//!
//! # The rule the rest of the runtime has to obey
//!
//! Objects in the nursery move. An [`Addr`] held anywhere the collector cannot
//! see becomes wrong the moment a collection happens -- so the VM never holds
//! one across an allocation. It calls [`crate::Vm::ensure`] first, with room
//! for everything an operation will build, and only then reads its arguments
//! out of registers. Registers are roots; Rust locals are not.
//!
//! # Layout
//!
//! Every slot is one word. An object is a header, then its fields, each the
//! bits of a value and nothing else; what each field holds is a descriptor in
//! the header -- see [`crate::object`]:
//!
//! ```text
//!   addr        kind, len, and for an array its one descriptor
//!   addr+1      meta, and the first fields' descriptors
//!   ...         more descriptors, for a wide object
//!   addr+h      field 0
//!   ...
//! ```
//!
//! `meta` is per-kind: a constructor tag, a method table, a sign. A collector
//! finds addresses by descriptor, the same way for every kind, so there are no
//! per-kind tracing rules. What this module hands out and takes in is still a
//! [`Value`], which is a field's word and its descriptor together.
//!
//! An address says where it points by its range: below [`OLD_BASE`] the
//! nursery, below [`REGION_BASE`] the old generation, and above that a region.
//!
//! # Compact regions
//!
//! `compact` moves a structure out of the heap into a **region** -- see
//! [`crate::region`] -- which no heap owns and no collector copies or scans. A
//! heap only refers to the regions it has addresses into: an address into one,
//! or a [`Kind::Compact`] handle naming it, found while collecting keeps the
//! heap's reference, and a region a whole collection did not reach is let go.
//! A region several heaps refer to is read by all of them, with no copying,
//! which is what lets compacted data, and a `TVar`'s value, be shared between
//! threads. With an old generation in use, only a finished marking cycle has
//! seen everything, so only that lets a region go.
//!
//! # The other collector
//!
//! [`Collector::Copying`] is the nursery alone, growing without limit and
//! never promoting: one Cheney semispace collector, as this heap was before it
//! had generations. It is there to compare against.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::evacuate::Evacuation;
use crate::mark;
use crate::object::{self, Head, forward, forwarded, write_header};
use crate::old::{self, OLD_BASE, Old};
use crate::region::{Block, Region};
use crate::value::{Addr, Value, Word};
use meadow_rt::desc;

pub use crate::region::REGION_BASE;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Bytes one slot takes -- what `compactSize` multiplies by. The other engines
/// estimate with `meadow_core::compact::SLOT_BYTES`, which a test holds equal.
pub const SLOT_BYTES: usize = std::mem::size_of::<Word>();

// --- configuration ---------------------------------------------------------------

/// Which collector heaps use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collector {
    /// A nursery, and an old generation marked concurrently. The default.
    Generational,
    /// One semispace, copied whole at every collection.
    Copying,
}

#[derive(Debug, Clone, Copy)]
pub struct GcConfig {
    pub collector: Collector,
    /// The nursery's largest size, in slots: what bounds a nursery pause.
    ///
    /// The default, 2 MiB, is a measured compromise and not an obvious number.
    /// A nursery collection costs what *survives* it, so a bigger nursery buys
    /// throughput -- fewer collections -- and spends tail latency, and past a
    /// point it spends throughput too, because a long-lived object sitting in a
    /// large nursery is copied again at every collection instead of being
    /// promoted out of it. On `benches/Latency` and `benchmarks/binarytrees`:
    ///
    /// ```text
    ///   nursery   Latency p99   over 1 ms   binarytrees   copied
    ///    256 KiB       115 us           0        3.72 s   926 MiB
    ///      2 MiB       459 us           0        2.75 s     -
    ///      8 MiB      1.31 ms           0        2.14 s   450 MiB
    ///     32 MiB      5.24 ms          79        3.40 s   5.3 GiB
    /// ```
    ///
    /// 2 MiB is the largest that keeps every pause well under a millisecond,
    /// which is the property `docs/RUNTIME.md` claims. `MEADOW_GC_NURSERY`
    /// moves it, in slots, for a program that would rather have the throughput.
    pub nursery: usize,
    /// OS threads marking old generations. With 0, each heap marks a slice at
    /// a time on its own thread, during nursery collections.
    pub mark_threads: usize,
    /// Old-generation slots allocated before a heap's first marking cycle.
    pub min_trigger: usize,
    /// After every marking cycle, check that it marked everything reachable.
    /// Slow: a whole trace in the pause. For tests.
    pub verify: bool,
    /// The most fields a pause reads marking, besides the time limit. For
    /// tests, to make marking take many collections.
    pub mark_slice: usize,
    /// Whether sparse old blocks are evacuated -- see [`crate::evacuate`].
    pub evacuate: bool,
}

impl GcConfig {
    /// The defaults, as `MEADOW_GC` (`generational` or `copying`),
    /// `MEADOW_GC_NURSERY`, `MEADOW_GC_MARK_THREADS`, `MEADOW_GC_TRIGGER` (in
    /// slots), `MEADOW_GC_EVACUATE` (`0` for off) and `MEADOW_GC_VERIFY` change
    /// them.
    pub fn from_env() -> GcConfig {
        let num = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
        };
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        GcConfig {
            collector: match std::env::var("MEADOW_GC").as_deref() {
                Ok("copying") => Collector::Copying,
                _ => Collector::Generational,
            },
            nursery: num("MEADOW_GC_NURSERY").unwrap_or(1 << 18).max(64),
            mark_threads: num("MEADOW_GC_MARK_THREADS").unwrap_or((cores / 4).max(1)),
            min_trigger: num("MEADOW_GC_TRIGGER").unwrap_or(1 << 18),
            verify: std::env::var_os("MEADOW_GC_VERIFY").is_some(),
            mark_slice: usize::MAX,
            evacuate: std::env::var("MEADOW_GC_EVACUATE").as_deref() != Ok("0"),
        }
    }
}

static CONFIG: OnceLock<GcConfig> = OnceLock::new();

/// Set how heaps made from now on collect. Only before the first heap is made:
/// false, and nothing changed, after.
pub fn configure(config: GcConfig) -> bool {
    CONFIG.set(config).is_ok()
}

/// How heaps collect: what [`configure`] set, or [`GcConfig::from_env`].
pub fn config() -> GcConfig {
    *CONFIG.get_or_init(GcConfig::from_env)
}

/// Overwrites logged before they are handed to the marker regardless of
/// whether a collection has come.
const SATB_FLUSH: usize = 4096;

/// How long a pause may spend marking, when there is no marking thread or the
/// one there is has fallen behind.
const ASSIST: Duration = Duration::from_micros(200);

/// How much a pause may spend evacuating sparse old blocks: a time, and slots
/// copied.
const EVACUATE: (Duration, usize) = (Duration::from_micros(25), 1 << 11);

/// Blocks looked at, per nursery collection, for ones to give back.
const RELEASE_BUDGET: usize = 64;

/// A collection that keeps alive less than one part in this of the nursery has
/// so little to copy that how it copies hardly matters: see [`Heap::tenure`].
const TENURE_LITTLE: usize = 16;

/// How many slots a fresh heap starts with. It doubles whenever a collection
/// leaves it more than half full.
const INITIAL: usize = 1 << 16;

/// How many a green thread's heap starts with. A thread may be one of very many,
/// so it starts small and grows like any other heap.
pub const THREAD_INITIAL: usize = 1 << 10;

// --- values that cannot go somewhere ------------------------------------------

/// Why a value cannot go into a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uncompactable {
    Ref,
    MutArray,
    Function,
}

impl Uncompactable {
    pub fn describe(self) -> &'static str {
        match self {
            Uncompactable::Ref => "a Ref",
            Uncompactable::MutArray => "a mutable array",
            Uncompactable::Function => "a function",
        }
    }
}

/// Why a value cannot be passed to another thread -- see `meadow_core::thread`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsendable {
    Ref,
    MutArray,
    Continuation,
}

impl Unsendable {
    pub fn describe(self) -> &'static str {
        match self {
            Unsendable::Ref => "a Ref",
            Unsendable::MutArray => "a mutable array",
            Unsendable::Continuation => "a continuation",
        }
    }
}

/// A value lifted out of one heap, to be put into another: the objects it
/// reaches, laid out as they will be, with addresses counted from the start
/// of the parcel. Holds nothing that belongs to a heap, so it can travel
/// between OS threads. What it reaches in shared regions stays where it is,
/// its addresses unchanged, and the parcel holds those regions alive.
#[derive(Clone)]
pub struct Parcel {
    slots: Vec<Word>,
    root: Value,
    regions: Vec<Arc<Region>>,
}

impl std::fmt::Debug for Parcel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Parcel({} slots, {} regions)",
            self.slots.len(),
            self.regions.len()
        )
    }
}

impl Parcel {
    /// Slots it takes in the heap it is imported into.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

/// A region a heap has addresses into.
struct Adopted {
    region: Arc<Region>,
    /// How many of its blocks this heap has in [`Heap::blocks`]. A region only
    /// grows, and what an address reached when it arrived was in these.
    blocks: usize,
    marked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// A constructor. `meta` is its tag; a tuple is the constructor `#tuple`.
    Data,
    /// The builtin `Array`.
    Array,
    /// Fields as `[label, value, label, value, …]`, sorted by label.
    ///
    /// Sorted, not in the order they were written, so that two records with the
    /// same fields compare and print alike however they were built — the same
    /// reason the CEK machine keeps them in a `BTreeMap`.
    Record,
    /// A closure, a continuation or a handler — the IR does not distinguish
    /// them. `meta` is the method table; the fields are the captures.
    Closure,
    /// A mutable cell: one field, and a value with identity.
    Ref,
    /// A mutable array, from `stNewArray`: its elements, written in place. The
    /// other value with identity.
    MutArray,
    /// `meta` is the sign (0 zero, 1 plus, 2 minus); the fields are base-2^32
    /// digits, least significant first, held as `Int`s.
    BigInt,
    /// A resumption's one-shot flag: `[taken]`. The resumption itself is a
    /// closure capturing it first -- see `meadow_seq`'s lowering -- so a
    /// resumption refused passage to another thread or into a region is refused
    /// as a continuation.
    Resume,
    /// A handle on a compact region: `meta` is the region, the one field is the
    /// value compacted into it. What keeps a region alive when the value itself
    /// is an immediate and points nowhere.
    Compact,
    /// A channel between green threads: `meta` is its number in the scheduler,
    /// and there are no fields. The channel itself lives outside every heap.
    Channel,
    /// A green thread, as `threadSpawn` answers it: `meta` is its number.
    Task,
    /// A `TVar`: `meta` is its number in the run's [`crate::stm::World`].
    TVar,
    /// A `String`: `meta` is its length in bytes, and the fields are its UTF-8
    /// bytes packed eight to a word, little-endian, with the last word's spare
    /// bytes zero -- so two strings are equal exactly when their words are.
    /// Uniform, and never an address, so a collector copies one without
    /// looking inside.
    Str,
    /// An `Array` of `UInt8`, a byte to an element: laid out as [`Kind::Str`]
    /// is, `meta` its length. An array is kept this way whenever it has
    /// elements and they are bytes -- see [`Heap::alloc_array`] -- and
    /// everything that reads an array reads either.
    Bytes,
    /// A function's continuation, on the thread's frame stack rather than in
    /// the heap: laid out as a [`Kind::Closure`] -- `meta` the method table,
    /// the fields the captures -- so that invoking it is invoking a closure,
    /// plus popping it and everything above it. Only [`Heap::push_frame`]
    /// makes one, and only in a chunk the old generation holds in state
    /// `STACK`.
    Frame,
    /// A stack segment a general handler clause cut off: `[top, fsp]`, the
    /// segment's top chunk and where its frames end, both as `Int`s. What a
    /// resumption holds instead of the continuation itself, and puts back
    /// exactly once -- see [`Heap::detach`] and [`Heap::reattach`].
    Stack,
}

impl Kind {
    const ALL: [Kind; 16] = [
        Kind::Data,
        Kind::Array,
        Kind::Record,
        Kind::Closure,
        Kind::Ref,
        Kind::MutArray,
        Kind::BigInt,
        Kind::Resume,
        Kind::Compact,
        Kind::Channel,
        Kind::Task,
        Kind::TVar,
        Kind::Str,
        Kind::Bytes,
        Kind::Frame,
        Kind::Stack,
    ];

    /// The kind a header's first byte names.
    pub fn from_byte(b: u8) -> Kind {
        Kind::try_from_byte(b).unwrap_or_else(|| panic!("{b} is not a kind of object"))
    }

    pub fn try_from_byte(b: u8) -> Option<Kind> {
        Kind::ALL.get(b as usize).copied()
    }

    /// Does every field hold the same representation, so that one descriptor
    /// describes them all? An array's elements, a `BigInt`'s digits.
    /// Is this one of the two ways an `Array` is kept?
    pub fn is_array(self) -> bool {
        matches!(self, Kind::Array | Kind::Bytes)
    }

    pub fn is_uniform(self) -> bool {
        matches!(
            self,
            Kind::Array | Kind::MutArray | Kind::BigInt | Kind::Str | Kind::Bytes
        )
    }
}

/// `repr(C)`, with what native code allocating in the nursery reads and writes
/// first -- see [`crate::codegen::layout`].
#[repr(C)]
pub struct Heap {
    /// `space`'s slots and length, kept equal to it for native code, which
    /// cannot look inside a `Vec`: see [`Heap::sync`].
    pub(crate) base: *mut Word,
    pub(crate) cap: usize,
    pub(crate) top: usize,
    pub allocated: u64,
    /// Region slots written since the last collection. Regions are only freed
    /// by collecting, so a program that compacts in a loop and allocates little
    /// else needs this to ask for a collection now and then.
    pub(crate) region_growth: usize,
    /// Where native code reads a heap word: `table[a >> 30][(a >> 13) &
    /// 0x1FFFF][a & 8191]`, in slots.
    ///
    /// Two entries, one per generation, because the two are laid out
    /// differently and an expansion has no branch to tell them apart with. The
    /// nursery is one flat array, so its table is `base`, `base + BLOCK`, …;
    /// the old generation is separate blocks, so its table is theirs. A heap
    /// word therefore costs three dependent loads in native code against one
    /// through `Heap::slot` and about fifty through the interpreter.
    ///
    /// Kept by [`Heap::sync`], which every path that can move the nursery or
    /// change the old generation's blocks already calls.
    pub(crate) tables: [*const *mut Word; 2],
    /// The frame stack's top and the end of its current chunk, in slots: where
    /// native code pushes a frame. See [`Heap::push_frame`].
    pub(crate) fsp: Addr,
    pub(crate) flim: Addr,
    /// The current chunk of the frame stack, `0` before the first frame, and
    /// its first slot as a machine address: a frame in it is at
    /// `fbase + (a - fcur) * 8`, one load rather than the block table's three.
    pub(crate) fcur: Addr,
    pub(crate) fbase: *mut Word,
    /// `fbase - fcur` slots, so that a frame at `a` in the current chunk is at
    /// `fbias + a * 8`: one load and an add, for every push and return.
    pub(crate) fbias: *mut Word,
    /// Chunks entered so far: the serial the next one gets.
    chunk_serial: u32,
    /// The nursery's half of `tables`, which this owns.
    nursery_blocks: Vec<*mut Word>,
    /// Chunks that were popped, kept for the next push rather than given
    /// back: a `handle` in a loop enters and leaves one at every iteration.
    free_chunks: Vec<Addr>,
    /// Stack segments cut off by a general handler clause and not yet put
    /// back, each with the marking epoch it was cut in. Their frames are roots
    /// until the object naming them dies -- see [`Heap::finish_cycle`].
    detached: Vec<Detached>,
    /// Chunks of the live stack, below the current one, found at a nursery
    /// collection to hold nothing young -- which, since only the current chunk
    /// is ever pushed into or popped from, they go on holding nothing young
    /// until they are the current chunk again. A nursery collection skips
    /// them: without this it walks every frame of the stack every time, and a
    /// deep recursion that allocates is quadratic. (GHC does the same with a
    /// dirty bit per stack chunk.) See [`Heap::minor`].
    clean_chunks: std::collections::HashSet<Addr>,
    config: GcConfig,
    /// The nursery, and its other half for collecting into.
    space: Vec<Word>,
    other: Vec<Word>,
    /// Nursery objects below this have survived a collection already.
    aged: usize,
    /// Whether what survives a nursery collection is promoted at once rather
    /// than kept young for one more: see [`Heap::tenure`].
    tenure: bool,
    /// Whether survivors are promoted: once the nursery is as big as it gets.
    promoting: bool,
    old: Old,
    /// Old slots that may hold a nursery address, each once.
    remembered: Vec<Addr>,
    /// Old values overwritten while marking, not yet handed to the marker.
    satb: Vec<Addr>,
    /// Sparse old blocks being emptied, and the pointers into them.
    evac: Evacuation,
    marking: Option<Arc<mark::Job>>,
    /// Held to overwrite an old object's field while marking; see
    /// [`crate::mark`].
    mutation: Arc<Mutex<()>>,
    /// Nursery collections so far -- and, in `allocated`, slots allocated in
    /// total: the two numbers worth knowing when a program is unexpectedly
    /// slow.
    pub collections: u64,
    /// Slots copied within the nursery, and promoted out of it, in total.
    pub copied: u64,
    pub promoted: u64,
    /// Marking cycles begun, and time spent marking, on any thread.
    pub cycles: u64,
    pub mark_nanos: u64,
    /// Slots and blocks evacuated out of sparse old blocks.
    pub evacuated: u64,
    pub evacuated_blocks: u64,
    /// Time the program was stopped for the collector, in total and pause by
    /// pause.
    pub gc_nanos: u64,
    pub pauses: crate::pauses::Pauses,
    /// The blocks of every region this heap refers to, sorted by base address:
    /// where a region address is looked up.
    blocks: Vec<Arc<Block>>,
    /// Those regions, by id.
    regions: HashMap<u32, Adopted>,
    /// The instruction the machine is carrying out, so that an allocation can
    /// be charged to it. Kept by [`crate::Vm`], which is the only thing that
    /// knows -- the heap is handed sizes, not program counters.
    ///
    /// Last, with `sites`, and that matters: native code reaches `base`, `cap`,
    /// `top`, `allocated` and `region_growth` by fixed offsets from the start
    /// of this struct (see [`crate::codegen::layout`]), so a field added ahead
    /// of them moves them. `the_layout_is_where_the_machine_is` says so.
    #[cfg(feature = "profile-alloc")]
    pub at: meadow_bytecode::Pc,
    /// What each instruction has allocated. See [`crate::profile::Sites`].
    #[cfg(feature = "profile-alloc")]
    pub sites: crate::profile::Sites,
}

// `base` points into the heap's own `space`, which moves with it.
unsafe impl Send for Heap {}

impl Default for Heap {
    fn default() -> Heap {
        Heap::new()
    }
}

/// An object's words, in order: see [`Heap::words_in`].
pub enum Words<'h> {
    /// A nursery object, read as the run of memory it is.
    Slice(std::slice::Iter<'h, Word>),
    /// Anywhere else, a slot at a time.
    Slots { heap: &'h Heap, at: Addr, end: Addr },
}

impl Iterator for Words<'_> {
    type Item = Word;

    #[inline]
    fn next(&mut self) -> Option<Word> {
        match self {
            Words::Slice(it) => it.next().copied(),
            Words::Slots { heap, at, end } => {
                if at < end {
                    let w = heap.slot(*at);
                    *at += 1;
                    Some(w)
                } else {
                    None
                }
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match self {
            Words::Slice(it) => it.len(),
            Words::Slots { at, end, .. } => end.saturating_sub(*at) as usize,
        };
        (n, Some(n))
    }
}

impl ExactSizeIterator for Words<'_> {}

impl Drop for Heap {
    fn drop(&mut self) {
        if let Some(job) = &self.marking {
            job.cancel();
        }
    }
}

impl Heap {
    pub fn new() -> Heap {
        Heap::with_capacity(INITIAL)
    }

    /// A heap of `slots` to begin with.
    pub fn with_capacity(slots: usize) -> Heap {
        Heap::with_config(slots, config())
    }

    pub fn with_config(slots: usize, config: GcConfig) -> Heap {
        let slots = match config.collector {
            Collector::Copying => slots,
            Collector::Generational => slots.min(config.nursery),
        };
        let mut heap = Heap {
            base: std::ptr::null_mut(),
            cap: 0,
            config,
            space: vec![0; slots.max(64)],
            other: Vec::new(),
            top: 0,
            aged: 0,
            tenure: false,
            promoting: false,
            old: Old::default(),
            remembered: Vec::new(),
            satb: Vec::new(),
            evac: Evacuation::default(),
            marking: None,
            mutation: Arc::new(Mutex::new(())),
            collections: 0,
            allocated: 0,
            copied: 0,
            promoted: 0,
            cycles: 0,
            mark_nanos: 0,
            evacuated: 0,
            evacuated_blocks: 0,
            gc_nanos: 0,
            pauses: Default::default(),
            blocks: Vec::new(),
            regions: HashMap::new(),
            #[cfg(feature = "profile-alloc")]
            at: 0,
            #[cfg(feature = "profile-alloc")]
            sites: crate::profile::Sites::default(),
            region_growth: 0,
            tables: [std::ptr::null(); 2],
            fsp: 0,
            flim: 0,
            fcur: 0,
            fbase: std::ptr::null_mut(),
            fbias: std::ptr::null_mut(),
            chunk_serial: 0,
            nursery_blocks: Vec::new(),
            free_chunks: Vec::new(),
            detached: Vec::new(),
            clean_chunks: std::collections::HashSet::new(),
        };
        heap.sync();
        heap
    }

    /// Bring native code's view of the nursery up to date, after `space` may
    /// have moved or grown.
    #[inline]
    fn sync(&mut self) {
        self.base = self.space.as_mut_ptr();
        self.cap = self.space.len();
        // The nursery a block at a time, so that native code can read it with
        // the same three loads it reads the old generation with. Rebuilt whole
        // rather than patched: it is one entry per 64 KiB, so a 2 MiB nursery
        // is thirty-two of them.
        let blocks = self.cap.div_ceil(old::BLOCK);
        self.nursery_blocks.clear();
        self.nursery_blocks.reserve(blocks + 1);
        for i in 0..blocks {
            // Safety: `i * BLOCK <= cap`, so this is inside `space` or one
            // past its end, which is what a table entry for the last partial
            // block has to be.
            self.nursery_blocks
                .push(unsafe { self.base.add(i * old::BLOCK) });
        }
        // An empty nursery still needs an entry: address 0 is in block 0.
        if self.nursery_blocks.is_empty() {
            self.nursery_blocks.push(self.base);
        }
        self.publish_tables();
    }

    /// Make [`Heap::tables`] current.
    ///
    /// The old generation's half is a `Vec` that reallocates as blocks are
    /// added, and blocks are added by promotion, by a large allocation and by
    /// evacuation -- none of which move the nursery, so none of which reach
    /// [`Heap::sync`]. Native code reads the table's address from here at every
    /// access, so what has to hold is only that this is called before control
    /// returns to native code. There is exactly one way back in --
    /// `crate::abi::exec`, which every instruction the interpreter runs for
    /// native code goes through -- and one way in to begin with, so both call
    /// this, and no path that changes the blocks has to remember to. The bug
    /// this closes was a `wordfreq` reading a freed table entry and crashing on
    /// the address it found there.
    pub(crate) fn publish_tables(&mut self) {
        self.tables = [self.nursery_blocks.as_ptr(), self.old.bases_ptr()];
        self.publish_chunk();
    }

    /// Make `fbase` the current chunk's: after anything that changes which
    /// chunk is current, or where the blocks are.
    fn publish_chunk(&mut self) {
        self.fbase = if self.fcur == 0 {
            std::ptr::null_mut()
        } else {
            self.old.base_ptr(self.fcur)
        };
        // Only ever added to an address in the current chunk, which brings it
        // back inside the chunk; with no chunk, nothing is.
        self.fbias = self.fbase.wrapping_sub(self.fcur as usize);
    }

    /// Has enough gone into regions since the last collection that dead ones
    /// should be looked for, even though the nursery has room?
    pub fn wants_collection(&self) -> bool {
        self.region_growth > self.space.len() || self.overdue()
    }

    /// Has the old generation outrun its marking so far that the next
    /// collection has to finish a whole cycle, pause or not?
    ///
    /// Marking runs beside the program, and what is allocated while a cycle
    /// marks is kept until the next one. A program making objects too large
    /// for the nursery -- each goes straight to the old generation -- can make
    /// them faster than a cycle frees them, and used to run the generation out
    /// of addresses and abort with most of it garbage. A long pause is the
    /// better answer.
    pub fn overdue(&self) -> bool {
        if self.config.collector != Collector::Generational || self.old.is_empty() {
            return false;
        }
        let live = self.old.live as usize;
        let trigger = (2 * live).max(self.config.min_trigger);
        self.old.half_full() || self.old.allocated as usize > 4 * trigger
    }

    /// Slots held by live regions, in total.
    pub fn region_slots(&self) -> usize {
        self.regions.values().map(|r| r.region.used()).sum()
    }

    /// Slots in use: the nursery's, and the old generation's as of the last
    /// cycle plus what has been allocated since.
    pub fn used(&self) -> usize {
        self.top + (self.old.live + self.old.allocated) as usize
    }

    /// Slots there are, in the nursery and the old generation.
    pub fn capacity(&self) -> usize {
        self.space.len() + self.old.capacity()
    }

    /// Slots in the old generation's blocks.
    pub fn old_slots(&self) -> usize {
        self.old.capacity()
    }

    /// Slots the last marking cycle found alive in the old generation.
    pub fn old_live(&self) -> usize {
        self.old.live as usize
    }

    /// Is there room for `slots` in the nursery? What decides whether an
    /// allocation has to collect first.
    pub fn room_for(&self, slots: usize) -> bool {
        self.top + slots <= self.space.len()
    }

    /// Make sure `slots` can be allocated without collecting.
    ///
    /// For what a collection cannot help with: one object larger than the
    /// whole nursery, which a program building a big array does routinely.
    /// The copying collector grows its one space; a generational heap grows its
    /// nursery as far as it may, and past that allocates in the old generation.
    pub fn reserve(&mut self, slots: usize) {
        if self.top + slots <= self.space.len() {
            return;
        }
        let limit = match self.config.collector {
            Collector::Copying => OLD_BASE as usize,
            Collector::Generational => self.config.nursery,
        };
        if self.top + slots > limit {
            match self.config.collector {
                Collector::Copying => panic!("the heap has used up its address space"),
                Collector::Generational => {
                    self.promoting = true;
                    return;
                }
            }
        }
        let mut size = self.space.len();
        while self.top + slots > size {
            size = (size * 2).max(64);
        }
        self.space.resize(size.min(limit), 0);
        self.sync();
    }

    /// Allocate. The caller must have checked [`Heap::room_for`], or
    /// [`Heap::reserve`]d — this is the bump, and it is the reason allocation is
    /// cheap enough not to think about.
    #[inline]
    /// Is this heap checking its own work -- `MEADOW_GC_VERIFY`?
    pub(crate) fn verifying(&self) -> bool {
        self.config.verify
    }

    /// Slots an object of `kind` with `len` fields takes.
    pub fn size_of(kind: Kind, len: usize) -> usize {
        meadow_rt::compact::object_slots(kind.is_uniform(), len)
    }

    pub fn alloc(&mut self, kind: Kind, meta: u32, fields: &[Value]) -> Addr {
        let at = self.top;
        let header = meadow_rt::compact::header_slots(kind.is_uniform(), fields.len());
        let size = header + fields.len();
        if at + size > self.space.len() {
            return self.alloc_old(kind, meta, fields);
        }
        let space = &mut self.space;
        write_header(kind, meta, fields.iter().map(|v| v.desc()), |k, w| {
            space[at + k] = w
        });
        for (i, v) in fields.iter().enumerate() {
            space[at + header + i] = v.bits();
        }
        self.top += size;
        self.allocated += size as u64;
        #[cfg(feature = "profile-alloc")]
        self.sites.note(self.at, size);
        at as Addr
    }

    /// Allocate an object of `len` fields, field `i` the word `word(i)` of
    /// descriptor `desc(i)`: [`Heap::alloc`] for a caller holding words, which
    /// need not be made values first.
    pub fn alloc_described(
        &mut self,
        kind: Kind,
        meta: u32,
        len: usize,
        word: impl Fn(usize) -> Word,
        desc: impl Fn(usize) -> desc::Desc,
    ) -> Addr {
        let at = self.top;
        let header = meadow_rt::compact::header_slots(kind.is_uniform(), len);
        let size = header + len;
        if at + size > self.space.len() {
            let fields: Vec<Value> = (0..len)
                .map(|i| Value::from_bits(word(i), desc(i)))
                .collect();
            return self.alloc_old(kind, meta, &fields);
        }
        let space = &mut self.space;
        write_header(kind, meta, (0..len).map(&desc), |k, w| space[at + k] = w);
        for i in 0..len {
            space[at + header + i] = word(i);
        }
        self.top += size;
        self.allocated += size as u64;
        #[cfg(feature = "profile-alloc")]
        self.sites.note(self.at, size);
        at as Addr
    }

    /// An object that does not fit in the nursery, in the old generation.
    #[cold]
    fn alloc_old(&mut self, kind: Kind, meta: u32, fields: &[Value]) -> Addr {
        let size = Heap::size_of(kind, fields.len());
        if self.config.collector == Collector::Copying {
            self.reserve(size);
            return self.alloc(kind, meta, fields);
        }
        self.promoting = true;
        let at = self.old.alloc(size, self.marking.is_some());
        let old = &self.old;
        write_header(kind, meta, fields.iter().map(|v| v.desc()), |k, w| {
            old.put(at + k as Addr, w)
        });
        let header = (size - fields.len()) as Addr;
        for (i, v) in fields.iter().enumerate() {
            self.init_old(at + header + i as Addr, *v);
        }
        self.allocated += size as u64;
        at
    }

    /// Write a field of an old object nothing else has seen yet: no marker can
    /// be reading it, and it held nothing to log.
    fn init_old(&mut self, s: Addr, v: Value) {
        self.old.put_field(s, v.bits(), v.addr().is_some());
        self.barriers(s, v);
    }

    /// Old slot `s` has just been given `v`: remember it if it points into the
    /// nursery, and record it if it points into a block chosen for evacuation.
    #[inline]
    fn barriers(&mut self, s: Addr, v: Value) {
        if let Value::Obj(x) = v {
            if x < OLD_BASE {
                if self.old.remember(s) {
                    self.remembered.push(s);
                }
            } else {
                self.evac.note(&self.old, s, x);
            }
        }
    }

    /// The slot at `a`, in the nursery, the old generation or a region.
    #[inline(always)]
    /// Is `a` somewhere an object could be?
    ///
    /// For [`crate::profile`], which follows a chain of continuations and must
    /// never fault while doing it: a profile that can bring down the program it
    /// is watching is worse than no profile. Conservative -- it says only that
    /// reading the header will not go outside the heap, not that anybody put an
    /// object there.
    pub fn holds_object(&self, a: Addr) -> bool {
        if a < OLD_BASE {
            let i = a as usize;
            // In the part of the nursery that has been allocated, and starting
            // with a byte that names a kind. Both are needed: a chain of
            // continuations ends at something that is not one, and what that
            // something is depends on where the chain was walked from.
            return i + 1 < self.top && Kind::try_from_byte(self.space[i] as u8).is_some();
        }
        if a < REGION_BASE {
            return self.old.is_object(a);
        }
        false
    }

    /// The heap word at slot `a`, wherever it lives.
    ///
    /// For [`crate::codegen::thin`], whose steps address the heap as the array
    /// of words the machine holds it as.
    pub fn word_at(&self, a: Addr) -> Word {
        self.slot(a)
    }

    /// Where slot `a` is in memory: what native code's `Locate` step finds
    /// through the block tables, for the thin machine to find the same way.
    /// Not for a region, whose blocks are found by search.
    pub(crate) fn machine_addr(&self, a: Addr) -> *mut Word {
        if a < OLD_BASE {
            self.space.as_ptr().wrapping_add(a as usize) as *mut Word
        } else {
            self.old.base_ptr(a).wrapping_add(old::offset_of(a))
        }
    }

    /// Set the `meta` of the object at `a`, keeping the rest of its header.
    /// For a handler's target `Ref`, whose `meta` is otherwise unused: the tag
    /// of the chunk its `handle` entered.
    pub fn set_meta(&mut self, a: Addr, meta: u32) {
        let w = self.slot(a + 1) & !0xFFFF_FFFF | meta as Word;
        if a < OLD_BASE {
            self.space[a as usize + 1] = w;
        } else {
            let _held = self.marking.is_some().then(|| lock(&self.mutation));
            self.old.put(a + 1, w);
        }
    }

    /// Write the heap word at slot `a`, and **nothing else**: no descriptor,
    /// no pointer bit, no remembered set, no snapshot barrier.
    ///
    /// This is what a [`crate::codegen::thin::Step::Store`] means, and it is
    /// only safe because an expansion that uses one has guarded that none of
    /// those is needed -- the object is a uniform array whose elements are not
    /// references, so its descriptor cannot change, nothing in it is ever
    /// followed by the collector, and no field of it can hold a young address.
    /// Anything else goes through [`Heap::set_field_of`].
    pub(crate) fn put_word_at(&mut self, a: Addr, w: Word) {
        if a < OLD_BASE {
            self.space[a as usize] = w;
        } else {
            debug_assert!(a < REGION_BASE, "a region is immutable");
            self.old.put(a, w);
        }
    }

    fn slot(&self, a: Addr) -> Word {
        if a < OLD_BASE {
            self.space[a as usize]
        } else if a < REGION_BASE {
            self.old.get(a)
        } else {
            self.blocks[Self::find_block(&self.blocks, a)].get(a)
        }
    }

    fn find_block(blocks: &[Arc<Block>], a: Addr) -> usize {
        let i = blocks.partition_point(|b| b.base <= a);
        debug_assert!(i > 0, "{a} is below every region block this heap knows");
        i - 1
    }

    #[inline(always)]
    /// An object's header.
    ///
    /// Cheap for a nursery object -- two slot reads -- and not at all cheap for
    /// an old one, where every slot read is a block lookup. A caller that needs
    /// the kind, then the length, then a field should read this once and use
    /// [`Heap::field_of`] and [`Heap::set_field_of`], rather than three
    /// accessors that each read it again. See `prims.rs`.
    pub fn head(&self, a: Addr) -> Head {
        // The nursery, where nearly every object read is, with one test.
        if a < OLD_BASE {
            let i = a as usize;
            return Head::read(self.space[i], self.space[i + 1]);
        }
        Head::read(self.slot(a), self.slot(a + 1))
    }

    /// Field `i`'s descriptor, in the object at `a` whose header is `h`.
    #[inline]
    fn desc(&self, a: Addr, h: &Head, i: usize) -> desc::Desc {
        h.desc(i, |k| self.slot(a + k as Addr))
    }

    /// Is there an object at `a`? For a debugger reading an address it cannot
    /// vouch for; the machine itself never needs to ask. Exact in the nursery,
    /// which is walked to find out; elsewhere, whether a header could be there.
    pub fn is_object(&self, a: Addr) -> bool {
        if a < OLD_BASE {
            return self.nursery_objects().any(|at| at == a);
        }
        if a < REGION_BASE {
            return self.old.is_object(a);
        }
        let i = self.blocks.partition_point(|b| b.base <= a);
        if i == 0 {
            return false;
        }
        let b = &self.blocks[i - 1];
        (a - b.base) + 1 < b.cap as Addr && Kind::try_from_byte(b.get(a) as u8).is_some()
    }

    /// Where every object in the nursery starts, in order.
    fn nursery_objects(&self) -> impl Iterator<Item = Addr> + '_ {
        let mut at = 0usize;
        std::iter::from_fn(move || {
            (at < self.top).then(|| {
                let here = at;
                at += Head::read(self.space[at], self.space[at + 1]).size();
                here as Addr
            })
        })
    }

    /// Is `a` an address in a compact region?
    pub fn in_region(&self, a: Addr) -> bool {
        a >= REGION_BASE
    }

    pub fn kind(&self, a: Addr) -> Kind {
        self.head(a).kind
    }

    pub fn len(&self, a: Addr) -> usize {
        self.head(a).len as usize
    }

    pub fn meta(&self, a: Addr) -> u32 {
        self.head(a).meta
    }

    pub fn field(&self, a: Addr, i: usize) -> Value {
        self.field_of(a, &self.head(a), i)
    }

    /// [`Heap::field`], for a caller that has already read the header.
    pub fn field_of(&self, a: Addr, h: &Head, i: usize) -> Value {
        debug_assert!(
            i < h.len as usize,
            "field {i} of a {:?} of {}",
            h.kind,
            h.len
        );
        let w = self.slot(a + (h.header() + i) as Addr);
        Value::from_bits(w, self.desc(a, h, i))
    }

    /// What an array's elements are: the one descriptor a uniform object
    /// has for all of them.
    pub fn element_desc(&self, a: Addr) -> desc::Desc {
        let h = self.head(a);
        debug_assert!(
            h.kind.is_uniform(),
            "a {:?} has no one element descriptor",
            h.kind
        );
        self.desc(a, &h, 0)
    }

    /// Every field's word, in order, without saying what they are.
    pub fn words(&self, a: Addr) -> Words<'_> {
        self.words_in(a, 0, usize::MAX)
    }

    /// The fields of the object at `a`, in place -- when it is in the nursery,
    /// which is where nearly every object a program is computing with lives.
    /// `None` in the old generation or a region, where the slots are not one
    /// run of memory to borrow.
    #[inline]
    pub fn nursery_words(&self, a: Addr) -> Option<&[Word]> {
        if a >= OLD_BASE {
            return None;
        }
        let h = self.head(a);
        let base = a as usize + h.header();
        self.space.get(base..base + h.len as usize)
    }

    // --- strings and bytes -------------------------------------------------------
    //
    // A string and a byte array are laid out alike: bytes packed eight to a
    // word, the length in `meta`. What reads one reads the other.

    /// Slots `len` packed bytes take.
    pub fn packed_slots(len: usize) -> usize {
        Heap::size_of(Kind::Str, len.div_ceil(8))
    }

    /// A string holding `bytes`, which must be UTF-8 -- see [`Kind::Str`].
    pub fn alloc_str(&mut self, bytes: &[u8]) -> Addr {
        self.alloc_packed(Kind::Str, bytes)
    }

    /// `bytes` as an `Array` of `UInt8` -- see [`Kind::Bytes`]. Empty, it is an
    /// ordinary empty array, which is what every empty array is.
    pub fn alloc_bytes(&mut self, bytes: &[u8]) -> Addr {
        if bytes.is_empty() {
            return self.alloc(Kind::Array, 0, &[]);
        }
        self.alloc_packed(Kind::Bytes, bytes)
    }

    fn alloc_packed(&mut self, kind: Kind, bytes: &[u8]) -> Addr {
        let words = bytes.len().div_ceil(8);
        let word = |i: usize| {
            let chunk = &bytes[8 * i..bytes.len().min(8 * i + 8)];
            let mut w = [0u8; 8];
            w[..chunk.len()].copy_from_slice(chunk);
            Word::from_le_bytes(w)
        };
        self.alloc_described(kind, bytes.len() as u32, words, word, |_| desc::INT)
    }

    // --- arrays, either way they are kept -------------------------------------------

    /// Slots an array of `n` elements described by `d` takes.
    pub fn array_slots(n: usize, d: desc::Desc) -> usize {
        if n > 0 && d == Heap::BYTE {
            Heap::packed_slots(n)
        } else {
            Heap::size_of(Kind::Array, n)
        }
    }

    /// The descriptor of a `UInt8`, whose arrays are packed.
    pub const BYTE: desc::Desc = desc::WORD + 3;

    /// An array of `words`, each described by `d`: packed, if they are bytes.
    pub fn alloc_array(&mut self, words: &[Word], d: desc::Desc) -> Addr {
        if !words.is_empty() && d == Heap::BYTE {
            let bytes: Vec<u8> = words.iter().map(|w| *w as u8).collect();
            return self.alloc_packed(Kind::Bytes, &bytes);
        }
        self.alloc_described(Kind::Array, 0, words.len(), |i| words[i], |_| d)
    }

    /// How many elements the array at `a` has.
    pub fn array_len(&self, a: Addr) -> usize {
        let h = self.head(a);
        match h.kind {
            Kind::Bytes => h.meta as usize,
            _ => h.len as usize,
        }
    }

    /// What every element of the array at `a` is.
    pub fn array_desc(&self, a: Addr) -> desc::Desc {
        match self.kind(a) {
            Kind::Bytes => Heap::BYTE,
            _ => self.element_desc(a),
        }
    }

    /// Element `i` of the array at `a`, which has one, as a word.
    pub fn array_word(&self, a: Addr, i: usize) -> Word {
        match self.kind(a) {
            Kind::Bytes => self.packed_byte(a, i) as Word,
            _ => self.field_word(a, i),
        }
    }

    /// Element `i` of the array at `a`, which has one.
    pub fn array_value(&self, a: Addr, i: usize) -> Value {
        match self.kind(a) {
            Kind::Bytes => Value::Word(meadow_rt::num::Width::U8, self.packed_byte(a, i) as u64),
            _ => self.field(a, i),
        }
    }

    /// Elements `from..to` of the array at `a`, as words.
    pub fn array_words(&self, a: Addr, from: usize, to: usize) -> Vec<Word> {
        match self.kind(a) {
            Kind::Bytes => self
                .packed_bytes_in(a, from, to)
                .into_iter()
                .map(Word::from)
                .collect(),
            _ => self.words_in(a, from, to).collect(),
        }
    }

    /// Every element of the array at `a`.
    pub fn array_values(&self, a: Addr) -> Vec<Value> {
        (0..self.array_len(a))
            .map(|i| self.array_value(a, i))
            .collect()
    }

    /// The length in bytes of the string or byte array at `a`.
    pub fn packed_len(&self, a: Addr) -> usize {
        self.head(a).meta as usize
    }

    /// Byte `i` of the string or byte array at `a`, which has one.
    pub fn packed_byte(&self, a: Addr, i: usize) -> u8 {
        let h = self.head(a);
        debug_assert!(i < h.meta as usize, "byte {i} of a string of {}", h.meta);
        let w = self.slot(a + (h.header() + i / 8) as Addr);
        (w >> (8 * (i % 8))) as u8
    }

    /// Bytes `from..to` of the string at `a`, copied out.
    pub fn packed_bytes_in(&self, a: Addr, from: usize, to: usize) -> Vec<u8> {
        let h = self.head(a);
        let to = to.min(h.meta as usize);
        let from = from.min(to);
        let base = a + h.header() as Addr;
        let mut out = Vec::with_capacity(to - from);
        let mut i = from;
        while i < to {
            let w = self.slot(base + (i / 8) as Addr).to_le_bytes();
            let k = i % 8;
            let take = (8 - k).min(to - i);
            out.extend_from_slice(&w[k..k + take]);
            i += take;
        }
        out
    }

    /// All of the string at `a`'s bytes.
    pub fn packed_bytes(&self, a: Addr) -> Vec<u8> {
        self.packed_bytes_in(a, 0, usize::MAX)
    }

    /// Whether the strings at `a` and `b` hold the same bytes.
    pub fn packed_eq(&self, a: Addr, b: Addr) -> bool {
        let (ha, hb) = (self.head(a), self.head(b));
        ha.meta == hb.meta
            && self
                .words_in(a, 0, ha.len as usize)
                .eq(self.words_in(b, 0, hb.len as usize))
    }

    /// Fields `from..to` of `a`'s words. Not `words(a).skip(from)`, which
    /// reads every field before `from` to throw it away -- and made slicing a
    /// long array cost its whole length each time.
    ///
    /// A nursery object is read as the slice it is. Going through `slot` for
    /// each word -- a range test and a bounds check apiece -- was 44% of adding
    /// two large `BigInt`s, and it is the same loop string comparison runs.
    pub fn words_in(&self, a: Addr, from: usize, to: usize) -> Words<'_> {
        let h = self.head(a);
        let to = to.min(h.len as usize);
        let from = from.min(to);
        if a < OLD_BASE {
            let base = a as usize + h.header();
            if let Some(slice) = self.space.get(base + from..base + to) {
                return Words::Slice(slice.iter());
            }
        }
        let base = a + h.header() as Addr;
        Words::Slots {
            heap: self,
            at: base + from as Addr,
            end: base + to as Addr,
        }
    }

    /// Field `i`'s word, without saying what it is.
    #[inline]
    pub fn field_word(&self, a: Addr, i: usize) -> Word {
        let h = self.head(a);
        self.slot(a + (h.header() + i) as Addr)
    }

    /// Overwrite a field. Only a `Ref`, a mutable array or a resumption is
    /// ever written after it is made, and this is where the barriers are: an
    /// old object's field that now points into the nursery is remembered, and
    /// while marking, the value it held is logged.
    ///
    /// A field of an array, whose one descriptor describes every element, keeps
    /// its representation; any other can change it -- a `()` placeholder
    /// replaced by an object -- and its descriptor changes with it, under the
    /// same lock the marker reads a mutable object's descriptors under.
    pub fn set_field(&mut self, a: Addr, i: usize, v: Value) {
        self.set_field_of(a, &self.head(a), i, v)
    }

    /// [`Heap::set_field`], for a caller that has already read the header.
    pub fn set_field_of(&mut self, a: Addr, h: &Head, i: usize, v: Value) {
        let h = *h;
        let s = a + (h.header() + i) as Addr;
        let was = self.desc(a, &h, i);
        assert!(
            was == v.desc() || !h.kind.is_uniform(),
            "a {:?} of {} given {}",
            h.kind,
            desc::name(was),
            v.kind()
        );
        if a < OLD_BASE {
            self.space[s as usize] = v.bits();
            if was != v.desc() {
                let (k, w) = object::with_desc(i, v.desc(), |k| self.space[a as usize + k]);
                self.space[a as usize + k] = w;
            }
            return;
        }
        debug_assert!(a < REGION_BASE, "a region is immutable");
        let _held = self.marking.is_some().then(|| lock(&self.mutation));
        if self.marking.is_some() && was == desc::REF {
            let x = self.old.get(s) as Addr;
            if x >= OLD_BASE {
                self.satb.push(x);
            }
        }
        self.old.put_field(s, v.bits(), v.addr().is_some());
        if was != v.desc() {
            let (k, w) = object::with_desc(i, v.desc(), |k| self.old.get(a + k as Addr));
            self.old.put(a + k as Addr, w);
        }
        drop(_held);
        self.barriers(s, v);
        if self.satb.len() >= SATB_FLUSH {
            self.flush_satb();
        }
    }

    /// Every field, as a fresh `Vec` — for the places that want to read an
    /// object and then allocate, which cannot hold heap references across the
    /// allocation.
    pub fn fields(&self, a: Addr) -> Vec<Value> {
        let h = self.head(a);
        let base = a + h.header() as Addr;
        (0..h.len as usize)
            .map(|i| Value::from_bits(self.slot(base + i as Addr), self.desc(a, &h, i)))
            .collect()
    }

    // --- collecting -----------------------------------------------------------------

    /// Collect, rewriting `roots` in place.
    ///
    /// The caller passes every address it can still reach — registers, the
    /// handler stack — and reads them back changed. Anything not in `roots` is
    /// garbage by definition, including addresses sitting in Rust locals, which
    /// is why the VM allocates only at points where it holds none.
    ///
    /// This is a nursery collection, and whatever the old generation's marking
    /// needs from a pause: to begin, to be helped along, or to finish.
    pub fn collect(&mut self, roots: &mut [Value]) {
        let started = Instant::now();
        self.collections += 1;
        let marked = self.marking.is_some();
        if marked {
            self.advance(roots);
        }
        // Finishing a cycle has a pause's work already; moving waits a pause.
        let finished = marked && self.marking.is_none();
        let begin = self.marking.is_none() && !self.old.is_empty() && self.wants_cycle();
        self.minor(roots, begin);
        if begin {
            self.begin_cycle(roots);
        } else if self.marking.is_none() && self.promoting {
            if !finished {
                self.evacuate(roots, Some(EVACUATE));
            }
            self.old.release(RELEASE_BUDGET);
        }
        if self.promoting && self.config.mark_threads > 0 {
            mark::tend_spares(self.config.mark_threads);
        }
        self.region_growth = 0;
        let took = started.elapsed().as_nanos() as u64;
        self.gc_nanos += took;
        self.pauses.record(took);
    }

    /// Collect everything now: finish the cycle in progress, if there is one,
    /// then run a whole cycle, marking on this thread. A long pause, for tests
    /// and for a program that has just let go of a great deal.
    pub fn collect_all(&mut self, roots: &mut [Value]) {
        while let Some(job) = self.marking.clone() {
            self.flush_satb();
            job.step(Duration::MAX, usize::MAX);
            if job.is_done() {
                self.finish_cycle(roots);
            }
        }
        self.minor(roots, !self.old.is_empty());
        if !self.old.is_empty() {
            self.begin_cycle(roots);
            let job = self.marking.clone().expect("a cycle just begun");
            while !job.is_done() {
                job.step(Duration::MAX, usize::MAX);
            }
            self.finish_cycle(roots);
        }
        self.evacuate(roots, None);
    }

    /// Move survivors out of sparse old blocks, within `budget` -- see
    /// [`crate::evacuate`]. Only between cycles, with the nursery collected.
    fn evacuate(&mut self, roots: &mut [Value], budget: Option<(Duration, usize)>) {
        if !self.config.evacuate || self.marking.is_some() || !self.evac.has_pending() {
            return;
        }
        let frames = frame_slots(&self.old, self.fcur, self.fsp, &self.detached);
        self.evac.evacuate(
            &mut self.old,
            &mut self.space[..self.top],
            roots,
            &frames,
            &mut self.remembered,
            budget,
        );
        self.evacuated = self.evac.moved;
        self.evacuated_blocks = self.evac.blocks;
        if self.config.verify {
            self.verify_addresses(roots);
        }
    }

    /// Has the old generation grown enough since the last cycle to mark it
    /// again?
    fn wants_cycle(&self) -> bool {
        let live = self.old.live as usize;
        let growth = live.max(self.config.min_trigger.saturating_sub(live));
        self.old.allocated as usize >= growth || self.region_growth > self.space.len()
    }

    /// While marking: hand over the log, help if the marker is behind or there
    /// is none, and finish if it is done.
    fn advance(&mut self, roots: &mut [Value]) {
        let job = self.marking.clone().expect("marking");
        self.flush_satb();
        let live = self.old.live as usize;
        let trigger = (2 * live).max(self.config.min_trigger);
        let behind = self.old.allocated as usize > 2 * trigger;
        if self.config.mark_threads == 0 || behind {
            job.step(ASSIST, self.config.mark_slice);
        }
        if job.is_done() {
            self.finish_cycle(roots);
        }
    }

    fn flush_satb(&mut self) {
        if let Some(job) = &self.marking
            && !self.satb.is_empty()
        {
            job.push(&self.satb);
            self.satb.clear();
            if self.config.mark_threads > 0 {
                mark::submit(job, self.config.mark_threads);
            }
        }
    }

    /// The nursery is empty: start marking from `roots`.
    fn begin_cycle(&mut self, roots: &[Value]) {
        debug_assert_eq!(self.top, 0, "a cycle begins with the nursery empty");
        let epoch = self
            .old
            .epoch()
            .checked_add(1)
            .expect("marking cycles have run out of epochs");
        self.old.begin(epoch);
        self.evac.begin(&self.old);
        for r in self.regions.values_mut() {
            r.marked = false;
        }
        let job = mark::Job::new(
            epoch,
            self.old.blocks.clone(),
            self.blocks.clone(),
            self.mutation.clone(),
        );
        let mut grey = Vec::new();
        for r in roots {
            if let Value::Obj(x) = *r {
                if x >= REGION_BASE {
                    self.mark_region_at(x);
                } else {
                    debug_assert!(x >= OLD_BASE, "a root in the emptied nursery");
                    grey.push(x);
                }
            }
        }
        // The frames, live and detached alike, exactly as the registers: what
        // they reach is in the snapshot. A segment detached after this is
        // black, and one put back is live -- neither needs the marker to look.
        let slots = frame_slots(&self.old, self.fcur, self.fsp, &self.detached);
        for slot in slots {
            let x = self.old.get(slot) as Addr;
            if x >= REGION_BASE {
                self.mark_region_at(x);
            } else if x >= OLD_BASE {
                grey.push(x);
            }
        }
        job.push(&grey);
        if self.config.mark_threads > 0 {
            mark::submit(&job, self.config.mark_threads);
        }
        self.marking = Some(job);
        self.cycles += 1;
    }

    /// Marking is done: adopt what it found.
    fn finish_cycle(&mut self, roots: &[Value]) {
        let job = self.marking.take().expect("marking");
        job.cancel();
        for id in job.found() {
            if let Some(r) = self.regions.get_mut(&id) {
                r.marked = true;
            }
        }
        if self.config.verify {
            self.verify(roots, job.epoch);
        }
        self.old.finish(job.marked());
        if self.config.evacuate {
            self.evac.finish(&self.old);
        }
        self.mark_nanos += job.nanos();

        let unreached: Vec<u32> = self
            .regions
            .iter()
            .filter(|(_, r)| !r.marked)
            .map(|(id, _)| *id)
            .collect();
        for id in unreached {
            self.free_region(id);
        }
        // A detached segment nothing named any more is dead, chunks and all.
        // One cut after the cycle began is black: it stays whatever the marker
        // saw. The marker only reads the old generation, so it can only judge a
        // segment whose object is there and was there unmarked: one whose
        // object is still young -- or not yet made -- is the nursery
        // collection's to judge, and one whose object was promoted while the
        // cycle ran was allocated marked, which the marker never reads. Freeing
        // either left a resumption with nothing to put back ("continuation
        // resumed more than once").
        let named = job.stacks();
        let old = &self.old;
        let mut dead = Vec::new();
        self.detached.retain(|d| {
            let young = d.obj < OLD_BASE;
            let promoted =
                d.obj >= OLD_BASE && d.obj < REGION_BASE && old.is_marked(d.obj, job.epoch);
            let keep = d.epoch >= job.epoch || named.contains(&d.top) || young || promoted;
            if !keep {
                dead.push(d.top);
            }
            keep
        });
        for top in dead {
            self.free_segment(top);
        }
        // A remembered field in a line the cycle freed belongs to a dead
        // object, and the line may be allocated into: forget it.
        let old = &self.old;
        self.remembered.retain(|s| {
            let keep = old.line_live(*s);
            if !keep {
                old.forget(*s);
            }
            keep
        });
    }

    /// Mark the region holding address `a`.
    fn mark_region_at(&mut self, a: Addr) {
        let id = self.blocks[Self::find_block(&self.blocks, a)].region;
        if let Some(r) = self.regions.get_mut(&id) {
            r.marked = true;
        }
    }

    /// Collect the nursery: copy what survives within it, or promote it, and
    /// with `promote_all` promote everything, leaving it empty.
    fn minor(&mut self, roots: &mut [Value], promote_all: bool) {
        let mut to = std::mem::take(&mut self.other);
        // As long as the from-space, and deliberately *not* zeroed. Every slot
        // below `top` is written by the copy below, and nothing reads above it:
        // `nursery_objects` and the verifier both stop at `top`, allocation
        // writes every slot it bumps past, and `alloc_packed` zeroes its own
        // padding word rather than trusting the heap to be clear.
        //
        // Zeroing it cost a memset of the whole nursery on every collection, so
        // a pause cost what the nursery *is* rather than what survived it --
        // which is the wrong scaling for a copying collector, and what stopped
        // the nursery being made bigger for throughput.
        //
        // The length has to match the from-space exactly, not merely be enough:
        // `Minor::forward` decides whether to promote by `to.len()`, so a to-space
        // left over from a larger nursery would quietly promote less.
        let want = self.space.len();
        if to.len() < want {
            to.resize(want, 0);
        } else {
            // No write: `truncate` on a `Copy` element only moves the length.
            to.truncate(want);
        }
        // With nothing in the old generation, this collection sees everything,
        // and can tell which regions nothing reaches.
        let whole = self.old.is_empty() && self.marking.is_none();
        if whole {
            for r in self.regions.values_mut() {
                r.marked = false;
            }
        }
        let mut gc = Minor {
            from: &mut self.space,
            to: &mut to,
            top: 0,
            aged: self.aged,
            promote: self.promoting || promote_all,
            promote_all: promote_all || (self.promoting && self.tenure),
            black: self.marking.is_some(),
            old: &mut self.old,
            promoted: Vec::new(),
            remembered: Vec::new(),
            evac: &mut self.evac,
            blocks: &self.blocks,
            regions: &mut self.regions,
            whole,
            copied: 0,
            promoted_slots: 0,
            promoted_aged: 0,
        };

        for r in roots.iter_mut() {
            if let Value::Obj(a) = *r {
                *r = Value::Obj(gc.forward(a));
            }
        }
        // Every frame on the stack, and on every detached segment, is a root:
        // its references into the nursery move with what they point at.
        //
        // A chunk at a time, skipping the ones known to hold nothing young
        // (see `clean_chunks`) -- except when this collection is the whole,
        // which also marks the regions frames point into and has to see them
        // all. A chunk that is not the current one and comes out of its scan
        // with nothing young in it is clean from now on: a survivor can stay
        // young for one collection, so that is not always the first scan.
        let scan = |gc: &mut Minor, chunk: Addr, end: Addr| -> bool {
            let mut young = false;
            let mut slots = Vec::new();
            chunk_slots(gc.old, chunk, end, &mut slots);
            for slot in slots {
                let x = gc.old.get(slot) as Addr;
                // Into the nursery: moved with what it points at. Into a
                // region: what keeps the region alive, when this collection
                // is the whole.
                if x < OLD_BASE || x >= REGION_BASE {
                    let n = gc.forward(x);
                    gc.old.put(slot, n as Word);
                    young |= n < OLD_BASE;
                }
            }
            young
        };
        for (chunk, end) in chunk_chain(gc.old, self.fcur, self.fsp) {
            let current = chunk == self.fcur;
            if !whole && !current && self.clean_chunks.contains(&chunk) {
                continue;
            }
            let young = scan(&mut gc, chunk, end);
            if !current && !young {
                self.clean_chunks.insert(chunk);
            }
        }
        for d in self.detached.iter_mut() {
            if !whole && d.clean {
                continue;
            }
            let mut young = false;
            for (chunk, end) in chunk_chain(gc.old, d.top, d.end) {
                young |= scan(&mut gc, chunk, end);
            }
            d.clean = !young;
        }

        {
            // A remembered field may be in an object the marker is reading.
            let _held = gc.black.then(|| lock(&self.mutation));
            for s in std::mem::take(&mut self.remembered) {
                let x = gc.old.get(s) as Addr;
                if gc.old.is_pointer(s) && x < OLD_BASE {
                    let n = gc.forward(x);
                    gc.old.put_field(s, n as Word, true);
                    if n < OLD_BASE {
                        gc.remembered.push(s);
                    } else {
                        gc.old.forget(s);
                    }
                } else {
                    gc.old.forget(s);
                }
            }
        }

        // To-space is its own queue; promoted objects wait on a stack.
        let mut scan = 0usize;
        loop {
            if scan < gc.top {
                let h = Head::read(gc.to[scan], gc.to[scan + 1]);
                let base = scan + h.header();
                if h.may_point() {
                    for i in 0..h.len as usize {
                        if h.desc(i, |k| gc.to[scan + k]) == desc::REF {
                            let n = gc.forward(gc.to[base + i] as Addr);
                            gc.to[base + i] = n as Word;
                        }
                    }
                }
                scan += h.size();
            } else if let Some(p) = gc.promoted.pop() {
                // Read and written in place: an old object is contiguous in
                // memory, one bigger than a block too (it has a run of blocks
                // to itself), and forwarding never moves a block -- adding
                // one only grows the table that points at them. Its fields'
                // pointer bits were written as it was promoted, so a field
                // rewritten here keeps the bit it has.
                let at = gc.old.base_ptr(p).wrapping_add(old::offset_of(p));
                // Safety: `p` is an object `alloc` answered and `put_object`
                // wrote, so every slot of it is there to be read.
                let word = |k: usize| unsafe { *at.add(k) };
                let h = Head::read(word(0), word(1));
                if !h.may_point() {
                    continue;
                }
                let header = h.header();
                let base = p + header as Addr;
                for i in 0..h.len as usize {
                    if h.desc(i, word) != desc::REF {
                        continue;
                    }
                    let s = base + i as Addr;
                    let a = word(header + i) as Addr;
                    let n = gc.forward(a);
                    if a < OLD_BASE {
                        // Safety: as for `word`.
                        unsafe { *at.add(header + i) = n as Word };
                    }
                    if n < OLD_BASE {
                        if gc.old.remember(s) {
                            gc.remembered.push(s);
                        }
                    } else {
                        gc.evac.note(gc.old, s, n);
                    }
                }
            } else {
                break;
            }
        }

        let top = gc.top;
        let (copied, promoted) = (gc.copied, gc.promoted_slots);
        let (aged, promoted_aged) = (self.aged, gc.promoted_aged);
        self.remembered = gc.remembered;
        self.other = std::mem::replace(&mut self.space, to);
        self.sync();
        self.top = top;
        self.aged = top;
        self.tenure(aged, promoted_aged, promoted);
        // A segment whose naming object was young and did not survive is dead
        // now, not at the end of some later marking cycle: a transaction that
        // retries cuts one off every attempt, and waiting for the marker to
        // free them left a chunk to allocate per attempt. One whose object
        // moved is followed; one whose object was promoted is the marker's.
        let from = &self.other;
        let mut dead = Vec::new();
        for d in &mut self.detached {
            if d.obj != 0 && d.obj < OLD_BASE {
                match object::forwarded(from[d.obj as usize]) {
                    Some(n) => d.obj = n,
                    None => dead.push(d.top),
                }
            }
        }
        if !dead.is_empty() {
            self.detached.retain(|d| !dead.contains(&d.top));
            for top in dead {
                self.free_segment(top);
            }
        }
        self.copied += copied as u64;
        self.promoted += promoted as u64;

        if whole {
            // A region nothing reached is no longer this heap's business. It
            // is freed, all at once, when nothing else refers to it either.
            let unreached: Vec<u32> = self
                .regions
                .iter()
                .filter(|(_, r)| !r.marked)
                .map(|(id, _)| *id)
                .collect();
            for id in unreached {
                self.free_region(id);
            }
        }

        // More than half full after collecting means the next collection would
        // come almost immediately. Grow instead, so collection stays amortised
        // -- or, at the nursery's largest, start promoting.
        if self.top * 2 > self.space.len() {
            let bigger = (self.space.len() * 2).max(64);
            match self.config.collector {
                Collector::Copying => {
                    assert!(
                        bigger <= OLD_BASE as usize,
                        "the heap has used up its address space"
                    );
                    self.space.resize(bigger, 0);
                    self.sync();
                }
                Collector::Generational if bigger <= self.config.nursery => {
                    self.space.resize(bigger, 0);
                    self.sync();
                }
                Collector::Generational => self.promoting = true,
            }
        }
    }

    /// Decide, after a nursery collection, whether the next one promotes what
    /// survives it at once -- **tenures** it -- or keeps it young for one more
    /// collection, as it does by default. `aged` slots had survived once before
    /// this collection, `promoted_aged` of them survived it too, and
    /// `promoted` slots were promoted in all.
    ///
    /// Keeping a survivor young for a collection is a bet that it is about to
    /// die: it is copied within the nursery once, and promoted only if it is
    /// still there at the next. Most of the time the bet is good. But a
    /// program building something larger than the nursery -- a tree that is
    /// walked once it is finished, a list that is reversed at the end -- loses
    /// it every time: everything the nursery holds survives the collection and
    /// the one after, and is copied twice. `benchmarks/binarytrees` lost it
    /// for a quarter of everything it allocated.
    ///
    /// So the bet is watched. When most of what was kept young survived to be
    /// promoted anyway, the next collections promote at once; when one of
    /// those finds little alive, so that keeping it young would have cost next
    /// to nothing, they go back to keeping it. A pause does the same work
    /// either way, give or take the cheaper copy -- what survives, copied
    /// once more -- so this moves throughput and not the longest pause.
    fn tenure(&mut self, aged: usize, promoted_aged: usize, promoted: usize) {
        if !self.promoting || self.config.collector != Collector::Generational {
            self.tenure = false;
            return;
        }
        let nursery = self.space.len();
        self.tenure = if self.tenure {
            promoted * TENURE_LITTLE >= nursery
        } else {
            aged * TENURE_LITTLE >= nursery && promoted_aged * 2 > aged
        };
    }

    /// Check that every address reachable from `roots` is an object where it
    /// should be: what moving objects has to leave true.
    fn verify_addresses(&self, roots: &[Value]) {
        let mut seen = std::collections::HashSet::new();
        let young: std::collections::HashSet<Addr> = self.nursery_objects().collect();
        let mut stack: Vec<Addr> = roots.iter().filter_map(|r| r.addr()).collect();
        while let Some(a) = stack.pop() {
            if a >= REGION_BASE || !seen.insert(a) {
                continue;
            }
            // A frame is in a stack chunk, which holds no objects as the old
            // generation counts them: it is pushed and popped, not allocated.
            // What it holds is checked like anything else.
            assert!(
                if a < OLD_BASE {
                    young.contains(&a)
                } else {
                    self.old.is_object(a) || self.old.in_stack(a)
                },
                "{a} is reachable but is not an object: a pointer was not moved"
            );
            for i in 0..self.len(a) {
                if let Value::Obj(x) = self.field(a, i) {
                    stack.push(x);
                }
            }
        }
    }

    /// Check that every old object reachable from `roots` is marked in `epoch`
    /// with its lines stamped, and every region reachable is marked: what a
    /// cycle has to have done before its result is adopted.
    fn verify(&self, roots: &[Value], epoch: u32) {
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<Addr> = roots.iter().filter_map(|r| r.addr()).collect();
        while let Some(a) = stack.pop() {
            if a >= REGION_BASE {
                let id = self.blocks[Self::find_block(&self.blocks, a)].region;
                assert!(
                    self.regions.get(&id).is_some_and(|r| r.marked),
                    "region {id} is reachable but was not marked"
                );
                continue;
            }
            if !seen.insert(a) {
                continue;
            }
            let h = self.head(a);
            let (kind, len, meta) = (h.kind, h.len, h.meta);
            // A frame is in a stack chunk, which the marker leaves alone: the
            // frame stack is a root, and a chunk is freed as one with its
            // segment (see `finish_cycle`), not line by line. What the frame
            // holds is checked like anything else.
            if a >= OLD_BASE && !self.old.in_stack(a) {
                assert!(
                    self.old.is_marked(a, epoch),
                    "old object {a} ({kind:?}) is reachable but was not marked"
                );
                for s in a..a + h.size() as Addr {
                    let b = &self.old.blocks[old::block_of(s)];
                    assert_eq!(
                        b.line(old::offset_of(s) / old::LINE),
                        epoch,
                        "a line of old object {a} was not stamped"
                    );
                }
            }
            if kind == Kind::Compact {
                assert!(
                    self.regions.get(&meta).is_some_and(|r| r.marked),
                    "compact handle {a}'s region {meta} was not marked"
                );
            }
            for i in 0..len as usize {
                if let Value::Obj(x) = self.field(a, i) {
                    stack.push(x);
                }
            }
        }
    }

    // --- passing values between heaps ----------------------------------------

    /// Lift `v` and everything it reaches out of this heap. The heap is only
    /// read. Sharing inside `v` survives. What is in a shared region is not
    /// copied: the parcel keeps the address, and holds the region.
    pub fn export(&self, v: Value) -> Result<Parcel, Unsendable> {
        let mut parcel = Parcel {
            slots: Vec::new(),
            root: v,
            regions: Vec::new(),
        };
        let Value::Obj(a) = v else {
            return Ok(parcel);
        };
        let mut copies: HashMap<Addr, Addr> = HashMap::new();
        parcel.root = Value::Obj(self.export_object(a, &mut parcel, &mut copies)?);
        // The parcel is its own queue, as to-space is for the collector.
        let mut scan = 0usize;
        while scan < parcel.slots.len() {
            let h = Head::read(parcel.slots[scan], parcel.slots[scan + 1]);
            for f in 0..h.len as usize {
                if h.desc(f, |k| parcel.slots[scan + k]) == desc::REF {
                    let at = scan + h.header() + f;
                    let to =
                        self.export_object(parcel.slots[at] as Addr, &mut parcel, &mut copies)?;
                    parcel.slots[at] = to as Word;
                }
            }
            scan += h.size();
        }
        Ok(parcel)
    }

    fn export_object(
        &self,
        a: Addr,
        parcel: &mut Parcel,
        copies: &mut HashMap<Addr, Addr>,
    ) -> Result<Addr, Unsendable> {
        if a >= REGION_BASE {
            let id = self.blocks[Self::find_block(&self.blocks, a)].region;
            self.carry(parcel, id);
            return Ok(a);
        }
        if let Some(&c) = copies.get(&a) {
            return Ok(c);
        }
        let h = self.head(a);
        match h.kind {
            Kind::Ref => return Err(Unsendable::Ref),
            Kind::MutArray => return Err(Unsendable::MutArray),
            Kind::Resume | Kind::Frame | Kind::Stack => return Err(Unsendable::Continuation),
            Kind::Compact => self.carry(parcel, h.meta),
            _ => {}
        }
        // Copied word for word: the header's descriptors say what the fields
        // are wherever the words go.
        let at = parcel.slots.len() as Addr;
        parcel
            .slots
            .extend((0..h.size()).map(|k| self.slot(a + k as Addr)));
        copies.insert(a, at);
        Ok(at)
    }

    /// Make the parcel hold region `id`, once.
    fn carry(&self, parcel: &mut Parcel, id: u32) {
        if parcel.regions.iter().any(|r| r.id == id) {
            return;
        }
        if let Some(r) = self.regions.get(&id) {
            parcel.regions.push(r.region.clone());
        }
    }

    /// Rebuild a parcel in this heap. The caller must have [`Heap::reserve`]d
    /// [`Parcel::len`] slots, as for any allocation.
    pub fn import(&mut self, p: &Parcel) -> Value {
        for r in &p.regions {
            self.adopt(r);
        }
        if !self.room_for(p.slots.len()) {
            return self.import_scattered(p);
        }
        let base = self.top as Addr;
        let rebase = |x: Addr| if x < REGION_BASE { x + base } else { x };
        let into = &mut self.space[self.top..self.top + p.slots.len()];
        into.copy_from_slice(&p.slots);
        let mut at = 0;
        while at < into.len() {
            let h = Head::read(into[at], into[at + 1]);
            for f in 0..h.len as usize {
                if h.desc(f, |k| into[at + k]) == desc::REF {
                    let s = at + h.header() + f;
                    into[s] = rebase(into[s] as Addr) as Word;
                }
            }
            at += h.size();
        }
        self.top += p.slots.len();
        self.allocated += p.slots.len() as u64;
        match p.root {
            Value::Obj(x) => Value::Obj(rebase(x)),
            v => v,
        }
    }

    /// A parcel too big for the nursery, rebuilt an object at a time wherever
    /// each fits: made first, then its fields filled in with where the
    /// objects they point at went.
    fn import_scattered(&mut self, p: &Parcel) -> Value {
        let mut at = vec![0 as Addr; p.slots.len()];
        let heads: Vec<(usize, Head)> = {
            let mut heads = Vec::new();
            let mut i = 0;
            while i < p.slots.len() {
                let h = Head::read(p.slots[i], p.slots[i + 1]);
                heads.push((i, h));
                i += h.size();
            }
            heads
        };
        // Each object made first, holding what the parcel's fields hold, so
        // that its descriptors are the parcel's; then its addresses fixed.
        for &(i, h) in &heads {
            let fields: Vec<Value> = (0..h.len as usize)
                .map(|f| {
                    let d = h.desc(f, |k| p.slots[i + k]);
                    let w = p.slots[i + h.header() + f];
                    // Not yet an address in this heap: nothing to trace.
                    if d == desc::REF {
                        Value::Obj(0)
                    } else {
                        Value::from_bits(w, d)
                    }
                })
                .collect();
            at[i] = self.alloc(h.kind, h.meta, &fields);
        }
        let rebase = |v: Value| match v {
            Value::Obj(x) if x < REGION_BASE => Value::Obj(at[x as usize]),
            v => v,
        };
        for &(i, h) in &heads {
            let header = h.header() as Addr;
            for f in 0..h.len as usize {
                if h.desc(f, |k| p.slots[i + k]) != desc::REF {
                    continue;
                }
                let v = rebase(Value::Obj(p.slots[i + h.header() + f] as Addr));
                let s = at[i] + header + f as Addr;
                if s < OLD_BASE {
                    self.space[s as usize] = v.bits();
                } else {
                    self.init_old(s, v);
                }
            }
        }
        rebase(p.root)
    }

    // --- compact regions ----------------------------------------------------

    /// Refer to `region`: from now on its addresses can be read here. Also how
    /// a heap learns of blocks a region grew since it last looked.
    pub fn adopt(&mut self, region: &Arc<Region>) {
        let contents = region.lock();
        let known = self.regions.get(&region.id).map_or(0, |r| r.blocks);
        for b in &contents.blocks[known.min(contents.blocks.len())..] {
            let at = self.blocks.partition_point(|x| x.base < b.base);
            self.blocks.insert(at, b.clone());
        }
        let blocks = contents.blocks.len().max(known);
        drop(contents);
        // A region adopted while a cycle marks was not in its snapshot, so
        // the cycle cannot tell whether it is reachable: it is kept.
        let marking = self.marking.is_some();
        self.regions
            .entry(region.id)
            .and_modify(|r| {
                r.blocks = blocks;
                r.marked |= marking;
            })
            .or_insert(Adopted {
                region: region.clone(),
                blocks,
                marked: marking,
            });
    }

    /// A new, empty region, referred to by this heap.
    pub fn new_region(&mut self) -> u32 {
        let region = Region::new();
        self.adopt(&region);
        region.id
    }

    /// The region `id`, if this heap refers to it.
    pub fn region(&self, id: u32) -> Option<&Arc<Region>> {
        self.regions.get(&id).map(|r| &r.region)
    }

    /// Slots in use by region `id`.
    pub fn region_used(&self, id: u32) -> usize {
        self.regions.get(&id).map_or(0, |r| r.region.used())
    }

    /// Stop referring to region `id`.
    pub fn free_region(&mut self, id: u32) {
        if self.regions.remove(&id).is_some() {
            self.blocks.retain(|b| b.region != id);
        }
    }

    /// Copy `v`, and everything it reaches, into region `id`, and answer the
    /// copy. What is already in `id` is shared rather than copied again, and
    /// sharing inside `v` survives: each object is copied once. What is in
    /// another region is copied too, so a region never points outside itself.
    ///
    /// Iterative, as the collector is: the objects just appended to the region
    /// are the queue. If `v` reaches something that cannot be compacted, the
    /// region is put back exactly as it was.
    pub fn compact_into(&mut self, id: u32, v: Value) -> Result<Value, Uncompactable> {
        let region = self
            .region(id)
            .expect("a region this heap refers to")
            .clone();
        let mut contents = region.lock();
        let end = contents.end();
        let before = contents.used;
        let result = self.copy_into(&mut contents, id, v, end);
        if result.is_err() {
            contents.truncate(end);
        }
        self.region_growth += contents.used - before;
        drop(contents);
        self.adopt(&region);
        result
    }

    fn copy_into(
        &self,
        contents: &mut crate::region::Contents,
        id: u32,
        v: Value,
        end: crate::region::End,
    ) -> Result<Value, Uncompactable> {
        let mut copies: HashMap<Addr, Addr> = HashMap::new();
        let root = match v {
            Value::Obj(a) => Value::Obj(self.copy_object(contents, id, a, &mut copies)?),
            other => other,
        };
        // Walk what was appended, fixing each field to the copy of what it
        // points at. Copies made on the way are appended, and walked in turn.
        let mut cursor = contents.cursor(end);
        while let Some((at, h)) = contents.next(&mut cursor) {
            let block = contents.block_of(at).expect("an appended object").clone();
            for f in 0..h.len as usize {
                if h.desc(f, |k| block.get(at + k as Addr)) == desc::REF {
                    let a = block.get(at + (h.header() + f) as Addr) as Addr;
                    let to = self.copy_object(contents, id, a, &mut copies)?;
                    contents.set_field(at, f, Value::Obj(to));
                }
            }
        }
        Ok(root)
    }

    /// The copy of object `a` in region `id`, making it if there is none yet.
    /// Its fields are copied as they are, to be fixed by the walk.
    fn copy_object(
        &self,
        contents: &mut crate::region::Contents,
        id: u32,
        a: Addr,
        copies: &mut HashMap<Addr, Addr>,
    ) -> Result<Addr, Uncompactable> {
        if a >= REGION_BASE {
            // In this region already, published or appended just now.
            if contents.block_of(a).is_some() {
                return Ok(a);
            }
        }
        if let Some(&c) = copies.get(&a) {
            return Ok(c);
        }
        let Head { kind, meta, .. } = self.head(a);
        let meta = match kind {
            Kind::Ref => return Err(Uncompactable::Ref),
            Kind::MutArray => return Err(Uncompactable::MutArray),
            Kind::Closure | Kind::Resume | Kind::Frame | Kind::Stack => {
                return Err(Uncompactable::Function);
            }
            // A handle inside a compacted value now names the region it is in,
            // since that is where its contents are copied.
            Kind::Compact => id,
            _ => meta,
        };
        let fields = self.fields(a);
        let c = contents.alloc(kind, meta, &fields);
        copies.insert(a, c);
        Ok(c)
    }
}

/// A nursery collection in progress.

/// A stack segment cut off by a general handler clause: see [`Heap::detach`].
struct Detached {
    /// Its top chunk, which names the rest through the chunk headers.
    top: Addr,
    /// Where the top chunk's frames end.
    end: Addr,
    /// The marking epoch it was cut in.
    epoch: u32,
    /// The [`Kind::Stack`] object naming it, once made -- `0` until then. While
    /// young, whether it survives a nursery collection says whether the
    /// segment is still wanted; once old, the marker says.
    obj: Addr,
    /// Found at a nursery collection to hold nothing young. Nothing pushes
    /// into a segment while it is off the stack, so it stays that way.
    clean: bool,
}

/// Slots in the header of a frame-stack chunk: the chunk below it in its
/// chain (`0` at the bottom), where that chunk's frames end, and the serial
/// number the chunk was entered with -- what tells a handler's chunk from a
/// later chunk that reused the same block.
const CHUNK_HEADER: Addr = 3;

/// How a `handle` names its chunk, in its target `Ref`'s `meta`: the block
/// number in the low 17 bits, the low 15 bits of the serial above.
const TAG_BLOCK_BITS: u32 = 17;

/// Every slot of every frame that is a root -- on the live chain from `fcur`
/// up to `fsp`, and on each detached segment -- that holds a reference.
fn frame_slots(old: &Old, fcur: Addr, fsp: Addr, detached: &[Detached]) -> Vec<Addr> {
    let mut out = Vec::new();
    for (chunk, end) in chunk_chain(old, fcur, fsp) {
        chunk_slots(old, chunk, end, &mut out);
    }
    for d in detached {
        for (chunk, end) in chunk_chain(old, d.top, d.end) {
            chunk_slots(old, chunk, end, &mut out);
        }
    }
    out
}

/// The chunks of a chain from `top` down, each with where its frames end:
/// `end` for the top one, and for each below it what the chunk above recorded
/// when it was entered.
fn chunk_chain(old: &Old, top: Addr, end: Addr) -> Vec<(Addr, Addr)> {
    let mut out = Vec::new();
    let (mut chunk, mut end) = (top, end);
    while chunk != 0 {
        out.push((chunk, end));
        end = old.get(chunk + 1) as Addr;
        chunk = old.get(chunk) as Addr;
    }
    out
}

/// Every slot holding a reference in the frames of `chunk`, up to `end`.
fn chunk_slots(old: &Old, chunk: Addr, end: Addr, out: &mut Vec<Addr>) {
    let mut a = chunk + CHUNK_HEADER;
    while a < end {
        let h = Head::read(old.get(a), old.get(a + 1));
        debug_assert_eq!(h.kind, Kind::Frame, "a frame stack holds frames");
        let header = h.header();
        for i in 0..h.len as usize {
            if h.desc(i, |k| old.get(a + k as Addr)) == desc::REF {
                out.push(a + (header + i) as Addr);
            }
        }
        a += h.size() as Addr;
    }
}

/// # The frame stack
///
/// A function's continuation used to be a heap object: four words allocated
/// per non-tail call, and collected later. It is a frame now: the same object,
/// [`Kind::Frame`], written into a per-thread stack of 64 KiB **chunks** and
/// reclaimed by moving the stack pointer back when a function returns through
/// it. The chunks are old-generation blocks in state `STACK`, so their
/// addresses are ordinary old addresses -- the interpreter, the collector's
/// tables and native code's block-table walk all reach a frame the way they
/// reach any old object -- and the allocator, the evacuator and the marker
/// leave them alone by their state.
///
/// A chunk's first two slots say which chunk is below it in its chain and
/// where that chunk's frames end. A new chunk is linked in when the current
/// one overflows, so a deep recursion grows a chain and never copies a frame;
/// and every `handle` enters a chunk of its own, so that a general clause can
/// cut off everything the handled body pushed by unlinking at a chunk boundary
/// -- O(1), no copying -- which is what one-shot resumption is. That is the
/// design of GHC's stack chunks and of OCaml 5's fibers, and it is why the
/// stack is chunked rather than contiguous.
///
/// Frames are roots: at every nursery collection and at the start of every
/// marking cycle the heap walks every frame on the live chain and on every
/// detached segment, so the marker never has to read a frame while the program
/// pushes and pops them.
impl Heap {
    /// A chunk to push frames into, from the popped ones first.
    fn new_chunk(&mut self) -> Addr {
        self.free_chunks
            .pop()
            .unwrap_or_else(|| self.old.stack_block())
    }

    /// Start a chunk on top of the current one -- what a push does when its
    /// chunk is full, and what [`meadow_core::Prim::Enter`] does at every
    /// `handle`, so that the handler's own chunk is a boundary a general
    /// clause can cut at. Answers the chunk's tag, for the handle's target.
    ///
    /// Every `handle`, even one whose chunk would sit empty: two handlers
    /// sharing a chunk would share a boundary, and the inner one's clause
    /// performing to the outer would cut off the outer's frames with its own.
    pub fn enter_frames(&mut self) -> u32 {
        let base = self.new_chunk();
        self.chunk_serial = self.chunk_serial.wrapping_add(1);
        self.old.put(base, self.fcur as Word);
        self.old.put(base + 1, self.fsp as Word);
        self.old.put(base + 2, self.chunk_serial as Word);
        self.fcur = base;
        self.fsp = base + CHUNK_HEADER;
        self.flim = base + old::BLOCK as Addr;
        self.publish_chunk();
        (self.chunk_serial & ((1 << (32 - TAG_BLOCK_BITS)) - 1)) << TAG_BLOCK_BITS
            | old::block_of(base) as u32
    }

    /// Push a frame with method table `meta` and `n` captures, each `word(j)`
    /// described by `desc(j)`, and answer its address.
    pub fn push_frame(
        &mut self,
        meta: u32,
        n: usize,
        word: impl Fn(usize) -> Word,
        desc: impl Fn(usize) -> desc::Desc,
    ) -> Addr {
        let size = Heap::size_of(Kind::Frame, n) as Addr;
        if self.fcur == 0 || self.fsp + size > self.flim {
            self.enter_frames();
        }
        let a = self.fsp;
        let old = &self.old;
        object::write_header(Kind::Frame, meta, (0..n).map(&desc), |k, w| {
            old.put(a + k as Addr, w)
        });
        let header = meadow_rt::compact::header_slots(false, n) as Addr;
        for j in 0..n {
            old.put(a + header + j as Addr, word(j));
        }
        self.fsp += size;
        a
    }

    /// Return through the frame at `a`: it and everything above it are gone.
    /// Chunks emptied on the way are kept for the next push.
    pub fn pop_to(&mut self, a: Addr) {
        debug_assert!(
            self.old.in_stack(a),
            "returning through {a}, which is not a frame"
        );
        while !(self.fcur..self.flim).contains(&a) {
            let below = self.old.get(self.fcur) as Addr;
            let below_fsp = self.old.get(self.fcur + 1) as Addr;
            assert_ne!(
                below, 0,
                "returning through a frame that is not on the stack"
            );
            self.clean_chunks.remove(&self.fcur);
            self.free_chunks.push(self.fcur);
            self.fcur = below;
            self.flim = below + old::BLOCK as Addr;
            self.fsp = below_fsp;
            // The current chunk is pushed into, so it is never clean.
            self.clean_chunks.remove(&below);
            self.publish_chunk();
        }
        self.fsp = a;
    }

    /// Cut off the handler's chunk -- the one `tag` names, entered at its
    /// `handle` -- and every chunk above it, and answer the segment as its top
    /// chunk and where its frames end. The program goes on below the cut, on
    /// the chunk the handler's own continuation is in.
    ///
    /// `None` when that chunk is no longer on the stack: the handler has
    /// returned, and what is performing to it is a closure that escaped its
    /// `handle` and kept the evidence. There is nothing to return into. The
    /// serial in the tag is what tells that from a later chunk in the same
    /// block.
    pub fn detach(&mut self, tag: u32) -> Option<(Addr, Addr)> {
        let block = (tag & ((1 << TAG_BLOCK_BITS) - 1)) as usize;
        let serial = tag >> TAG_BLOCK_BITS;
        let base = OLD_BASE + (block * old::BLOCK) as Addr;
        if self.fcur == 0
            || !self.old.in_stack(base)
            || self.old.get(base + 2) as u32 & ((1 << (32 - TAG_BLOCK_BITS)) - 1) != serial
        {
            return None;
        }
        let (top, top_fsp) = (self.fcur, self.fsp);
        let mut lowest = self.fcur;
        let mut cut = Vec::new();
        loop {
            let below = self.old.get(lowest) as Addr;
            let below_fsp = self.old.get(lowest + 1) as Addr;
            let bottom = lowest == base;
            if !bottom && below == 0 {
                return None;
            }
            cut.push(lowest);
            if bottom {
                // What is cut off is the segment's to account for from here
                // (`Detached::clean`), and the chunk under it is current.
                for c in &cut {
                    self.clean_chunks.remove(c);
                }
                self.clean_chunks.remove(&below);
                self.old.put(lowest, 0);
                self.fcur = below;
                self.flim = if below == 0 {
                    0
                } else {
                    below + old::BLOCK as Addr
                };
                self.fsp = below_fsp;
                self.publish_chunk();
                break;
            }
            lowest = below;
        }
        // The segment's end is kept beside it, not in a chunk header: the top
        // chunk's second slot says where the chunk *below* it ends, which a
        // segment of more than one chunk -- any `handle` inside a `handle` --
        // still needs, for the collector's walk and for the pop back into it.
        self.detached.push(Detached {
            top,
            end: top_fsp,
            epoch: self.old.epoch(),
            obj: 0,
            clean: false,
        });
        Some((top, top_fsp))
    }

    /// The object made to name the segment `top`: see [`Detached::obj`].
    pub fn name_segment(&mut self, top: Addr, obj: Addr) {
        if let Some(d) = self.detached.iter_mut().find(|d| d.top == top) {
            d.obj = obj;
        }
    }

    /// Give a whole segment's chunks back for the next push.
    fn free_segment(&mut self, top: Addr) {
        let mut chunk = top;
        while chunk != 0 {
            let below = self.old.get(chunk) as Addr;
            self.free_chunks.push(chunk);
            chunk = below;
        }
    }

    /// Put a detached segment back on top of the stack. `false` if it has been
    /// put back already: a resumption runs once.
    pub fn reattach(&mut self, top: Addr, top_fsp: Addr) -> bool {
        let Some(i) = self.detached.iter().position(|d| d.top == top) else {
            return false;
        };
        let segment = self.detached.swap_remove(i);
        let mut bottom = top;
        while self.old.get(bottom) as Addr != 0 {
            bottom = self.old.get(bottom) as Addr;
            // Below the top of a segment found clean: clean still, on the
            // stack as off it. The top becomes the current chunk.
            if segment.clean {
                self.clean_chunks.insert(bottom);
            }
        }
        self.old.put(bottom, self.fcur as Word);
        self.old.put(bottom + 1, self.fsp as Word);
        self.fcur = top;
        self.flim = top + old::BLOCK as Addr;
        self.fsp = top_fsp;
        self.publish_chunk();
        true
    }

    /// Whether `a` is a frame on this heap's stack.
    pub fn is_frame(&self, a: Addr) -> bool {
        self.old.in_stack(a)
    }
}

struct Minor<'h> {
    from: &'h mut Vec<Word>,
    to: &'h mut Vec<Word>,
    top: usize,
    /// Nursery objects below this are old enough to promote.
    aged: usize,
    promote: bool,
    promote_all: bool,
    /// Whether a cycle is marking, so what is promoted is marked as it is.
    black: bool,
    old: &'h mut Old,
    /// Promoted objects whose fields are still to be forwarded.
    promoted: Vec<Addr>,
    /// The remembered set as it will be after.
    remembered: Vec<Addr>,
    /// For recording promoted fields that point into blocks to be evacuated.
    evac: &'h mut Evacuation,
    blocks: &'h [Arc<Block>],
    regions: &'h mut HashMap<u32, Adopted>,
    /// Whether this collection sees everything, and so marks regions.
    whole: bool,
    copied: usize,
    promoted_slots: usize,
    /// Of those, the slots of objects that had survived a collection before.
    promoted_aged: usize,
}

impl Minor<'_> {
    /// Where nursery object `a` is now: copied within the nursery, or promoted,
    /// the first time it is asked for, and the same place every time after.
    /// An old address stays put; a region address stays put and, if this
    /// collection sees everything, marks its region.
    fn forward(&mut self, a: Addr) -> Addr {
        if a >= REGION_BASE {
            if self.whole {
                let region = self.blocks[Heap::find_block(self.blocks, a)].region;
                self.mark(region);
            }
            return a;
        }
        if a >= OLD_BASE {
            return a;
        }
        let a = a as usize;
        // Already moved: every other reference to it lands here and shares.
        if let Some(n) = forwarded(self.from[a]) {
            return n;
        }
        let h = Head::read(self.from[a], self.from[a + 1]);
        if self.whole && h.kind == Kind::Compact {
            self.mark(h.meta);
        }
        let size = h.size();
        let promote =
            self.promote && (self.promote_all || a < self.aged || self.top + size > self.to.len());
        let at = if promote {
            let at = self.old.alloc(size, self.black);
            let words = &self.from[a..a + size];
            self.old.put_object(at, words, h.header(), |i| {
                h.desc(i, |k| words[k]) == desc::REF
            });
            self.promoted.push(at);
            self.promoted_slots += size;
            if a < self.aged {
                self.promoted_aged += size;
            }
            at
        } else {
            let at = self.top;
            self.to[at..at + size].copy_from_slice(&self.from[a..a + size]);
            self.top += size;
            self.copied += size;
            at as Addr
        };
        self.from[a] = forward(at);
        at
    }

    fn mark(&mut self, region: u32) {
        if let Some(r) = self.regions.get_mut(&region) {
            r.marked = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The copying collector: one space, and every collection sees everything,
    /// which is what the tests of it below count on.
    fn heap() -> Heap {
        Heap::with_config(
            INITIAL,
            GcConfig {
                collector: Collector::Copying,
                ..config()
            },
        )
    }

    #[test]
    fn allocation_is_a_bump_and_reading_back_works() {
        let mut h = heap();
        let a = h.alloc(Kind::Data, 7, &[Value::Int(1), Value::Int(2)]);
        assert_eq!(h.kind(a), Kind::Data);
        assert_eq!(h.meta(a), 7);
        assert_eq!(h.len(a), 2);
        assert_eq!(h.field(a, 1), Value::Int(2));
        assert_eq!(h.used(), 4, "two words of header, and the fields");
    }

    #[test]
    fn collecting_keeps_what_is_rooted_and_drops_the_rest() {
        let mut h = heap();
        let keep = h.alloc(Kind::Data, 1, &[Value::Int(42)]);
        let _drop = h.alloc(Kind::Data, 2, &[Value::Int(99)]);
        assert_eq!(h.used(), 6);

        let mut roots = [Value::Obj(keep)];
        h.collect(&mut roots);

        assert_eq!(h.used(), 3, "only the rooted object survived");
        let a = roots[0].addr().expect("still an object");
        assert_eq!(h.meta(a), 1);
        assert_eq!(h.field(a, 0), Value::Int(42));
    }

    #[test]
    fn sharing_survives_and_a_cycle_terminates() {
        let mut h = heap();
        let shared = h.alloc(Kind::Data, 1, &[Value::Int(1)]);
        let a = h.alloc(Kind::Data, 2, &[Value::Obj(shared)]);
        let b = h.alloc(Kind::Data, 3, &[Value::Obj(shared)]);
        // A cycle: `a` points at itself through a mutable field. Reference
        // counting could never free this; a copying collector does not care.
        let cyc = h.alloc(Kind::Ref, 0, &[Value::Unit]);
        h.set_field(cyc, 0, Value::Obj(cyc));

        let mut roots = [Value::Obj(a), Value::Obj(b), Value::Obj(cyc)];
        h.collect(&mut roots);

        let (a, b, cyc) = (
            roots[0].addr().unwrap(),
            roots[1].addr().unwrap(),
            roots[2].addr().unwrap(),
        );
        assert_eq!(
            h.field(a, 0),
            h.field(b, 0),
            "one copy of the shared object, not two"
        );
        assert_eq!(
            h.field(cyc, 0),
            Value::Obj(cyc),
            "the cycle came back intact"
        );
    }

    #[test]
    fn a_long_chain_collects_without_recursing() {
        // 200_000 deep. Cheney scans the copied region as a queue, so this costs
        // no stack at all — which is exactly what the `Rc` version could not do
        // when it came to dropping one of these.
        let mut h = heap();
        let mut cur = h.alloc(Kind::Data, 0, &[]);
        for i in 0..200_000 {
            if !h.room_for(3) {
                let mut roots = [Value::Obj(cur)];
                h.collect(&mut roots);
                cur = roots[0].addr().unwrap();
            }
            cur = h.alloc(Kind::Data, 1, &[Value::Int(i), Value::Obj(cur)]);
        }
        let mut roots = [Value::Obj(cur)];
        h.collect(&mut roots);

        // Walk it to be sure every link came through.
        let mut n = 0;
        let mut at = roots[0].addr().unwrap();
        while h.len(at) == 2 {
            n += 1;
            at = h.field(at, 1).addr().unwrap();
        }
        assert_eq!(n, 200_000);
    }

    /// A `Cons`-like chain of `n` cells, newest first.
    fn chain(h: &mut Heap, n: i64) -> Value {
        let mut cur = Value::Obj(h.alloc(Kind::Data, 0, &[]));
        for i in 0..n {
            if !h.room_for(3) {
                let mut roots = [cur];
                h.collect(&mut roots);
                cur = roots[0];
            }
            cur = Value::Obj(h.alloc(Kind::Data, 1, &[Value::Int(i), cur]));
        }
        cur
    }

    fn chain_len(h: &Heap, mut v: Value) -> usize {
        let mut n = 0;
        while let Value::Obj(a) = v {
            if h.len(a) == 0 {
                break;
            }
            n += 1;
            v = h.field(a, 1);
        }
        n
    }

    #[test]
    fn the_engines_estimate_compact_size_with_the_real_slot() {
        assert_eq!(SLOT_BYTES, meadow_rt::compact::SLOT_BYTES);
    }

    #[test]
    fn a_compacted_value_is_neither_copied_nor_scanned() {
        let mut h = heap();
        let big = chain(&mut h, 50_000);
        let region = h.new_region();
        let root = h.compact_into(region, big).unwrap();
        assert!(h.in_region(root.addr().unwrap()));
        assert_eq!(h.region_used(region), 2 + 50_000 * 4);

        let handle = h.alloc(Kind::Compact, region, &[root]);
        let mut roots = [Value::Obj(handle)];
        let copied = h.copied;
        h.collect(&mut roots);
        // Only the handle came across; the chain stayed where it was.
        assert_eq!(h.copied - copied, 3);
        assert_eq!(h.used(), 3);
        let root = h.field(roots[0].addr().unwrap(), 0);
        assert_eq!(chain_len(&h, root), 50_000);
    }

    #[test]
    fn an_address_into_a_region_keeps_it_alive_without_the_handle() {
        let mut h = heap();
        let big = chain(&mut h, 100);
        let region = h.new_region();
        let root = h.compact_into(region, big).unwrap();
        // A semispace object pointing into the region, and no handle at all.
        let holder = h.alloc(Kind::Data, 9, &[root]);
        let mut roots = [Value::Obj(holder)];
        h.collect(&mut roots);
        assert_eq!(h.region_used(region), 2 + 100 * 4);
        let root = h.field(roots[0].addr().unwrap(), 0);
        assert_eq!(chain_len(&h, root), 100);
    }

    #[test]
    fn an_unreachable_region_is_freed_at_the_next_collection() {
        let mut h = heap();
        let big = chain(&mut h, 1000);
        let region = h.new_region();
        h.compact_into(region, big).unwrap();
        assert!(h.region_slots() > 0);
        h.collect(&mut []);
        assert_eq!(h.region_slots(), 0);
        assert!(h.blocks.is_empty(), "its blocks went with it");
        // Nothing else refers to it, so it is gone: its addresses are free for
        // the next region.
        assert!(h.region(region).is_none());
    }

    #[test]
    fn sharing_inside_a_compacted_value_survives() {
        let mut h = heap();
        let shared = h.alloc(Kind::Data, 1, &[Value::Int(7)]);
        let pair = h.alloc(Kind::Data, 2, &[Value::Obj(shared), Value::Obj(shared)]);
        let region = h.new_region();
        let root = h
            .compact_into(region, Value::Obj(pair))
            .unwrap()
            .addr()
            .unwrap();
        assert_eq!(h.field(root, 0), h.field(root, 1), "one copy, not two");
        assert_eq!(h.region_used(region), 4 + 3);
    }

    #[test]
    fn adding_to_a_region_shares_what_is_already_there() {
        let mut h = heap();
        let big = chain(&mut h, 1000);
        let region = h.new_region();
        let root = h.compact_into(region, big).unwrap();
        let before = h.region_used(region);
        // A new cell on top of the compacted chain: only the cell is copied.
        let longer = h.alloc(Kind::Data, 1, &[Value::Int(-1), root]);
        let root2 = h.compact_into(region, Value::Obj(longer)).unwrap();
        assert_eq!(h.region_used(region), before + 4);
        assert_eq!(chain_len(&h, root2), 1001);
    }

    #[test]
    fn a_mutable_cell_is_refused_and_leaves_the_region_as_it_was() {
        let mut h = heap();
        let ok = chain(&mut h, 10);
        let region = h.new_region();
        h.compact_into(region, ok).unwrap();
        let before = h.region_used(region);

        let cell = h.alloc(Kind::Ref, 0, &[Value::Int(1)]);
        let bad = h.alloc(Kind::Data, 1, &[Value::Int(0), Value::Obj(cell)]);
        assert_eq!(
            h.compact_into(region, Value::Obj(bad)),
            Err(Uncompactable::Ref)
        );
        assert_eq!(h.region_used(region), before);
        let f = h.alloc(Kind::Closure, 0, &[]);
        assert_eq!(
            h.compact_into(region, Value::Obj(f)),
            Err(Uncompactable::Function)
        );
    }

    #[test]
    fn a_region_spans_blocks_and_scans_across_them() {
        // Bigger than the first block, so the copy has to continue its scan in
        // the next one.
        let mut h = heap();
        let big = chain(&mut h, crate::region::FIRST_BLOCK as i64);
        let region = h.new_region();
        let root = h.compact_into(region, big).unwrap();
        assert!(h.region(region).unwrap().lock().blocks.len() > 1);
        assert_eq!(chain_len(&h, root), crate::region::FIRST_BLOCK);
        // Everything in it points inside it.
        let mut v = root;
        while let Value::Obj(a) = v {
            assert!(h.in_region(a));
            if h.len(a) == 0 {
                break;
            }
            v = h.field(a, 1);
        }
    }

    #[test]
    fn a_region_is_shared_between_heaps_without_copying() {
        let mut a = heap();
        let big = chain(&mut a, 10_000);
        let region = a.new_region();
        let root = a.compact_into(region, big).unwrap();
        let handle = a.alloc(Kind::Compact, region, &[root]);

        // Across to another heap: the handle is copied, the chain is not.
        let parcel = a.export(Value::Obj(handle)).unwrap();
        assert_eq!(parcel.len(), 3, "one handle object; the region stays put");
        let mut b = heap();
        b.reserve(parcel.len());
        let got = b.import(&parcel);
        drop(parcel);
        let inner = b.field(got.addr().unwrap(), 0);
        assert_eq!(inner, root, "the same address, in both heaps");

        // The heap that made it lets go; the other still reads it.
        let weak = std::sync::Arc::downgrade(a.region(region).unwrap());
        a.collect(&mut []);
        assert!(a.region(region).is_none());
        assert_eq!(chain_len(&b, inner), 10_000);

        // And when the last heap lets go, the region is freed.
        let mut roots = [got];
        b.collect(&mut roots);
        assert!(weak.upgrade().is_some(), "still reachable from b");
        b.collect(&mut []);
        assert!(weak.upgrade().is_none(), "nobody refers to it any more");
    }

    #[test]
    fn a_region_can_grow_while_another_heap_reads_it() {
        let mut a = heap();
        let first = chain(&mut a, 100);
        let region = a.new_region();
        let root = a.compact_into(region, first).unwrap();
        let mut b = heap();
        b.adopt(a.region(region).unwrap());

        // `a` appends far past the first block; `b` keeps reading what it had.
        let more = chain(&mut a, (crate::region::FIRST_BLOCK * 3) as i64);
        let root2 = a.compact_into(region, more).unwrap();
        assert_eq!(chain_len(&b, root), 100);
        // `b` learns of the new blocks when it adopts the region again.
        b.adopt(a.region(region).unwrap());
        assert_eq!(chain_len(&b, root2), crate::region::FIRST_BLOCK * 3);
    }

    #[test]
    fn the_heap_grows_rather_than_collecting_forever() {
        let mut h = heap();
        let before = h.capacity();
        // Keep everything alive, so collection can never reclaim anything.
        let mut live = Vec::new();
        while h.capacity() == before {
            if !h.room_for(2) {
                let mut roots: Vec<Value> = live.clone();
                h.collect(&mut roots);
                live = roots;
            }
            live.push(Value::Obj(h.alloc(Kind::Data, 0, &[Value::Int(0)])));
        }
        assert!(h.capacity() > before, "a full heap has to grow");
    }

    // --- the generational collector -------------------------------------------

    /// A generational heap with a nursery of `nursery` slots, marking on its
    /// own thread at most `slice` fields per pause, and checking every cycle.
    fn generational(nursery: usize, slice: usize, threads: usize) -> Heap {
        Heap::with_config(
            nursery,
            GcConfig {
                collector: Collector::Generational,
                nursery,
                mark_threads: threads,
                min_trigger: 1024,
                verify: true,
                mark_slice: slice,
                evacuate: true,
            },
        )
    }

    /// Allocate as the VM does: collect first if there is no room, with every
    /// live value in `roots`, and read what to allocate from there after.
    fn alloc_rooted(
        h: &mut Heap,
        roots: &mut [Value],
        kind: Kind,
        meta: u32,
        fields: &[usize],
    ) -> Value {
        if !h.room_for(1 + fields.len()) {
            h.collect(roots);
            h.reserve(1 + fields.len());
        }
        let fields: Vec<Value> = fields.iter().map(|i| roots[*i]).collect();
        Value::Obj(h.alloc(kind, meta, &fields))
    }

    /// A chain of `n` cells counting down to 0, built with its head rooted.
    fn rooted_chain(h: &mut Heap, roots: &mut Vec<Value>, n: i64) -> Value {
        roots.push(Value::Unit);
        let head = roots.len() - 1;
        roots[head] = alloc_rooted(h, roots, Kind::Data, 0, &[]);
        for i in 0..n {
            roots.push(Value::Int(i));
            let k = roots.len() - 1;
            let cell = alloc_rooted(h, roots, Kind::Data, 1, &[k, head]);
            roots.pop();
            roots[head] = cell;
        }
        roots.pop().expect("the head")
    }

    /// Is `v` a whole chain, counting down from `n - 1`?
    fn well_formed(h: &Heap, mut v: Value, n: i64) -> bool {
        let mut expect = n - 1;
        while let Value::Obj(a) = v {
            if h.len(a) == 0 {
                return expect == -1;
            }
            if h.field(a, 0) != Value::Int(expect) {
                return false;
            }
            expect -= 1;
            v = h.field(a, 1);
        }
        false
    }

    fn churn(h: &mut Heap, roots: &mut [Value], cells: usize) {
        for i in 0..cells {
            if !h.room_for(3) {
                h.collect(roots);
            }
            h.alloc(Kind::Data, 1, &[Value::Int(i as i64), Value::Unit]);
        }
    }

    #[test]
    fn what_survives_twice_is_promoted_and_stays_whole() {
        let mut h = generational(1024, usize::MAX, 0);
        let mut roots = Vec::new();
        let c = rooted_chain(&mut h, &mut roots, 2000);
        roots.push(c);
        for _ in 0..3 {
            h.collect(&mut roots);
        }
        assert!(roots[0].addr().unwrap() >= OLD_BASE, "promoted");
        assert!(h.promoted > 0);
        assert_eq!(h.used() - h.top, (h.old.live + h.old.allocated) as usize);
        assert!(well_formed(&h, roots[0], 2000));
    }

    #[test]
    fn a_young_object_only_an_old_ref_holds_survives() {
        let mut h = generational(1024, usize::MAX, 0);
        let mut roots = vec![Value::Unit];
        roots[0] = alloc_rooted(&mut h, &mut roots, Kind::Ref, 0, &[0]);
        h.promoting = true;
        h.collect(&mut roots);
        h.collect(&mut roots);
        let r = roots[0].addr().unwrap();
        assert!(r >= OLD_BASE);
        let young = h.alloc(Kind::Data, 5, &[Value::Int(7)]);
        h.set_field(r, 0, Value::Obj(young));
        assert_eq!(h.remembered.len(), 1);
        churn(&mut h, &mut roots, 20_000);
        let v = h.field(roots[0].addr().unwrap(), 0).addr().unwrap();
        assert_eq!((h.meta(v), h.field(v, 0)), (5, Value::Int(7)));
        assert!(v >= OLD_BASE, "promoted in time");
        assert!(h.remembered.is_empty(), "nothing old points young any more");
    }

    #[test]
    fn a_cycle_frees_what_died_and_keeps_what_did_not() {
        let mut h = generational(1024, usize::MAX, 0);
        let mut roots = Vec::new();
        let big = rooted_chain(&mut h, &mut roots, 50_000);
        roots.push(big);
        h.collect_all(&mut roots);
        let alive = h.old.live;
        assert!(alive >= 150_000);
        // Let it go, keep a small one.
        roots.clear();
        let small = rooted_chain(&mut h, &mut roots, 100);
        roots.push(small);
        h.collect_all(&mut roots);
        assert!(
            h.old.live < alive / 100,
            "{} of {alive} still counted",
            h.old.live
        );
        assert!(well_formed(&h, roots[0], 100));
        // Its lines are allocated into again rather than new blocks made.
        let blocks = h.old.real_blocks;
        let again = rooted_chain(&mut h, &mut roots, 20_000);
        roots.push(again);
        assert!(
            h.old.real_blocks <= blocks,
            "{} blocks, from {blocks}",
            h.old.real_blocks
        );
        assert!(well_formed(&h, roots[1], 20_000));
    }

    #[test]
    fn empty_blocks_are_given_back() {
        let mut h = generational(1024, usize::MAX, 0);
        let mut roots = Vec::new();
        let big = rooted_chain(&mut h, &mut roots, 200_000);
        roots.push(big);
        h.collect_all(&mut roots);
        let before = h.old.real_blocks;
        roots.clear();
        h.collect_all(&mut roots);
        churn(&mut h, &mut roots, 200_000);
        assert!(
            h.old.real_blocks < before / 4,
            "{} of {before} blocks kept",
            h.old.real_blocks
        );
    }

    /// Refs rewired at random while a cycle marks a few fields per pause: the
    /// marker keeps losing paths to overwrites, and has to find them in the
    /// log. `verify` checks every cycle, and the chains are checked whole.
    fn rewire(h: &mut Heap, steps: usize, refs: usize, longest: u64) {
        let mut roots = vec![Value::Unit; refs];
        for i in 0..refs {
            roots[i] = alloc_rooted(h, &mut roots, Kind::Ref, 0, &[refs - 1]);
            h.set_field(roots[i].addr().unwrap(), 0, Value::Unit);
        }
        // What each ref holds: the length of its chain, or none.
        let mut lens: Vec<Option<i64>> = vec![None; refs];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..steps {
            let a = (next() % refs as u64) as usize;
            let b = (next() % refs as u64) as usize;
            match next() % 4 {
                0 => {
                    let n = (next() % longest) as i64;
                    let c = rooted_chain(h, &mut roots, n);
                    h.set_field(roots[a].addr().unwrap(), 0, c);
                    lens[a] = Some(n);
                }
                1 => {
                    let (ra, rb) = (roots[a].addr().unwrap(), roots[b].addr().unwrap());
                    let (va, vb) = (h.field(ra, 0), h.field(rb, 0));
                    h.set_field(ra, 0, vb);
                    h.set_field(rb, 0, va);
                    lens.swap(a, b);
                }
                2 => {
                    // Held only by a register for a while.
                    let ra = roots[a].addr().unwrap();
                    roots.push(h.field(ra, 0));
                    h.set_field(ra, 0, Value::Unit);
                    churn(h, &mut roots, 64);
                    let v = roots.pop().unwrap();
                    h.set_field(roots[b].addr().unwrap(), 0, v);
                    lens[b] = lens[a].take();
                }
                _ => churn(h, &mut roots, 32),
            }
        }
        h.collect_all(&mut roots);
        for (i, len) in lens.iter().enumerate() {
            let v = h.field(roots[i].addr().unwrap(), 0);
            match len {
                Some(n) => assert!(well_formed(h, v, *n), "ref {i}'s chain of {n} is broken"),
                None => assert_eq!(v, Value::Unit),
            }
        }
        assert!(h.cycles > 3, "only {} cycles ran", h.cycles);
    }

    #[test]
    fn overwrites_while_marking_a_little_at_a_time_lose_nothing() {
        let mut h = generational(512, 16, 0);
        rewire(&mut h, 30_000, 48, 40);
    }

    #[test]
    fn overwrites_while_marking_concurrently_lose_nothing() {
        let mut h = generational(512, usize::MAX, 2);
        rewire(&mut h, 60_000, 48, 40);
    }

    #[test]
    fn a_region_only_an_old_object_reaches_lives_until_it_does_not() {
        let mut h = generational(1024, usize::MAX, 0);
        let mut roots = Vec::new();
        let c = rooted_chain(&mut h, &mut roots, 100);
        let region = h.new_region();
        roots.push(h.compact_into(region, c).unwrap());
        let holder = alloc_rooted(&mut h, &mut roots, Kind::Ref, 0, &[0]);
        roots[0] = holder;
        h.promoting = true;
        h.collect(&mut roots);
        h.collect(&mut roots);
        assert!(roots[0].addr().unwrap() >= OLD_BASE);
        h.collect_all(&mut roots);
        h.collect_all(&mut roots);
        assert!(h.region(region).is_some(), "still reachable");
        let inner = h.field(roots[0].addr().unwrap(), 0);
        assert_eq!(chain_len(&h, inner), 100);
        h.set_field(roots[0].addr().unwrap(), 0, Value::Unit);
        h.collect_all(&mut roots);
        assert!(h.region(region).is_none(), "let go once nothing reaches it");
    }

    #[test]
    fn a_parcel_bigger_than_the_nursery_is_rebuilt_in_pieces() {
        let mut a = heap();
        let big = chain(&mut a, 5000);
        let parcel = a.export(big).unwrap();
        let mut b = generational(256, usize::MAX, 0);
        b.reserve(parcel.len());
        let mut roots = vec![b.import(&parcel)];
        assert!(well_formed(&b, roots[0], 5000));
        churn(&mut b, &mut roots, 10_000);
        b.collect_all(&mut roots);
        assert!(well_formed(&b, roots[0], 5000));
    }

    #[test]
    fn an_array_bigger_than_a_block_of_young_objects() {
        let mut h = generational(1024, usize::MAX, 0);
        let n = old::BLOCK + 100;
        let mut roots = Vec::new();
        h.reserve(1 + n);
        let young = h.alloc(Kind::Data, 9, &[Value::Int(42)]);
        let fields = vec![Value::Obj(young); n];
        let arr = h.alloc(Kind::Array, 0, &fields);
        assert!(arr >= OLD_BASE, "too big for the nursery");
        roots.push(Value::Obj(arr));
        churn(&mut h, &mut roots, 50_000);
        h.collect_all(&mut roots);
        churn(&mut h, &mut roots, 50_000);
        let arr = roots[0].addr().unwrap();
        let first = h.field(arr, 0);
        assert_eq!(h.field(arr, n - 1), first, "one object, shared");
        assert_eq!(h.field(first.addr().unwrap(), 0), Value::Int(42));
    }

    // --- evacuation ---------------------------------------------------------------

    /// Eight chains built a cell of each at a time, so they lie interleaved in
    /// the old generation, and seven of them dropped: every block left an
    /// eighth full.
    fn sparse(h: &mut Heap, cells: i64) -> Vec<Value> {
        let mut roots = vec![Value::Unit; 8];
        for j in 0..8 {
            roots[j] = alloc_rooted(h, &mut roots, Kind::Data, 0, &[]);
        }
        for k in 0..cells {
            for j in 0..8 {
                roots.push(Value::Int(k));
                let at = roots.len() - 1;
                let cell = alloc_rooted(h, &mut roots, Kind::Data, 1, &[at, j]);
                roots.pop();
                roots[j] = cell;
            }
        }
        roots.truncate(1);
        roots
    }

    #[test]
    fn sparse_blocks_are_emptied_and_given_back() {
        let mut h = generational(1024, usize::MAX, 0);
        let mut roots = sparse(&mut h, 40_000);
        // The first cycle finds the blocks sparse and chooses them; the second
        // records every pointer into them, and then they move.
        h.collect_all(&mut roots);
        let before = h.old.real_blocks;
        // Each cycle moves up to an eighth of what is alive.
        for _ in 0..8 {
            h.collect_all(&mut roots);
            h.old.release(usize::MAX);
        }
        assert!(h.evacuated_blocks > 0, "nothing was evacuated");
        assert!(
            h.old.real_blocks * 2 < before,
            "{} blocks from {before}",
            h.old.real_blocks
        );
        assert!(well_formed(&h, roots[0], 40_000));
        // And it all still holds together through more cycles.
        churn(&mut h, &mut roots, 100_000);
        h.collect_all(&mut roots);
        assert!(well_formed(&h, roots[0], 40_000));
    }

    #[test]
    fn evacuation_a_little_per_pause_while_the_program_runs() {
        let mut h = generational(1024, usize::MAX, 0);
        let mut roots = sparse(&mut h, 40_000);
        h.collect_all(&mut roots);
        // The second cycle, and the moves after it, happen in ordinary pauses:
        // with pointers into the moving blocks from the nursery and a `Ref`.
        let r = alloc_rooted(&mut h, &mut roots, Kind::Ref, 0, &[0]);
        roots.push(r);
        let cycles = h.cycles;
        let mut pauses = 0;
        while h.evacuated_blocks == 0 || h.evac.has_pending() {
            roots.push(Value::Unit);
            let n = roots.len() - 1;
            roots[n] = alloc_rooted(&mut h, &mut roots, Kind::Data, 2, &[0]);
            let holder = roots.pop().unwrap();
            h.set_field(roots[1].addr().unwrap(), 0, holder);
            if h.cycles == cycles {
                // Ask for the second cycle, as enough promotion would.
                h.old.allocated += 1 << 20;
            }
            churn(&mut h, &mut roots, 2_000);
            pauses += 1;
            assert!(pauses < 10_000, "evacuation never finished");
        }
        assert!(h.cycles > cycles);
        assert!(h.evacuated_blocks > 1, "all in one pause");
        let held = h.field(roots[1].addr().unwrap(), 0).addr().unwrap();
        assert_eq!(h.field(held, 0), roots[0]);
        assert!(well_formed(&h, roots[0], 40_000));
    }

    #[test]
    fn rewiring_refs_while_blocks_move_loses_nothing() {
        let mut h = generational(1024, 64, 0);
        rewire(&mut h, 40_000, 512, 160);
        assert!(h.evacuated_blocks > 0, "nothing was evacuated");
    }

    #[test]
    fn rewiring_refs_while_blocks_move_concurrently_loses_nothing() {
        let mut h = generational(1024, usize::MAX, 2);
        rewire(&mut h, 60_000, 512, 160);
        assert!(h.evacuated_blocks > 0, "nothing was evacuated");
    }

    #[test]
    fn a_pointer_only_promotion_records_is_moved_too() {
        let mut h = generational(1024, 4096, 0);
        let mut roots = sparse(&mut h, 40_000);
        h.collect_all(&mut roots);
        // A cell deep in the chain, in a block chosen to move.
        let (mut cell, mut n) = (roots[0], 40_000);
        while !h.old.blocks[old::block_of(cell.addr().unwrap())].is_tracked() {
            cell = h.field(cell.addr().unwrap(), 1);
            n -= 1;
            assert!(n > 0, "no cell in a chosen block");
        }
        roots.push(cell);
        let r = alloc_rooted(&mut h, &mut roots, Kind::Ref, 0, &[0]);
        roots[1] = r;
        // The second cycle begins, and marks a little per pause.
        h.old.allocated += 1 << 20;
        while h.marking.is_none() {
            churn(&mut h, &mut roots, 1_000);
        }
        // Made now, the holder is not in the cycle's snapshot, so the marker
        // never reads it; once promoted, only an old `Ref` holds it, so no root
        // or nursery scan finds it either. Only promoting it records its field.
        roots.push(cell);
        let at = roots.len() - 1;
        let holder = alloc_rooted(&mut h, &mut roots, Kind::Data, 7, &[at]);
        roots.truncate(2);
        h.set_field(roots[1].addr().unwrap(), 0, holder);
        let mut pauses = 0;
        while h.evacuated_blocks == 0 || h.evac.has_pending() || h.marking.is_some() {
            churn(&mut h, &mut roots, 2_000);
            pauses += 1;
            assert!(pauses < 10_000, "evacuation never finished");
        }
        let holder = h.field(roots[1].addr().unwrap(), 0).addr().unwrap();
        assert!(holder >= OLD_BASE, "promoted");
        assert!(well_formed(&h, h.field(holder, 0), n));
        assert!(well_formed(&h, roots[0], 40_000));
    }

    // --- the frame stack and the collector -------------------------------------------

    #[test]
    fn what_a_frame_holds_moves_with_it_when_its_block_is_evacuated() {
        // A frame is pushed with a bare store, so nothing records that it
        // points into a block chosen to move: evacuation has to treat frames
        // as it treats the registers, or the frame goes on naming where the
        // object used to be. (`verify` is on, so before this was so the
        // collection itself said "a pointer was not moved".)
        let mut h = generational(1024, usize::MAX, 0);
        let mut roots = sparse(&mut h, 40_000);
        h.collect_all(&mut roots);
        let (mut cell, mut n) = (roots[0], 40_000);
        while !h.old.blocks[old::block_of(cell.addr().unwrap())].is_tracked() {
            cell = h.field(cell.addr().unwrap(), 1);
            n -= 1;
            assert!(n > 0, "no cell in a chosen block");
        }
        roots.push(cell);
        let held = roots.len() - 1;
        let word = cell.bits();
        let frame = h.push_frame(0, 1, |_| word, |_| desc::REF);
        let mut cycles = 0;
        while roots[held] == cell {
            h.collect_all(&mut roots);
            cycles += 1;
            assert!(cycles < 32, "the cell's block never moved");
        }
        assert_eq!(h.field(frame, 0), roots[held], "the frame was left behind");
        assert!(well_formed(&h, roots[0], 40_000));
    }

    #[test]
    fn a_segment_of_two_chunks_is_walked_to_each_chunks_own_end() {
        // A `handle` inside a `handle`: the segment a clause cuts off is two
        // chunks. Where the lower one's frames end is in the upper one's
        // header, which is therefore not a place to keep anything else.
        let mut h = generational(1024, usize::MAX, 0);
        h.promoting = true;
        let mut roots = vec![Value::Unit];
        roots[0] = alloc_rooted(&mut h, &mut roots, Kind::Data, 0, &[]);
        let word = roots[0].bits();
        h.enter_frames();
        h.push_frame(0, 1, |_| word, |_| desc::REF);
        let tag = h.enter_frames();
        let lower = h.push_frame(0, 1, |_| word, |_| desc::REF);
        h.enter_frames();
        let mut upper = 0;
        for _ in 0..5 {
            upper = h.push_frame(0, 1, |_| word, |_| desc::REF);
        }
        let (top, end) = h.detach(tag).expect("the handler's chunk is on the stack");
        // Collections with the segment off the stack: every frame of both
        // chunks is a root, and what it holds moves.
        churn(&mut h, &mut roots, 5_000);
        assert_eq!(
            h.field(lower, 0),
            roots[0],
            "the lower chunk was not walked"
        );
        assert_eq!(
            h.field(upper, 0),
            roots[0],
            "the upper chunk was not walked"
        );
        // Put back, it is walked the same way on the stack, and the stack
        // pops into the lower chunk where that chunk's frames end.
        assert!(h.reattach(top, end));
        churn(&mut h, &mut roots, 5_000);
        assert_eq!(h.field(lower, 0), roots[0]);
        h.pop_to(lower);
        assert_eq!(h.fsp, lower);
    }

    #[test]
    fn a_chunk_below_the_top_is_scanned_until_it_holds_nothing_young() {
        let mut h = generational(1024, usize::MAX, 0);
        h.promoting = true;
        let mut roots = vec![Value::Unit];
        roots[0] = alloc_rooted(&mut h, &mut roots, Kind::Data, 0, &[]);
        let word = roots[0].bits();
        h.enter_frames();
        let lower = h.fcur;
        let frame = h.push_frame(0, 1, |_| word, |_| desc::REF);
        h.enter_frames();
        assert!(!h.clean_chunks.contains(&lower));
        // A survivor stays young for one collection and is promoted at the
        // next, so the chunk holding it comes clean by the third.
        for _ in 0..3 {
            h.collect(&mut roots);
        }
        assert_eq!(h.field(frame, 0), roots[0], "the frame was left behind");
        assert!(roots[0].addr().unwrap() >= OLD_BASE, "promoted by now");
        assert!(h.clean_chunks.contains(&lower), "still being rescanned");
        assert!(
            !h.clean_chunks.contains(&h.fcur),
            "the current chunk is never clean"
        );
        // Popping back into it makes it the current chunk again.
        h.pop_to(frame);
        assert!(!h.clean_chunks.contains(&lower));
    }

    // --- strings ------------------------------------------------------------------

    #[test]
    fn a_string_holds_its_bytes_through_a_collection() {
        for mut h in [heap(), generational(64, usize::MAX, 0)] {
            let text = "héllo, wörld -- more than one word of it";
            let a = h.alloc_str(text.as_bytes());
            let empty = h.alloc_str(b"");
            assert_eq!(h.kind(a), Kind::Str);
            assert_eq!(h.packed_len(a), text.len());
            assert_eq!(h.packed_len(empty), 0);
            let mut roots = [Value::Obj(a), Value::Obj(empty)];
            h.collect(&mut roots);
            let (a, empty) = (roots[0].addr().unwrap(), roots[1].addr().unwrap());
            assert_eq!(h.packed_bytes(a), text.as_bytes());
            assert_eq!(h.packed_bytes(empty), b"");
            assert_eq!(h.packed_bytes_in(a, 1, 3), "é".as_bytes());
            assert_eq!(h.packed_byte(a, 6), b',');
            assert_eq!(h.packed_find(a, "wörld".as_bytes(), 0), 8);
            assert_eq!(
                h.packed_find(a, b"o", 9),
                text[9..].find('o').map_or(-1, |i| (i + 9) as i64)
            );
            assert_eq!(h.packed_find(a, b"zzz", 0), -1);
            let same = h.alloc_str(text.as_bytes());
            let other = h.alloc_str("héllo, wörld -- more than one word of iT".as_bytes());
            assert!(h.packed_eq(a, same));
            assert!(!h.packed_eq(a, other));
            assert!(!h.packed_eq(a, empty));
        }
    }

    /// Word-at-a-time comparison agrees with comparing the bytes, across word
    /// boundaries, for prefixes, and for real zero bytes against a word's
    /// zero padding.
    #[test]
    fn strings_compare_as_their_bytes_do() {
        let mut h = heap();
        let texts: [&[u8]; 12] = [
            b"",
            b"a",
            b"abcdefgh",
            b"abcdefgh\0",
            b"abcdefghi",
            b"abcdefgi",
            b"abcdefg",
            b"\0",
            b"\0\0",
            b"Zebra",
            "é".as_bytes(),
            "z—more than two words of text".as_bytes(),
        ];
        let addrs: Vec<_> = texts.iter().map(|t| h.alloc_str(t)).collect();
        for (i, a) in texts.iter().enumerate() {
            for (j, b) in texts.iter().enumerate() {
                assert_eq!(
                    h.packed_compare(addrs[i], addrs[j]),
                    meadow_rt::text::compare(a, b),
                    "{a:?} against {b:?}"
                );
            }
        }
    }

    #[test]
    fn an_array_of_bytes_takes_a_byte_an_element() {
        assert_eq!(
            Heap::BYTE,
            desc::word(meadow_rt::num::Width::U8),
            "the descriptor packing is keyed on"
        );
        let mut h = generational(64, usize::MAX, 0);
        let words: Vec<Word> = (0..1000u64).map(|i| i % 256).collect();
        let before = h.used();
        let a = h.alloc_array(&words, Heap::BYTE);
        assert_eq!(h.kind(a), Kind::Bytes);
        assert!(
            h.used() - before <= 2 + 125,
            "1000 bytes in {} slots",
            h.used() - before
        );
        assert_eq!(h.array_len(a), 1000);
        assert_eq!(h.array_desc(a), Heap::BYTE);
        assert_eq!(
            h.array_value(a, 257),
            Value::Word(meadow_rt::num::Width::U8, 1)
        );
        assert_eq!(h.array_words(a, 254, 258), [254, 255, 0, 1]);
        let mut roots = [Value::Obj(a)];
        h.collect(&mut roots);
        let a = roots[0].addr().unwrap();
        assert_eq!(
            h.array_words(a, 0, 1000),
            words,
            "and it survives a collection"
        );

        // Nothing to pack, or not bytes: an ordinary array.
        let empty = h.alloc_array(&[], Heap::BYTE);
        assert_eq!((h.kind(empty), h.array_len(empty)), (Kind::Array, 0));
        let ints = h.alloc_array(&[1, 2], desc::INT);
        assert_eq!(h.kind(ints), Kind::Array);
        assert_eq!(h.array_value(ints, 1), Value::Int(2));
    }

    #[test]
    fn a_range_of_words_reads_only_that_range() {
        let mut h = heap();
        let fields: Vec<Value> = (0..100).map(Value::Int).collect();
        let a = h.alloc(Kind::Array, 0, &fields);
        let words: Vec<u64> = h.words_in(a, 95, 200).collect();
        assert_eq!(words, [95, 96, 97, 98, 99]);
        assert_eq!(h.words_in(a, 10, 10).count(), 0);
    }

    /// Objects too large for the nursery go straight to the old generation,
    /// and while a cycle is marking they are kept until the next one. A
    /// program making them faster than marking finishes used to grow the
    /// generation until it ran out of addresses; an overdue heap collects
    /// everything instead.
    #[test]
    fn large_garbage_made_while_marking_does_not_pile_up() {
        let mut h = generational(1 << 10, 64, 0);
        h.config.verify = false;
        let mut roots = Vec::new();
        // Enough live data that a cycle takes many pauses to mark.
        let live = rooted_chain(&mut h, &mut roots, 60_000);
        roots.push(live);
        let big = 1 << 14;
        let fields = vec![Value::Int(7); big];
        for _ in 0..400 {
            if !h.room_for(Heap::size_of(Kind::Array, big)) || h.wants_collection() {
                if h.overdue() {
                    h.collect_all(&mut roots);
                } else {
                    h.collect(&mut roots);
                }
                h.reserve(Heap::size_of(Kind::Array, big));
            }
            h.alloc(Kind::Array, 0, &fields);
        }
        assert!(well_formed(&h, roots[0], 60_000));
        // 400 of them made is 6.5 million slots; kept, they would all be here.
        assert!(
            h.old_slots() < 3_000_000,
            "the old generation grew to {} slots",
            h.old_slots()
        );
    }
}
