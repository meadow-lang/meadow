# Cancellation, cleanup and time

*The design for what a thread does as it ends, for the sockets it held, and
for waiting on the clock. Built in three steps; the status of each is at the
end.*

A green thread can be cancelled (`Std.Thread.cancel`, see
`meadow_rt::thread`). It stops softly, failing "cancelled" at its next
operation the runtime answers, or hard, at the second preemption point after
the cancel if it performs nothing. Three things were missing from that:

1. **Nothing of the thread ran on the way out.** Meadow's handlers have no
   clause that runs when the computation they handle is abandoned, and a
   runtime failure goes past every handler anyway. So a thread that failed,
   or was cancelled, could not remove the temporary file it made or tell
   anyone it was gone.
2. **What it held outside its heap stayed.** A socket belongs to the run, not
   to a heap, so a cancelled server thread kept its port until the program
   ended.
3. **Waiting for time held an OS thread.** `Time.sleep` slept on the worker
   that ran the thread, so a cancel could not wake it, a thousand sleeping
   threads needed a thousand workers' worth of waiting, and there was nothing
   to build a timeout on.

Each part below says what was weighed, what was chosen, and why.

## 1. Release actions

### What it has to do

- Run in the soft tier. A cancelled thread stops *at* an operation, so its
  own code can still run afterwards; that is when cleanup should happen.
- Have an answer for the hard tier, where the thread was stopped in pure
  code.
- Run on normal exit too. A cleanup that runs only when things go wrong is
  half of one.
- Run at known points only. The two tiers are safe because nothing is
  interrupted between two of its operations. Cleanup must not bring back
  asynchronous exceptions, which every cleanup must guard against and which
  masking exists to undo.
- Make sense with effect handlers: a continuation a handler drops, and one it
  keeps and resumes later.
- Have a stated order. The language specifies evaluation order (the
  tutorial's "Evaluation order"), so release order is specified too: the
  newest first.

### What was weighed

**A `finally` clause on `handle`.** This is a language change that touches
the parser, both front ends, every lowering, and every engine. A handler is
also the wrong scope for it. Handlers are about operations. A resource's
lifetime is about a computation, and the most common cleanup has no effect of
its own to handle. Worst, a runtime failure (a cancel, a division by zero)
does not go through handlers at all. It ends the thread past all of them, so
a handler clause would never be reached on the one path that needs it most.

**A per-function `defer`.** After lowering, functions are not a scope the
runtime can see: calls become jumps, tail calls loop, and definitions are
inlined. A `defer` needs a scope the program states.

**A library `bracket` over a list the runtime keeps for each thread.** The
runtime keeps a stack of release actions per thread. `Std.Thread` pushes onto
it and pops from it, and the runtime runs what is left on it when the thread
ends. This is what was chosen. It needs two primitives and no language
change, and the runtime's half works the same way on every engine: when a
thread ends, call the functions left on its stack.

### The design

```meadow
use Std.Thread as Thread

fun copyTo (path : String) =
  Thread.bracket
    (\() -> makeTemp ())
    (\tmp -> removeQuietly tmp)
    (\tmp -> writeThenRename tmp path)
```

- `bracket acquire release use` runs `acquire`, registers `release` of what
  it gave, runs `use`, and then releases. `finally release act` is a
  `bracket` with nothing to acquire. `defer release` only registers: the
  release runs when the innermost `bracket` or `finally` around it ends, or
  when the thread does if there is none.
- Every thread has a **release stack**. Registering pushes and gives back a
  key. A release runs at the first of these:
  1. its `bracket` ends normally. The bracket takes back, newest first,
     everything registered since its own registration that is still there,
     then its own, and runs each one in place;
  2. the thread ends, by returning, failing or being cancelled. The runtime
     runs every release still registered, newest first, before `await` sees
     the end.
- **Order is newest first, always.** What a later registration holds may
  depend on what an earlier one does, as a file in a temporary directory
  depends on the directory.
- **A release may perform only what the runtime answers**, as a spawned
  function may (`Std.Eff`), and this is checked by its type. On the way out
  of a failure there are no handlers left: the computation that had them is
  gone. So a release that performed an effect of the program's own would have
  nobody to answer it. Restricting releases to the runtime's effects makes
  that a type error, as it is for `spawn`. On the normal path a release runs
  in place, so a handler of one of the runtime's effects around the bracket
  (`Time.withClock`, say) sees what it does. On the way out of a failure that
  handler is gone too, and the runtime answers.
- **A release that fails** ends that release only. The rest still run. A
  thread that had succeeded then fails with the first release's failure, and
  one that had failed keeps its own failure.

### Cancellation while releasing

A release runs with **cancellation held off**. Its operations answer as
usual, waits included, and a cancel does not wake it from a wait. Otherwise
a cancel landing during cleanup would cut the cleanup short at its next
operation, which is exactly what cleanup must not be.

Held off is not ignored. The hard tier still applies, to each release on its
own: in a cancelled thread, a release still running at its 16th preemption
point after it began (where ordinary code gets two; on Silo, 16 ticks of its
timer rather than one) is ended there,
counts as a failed release, and the next one runs. A release gets more grace
than code because it has work it must finish, but no release can keep a
cancelled thread alive by computing forever. A release that waits forever
does keep its thread, as any waiting code would. Under held-off cancellation
the runtime cannot tell a wait that will end from one that will not.

This brings back no asynchronous exception. Releases run only at the two
points above: the end of a bracket, which the program reaches, and the end
of the thread, after its own code has stopped. Inside a release, a cancel
changes nothing until the release is over.

There is one gap, between `acquire`'s last operation and the registration,
and the tiers' own rules close it. A cancelled thread fails at its next
operation *before* the operation does anything, so an `acquire` that
finished was not cancelled at its start. It was cancelled during that last
operation at the earliest. The hard tier ends a thread only at its second
preemption point after the cancel, at least a slice later (a tick of the
timer on Silo). An `acquire` that ends with what acquires (a `connect`,
say) is a few steps from the registration, which fit well inside that, so
the registration is made. The registration itself never notices a cancel.
An `acquire` that computes for long after its last operation can be ended
before it registers, and what it got is lost to the release. For a socket,
the ownership of part 2 still closes it.

### Continuations a handler drops or keeps

Meadow's handlers are one-shot: a continuation is resumed at most once, and
resuming one twice is an error. So a bracket's normal end happens at most
once, and a release runs at most once. Nothing like "release on each
resumption" has to be defined.

A continuation a handler *drops* (for example, `Std.Exn`'s `toResult` when
something is raised) never reaches the end of the brackets inside it. Their
releases stay registered until an enclosing bracket ends, which takes back
everything registered after it, or until the thread ends. This is the
structured rule: **a resource does not outlive the bracket it was acquired
in, nor the thread**. A long-lived loop that may abandon brackets puts each
round under a `finally` (or runs it on a thread of its own), so that what a
dropped round held is released when the round ends.

A continuation a handler *keeps* and resumes later, as a generator's does,
is treated the same way. If it is resumed after its enclosing bracket has
ended, what it holds has been released. This is the same mistake as
returning a lazy value from a bracket in any language. The stack is a stack:
a bracket that ends takes back whatever was registered after it, so a
continuation resumed inside a newer bracket and ended there releases that
bracket's registrations too.

### Threads still running when `main` ends

They are stopped with the program, as before, without unwinding. Running
their releases would mean waiting for threads the program has finished
with. A program that wants their cleanup cancels them and awaits them before
`main` returns. `main`'s own releases do run, on every way out.

### The primitives

- `threadDefer : (() -> () ! Eff) -> Int ! { Thread | e }` pushes a release
  and answers its key. It never notices a cancel, so a resource just acquired
  is always registered.
- `threadTake : Int -> Bool -> Maybe (() -> () ! e) ! { Thread | r }` takes
  back the newest registration whose key is at least the one given, and holds
  cancellation off until the next `threadTake` says (with `True`) that the
  release it took has run. It answers `None` when nothing is left. The answer
  is typed at the caller's row, so that running it in place does not add
  `Eff` to every bracket's type. That is sound because registration already
  required the release to perform only the runtime's effects, and anything
  that performs `Thread` runs under the runtime.

## 2. Sockets a thread held

The only handles a program holds outside its heap are sockets: listeners,
streams, and TLS streams, which `Std.Net` and `Std.Tls` make. `Std.Process`
runs a child to its end and `Std.Fs` reads and writes whole files, so neither
gives a handle. A library `Std.Ffi` opens belongs to the process.

### Ownership

- A socket **belongs to the thread that opened it**: by `listen`, `accept`,
  `connect`, or a TLS handshake (the TLS stream takes the plain one's place).
- A socket handle that **leaves a thread's heap is handed on**. If it goes
  into a spawned function, a message or an answer, the thread it arrives in
  owns it from then on. If it goes into a `TVar` or a `Compact`, which any
  thread may read, nobody owns it, and it stays open until it is closed or
  the run ends.
- A thread that **ends, whichever way**, closes every socket it still owns,
  after its releases have run, so a release can still say goodbye on one.

Closing on a normal end as well as on a failure follows from threads sharing
nothing mutable. Everything a thread holds that it did not hand on is
unreachable once the thread is over, so nobody can use the socket again, and
leaving it open only makes the port and the peer wait for the end of the run.

### What was weighed

**Closing at a thread's end only what it opened.** This is wrong for the
commonest server shape: one thread accepts and hands each connection to a
worker. If the accepting thread were cancelled, every live connection would
be closed under its worker.

**Each thread owning what it last used.** This depends on timing. A worker
handed a connection that has not read from it yet would lose the connection
if the accepting thread ended first.

**Releases in `Std.Net`.** A `bracket` cannot follow a handle to another
thread, which is the hard case.

**Handing on with the handle**, which was chosen, needs the runtime to see a
socket in a value it copies. A socket is `Std.Net`'s `Listener n` or
`Stream n`, data around an `Int`. The two constructors become **roles**
(`meadow_rt::roles`), as `Maybe.Just` is one: the program declares which
constructors they are, and every engine's copier (Glade's parcels and
regions, Silo's parcels and regions, the CEK machine's sends) looks for
them. The table of sockets (`meadow_rt::net`) keeps each socket's owner. A
program that declares no such roles, such as one built from Cut that has no
`Std.Net`, gets no automatic closing and keeps the old behaviour.

## 3. Sleep on the scheduler, `race` and `timeout`

### Sleep

`Time.sleep` with no handler, on a thread a scheduler runs, **parks the
thread on a timer** instead of sleeping on its worker. Each scheduler keeps
its sleepers' deadlines. Glade has a timer thread per run, Silo has one per
process, and the CEK machine waits for the nearest deadline itself when
there is nothing else to run. The worker goes on running other threads, so
ten thousand sleepers need no more workers than one does. A sleeper counts
as something that will wake, so a program whose only other threads wait on
it is not a deadlock. A sleeping thread that is cancelled is woken at once
and fails, as a thread waiting on a channel is.

A machine with no scheduler (a procedural macro's, or a Silo program that
never spawns, where there is nothing else to run and nothing to cancel it)
still sleeps in place.

### `race` and `timeout`

```meadow
use Std.Thread as Thread
use Std.Time as Time

fun main () =
  match Thread.timeout 100 (\() -> slowLookup ()) with
  | Just answer -> println answer
  | None -> println "gave up after 100ms"
```

- `race f g` runs `f` and `g` on threads of their own. The first to end
  decides: its value comes back as `Left` or `Right`, or race fails with its
  failure. The other is cancelled, and `race` returns once that one has run
  its releases.
- `timeout ms act` is a race of `act` against a sleep of `ms` milliseconds,
  in `Std.Time`'s unit: `Just` the answer, or `None`. A timed-out action is
  cancelled, so it stops softly at its next operation, or at once if it is
  sleeping or waiting, and its releases run before `timeout` returns.

Both run their functions on other threads, so a function given to them may
perform only what the runtime answers, exactly as for `spawn`. The other way
would run `act` in the caller and have a timer stop the caller. That would
mean one thread interrupting another in the middle of its code, which is the
asynchronous exception the whole design avoids, and the caller could not go
on afterwards anyway. A cancel always ends a thread, so what is timed out has
to be a thread of its own. A caller whose effects `act` needs handles them
inside `act`, as a spawned function does.

## Status

| Part | What | Where |
| ---- | ---- | ----- |
| 3 | `Time.sleep` parks on the scheduler, and a cancel wakes it | first step |
| 1 | release stack, `bracket`, `finally`, `defer`; `race`, `timeout` | second step |
| 2 | sockets owned by threads, handed on, closed at the end | third step |
