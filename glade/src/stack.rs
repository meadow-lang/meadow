//! **Growable stacks**: a program run with `--stack growable` calls as deep
//! as memory allows, as Glade's always could (`docs/RUNTIME-MERGE.md`).
//!
//! Every stack the runtime runs code on -- `main`'s, a thread's, a handler's
//! segment -- has a *limit*: the lowest address a function may begin below
//! and still have room for itself and for what the runtime's own calls under
//! it need. Where a function begins, a growable program compares the stack
//! pointer with it ([`low`]). Past it, the function runs on a new piece of
//! stack ([`grow`]), whose own limit is then the one compared with, and comes
//! back to the old piece when it returns. So a deep recursion costs a switch
//! of stacks once every piece -- [`CHUNK`] bytes of them -- and nothing else;
//! and a program run with a fixed stack, which never compares, pays nothing.
//!
//! The limit is the running thread's: `Ctx::stack_limit` in a program of
//! threads, which moves between OS threads with its thread, and
//! `meadow_stack_limit_single` in one without. A handler's segment keeps its
//! own: it sets it when it starts, takes it back whenever it is resumed
//! (`segments::suspend`), and the frame that drove it puts the one outside
//! back when it stops. A limit of `0` is no limit: code the runtime did not
//! start, a library's export entered from C, never moves.
//!
//! A piece in use when its segment's continuation is abandoned is not given
//! back: the segment is reset without its frames returning, and the piece
//! with them. That is memory a program loses once for each continuation it
//! abandons while deeper than a piece -- what a fixed stack would have
//! stopped it for.

use crate::heap::Word;
use corosensei::stack::{DefaultStack, Stack};

/// How much a piece of stack reserves. Reserved, not committed: what is
/// never touched costs address space only.
pub const CHUNK: usize = 64 << 20;

/// What a function must have left of its stack when it begins: its own frame,
/// and the runtime's calls below it, which print a structure a thousand deep
/// or walk a heap.
pub const RED: usize = 1 << 20;

/// The running thread's limit, in a program with one thread.
#[unsafe(no_mangle)]
pub static mut meadow_stack_limit_single: usize = 0;

fn cx() -> &'static mut crate::ctx::Ctx {
    // Safety: the running thread's context, used by it alone.
    unsafe { &mut *crate::ctx::get() }
}

/// How many pieces a thread keeps to use again.
const POOL: usize = 4;

fn slot() -> *mut usize {
    if crate::ctx::threaded() {
        // Safety: the running thread's context.
        unsafe { &raw mut (*crate::ctx::get()).stack_limit }
    } else {
        &raw mut meadow_stack_limit_single
    }
}

/// The running stack's limit.
pub fn limit() -> usize {
    // Safety: this thread's alone.
    unsafe { *slot() }
}

pub fn set_limit(limit: usize) {
    // Safety: as above.
    unsafe { *slot() = limit }
}

/// The limit of a stack whose lowest address is `bottom`.
pub fn above(bottom: usize) -> usize {
    bottom + RED
}

/// The limit of `stack`, which code is about to run on.
pub fn limit_of(stack: &impl Stack) -> usize {
    above(stack.limit().get())
}

/// The limit of the stack running now, `size` bytes of which lie below about
/// here: for an OS thread's stack, whose bounds the runtime did not choose.
pub fn below_here(size: usize) -> usize {
    above(here().saturating_sub(size))
}

/// About where the stack pointer is.
#[inline(always)]
pub fn here() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// Whether a function beginning here has too little stack left.
#[inline(always)]
pub fn low() -> bool {
    here() < limit()
}

/// `f`, on a new piece of stack: what a function that begins [`low`] does.
pub fn grow<R>(f: impl FnOnce() -> R) -> R {
    let mut piece = cx().chunks.pop().unwrap_or_else(|| {
        DefaultStack::new(CHUNK)
            .unwrap_or_else(|e| crate::fail(&format!("could not grow the stack: {e}")))
    });
    let outside = limit();
    set_limit(limit_of(&piece));
    let r = corosensei::on_stack(&mut piece, f);
    set_limit(outside);
    // This thread's context again, though perhaps on another OS thread.
    let pool = &mut cx().chunks;
    if pool.len() < POOL {
        pool.push(piece);
    }
    r
}

/// For emitted code: `f`, given `args`, on a new piece of stack. `f` is the
/// function's own trampoline, which calls it with the arguments it began
/// with.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_stack_grow(
    f: extern "C" fn(*const i64) -> Word,
    args: *const i64,
) -> Word {
    grow(|| f(args))
}
