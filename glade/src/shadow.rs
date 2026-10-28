//! **The shadow stack**: what the native frames of a segment hold, where a
//! discarded continuation can find it.
//!
//! A continuation that is never resumed is discarded without unwinding (see
//! [`crate::segments`]), and a native frame has no map saying which of its
//! slots are references -- so whatever the frames on the discarded segments
//! were holding across a call would never be given up. Most of it is a
//! *frame not built*: a non-tail call's continuation, which `meadow-llvm`
//! keeps as values in the caller's native frame while the callee runs, and
//! which that frame's code owns and gives up once the call returns.
//!
//! So around such a call the emitted code links a record of those values into
//! a chain, and unlinks it when the call returns:
//!
//! ```text
//! [ prev, n, value 0, descriptor 0, ..., value n-1, descriptor n-1 ]
//! ```
//!
//! a stack allocation of the caller's, and the chain's head is a word only the
//! running code touches. A descriptor is the one the value would be erased
//! with, or [`TOKEN`] for a reuse token -- a block whose fields are already
//! gone, which is freed rather than erased. Only a program that can capture a
//! continuation builds records at all: see `meadow_llvm::emit`.
//!
//! **Each segment has a chain of its own.** Segments are entered and left in
//! a strict nest while they run, but a suspended one stays suspended while
//! its handler goes on pushing and popping its own frames, so one chain for
//! all of them would interleave. [`crate::segments`] saves the head when a
//! segment suspends and puts it back when it resumes, and a new segment
//! starts from an empty chain. What a discarded continuation's segments were
//! holding is then exactly what their saved chains say, and [`give_up`] gives
//! it up before their stacks go back.
//!
//! The runtime links records of its own where one of its frames holds
//! something across running Meadow code, and there is one such frame:
//! `meadow_enter` holds the continuation the `handle`'s value goes to. The
//! others that run Meadow code hold nothing a discard could lose -- `drive`
//! parks the segment it drives in the context, `meadow_detach` and
//! `meadow_reattach` hand what they are given straight on, and a thread's
//! first frame is never part of a continuation -- and no primitive calls
//! back into Meadow code at all. A new one that did, holding a value across
//! the call, would have to link a record as `meadow_enter` does.

use crate::heap::{self, Word};

/// A record's descriptor for a reuse token: freed, not erased.
pub const TOKEN: i64 = -1;

/// The chain's head for a program with one thread. One with several keeps it
/// in each thread's context: see [`crate::ctx::Ctx::shadow`].
#[unsafe(no_mangle)]
pub static mut meadow_shadow_single: Word = 0;

/// Where the running thread's head is, for the emitted code of a program that
/// can have more than one thread. The context moves between OS threads with
/// its thread, and never within memory, so the address holds for as long as
/// the thread runs.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_shadow_head() -> *mut Word {
    slot()
}

fn slot() -> *mut Word {
    if crate::ctx::threaded() {
        // Safety: the running thread's context.
        unsafe { &raw mut (*crate::ctx::get()).shadow }
    } else {
        &raw mut meadow_shadow_single
    }
}

/// The running segment's head.
pub fn head() -> Word {
    // Safety: see `slot`; this thread's alone.
    unsafe { *slot() }
}

pub fn set(head: Word) {
    // Safety: as above.
    unsafe { *slot() = head }
}

/// Give up every value the records from `head` down hold.
///
/// # Safety
///
/// `head` must be a chain whose records are still in memory: that of a
/// segment whose stack has not yet been reset or reused.
pub unsafe fn give_up(head: Word) {
    let mut at = head;
    while at != 0 {
        let p = at as *const i64;
        // Safety: a record, as the caller promises.
        let (prev, n) = unsafe { (*p, *p.add(1)) };
        for i in 0..n.max(0) as usize {
            // Safety: within the record's `n` pairs.
            let (v, d) = unsafe { (*p.add(2 + 2 * i) as Word, *p.add(3 + 2 * i)) };
            if d == TOKEN {
                if v != 0 {
                    heap::clean(v);
                }
            } else {
                heap::erase(v, d);
            }
        }
        at = prev as Word;
    }
}
