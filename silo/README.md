# Silo (`meadow-silo`): the runtime for programs compiled by LLVM

This is what a program built with `meadow build --runtime silo` links against.
There is no bytecode in that executable, no interpreter and no tracing
collector: the program is machine code from LLVM, it runs on the native stack,
and its memory is managed by counting references the way the AxCut paper does.

```sh
meadow build --release --runtime silo        # the executable, under native/silo/
meadow run --release --runtime silo          # build it and run it
meadow run --release --runtime silo --leaks  # and say what it still held at exit
meadow test --std --release --runtime silo   # the standard library's tests, on it
```

The other runtime, `meadow-glade`, is the default: a bytecode VM, a JIT and a
generational collector, which is what the debugger and the REPL need. Both are
kept, and `benchmarks/README.md` measures them against each other and against
other languages. `docs/SILO.md` is the design; this file is the part of it worth
reading first, which is how the counting works.

## Counting, and why it is exact

Reference counting is usually approximate: a compiler emits an increment where
it thinks a value is copied and a decrement where it thinks one dies, and
getting that wrong leaks or crashes. Here it is not a guess. The program is put
into a form where **every name is used exactly once**, and in that form the
counting is forced: there is exactly one place a reference can be copied, and
exactly one place it can die, and both are written down as statements before
any code is emitted.

That form is the AxCut paper's, and the pass that produces it is
`compiler/meadow-llvm/src/linear.rs`.

### The environment is linear

`meadow_seq` lowers Meadow to AxCut, a sequent-calculus IR the bytecode backend
also uses. Its statements read an environment of names:

| statement           | what it does                                        |
| ------------------- | --------------------------------------------------- |
| `substitute [x, y]` | rebuild the environment as exactly these names      |
| `let z = K(x, y)`   | build a data value from fields                      |
| `switch x`          | branch on a constructor, binding its fields in arms |
| `new f {…} [x]`     | build a closure capturing names                     |
| `invoke f#m`        | enter method `m` of `f`                             |
| `jump L`            | go to a block, environment unchanged                |

As `meadow_seq` produces it, that IR is **not** linear: `let` reads its fields
and leaves them in the environment, `switch` leaves the scrutinee, and a
`substitute` does not have to list the names it is dropping. The bytecode
backend does not care, because a collector finds the garbage either way.

Linearization rewrites it so that it does care. It walks each block with a map
of how many times each name is used from that point on, and at every statement
inserts, before the statement runs:

- **`Erase(x)`** for each name the statement does not use and nothing after it
  uses. Erasing happens as early as it can, so memory comes back at the last
  use rather than at the end of a block.
- **`Share(x)`** where something consumes a name that is used again, or
  consumes it more than once — a `substitute [x, x]` shares once, and a
  `let z = Pair(x, x)` where `x` lives on shares twice.

What comes out consumes exactly as the paper's statements do: `let` consumes
its fields, `new` its captures, `switch` its scrutinee, `invoke` the object and
the arguments, `jump` the whole environment. A primitive is the exception: it
**borrows** its arguments and returns something owned, which is what lets
`hash x` not touch `x`'s count at all.

A name whose representation is not a reference — an `Int`, a `Bool`, a
`Float` — goes through the same bookkeeping, and its share and erase emit
nothing. A name of a type variable's type is shared and erased **with its
descriptor**, a second value the lowering passes alongside it, and the emitted
code asks the descriptor at run time whether there is a count to touch.

### What a count is

A block's first word holds its length and its count, and the count is the
number of references **besides one**. A fresh block has 0. "Is this the last
reference?" is a compare against zero, not against one, which is the paper's
representation and it matters more than it looks: the common case in a linear
program is a block nobody else holds, and that case is a test of a word that is
already in cache.

```text
word 0   extra references: u32 (0: the only one) | length: u32
word 1   kind: u8 | element descriptor | flags | meta: u32
words    field descriptors, four bits each, sixteen to a word
fields
```

`share` adds one. `erase` takes one away, or — if it was the last — puts the
block on a _pending_ list, **with its fields still in it**, and does not look
inside.

### Loading fields is where the saving is

The paper's third operation is the one that makes this cheap, and it is why
`switch` and `invoke` consume what they name. When an arm loads a data value's
fields, or a method loads its captures, the block is being consumed, so the
code does this (`emit::release`):

- **count is 0** — nobody else holds this block. The fields move out into
  registers, no counts are touched at all, and the block is handed straight
  back to its size class as reusable. What a `switch` on a unique value costs
  is the load of the count, the compare, and that hand-back.
- **count is not 0** — somebody else holds it. The count goes down by one, and
  each field that was loaded is shared, because now there is one more reference
  to it.

So a linear program that builds a value, matches on it and drops it does no
counting whatsoever: the build writes the fields, the match moves them out, and
the block goes on the free list of its size class. Counting is paid only where
a value really is shared.

### Freeing is lazy, and bounded

`erase` of the last reference does not recurse. It pushes the block, fields
intact, onto the pending list. The work of erasing those fields happens in
`acquire` — the next time the program wants a block — a step at a time, where
a step is at most a fixed number of fields: one step at every allocation, and
more only while the size class the program is asking for has nothing clean in
it. A list of a million nodes does not stop the program when its head goes; it
is taken apart a little at each subsequent allocation, and its blocks come back
as they are finished.

This is what keeps every operation constant work. No erase ever walks a
structure, and no allocation ever waits for a whole structure to be walked.

### Cycles, without tracing from roots

Counting misses a cycle, and the usual answer — a tracing collector as a
backstop — is not available here: values live in native frames and registers
with no map saying which slots hold references, so the roots cannot be
enumerated. What works without roots is **trial deletion**, from Bacon and
Rajan's _Concurrent Cycle Collection in Reference Counted Systems_ (ECOOP
2001), and `silo/src/cycles.rs` is it.

Take a block whose count went down without reaching zero — a cycle must
contain one — and ask a local question: subtract the references that come from
_inside_ the subgraph it reaches, and see whose count reaches zero. Those are
reachable only from each other, which is what garbage means. Three passes do
it: mark gray subtracting the inside edges, scan putting back everything an
outside reference still holds, and collect what is left white.

**A program that cannot tie a knot has none of it.** A cycle needs a store
into a block that already exists, which in Meadow is `setRef` or a write into
a mutable array, and it must store a _reference_: the compiler looks for that,
and a program without one is emitted with no candidate buffering in its
counting helpers and no collector in its runtime.

**Pauses are bounded, not amortized away.** A run looks at 64 buffered
candidates and walks at most 6,000 blocks; past that the mark pass stops and
puts back exactly what it took, which it can do because it recorded every
block it coloured. Once the collector starts it keeps going, one bounded run
per allocation, until the buffer is empty — the program runs in between, since
allocation is where a run happens. Measured on cyclic garbage:

| what                                  | garbage freed | longest pause |
| ------------------------------------- | ------------- | ------------- |
| 200,000 cycles holding 50 nodes each  | 10.4M blocks  | 0.35 ms       |
| 2,000 cycles holding 500 nodes each   | 1.0M blocks   | 0.18 ms       |
| 200 cycles holding 100,000 nodes each | 17.9M blocks  | 10.9 ms       |

The last row is the one exception, and it is inherent: a cycle is freed in the
run that proves it garbage or not at all, so **one cycle bigger than the
budget is one long pause**. Those candidates are put aside and walked with no
budget only once the heap has doubled — so the pause is paid for by a
doubling's worth of allocation, and the memory is bounded rather than held to
exit. Bounding that too would need the mark pass itself to be incremental,
which needs a write barrier this runtime does not have.

**A program that makes candidates and no cycles pays less and less.** The
collector starts once 1,024 candidates have gathered. While its runs walk
from candidates and free nothing, it waits for twice as many before the next
round of runs, up to 262,144, and the first run that frees a cycle puts that
back to 1,024. A candidate that died while it waited is freed without a walk,
and in a program whose candidates are short-lived that is most of them: an
interpreter whose every scope is a map of cells, shared with the scopes made
from it, lets go of a shared cell at every return. MeadowBoot's CEK machine
running one `lexer!` call spent 72% of 312 seconds in the collector, which
freed nothing, and takes 54 seconds since. A doubling of the heap still starts
a run whatever the count of candidates, so what waiting can cost in memory is
bounded as before.

**What it does not catch.** Only a `Ref` and a mutable array are kept as
candidates, because every cycle contains one and the program must have been
holding it to tie the knot. A cycle whose `Ref` is let go of while the cycle
is still alive, and which becomes garbage later when something that is not a
`Ref` is let go of, is never noticed. Keeping every block that is decremented
— the algorithm as published — closes that hole and costs 11% of `wordfreq`
and 27% of `binarytrees` in buffering alone, on programs that never make a
cycle at all. `MEADOW_SILO_CYCLES` reports what the collector did, and
`--leaks` still reports what a run really held at exit.

### What it cannot do

**A continuation that is never resumed leaks what its frames hold.** Discarding
a suspended stack segment does not unwind it, because the native frames on it
carry no maps saying which slots hold references.

## What is in here

| module                | what it is                                                                     |
| --------------------- | ------------------------------------------------------------------------------ |
| `heap.rs`             | blocks, `share`/`erase`/`acquire`/`clean`, size classes, the pending list      |
| `cycles.rs`           | trial deletion, the candidate buffer, and what bounds a pause                  |
| `ctx.rs`              | one green thread's state: its heap, segments, literals, spill area             |
| `prims.rs`            | the primitives the emitted code does not do inline                             |
| `native.rs`           | the effects that reach the world: `Console`, `Fs`, `Process`, `Time`, `Random` |
| `segments.rs`         | effect handlers as stack segments, and where a `handle` ends                   |
| `sched.rs`            | green threads, channels and transactions, M:N over OS threads                  |
| `parcel.rs`           | copying a value out of one thread's heap and into another's                    |
| `value.rs`, `show.rs` | reading a word by its descriptor, and printing one                             |

The compiler half lives in `compiler/meadow-llvm`: `linear.rs` is the pass
described above, and `emit.rs` writes the LLVM IR.

## The rest of the design, in one paragraph each

**The calling convention** is LLVM's `ghccc`: ten arguments in registers --
`tailcc` and eight on aarch64, with nothing on the stack in either -- the
rest through a per-thread spill area, and every jump and invoke a `tail call`,
so a loop is a loop and nothing grows. Each block of the IR is a function;
everything inside one is basic blocks and SSA, so a `substitute` emits no moves
at all.

**Frames are the native stack.** Where a continuation is used for nothing but
the one call it was made for, no object is built: the call is an ordinary
`call`, the continuation's code is what runs when it returns, and invoking it
is `ret`. A continuation used any other way is an ordinary object with a
method.

**Effects are stack segments**, one coroutine per `handle`, with the
continuation captured by suspending it. A segment ends exactly where its
`handle` does — the runtime takes the continuation the handle's value goes to
out of the target `Ref` and leaves a native return in its place, which is what
stops handlers in a loop from piling up a stack each.

**Primitives** take one of three roads, and which one decides what a loop
costs: emitted inline (arithmetic, an array's length, indexing one, reading or
writing a `Ref`), an entry of their own taking arguments in registers (`hash`,
`==`, `stringIndexOf`, `stringSlice`), or the generic `meadow_prim`, which
takes its arguments in memory and decodes them from their descriptors. Moving
one road inwards is measurable: `binarytrees` against an arena writes a `Ref`
once per node, and inlining that write took the benchmark from 1.36s to 314ms.

**Threads** are green, M:N over a worker per core, each with a heap of its own
so counting needs no atomics. What crosses between threads is copied. A thread
that neither waits nor finishes gives way at a safe point once its turn is up.

**A program that cannot spawn pays for none of that**: the emitter looks for a
`spawn`, and finding none, emits no safe points and a spill area that is an
ordinary global, and the runtime keeps its one thread's state in a static with
no thread-local lookup and starts no scheduler at all.

## Building and testing it

The library is an ordinary cargo crate, built by `meadow` when it needs it:

```sh
cd aot && cargo build --release        # the static library
cd compiler && cargo test -p meadow-llvm --test native
```

Those tests compile small programs with `meadow-llvm`, link them against this
library, run them, and check three things: the answer matches the AxCut
abstract machine's, the program exits cleanly, and `MEADOW_SILO_LEAKS` reports
nothing live at exit. A program that abandons a continuation is excused the
last one, for the reason two sections up.

The emitted module and this library must agree about everything above, so the
library exports a symbol whose name is a hash of both crates' sources
(`build.rs`), and the module refers to it. Change one without rebuilding the
other and the program does not link, which is the check working.

## Environment

| variable             | what it does                                              |
| -------------------- | --------------------------------------------------------- |
| `MEADOW_THREADS`     | how many OS threads run green threads (`meadow run -j N`) |
| `MEADOW_SILO_LEAKS`  | report blocks still held at exit (`meadow run --leaks`)   |
| `MEADOW_SILO_PRIMS`  | count the primitives a run called, and report them        |
| `MEADOW_SILO_CYCLES` | the cycle collector's runs, what they freed, and pauses   |
| `MEADOW_CLANG`       | the clang to compile and link with                        |
