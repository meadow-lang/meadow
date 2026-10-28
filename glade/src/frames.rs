//! **The frames a debugger reads**: when an engine that is not compiled code
//! runs a program under a debugger -- the VM, `glade/vm` -- each of
//! its activations keeps a record on the native stack, linked to the one
//! that called it, and this is the head of that chain.
//!
//! The head is the running segment's, as the shadow chain's is
//! (`crate::shadow`): a handler's segment takes its own back whenever it is
//! resumed (`segments::suspend`), and the frame that drove it puts the one
//! outside back when it stops. So the chain a debugger walks is always the
//! running code's -- its callers, and the frames of the segments it runs
//! inside -- and never a suspended continuation's, nor one that was given up.
//!
//! What a record holds is the engine's business: the runtime only keeps the
//! head. `0` is no record, which is all a program that is not being debugged
//! ever has.

use crate::heap::Word;

/// The running thread's head, in a program with one thread.
static mut FRAMES_SINGLE: Word = 0;

fn slot() -> *mut Word {
    if crate::ctx::threaded() {
        // Safety: the running thread's context.
        unsafe { &raw mut (*crate::ctx::get()).frames }
    } else {
        &raw mut FRAMES_SINGLE
    }
}

/// The running segment's innermost record.
pub fn top() -> Word {
    // Safety: this thread's alone.
    unsafe { *slot() }
}

pub fn set_top(head: Word) {
    // Safety: as above.
    unsafe { *slot() = head }
}
