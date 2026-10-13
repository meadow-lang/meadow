//! **Collecting cycles, without ever tracing from roots.**
//!
//! Counting misses a cycle: two blocks that hold each other keep one another's
//! count above zero after the last reference from outside is gone. The usual
//! answer is a tracing collector as a backstop, and this runtime cannot have
//! one -- a program compiled ahead of time keeps its values in native frames
//! and registers, with no map saying which slots hold references, so there is
//! no way to enumerate the roots.
//!
//! What works without roots is **trial deletion**, from Bacon and Rajan,
//! _Concurrent Cycle Collection in Reference Counted Systems_ (ECOOP 2001).
//! Take a block whose count went down without reaching zero -- a cycle, if
//! there is one, must contain such a block -- and ask a local question: if the
//! references *inside* this subgraph did not exist, would anything still point
//! at it? Subtract the internal edges and see whose count reaches zero. Those
//! are reachable only from each other, which is what garbage means, and the
//! answer needs nothing but the subgraph itself.
//!
//! The three passes, over the buffered candidates:
//!
//! 1. **Mark**: from each candidate, walk the subgraph colouring it gray and
//!    taking one off the count of every block an edge inside it points to.
//!    What is left in a count is the references from outside.
//! 2. **Scan**: a gray block whose count says something outside still points
//!    at it is alive, and so is everything it reaches: colour those black and
//!    put back what the mark pass took. Everything else is white.
//! 3. **Collect**: the white blocks are a garbage cycle. Free them. Their
//!    edges to live blocks are already accounted for, because the mark pass
//!    took those references away and the scan pass did not put them back.
//!
//! Nothing at all for a program that cannot make a cycle. A cycle needs a
//! store into a block that already exists, which in Meadow is `setRef` or a
//! write into a mutable array; immutable data is built bottom-up and points
//! only backwards. The compiler looks for those two primitives, and a program
//! without them is emitted with no candidate buffering in its counting helpers
//! and no collector in its runtime (`meadow_llvm::emit`, `cycles`).
//!
//! # What it does not catch
//!
//! Only a `Ref` and a mutable array are kept as candidates, since every cycle
//! must contain one and the program must have been holding it to tie the knot
//! -- so letting go of it is a candidate, and that is how the cycles people
//! actually write are found. A cycle whose `Ref` is let go of *while the cycle
//! is still alive*, and which becomes garbage later when something else lets
//! go of a block that is not a `Ref`, is not noticed: nothing buffers a
//! candidate at that last moment. That cycle leaks, as every cycle did before
//! this module existed.
//!
//! Keeping every block that is decremented, which is what the algorithm as
//! published does, closes that hole and costs 11% of `wordfreq` and 27% of
//! `binarytrees` in buffering alone, on programs that never make a cycle at
//! all. The trade is written down here rather than made quietly.
//!
//! # What it costs, and when it runs
//!
//! For a program that can, the collector runs from [`crate::heap::acquire`]
//! once [`TRIGGER`] candidates have gathered, and looks at [`BATCH`] of them,
//! leaving the rest for the next run. The pause is what one batch reaches:
//! the garbage it finds, and the live blocks on that garbage's edge. Not the
//! live set, and not the whole heap.
//!
//! So the pause does not grow with how much garbage has piled up -- that is
//! spread over runs -- but it does grow with the size of a single cycle,
//! since a cycle is freed in the run that proves it garbage or not at all.
//! One cycle holding a list of a million nodes is one long pause, and no
//! batching helps; bounding that needs the walk itself to be incremental,
//! which needs a write barrier this runtime does not have.

use crate::heap::{self, Word};

/// Candidates gathered before the collector runs at all. Buffering one is
/// nearly free, so this is set high enough that a program which makes
/// candidates and no cycles -- which is most of them -- hardly ever pays for
/// a run.
pub const TRIGGER: usize = 1024;

/// The most candidates the collector ever waits for, where its runs have been
/// finding nothing: see [`crate::heap::Heap::paced`].
pub const TRIGGER_MOST: usize = 1 << 18;

/// Runs in a row that free nothing before the collector waits for twice as
/// many candidates: what [`TRIGGER`] candidates take, [`BATCH`] at a time.
pub const FUTILE: u32 = (TRIGGER / BATCH) as u32;

/// Candidates looked at in one run, which is what bounds a pause: the walks
/// visit what these reach, and nothing else. What is left over waits, still
/// buffered, and the next run takes the next batch.
///
/// A run must be allowed to free what it proved garbage -- put that off and
/// the proof is lost -- so leaving candidates behind takes the care in
/// [`collect`]: a leftover that an earlier run already proved white is
/// collected where it is found.
pub const BATCH: usize = 64;

/// Blocks a run may walk before it gives up, which is what holds a pause
/// under a millisecond however big a cycle is. The mark pass counts what it
/// visits, and past this it stops and puts back exactly what it took -- it
/// recorded every block it coloured, so undoing it is the same walk backwards
/// and costs no more than the walk did.
///
/// The candidate it gave up on is not lost: it waits in the heap's oversized
/// list, and is walked with no budget at all once the heap has doubled (see
/// [`crate::heap::Heap::set_oversized`]), which is the one place a long pause
/// can still happen -- and there it is paid for by a doubling's worth of
/// allocation, or at exit, where nothing is waiting on it.
pub const BUDGET: usize = 6_000;

unsafe extern "C" {
    /// Whether the program can make a cycle at all: the emitted module says
    /// so, having looked for a store into a block that already exists. See
    /// `meadow_llvm::emit`.
    static meadow_cycles: u8;
}

/// Whether this program can make a cycle. A program that cannot pays nothing
/// for this module: no buffering, no colours, no collection.
#[inline(always)]
pub fn possible() -> bool {
    // Safety: a constant the emitted module defines.
    unsafe { meadow_cycles != 0 }
}

/// A block whose count went down without reaching zero. Kept for the next
/// run, unless it is already kept or cannot be part of a cycle.
///
/// # Safety
///
/// `v` must be a live block.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_candidate(v: Word) {
    if !possible() || !heap::is_block(v) || !worth_keeping(v) {
        return;
    }
    heap::keep_candidate(v);
}

/// Is this block worth keeping as a candidate?
///
/// Only a `Ref` and a mutable array are, and that is what makes this cheap
/// enough to have. A cycle needs a store into a block that already exists, so
/// every cycle contains one of those two; and the program has to be holding
/// it to store into it, so its count goes down when that hold goes -- which
/// is a candidate, and the cycle is found from there.
///
/// What this gives up is stated where it is paid for: see "What it does not
/// catch" in the module docs.
fn worth_keeping(v: Word) -> bool {
    matches!(heap::kind(v), heap::CELL | heap::MUT_ARRAY) && heap::holds_refs(v)
}

/// Can a block of this kind hold a reference the collector should follow? A
/// string or a big integer holds bytes, and a channel, a thread or a `TVar`
/// holds a number the scheduler looks up.
///
/// A compact is the interesting one: it is one object, and what it holds is a
/// region -- closed, so nothing in it points out, and never counted, so there
/// is nothing in there for this to subtract. Walking in would be a pause the
/// size of the region for no possible result, which is exactly what
/// `Std.Compact` promises does not happen.
pub(crate) fn may_cycle(v: Word) -> bool {
    !matches!(
        heap::kind(v),
        heap::STRING
            | heap::BIGINT
            | heap::CHANNEL
            | heap::TASK
            | heap::TVAR
            | heap::ONCE
            | heap::COMPACT
    )
}

/// What `v` points at that could be part of a cycle with it: its fields that
/// are references to blocks, less the leaves -- a string, a big integer, a
/// compact -- which point at nothing a cycle could run through. The passes
/// never colour a leaf or touch its count, so a column of ten thousand names
/// costs a walk a look at each and no more; and [`collect_white`] lets go of
/// the leaves a garbage cycle held, which it would otherwise not see.
fn children(v: Word, out: &mut Vec<Word>) {
    out.clear();
    // An array of numbers -- a table of a program's -- points at nothing,
    // however long it is: not looked through a word at a time.
    if !walked_into(v) {
        return;
    }
    for i in 0..heap::len(v) {
        if heap::field_desc(v, i) != meadow_rt::desc::REF {
            continue;
        }
        let x = heap::field(v, i);
        if heap::is_block(x) && may_cycle(x) {
            out.push(x);
        }
    }
}

/// Let go of the leaves the white block `v` holds: the references
/// [`children`] leaves out, which no pass took away.
fn release_leaves(v: Word, heap: &mut heap::Heap) {
    if !may_cycle(v) || !heap::holds_refs(v) {
        return;
    }
    for i in 0..heap::len(v) {
        if heap::field_desc(v, i) != meadow_rt::desc::REF {
            continue;
        }
        let x = heap::field(v, i);
        if heap::is_block(x) && !may_cycle(x) {
            heap.let_go(x);
        }
    }
}

/// Whether a walk goes into `v`: not a leaf, not a block that holds nothing
/// a cycle runs through, and not one waiting for a walk of its own
/// ([`heap::DEFERRED`]).
fn walked_into(v: Word) -> bool {
    may_cycle(v) && heap::may_hold_cycles(v) && !heap::deferred(v)
}

/// What a walk pays for coming to `v`: one for the block, and one for each
/// field it looks through -- so that a table of ten thousand
/// slots costs what it takes to look at, and a walk that comes to one runs
/// out of budget rather than looking through it at every run.
fn cost(v: Word) -> usize {
    1 + if walked_into(v) { heap::len(v) } else { 0 }
}

/// The count, as the algorithm wants it: how many references there are, where
/// the header holds one fewer. It goes negative while the mark pass has taken
/// the inside edges away, which is the whole point of the pass.
fn refs(v: Word) -> i32 {
    // Safety: a live block's count.
    (unsafe { *(v as *const u32) } as i32) + 1
}

fn set_refs(v: Word, n: i32) {
    // Safety: as `refs`.
    unsafe { *(v as *mut u32) = (n - 1) as u32 }
}

/// What the collector has done, for `MEADOW_SILO_CYCLES`: runs, candidates
/// looked at, blocks freed, and how long the program was stopped -- in total
/// and for the longest single run.
#[derive(Default)]
pub struct Tally {
    pub runs: u64,
    pub candidates: u64,
    pub freed: u64,
    pub nanos: u64,
    pub longest: u64,
    /// Candidates that reached more than [`BUDGET`] blocks, and so were put
    /// off; and the runs with no budget that then had to walk them.
    pub deferred: u64,
    pub unbounded: u64,
}

thread_local! {
    static TALLY: std::cell::RefCell<Tally> = const {
        std::cell::RefCell::new(Tally {
            runs: 0,
            candidates: 0,
            freed: 0,
            nanos: 0,
            longest: 0,
            deferred: 0,
            unbounded: 0,
        })
    };
}

/// Is the collector's tally wanted? `MEADOW_SILO_CYCLES`.
pub fn counting() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MEADOW_SILO_CYCLES").is_some())
}

/// What this thread's collector has done.
pub fn report() {
    if !counting() {
        return;
    }
    TALLY.with(|t| {
        let t = t.borrow();
        if t.runs == 0 {
            eprintln!("aot: no cycle collection: nothing was ever a candidate");
            return;
        }
        eprintln!(
            "aot: {} cycle collections over {} candidates, {} blocks freed",
            t.runs, t.candidates, t.freed
        );
        eprintln!(
            "aot: {:.3} ms collecting, longest pause {:.3} ms, {:.1} us each on average",
            t.nanos as f64 / 1e6,
            t.longest as f64 / 1e6,
            t.nanos as f64 / t.runs as f64 / 1e3
        );
        if t.deferred > 0 {
            eprintln!(
                "aot: {} candidates reached over {} blocks and were put off, \
                 {} of those runs walked with no budget",
                t.deferred, BUDGET, t.unbounded
            );
        }
    });
}

/// Look at every candidate there is, however many runs that takes: what a
/// program asks for when it wants to know what it really has left, and what
/// the leak check asks before it counts.
pub fn collect_all(heap: &mut heap::Heap) {
    if !possible() {
        return;
    }
    heap.retry_oversized();
    while heap.has_candidates() {
        run(heap, usize::MAX, usize::MAX);
    }
}

/// Look for cycles among the candidates gathered so far, inside a pause.
pub fn collect(heap: &mut heap::Heap) {
    run(heap, BATCH, BUDGET);
}

/// Walk the candidates that were too big for a pause, with no budget: the
/// heap has grown enough that the memory is worth the stop. See
/// [`heap::Heap::set_oversized`].
pub fn collect_oversized(heap: &mut heap::Heap) {
    // One at a time, and only the ones that were waiting: a pause of one
    // large cycle is as short as this can be made, and there is no reason to
    // put two of them together. They were appended, and a run takes from the
    // end, so this takes exactly them.
    for _ in 0..heap.retry_oversized() {
        run(heap, 1, usize::MAX);
    }
}

/// One run of the three passes, over at most `batch` candidates and walking
/// at most `budget` blocks. See the module docs.
fn run(heap: &mut heap::Heap, batch: usize, budget: usize) {
    let roots = heap.take_candidates(batch);
    if roots.is_empty() {
        return;
    }
    let began = counting().then(std::time::Instant::now);
    let was = heap.freed();
    let mut kids = Vec::new();
    let mut work: Vec<Word> = Vec::new();

    // 1. Mark. A candidate that is no longer purple has been handled since it
    //    was buffered; one that died while buffered is freed here, which is
    //    why a buffered block is never freed where it dies.
    let mut marked: Vec<Word> = Vec::new();
    let mut grayed: Vec<(Word, u64)> = Vec::new();
    let mut left = budget;
    let mut walked = 0usize;
    for (i, &s) in roots.iter().enumerate() {
        // A block that died while it was waiting here is freed now, and never
        // walked: what its count says is one reference, and it has none. This
        // is the whole reason a buffered block is not freed where it dies.
        if heap::dead(s) {
            heap::set_buffered(s, false);
            heap.free_dead(s);
            continue;
        }
        // White: an earlier run's scan pass proved this garbage and its
        // collect pass left it alone, because it was still waiting here. It
        // cannot have come back to life -- nothing outside the cycle points at
        // it, which is what white means -- so it and what it reaches go now.
        if heap::colour(s) == heap::WHITE {
            heap::set_buffered(s, false);
            collect_white(s, heap, &mut work, &mut kids);
            continue;
        }
        if heap::colour(s) == heap::PURPLE && refs(s) > 0 {
            grayed.clear();
            walked += 1;
            let had = left;
            if mark_gray(s, &mut left, &mut work, &mut kids, &mut grayed) {
                marked.push(s);
                continue;
            }
            // Out of budget. Put back every count this walk took -- `grayed`
            // is exactly the blocks whose children it went through -- and end
            // the run here. What was marked before stands: those walks
            // finished, and this one touched nothing they had coloured.
            unmark(&grayed, &mut kids);
            if had >= budget {
                // This one candidate ate a whole budget, so no run of this
                // size will ever get through it: it waits for one with none.
                heap.set_oversized(s);
                if counting() {
                    TALLY.with(|t| t.borrow_mut().deferred += 1);
                }
            } else {
                // The budget went on the candidates before it. It gets a
                // fresh one in the next run, and will very likely fit.
                heap.return_candidates(&roots[i..=i]);
            }
            heap.return_candidates(&roots[i + 1..]);
            break;
        }
        heap::set_buffered(s, false);
    }

    // 2. Scan.
    for &s in &marked {
        scan(s, &mut work, &mut kids);
    }

    // 3. Collect. Unbuffer first: `collect_white` leaves a block that is still
    //    buffered alone, since another candidate may yet reach it.
    for &s in &marked {
        heap::set_buffered(s, false);
    }
    for &s in &marked {
        collect_white(s, heap, &mut work, &mut kids);
    }
    heap.paced(walked, heap.freed() - was);
    heap.reschedule();

    if let Some(began) = began {
        let took = began.elapsed().as_nanos() as u64;
        let freed = heap.freed() - was;
        let seen = roots.len() as u64;
        TALLY.with(|t| {
            let mut t = t.borrow_mut();
            t.runs += 1;
            t.unbounded += u64::from(budget == usize::MAX);
            t.candidates += seen;
            t.freed += freed;
            t.nanos += took;
            t.longest = t.longest.max(took);
        });
    }
}

/// Colour the subgraph gray, taking one off the count for every edge inside
/// it. What is left in a count is the references from outside.
///
/// Every block coloured goes into `grayed`, with the colour it had, and every
/// one costs a block of `left`. Running out answers `false` with the walk
/// unfinished, for [`unmark`] to undo.
fn mark_gray(
    s: Word,
    left: &mut usize,
    work: &mut Vec<Word>,
    kids: &mut Vec<Word>,
    grayed: &mut Vec<(Word, u64)>,
) -> bool {
    work.clear();
    work.push(s);
    while let Some(v) = work.pop() {
        if heap::colour(v) == heap::GRAY {
            continue;
        }
        let c = cost(v);
        if *left < c {
            return false;
        }
        *left -= c;
        grayed.push((v, heap::colour(v)));
        heap::set_colour(v, heap::GRAY);
        children(v, kids);
        for &t in kids.iter() {
            set_refs(t, refs(t) - 1);
            work.push(t);
        }
    }
    true
}

/// Undo an unfinished [`mark_gray`]: it went through the children of exactly
/// the blocks in `grayed`, so putting those counts back and those colours
/// back leaves the heap as it was. Nothing else saw the difference, since the
/// program is stopped for the whole run.
fn unmark(grayed: &[(Word, u64)], kids: &mut Vec<Word>) {
    for &(v, was) in grayed {
        children(v, kids);
        for &t in kids.iter() {
            set_refs(t, refs(t) + 1);
        }
        heap::set_colour(v, was);
    }
}

/// A gray block something outside still points at is alive, and so is all it
/// reaches; the rest is white.
fn scan(s: Word, work: &mut Vec<Word>, kids: &mut Vec<Word>) {
    work.clear();
    work.push(s);
    while let Some(v) = work.pop() {
        if heap::colour(v) != heap::GRAY {
            continue;
        }
        // Alive, or taken for alive: see `heap::DEFERRED`.
        if refs(v) > 0 || heap::deferred(v) {
            scan_black(v, kids);
            continue;
        }
        heap::set_colour(v, heap::WHITE);
        children(v, kids);
        for &t in kids.iter() {
            work.push(t);
        }
    }
}

/// Alive after all: put back what the mark pass took, and say so.
fn scan_black(s: Word, kids: &mut Vec<Word>) {
    let mut work = vec![s];
    while let Some(v) = work.pop() {
        // Two parents can push the same block before either is looked at, and
        // its children must not be counted up twice.
        if heap::colour(v) == heap::BLACK {
            continue;
        }
        heap::set_colour(v, heap::BLACK);
        children(v, kids);
        for &t in kids.iter() {
            set_refs(t, refs(t) + 1);
            if heap::colour(t) != heap::BLACK {
                work.push(t);
            }
        }
    }
}

/// The white blocks are reachable only from each other. Free them.
fn collect_white(s: Word, heap: &mut heap::Heap, work: &mut Vec<Word>, kids: &mut Vec<Word>) {
    work.clear();
    work.push(s);
    // Freed after the walk, not during it: the walk reads their fields.
    let mut doomed = Vec::new();
    while let Some(v) = work.pop() {
        if heap::colour(v) != heap::WHITE || heap::buffered(v) {
            continue;
        }
        heap::set_colour(v, heap::BLACK);
        children(v, kids);
        for &t in kids.iter() {
            work.push(t);
        }
        doomed.push(v);
    }
    for &v in &doomed {
        release_leaves(v, heap);
    }
    for v in doomed {
        heap.free_cycle(v);
    }
}
