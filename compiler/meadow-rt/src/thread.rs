//! What the engines share about green threads.
//!
//! A green thread has a heap of its own and never touches another's. The only
//! ways a value crosses between threads are the function a thread is started
//! with, a message on a channel, and the result `await` hands back -- and each is
//! a copy. On the bytecode VM it is literally one: the value is lifted out of
//! one heap and rebuilt in the other, by the OS thread running the receiver. The
//! CEK machine shares immutable values instead of copying them, which no
//! program can tell apart from a copy -- provided nothing mutable crosses.
//!
//! So what may cross is decided here, and every engine refuses the same things
//! in the same words:
//!
//! * a `Ref` or a mutable array, which would be one cell reachable from two
//!   heaps -- shared mutable state, which is exactly what threads with their own
//!   heaps rule out;
//! * a continuation, which is a piece of the thread that captured it.
//!
//! A `Compact` crosses without being copied at all: its region belongs to no
//! heap, and every thread reads the same one.
//!
//! Functions may cross -- `spawn` could not work otherwise -- carrying copies of
//! exactly what they capture. The engines agree on what that is: the VM's
//! closures hold exactly their free variables, and the CEK machine looks at a
//! closure's free variables rather than its whole environment.
//!
//! Scheduling is not shared. The VM runs threads on every core, preempting each
//! after a slice of instructions; the CEK machine runs them one at a time, in
//! order. Programs whose answer depends on interleaving will differ, as they
//! would between two runs on the VM.
//!
//! # Cancellation
//!
//! A thread is stopped by asking it to stop (`threadCancel`). Where it stops
//! depends on what it is doing, in two tiers, so that no thread can ignore a
//! cancel and no thread is ever interrupted between two steps of code that
//! deals with the world.
//!
//! **Softly, at an operation.** A cancelled thread stops at the next operation
//! it performs that the runtime answers: a thread operation -- `spawn`,
//! `await`, `yieldNow`, `newChannel`, `send`, `receive`, `cancel`, the wait of
//! a transaction's `retry` -- or an operation of `Console`, `Fs`, `Net`,
//! `Time`, `Random`, `Process`, `Log` or `Test` that no handler of its own
//! answers. The operation fails, before it does anything, with [`CANCELLED`].
//! A thread already waiting in one -- on a channel, on another thread, in
//! `retry`, on a socket -- is woken to fail. So a worker that does any I/O at
//! all sees a cancel at its next step without anyone remembering to check for
//! it, which is the mistake a cancel that has to be polled for invites.
//!
//! **Hard, at a preemption point.** A thread that performs nothing -- a pure
//! loop -- reaches no operation, but it is preempted now and then to let the
//! others run. The first preemption point after the cancel marks it, and the
//! next ends it there: on the VM at least a slice of instructions later, on
//! Silo at least a tick of its timer later. That grace lets a thread that was
//! about to perform an operation, or that asks (`threadCancelled`), stop at an
//! operation instead. A thread that asks and is told it has been cancelled has
//! taken the cancel on itself: it is not ended at a preemption point, and it
//! stops where it chooses, or at its next operation.
//!
//! Either way the thread is over and runs nothing more of its own, and
//! `await` of it fails with the same message. Ending a thread in pure code is
//! safe because threads share nothing mutable: a thread stopped half way
//! through a computation can leave nobody else's state half made, and it never
//! sees anything itself. Meadow's handlers have no clause that runs on the way
//! out, so in neither tier does any of the thread's code run after the cancel
//! takes hold; what it held outside its heap -- on Silo, the stacks of the
//! handlers it was inside -- the runtime gives back.
//!
//! None of this is an exception a thread did not ask for. The failure is the
//! runtime's, not an effect: no handler in the thread, an `Exn` handler
//! included, can catch it and carry on, so a thread cannot be talked out of
//! stopping, and code that does not ask never has to guard its cleanup against
//! being cut short, which is what asynchronous exceptions make every cleanup
//! do, and what masking exists to undo.
//!
//! A request is not an outcome. A thread that finishes before the cancel takes
//! hold, or that asks and returns what it had, answers `await` with that.
//! Cancelling a thread that has finished does nothing, and cancelling twice is
//! cancelling once. A wait already answered when the request comes keeps its
//! answer; the thread stops at its next operation.
//!
//! What cannot be woken: an operation the runtime makes in place, on the OS
//! thread running the thread -- a file read, a `Time.sleep` -- runs to its
//! end, and the thread fails at the operation after it. A wait on a socket is
//! made on an I/O thread of its own, so the thread is woken at once; the call
//! itself goes on to its end, and what it answers is dropped. A socket the
//! thread opened stays open: it is the run's, not the thread's heap's.

/// The error for a value that cannot be passed to another thread.
pub fn unsendable(what: &str) -> String {
    format!("thread: cannot pass {what} to another thread; threads share only immutable data")
}

/// The error when every thread is waiting and none can wake another.
pub const DEADLOCK: &str =
    "deadlock: every thread is waiting on a channel or a thread that will never answer";

/// What a cancelled thread fails with, and so what `await` of it fails with.
pub const CANCELLED: &str = "cancelled";
