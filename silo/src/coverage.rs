//! **What a program has reached**, for `Process.covered`: how many places in
//! it this thread has been, which a fuzzer steers by (`Std.Test.Fuzz`) -- an
//! input that gets somewhere no input before it did is one worth making
//! more of.
//!
//! The engine running the program does the noting, as Glade's machine does:
//! Silo's interpreter, from the first time any thread asks, notes every
//! instruction it runs in what it enters after (`silo/vm`). Native code notes
//! nothing, and a native program is told 0, as it always has been.
//!
//! What is noted is the thread's, in its context: it goes where the thread
//! goes, between OS threads, as the thread waits and is woken.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

/// The places a thread has reached, and how many.
#[derive(Default)]
pub struct Reached {
    /// A bit a place, by its number, for the numbers an engine counts from
    /// nought; and the rest, which a reloaded program's code may have.
    seen: Vec<u64>,
    far: HashSet<u64>,
    count: u64,
}

/// Places numbered below this are kept as bits.
const NEAR: u64 = 1 << 28;

impl Reached {
    #[inline]
    fn note(&mut self, at: u64) {
        if at < NEAR {
            let (word, bit) = ((at / 64) as usize, 1u64 << (at % 64));
            if word >= self.seen.len() {
                self.seen.resize(word + 1, 0);
            }
            if self.seen[word] & bit == 0 {
                self.seen[word] |= bit;
                self.count += 1;
            }
        } else if self.far.insert(at) {
            self.count += 1;
        }
    }
}

static ASKED: AtomicBool = AtomicBool::new(false);

/// Whether any thread has asked yet: from when an engine notes.
#[inline(always)]
pub fn asked() -> bool {
    ASKED.load(Ordering::Relaxed)
}

/// Whether the engine keeps records of what it runs -- for a debugger, a
/// profile, or since the program asked what it reached: one flag, which an
/// interpreter reads where each call begins.
static RECORDING: AtomicBool = AtomicBool::new(false);

#[inline(always)]
pub fn recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

/// Keep records from now on: what an engine says for a debugger or a
/// profile, before the program starts.
pub fn record() {
    RECORDING.store(true, Ordering::Relaxed);
}

/// The running thread has reached place `at`.
#[inline]
pub fn note(at: u64) {
    // Safety: the running thread's context, which only it touches.
    let cx = unsafe { &mut *crate::ctx::get() };
    cx.reached.get_or_insert_with(Default::default).note(at);
}

/// How many places the running thread has reached since it first asked, or,
/// `afresh`, since now: what was noted is forgotten, and the answer is 0.
pub fn covered(afresh: bool) -> i64 {
    ASKED.store(true, Ordering::Relaxed);
    RECORDING.store(true, Ordering::Relaxed);
    // Safety: as above.
    let cx = unsafe { &mut *crate::ctx::get() };
    if afresh {
        cx.reached = Some(Default::default());
    }
    cx.reached.get_or_insert_with(Default::default).count as i64
}
