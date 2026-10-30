# Silo: AxCut to LLVM, with reference counting

**Silo** is Meadow's second runtime system, in `silo/`: `meadow build --runtime
silo` compiles a program with a backend of its own, for
production: the program is compiled all the way to machine code by LLVM, runs
on the native stack, and manages its memory by reference counting, as the
AxCut paper does. There is no bytecode, no interpreter and no tracing
collector in the executable.

The memory discipline is the paper's:

> Philipp Schuster, Marius Müller, Klaus Ostermann and Jonathan Immanuel
> Brachthäuser. _Compiling Classical Sequent Calculus to Stock Hardware: The
> Duality of Compilation._ Proc. ACM Program. Lang. 9, OOPSLA1, Article 142
> (April 2025). <https://doi.org/10.1145/3720507>. Artifact:
> <https://github.com/se-tuebingen/oopsla-2025-artifact-axcut>, whose
> `axcut/source/X86_64/Coder.idr` is the reference for what each statement
> emits.

Where this document says "the paper", it means that, and where Meadow departs
from it the reason is given. Glade, the default runtime, is the other way
round -- bytecode, native code for the blocks the code generator can do, the
interpreter for the rest, and the collector the JIT and the debugger share --
and stays as it is, so the two can be measured against each other on the same
program.

This document is the design the implementation follows. Where the
implementation is not there yet, [Stages](#stages) says so.

## Pipeline

```text
core ──meadow_seq::lower──▶ AxCut ──linearize──▶ linear AxCut ──emit──▶ LLVM IR (.ll)
                                                                          │ clang -O2
                                                    libmeadow_silo ──link──▶ executable
```

- `meadow_seq::lower` is shared with the bytecode backend: the same AxCut.
- **Linearize** (`compiler/meadow-llvm/src/linear.rs`) makes every copy and
  every drop of a reference explicit, which is the paper's discipline. See
  [Linearity](#linearity).
- **Emit** (`compiler/meadow-llvm/src/emit.rs`) writes LLVM IR as text. clang
  optimizes, compiles and links it; no LLVM library is linked into `meadow`.
- **The runtime** (`aot/`, the static library `meadow_silo`) is the heap, the
  primitives, the effects that reach the world, stack segments, and the
  scheduler. It does not depend on `meadow-glade`.

Primitives reach the runtime by one of three roads, and which one decides
what a loop costs. **Inline**: arithmetic and comparison, the length of an
array or a string, indexing an array, and reading or writing a `Ref` are
emitted as instructions, with the bounds check where one is needed
(`emit::inline_prim`). **An entry of its own**: `hash`, `==`, `stringIndexOf`,
`stringSlice` and a store of a reference into a mutable array take their
arguments in registers (`emit::direct`). **The generic road**: everything else
goes through `meadow_prim`, which takes its arguments in memory and decodes
them from their descriptors.

Where the line falls is worth measuring rather than guessing. `binarytrees`
written against an arena spends one `Ref` write per node on its bump counter:
as a call into the runtime that benchmark took 1.36s, and emitted inline it
takes 314ms.

## Values

A value is a 64-bit word, represented as `meadow_seq::Rep` says: `Int`, `Float`
(its bits), an immediate (`()`, `Bool`, `Char`, sized integers, `Float32`, an
interned name's key), or a **reference** -- data, a closure, a string, an
array, a record, a `BigInt`, a `Ref`. Only references are counted. A value of a
type variable's type is counted when its descriptor, held in another name at
run time, says it is a reference.

In the paper a value of data or codata type is two words: a memory block,
which is null when there are no fields, and a tag or a method table. So a
nullary constructor and a closure that captures nothing allocate nothing, and
a `switch` reads its tag from a register. Meadow keeps one word per value --
its generic code, its descriptors and the runtime's primitives all assume one
-- and gets the same effect by tagging the word:

| word          | is                                                                                                                        |
| ------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `0`           | nothing: a continuation that returns natively (see [Frames](#frames-are-the-native-stack))                                |
| odd           | an object with no block: `tag << 1 \| 1` for a nullary constructor, `table << 1 \| 1` for a closure that captures nothing |
| even, not `0` | the address of a block                                                                                                    |

Counting skips `0` and odd words, as the paper's `share` and `erase` skip a
null block.

## Blocks

```text
word 0   extra references: u32 (0: this is the only one) | length: u32
word 1   kind: u8 | flags: u8 | spare: u16 | meta: u32
         meta: a data value's tag, a closure's method table, a string's
         length in bytes
words    field descriptors, four bits each, sixteen to a word -- absent for a
         uniform block (an array, a string), whose one descriptor is in flags
fields
```

The count is the paper's: the number of references _besides_ one, so a fresh
block has 0 and "is this the last reference?" is a compare against zero. The
descriptors are what erasing a block needs to know which fields to release,
and what printing needs to know how to show them. A closure's `meta` indexes
the program's method tables.

The paper has one size of block, and chains an object with more fields than a
block holds through the block's last field. Meadow has arrays and strings,
which want indexing in constant time, so blocks come in **size classes**
instead, one object to a block. Literals -- strings, big integers -- are made
once at start-up and held by a global that is never erased.

An array's block has **room to grow**: an array of `n` elements is given
`heap::array_room(n)` slots -- `n` itself up to 8, then the next of 12, 16, 24,
32, 48, ... -- so at most a third is spare. The room is a function of the
length alone, so no block records it and the free path works it out as it
does a block's size. `arrayPush` and `arrayConcat` **consume** the array they
grow (see `emit::consumes`): the linearization gives them a reference of their
own, sharing it first where the old array is still wanted, so a count of zero
inside them means nobody else can see the array, and the new elements go into
its room in place. Building an array a push at a time is linear, where copying
at every push made it quadratic.

### Acquiring and releasing, lazily

Freeing is the paper's, and it is what makes every operation constant work: no
erase ever walks a structure.

- **Erasing** a block with other references decrements its count. Erasing the
  last reference puts the block on the _pending_ list **with its fields still
  in it** -- they are not erased yet.
- **Acquiring** a block for a `let` or a `new` first erases some fields of a
  pending block -- at most a fixed number, so a big array is worked through
  across many acquisitions -- and a pending block whose fields are all erased
  is clean. Then it takes a clean block of its size class if there is one,
  working through more of the pending list while there is not, and fresh
  memory last.
- **Loading** fields in a `switch` or an `invoke` (the paper's `release`):
  when the block's count is 0, the fields move out -- nobody else can see them
  -- and the block is clean at once, with no counts touched; otherwise the
  count is decremented and each field loaded is shared.

The paper erases a block's old fields when the block is reused, which with one
size of block means every freed block's fields are erased in the end. With
size classes a block could wait for a class nothing allocates from, holding
what it points to -- so the pending list is one list for every class, and
every acquisition works on it: the lag between a structure becoming garbage
and its memory being reusable stays bounded, and no single operation does
more than a constant amount.

## Linearity

After linearization every name is used exactly once. In the paper's terms (its section on memory management, and `Coder.idr`'s
`codeWeakeningContraction`): a `substitute` that names a variable _n_ times
shares it _n_ − 1 times, and one that leaves it out erases it -- contraction
and weakening. What each statement then does:

| statement           | consumes                                                                                     |
| ------------------- | -------------------------------------------------------------------------------------------- |
| `substitute [x, y]` | the whole environment: names it repeats are shared, names it leaves out are erased           |
| `let z = K(x, y)`   | its fields, which are stored in an acquired block                                            |
| `switch x`          | the scrutinee: an arm loads its fields (release, above)                                      |
| `new f {…} [x]`     | its captures, stored in an acquired block                                                    |
| `invoke f#m`        | `f`: the method loads its captures (release)                                                 |
| `jump L`            | the environment, handed to `L`                                                               |
| `extern p(x)`       | nothing: a primitive borrows its arguments; ones not needed after are erased once it returns |

Meadow's AxCut reads what it names rather than consuming it (see
`meadow_seq`'s module docs), and its `substitute` does not list every name, so
the linearization pass does the paper's accounting at every statement: it
shares a name where it is used again after something consumes it, and erases
it where it is not used again at all. After the pass the program is linear,
and each statement consumes as in the table.

### Reuse: functional but in place

Counting at the last use says, the moment a `switch` takes a block apart,
whether it was the last reference -- `release` already asks, to hand the
fields over without a count moving. With that known, a block about to go back
to the runtime can instead be built in, as Koka's Perceus does ("FBIP":
functional, but in place):

```meadow
fun inc (xs : L) : L = match xs with
  | Nil -> Nil
  | Cons x rest -> Cons (x + 1) (inc rest)   -- built in the cell just taken apart
```

The linearization pass pairs them. An arm that takes apart a block of _n_
fields, from a `switch` that consumes its scrutinee, binds a **reuse token**;
the first `let` of _n_ fields on each path out of the arm -- the same size of
block -- builds in it. `release` makes the token the scrutinee's block when it
was the last reference, its fields now the arm's, and nothing when it was
shared (the count goes down and the fields are shared, as always). A `let`
given a token writes every word of the block it holds, and acquires one only
when it holds none. A path out of the arm that builds nothing that size gives
the token back (`clean`) where it parts from the paths that do, so the memory
is held no longer than it could be used.

So `inc` over a list it holds the only reference to rebuilds it in place: two
passes over a thousand cells acquire the thousand and nothing more, where they
acquired three thousand before. A list still held elsewhere is copied, as it
must be.

The rest of what Perceus does is here too, and each piece was measured on a
red-black tree taking 4.2 million insertions (`benchmarks/`, `rbtree`):

- **Across a call.** A block built after a non-tail call returns -- `Node c
(ins l k) x r` -- is built in the frame's code. The token rides in the frame
  as one more capture, an `i64` nobody counts; a frame erased without being
  entered gives it back. Frames are entered once, so this is safe where a
  closure's code, which may run any number of times, is not.
- **Per arm.** An arm that still wants the scrutinee -- it returns it, or a
  decision tree tested it and an arm uses it whole -- _keeps_ it, borrowing
  the fields it uses, while every other arm consumes it and can build in its
  block. Deciding for the whole `switch` at once shared the scrutinee first if
  any arm wanted it, and then no arm could reuse it.
- **Drop-reuse.** Where a kept scrutinee dies on a path that builds a block its
  size, it becomes a token there: its references to its fields are given up
  and its block is built in, if it was the last reference.
- **Reuse specialization.** A `let` building in a token it knows the origin of
  writes only what changed: not the first word, not the constructor or the
  descriptors when they are the same, and not a field whose new value is what
  was loaded out of that slot.
- **Matches that can consume.** `meadow_seq` compiles a `match` to a decision
  tree (`Lower::case_matrix`), which never backtracks, so nothing keeps the
  scrutinee alive to backtrack with; the chain of failure objects it replaced
  captured every scrutinee and made reuse impossible under a nested pattern. A
  `match` on a tuple written out -- `match (c, l, r) with` -- binds the
  elements and never builds the tuple, whose fields would otherwise be shared
  with it.

Two things outside this pass matter as much. The inliner copies a definition
mentioned exactly once to its call, however big (up to `ONCE_BUDGET`), so
`balanceLeft (ins l k) x r` builds in the node `ins` took apart rather than
giving it back for `balanceLeft` to ask for another. And tail modulo cons's
hole is filled inline (`SetField`) rather than by a runtime call.

Taken together, the idiomatic insertion -- nested patterns, a `balance` per
side -- went from 1.27 billion blocks acquired and 22 seconds to 8.4 million
and 1.6 -- two blocks per insertion, where the leaf is one. `MEADOW_NO_REUSE` builds the
same compiler without reuse, to measure what it is worth.

### Borrowing

A parameter a function only reads -- switches on, hands to primitives, passes
on to another reader -- is _borrowed_, after Lean 4 (Ullrich and de Moura,
_Counting Immutable Beans_, IFL 2019): the caller keeps its reference, and
the callee counts nothing, not even the fields it walks into. Which
parameters are borrowed is worked out over the whole program
(`linear::program`): every one starts borrowed, and becomes owned when its
function consumes it -- builds it into a block, captures it, passes it to an
owner -- or could build in its block. Only from borrowed to owned, so it ends.

Lean has a caller that hands a borrowed parameter a value it will not use
again give it up after the call. For `check (build n)` that walks the tree
twice -- once to read, once to free -- where a callee that owned it would
have freed each node as it read it: binarytrees ran 15% slower. Such a call
goes instead to a **copy** of the function that owns those parameters, made
once for each set of them some caller needs and linearized like any other;
its recursive calls on what it owns go to itself. A copy no one calls in the
end is not emitted.

A non-tail call's frame, in a function whose activation no handler can
capture (`Program::pure`), runs before the function's caller goes on, so it
lends what the function borrowed rather than counting it in.
`MEADOW_NO_BORROW` builds everything owned, to measure what this is worth.

## Cycles

Counting misses a cycle: two blocks that hold each other keep each other's
count above zero once the last reference from outside has gone. So a program
that can tie a knot is built with a collector for what counting cannot free,
and the build chooses which:

- **concurrent tracing**, the default (`silo/src/trace.rs`): a mark-and-sweep
  of the thread's heap, marking on a thread of its own while the program
  runs, which frees every garbage cycle and stops the program only to take
  its roots;
- **trial deletion**, with `cycles = "trial"` in a `[profile.<name>]` of
  `Meadow.toml`, or `MEADOW_CYCLES=trial` for any build (`silo/src/cycles.rs`):
  no thread, no barrier, and nothing that changes as the program runs, for
  where a background thread is not wanted; it misses one kind of cycle, and
  a large cycle is one long pause.

Counting does everything else under either: it frees what dies the moment it
dies, and reuse is untouched. The collector only frees what counting left.

**Emitted only where it can matter.** A cycle needs a store of a reference
into a block that already exists, which is `setRef` or a write into a mutable
array; everything else Meadow builds is built bottom-up and points only
backwards, and `setField` -- which destination-passing uses to fill a hole --
writes something newer than what it writes into. `emit::ties_knots` looks for
those two primitives _and at what they store_, since `Std` builds every string
it prints in a mutable array of bytes and a byte cannot point at anything. A
program without one defines `@meadow_cycles` as zero: no barrier, no
candidate test, and no collector of either kind. One with one defines it as 1
for trial deletion or 2 for tracing, and is emitted for that one alone.

### Concurrent tracing

Trial deletion asks a local question -- would this subgraph still be held if
its own references were taken away? -- and that is why it needs no roots and
why it has its two limits: it only asks of blocks it has seen let go of, and
it must answer in one pause. A tracer asks the global one -- what can the
program still reach? -- which every cycle, of any size and however it came to
be garbage, fails. What it needs is the roots, and a way to keep tracing while
the program changes the heap under it.

**When.** A heap starts a trace once it has grown, since the last one ended,
by as much as that one found live -- from 4,096 blocks. A trace walks what is
live, so this keeps its work in proportion to the program's. It is measured
from what the trace marked, not from what is in use when it ends: that
counts as live the garbage made while it ran, which only the next trace can
free, and every trace would wait longer than the last. And a heap with no
block a cycle could run through in use -- no `Ref`, no mutable array that may
hold references -- has no cycle to find, however large it is, and skips the
trace: a program that can tie a knot by what it is compiled from, and never
does, never marks its heap. A heap is a green thread's (`crate::ctx`), and so
is a trace: each thread's heap is traced on its own, with its own roots,
which is what one heap per thread -- nothing shared, everything that crosses
copied -- makes possible.

**The roots, and the one pause.** A trace starts inside `meadow_acquire`
called from compiled code, before the new block is taken: the one point at
which every block the program has made is written whole. (The runtime's own
allocations never start one: a parcel being unpacked, or an array being
filled, is half made in between.) There the program stops while its roots are
taken:

- the native stacks it runs on, scanned conservatively: the OS thread's stack
  below where the program began, for one that never spawns, and every
  segment the thread has -- running, parked, or a continuation's -- from
  where it stopped to its base, with the callee-saved registers written out
  first. A word is taken for a reference if it points into a block of this
  heap: a block's start is known from a bit set when it is first carved out
  of a chunk, and a block never moves, splits or merges -- a size class is a
  word count -- so a pointer into the middle of one finds it;
- what the context holds of the heap's: top-level values, literals, a
  transaction's writes, the spill area, what the leak check keeps;
- the blocks waiting to be erased, and what each still holds: they are dead,
  and counting frees them, but the tracer must not free under it what they
  will give back.

No stack map is needed. Counting never moves a block, so a word that only
looks like a reference costs nothing but keeping what it seems to point at
until the next trace.

**Marking, while the program runs.** One thread marks for the whole process,
a heap at a time. Marks are bits beside each chunk, not in the block, since the
program writes a block's header as it builds one. A block's fields are read by
their descriptors, and every word the marker follows is first checked to be a
block of the heap: it reads blocks the program may be rewriting -- a dead one
being reused, a token whose fields were taken -- and a word read half-changed
can then only mark too much, never lead it astray.

**What the program does meanwhile.**

- _New blocks are black._ A block made after the roots were taken is marked
  as it is acquired, in a chunk the trace knows; one in a chunk made since is
  new by where it is.
- _A barrier before every overwrite_ (snapshot at the beginning: Yuasa's). A
  store into a block that already exists logs the reference it replaces,
  while a trace is marking: `setRef`, `setField`, the reuse of a dead block
  -- which logs the fields it had and is blackened, being a new block now --
  and the runtime's writes in place (`meadow_st_set`, an array pushed or
  concatenated into its room, a record updated in place, a handler's target).
  So whatever was reachable when the roots were taken is marked, however the
  program rearranges it after. Outside a trace the barrier is a load and a
  branch, in a program that can tie a knot and nowhere else.
- _Nothing freed is handed out again while marking._ What counting frees then
  waits, unused, until the marking ends, so the marker never reads a block
  that has become another.

**The end.** Once the marker has done all it was given, the program's thread
marks what it has logged since and ends the marking: what it logged is a
batch at most, and waiting instead for a moment when it had logged nothing
would wait forever on a program that keeps rewriting its `Ref`s. And if the
program has allocated as much as was live before the marker is through --
the marker is behind, or busy with another thread's heap -- the program's
thread stops it and finishes the marking itself, so that a trace ends however
busy the marker is, and what it holds back meanwhile is bounded. Then a
sweep, on the program's thread, a slice per allocation. An unmarked block in the heap as it was is garbage -- nothing
reached it when the roots were taken, and nothing can reach garbage after --
and every such block is either in a cycle or held only by one. The sweep
first finds them all, then for each gives up its references to blocks that are
live (counting frees those that were held only by garbage), lets go of what
it held outside the heap (a continuation's segments, a compact's region, a
thread or a `TVar`), and frees it. The memory comes back once the sweep is
through, so that no address one garbage block names is handed out while
another may still name it.

**Tokens.** A reuse token -- a dead block kept to build the next one in -- is
carried in a frame built as a closure as an `Int`, since it is not counted.
So a closure's `Int` fields are read as possible references too: a token a
live frame holds is kept, and one only garbage holds is freed with it, as the
memory it is, without giving up the fields it no longer has.

**What it costs.** For a program that cannot tie a knot, nothing. For one that
can: a bit set as each block is carved from a chunk; the barrier's load and
branch at each store into a block and each reuse; during a trace, a mark at
each allocation and the memory counting frees held until it ends; and the
marking itself, on another core. Against trial deletion it also saves the
test at every decrement of a mutable block.

Measured against trial deletion on the Windows desktop, each built with
`--runtime silo`, best of five whole runs:

The benchmarks of `benchmarks/` run the same under either, `binarytrees`
13% faster under tracing for the test it leaves out: they tie no knot at run
time, and no trace starts. Where knots are tied:

| program | trial deletion | tracing |
|---|---|---|
| two million knots, each let go of at once | 187 ms | 107 ms |
| a live ring of 200,000 `Ref`s, a million knots made and let go of around it | 249 ms | 67 ms; the longest pause 0.007 ms against 4.1 |
| the same, twenty million knots | 2.0 s | 1.3 s |
| knots made a million frames deep in a recursion | 45 ms | 44 ms; the longest pause 4.6 ms, scanning the stack |
| MeadowBoot building itself | 74.8 s | 75.8 s; the longest pause 3.2 ms |

A pause is what its roots take, and so it grows with the stacks: a frame a
few words, a million of them a few milliseconds. MeadowBoot holds its `Ref`s
to the end and makes no cyclic garbage, so its traces find nothing, on the
marker's core; the next waits for as much allocation as the last one marked.

What tracing pays for that is memory. It frees garbage a trace at a time,
where trial deletion frees a knot as soon as it looks at it: under a stream
of cyclic garbage the heap holds what was made since the last trace, and the
last program above peaks at 47 MB where trial deletion keeps to 26.

**What it does not catch.** A cycle that runs through the frames of a
suspended continuation -- a `Ref` holding a continuation whose frames hold the
`Ref` -- is kept: a continuation's stack is scanned as a root, not as
something garbage could hold. Trial deletion does not see through a
continuation's frames either.

### Trial deletion

What needs no roots at all is **trial deletion** -- Bacon and Rajan,
_Concurrent Cycle Collection in Reference Counted Systems_, ECOOP 2001 -- and
`silo/src/cycles.rs` implements it: buffer each block whose count goes down
without reaching zero, then mark its subgraph gray while subtracting the
references that come from inside it, scan to restore everything an outside
reference still holds, and free what is left white.

**What a program that does pay, pays.** The test before the call is inline and
is one mask and one compare against a constant: a block is worth keeping only
if it is a `Ref` or a mutable array -- marked as such in word 1 when it is
built -- and only the first time its count goes down, since it stays coloured
until a run looks at it. Both questions live in the same word, so both are
asked at once.

**Pauses.** A run takes 64 candidates and walks at most 6,000 blocks; past
that the mark pass stops and puts back exactly what it took, which is possible
because it recorded every block it coloured and undoing is that walk
backwards. Once started the collector keeps going, one bounded run per
allocation, until the buffer is empty -- the program runs between one run and
the next. That holds a pause under a millisecond: 0.35 ms while freeing 10.4M
blocks of small cyclic garbage, 0.18 ms freeing 1.0M.

A cycle _larger_ than the budget is the exception, and it is inherent: what a
run proves garbage is freed in that run or the proof is lost, so one cycle of
100,000 blocks is one pause of about 11 ms. Such a candidate is set aside and
walked with no budget only once the heap has doubled, so the pause is paid for
by a doubling's worth of allocation and the memory is bounded rather than held
to exit -- 111 MB on a program that makes 640 MB of giant cyclic garbage.
Bounding it properly would mean marking incrementally, which means a write
barrier, which this runtime does not have.

**What is still not caught.** Only a `Ref` and a mutable array are buffered.
A cycle whose `Ref` is let go of while the cycle is still live, and which
becomes garbage later when something that is not a `Ref` is let go of, is
never noticed and leaks as every cycle did before. Buffering every decremented
block -- the published algorithm -- closes that hole and costs 11% of
`wordfreq` and 27% of `binarytrees` on programs that make no cycles at all.

## Calling convention

Every block that is entered from elsewhere -- a definition, a method -- is an
LLVM function in the `ghccc` convention, taking its parameters (the
environment) as `i64`s and returning `i64`. Everything inside it -- a
`switch`'s arms, an `extern`'s continuations, a `substitute` -- is basic blocks
and SSA renaming: `substitute` emits no moves at all.

A `jump` and an `invoke` are `tail call`s. `ghccc` has no callee-saved
registers and passes everything in registers, so LLVM makes every such call a
jump -- on Windows too, where `tailcc` with `musttail` is not supported. It
passes ten arguments in registers; a block with more takes the rest from
`meadow_spill`, a thread-local area the caller fills just before the jump.

On aarch64 the convention is `tailcc` instead. There LLVM gives a `ghccc`
function no prologue -- GHC keeps its own stack -- so the link register is
never saved, and it makes no `ghccc` tail calls: a `tail call` became `bl`
then `ret`, and the `ret` returned to itself for ever. `tailcc` functions
have ordinary prologues, and LLVM guarantees their marked tail calls on
aarch64. The emitter is told which (`meadow_llvm::CallConv`); the IR is
otherwise the same, but for how many arguments a call has.

`tailcc` passes eight arguments in registers on aarch64, and a block with more
takes the rest from the spill area there too, so nothing is ever passed on the
stack. `tailcc` would pass a ninth and a tenth there, and LLVM's aarch64 back
end gets that wrong (clang 22). The callee owns what it is passed on the stack
and pops it, sixteen bytes at a time; a caller passing one word is laid out as
if the other eight bytes were still its own and keeps a local in them, which
the callee's tail call to a block of two such words overwrites -- a value
live across a call of nine words came back as an address. At `-O0` the caller
also fails to take the stack pointer back after the callee has popped, and
reads everything afterwards sixteen bytes out.

Reference counting is calls to small helpers the module defines (`mw.share`,
`mw.erase`, and forms that look at a descriptor first), which LLVM inlines where
it pays: emitting the checks at every site made the standard library's module
several times the size.

A method's function takes the object itself first, then the arguments, and
loads its captures from the object -- releasing it, as above. The method knows
how many captures it has and so how its block is laid out, where the call site
knows neither; the paper's `invoke` is likewise a jump through the method
table, with the loading done by the method's code.

### Frames are the native stack

`meadow_seq` marks the continuation a function makes for its own non-tail call
as a **frame**. Where a frame is used for nothing but the continuation of the
one call it was made for, it is never built: the call is an LLVM `call`, the
frame's captures are values live across it, and the frame's method is the code
after it, with the call's result as its argument. The callee receives a null
continuation -- _return natively_ -- and invoking that is `ret`. A function
reached by a tail call passes its own continuation on, so a `ret` anywhere
returns to the nearest native call.

A frame used any other way -- captured, stored -- is built as an ordinary
object, and invoked through its method table as any other continuation is.

A frame's code is one function (`mw.F<n>`) that each call site tail-calls
after its native `call` returns, so a frame reached from many places is
emitted once.

The native stack runs deep: `main` runs on a segment whose reservation is large
(committed as it is touched), since non-tail recursion over a long list is
ordinary Meadow.

## Effects: stack segments

Effects arrive as evidence passing (see `meadow_seq::lower`): a tail-resumptive
clause is an ordinary call, and a general one uses four primitives -- `Enter`
where `handle` begins, `Detach` where an operation captures the continuation up
to its handler, `Reattach` where the resumption puts it back, and
`Once`/`TakeOnce` for one-shotness.

On the native stack those are switches between **stack segments**:

- `Enter` runs the handled body on a segment of its own. When the body returns,
  the segment is finished and control is back on the handler's.
- `Detach` suspends the body's segment where it is, and runs the rest of the
  operation -- building the resumption, calling the clause -- on the handler's
  segment. The code after it is compiled as a function of its own, which the
  runtime calls there.
- `Reattach` switches back to the suspended segment, and runs the code after it
  there -- which is where invoking the operation's continuation is a `ret` into
  the frames the segment kept.

Switching is `corosensei`'s: stackful coroutines on Windows x64, Linux x64 and
aarch64, with guard pages, and on Windows the thread information block kept in
step so that stack probes work.

The emitted code packs the code after each of the three as a closure of one
argument (what it captures: the names free in it), calls `meadow_enter`,
`meadow_detach` or `meadow_reattach` with it, and returns natively with what
the runtime answers. The runtime calls such a closure back through
`meadow_invoke1`, which the module defines, since methods are in `ghccc`.

**A segment ends where its `handle` does.** In AxCut the code after a `handle`
is part of the continuation the code inside it is given, so a segment told to
run "what follows" would run the rest of the program, and handlers in a loop
would pile up a stack each. What the lowering does say is where the value of
the whole `handle` goes: it puts that continuation in the target `Ref` before
`Enter`. So `meadow_enter` takes it out and leaves `0` -- return natively --
in its place. When the value is produced, whichever stack produces it returns,
the segment is over, and the runtime carries on with the continuation it took,
on the stack the `handle` began on. Before this, a program that handled in a
loop slowed down as it went, with an unfinished segment for every turn.

**An abandoned continuation gives up what its frames hold.** A continuation
never resumed is discarded when its stack object is erased, without unwinding:
the native frames on its segment have no maps saying which slots hold
references, and nothing on them runs again. What they hold across a call is
what they need when it returns -- a frame not built, waiting on the call -- so
around such a call the emitted code links a record of those values, each with
its descriptor, into a chain kept for the segment it runs on, and unlinks it
when the call returns (`silo/src/shadow.rs`). A segment that suspends keeps its
chain's head; a discard walks it and gives up every value on it before the
stack goes back. The runtime links records of its own where one of its frames
holds something across Meadow code: `meadow_enter` holds the continuation a
`handle`'s value goes to. Only a program that performs an operation of a
general handler -- the only way to capture a continuation -- makes records at
all, so one that does not pays nothing; one that does pays a few stores and a
load around each non-tail call.

The segments themselves are given back, including those nested inside the
continuation. An operation that passes an inner handler on its way out
suspends the inner segment from a `drive` frame on the outer segment's stack,
and a frame that is never unwound would never free what it held, so while the
suspension lasts the inner segment is parked in the thread's context instead.
The continuation carries the segments parked during its suspension; a discard
frees them with it, and a resumption hands them back to their frames, from the
outside in. `MEADOW_SILO_LEAKS` reports how many segments are still live at exit.

## Threads

A green thread is a segment too (`silo/src/sched.rs`), and `main` is one. The
scheduler runs the ready threads in turn, each until it waits: on a thread it
awaits, on an empty channel, in a transaction's `retry`, or by yielding. A
thread that waits inside a `handle` suspends every segment it has, out to the
scheduler, as an operation performed further out does, and resumes as one.
A failure in a thread other than `main` fails only that thread; `await` fails
with its message.

Threads run **M:N over OS threads**: a worker per core takes ready threads off
one queue, and a thread runs on whichever worker picks it up. The workers
besides the one `main` starts on begin with the first `spawn`, so a program
that never spawns never has a second OS thread.

**A call into the world that waits** -- a socket's accept or read -- would
hold up the worker that made it, and enough of them (a server's connections,
each reading) would leave none to run the thread that answers. So a thread on
the scheduler hands such a call to an I/O thread and parks
(`sched::blocking`); the I/O thread makes it and puts the thread back to run.
One is started whenever a call finds none of them free, and a thread waiting
on a call is not deadlocked, whatever else waits. `meadow-glade` does the
same, and the CEK machine makes each such call on an OS thread of its own.

**A sleep** waits on a timer rather than on a worker (`sched::sleep`): the
thread parks with the time it wakes at, and one timer thread, started by the
first sleep, puts each sleeper back to run when its time comes. A sleeper is
counted with the calls, as a thread that will wake, and a cancel takes it off
the timer at once (`docs/CANCELLATION.md`).

**A heap per thread** (`silo/src/ctx.rs`), so counting needs no atomics and no
two workers ever touch the same block. What crosses between threads is copied
(`silo/src/parcel.rs`): the function a thread is spawned with and everything it
captures, a message, a thread's answer, a `TVar`'s value. Sharing inside a
value is kept, so what was a DAG arrives as one. A `Ref`, a mutable array or a
continuation is refused, with the message `meadow-glade` gives.

**Waiting.** A thread that waits says why in its context and suspends, and
the worker files it where what it waits for will find it, or puts it straight
back if that is already there. So a wake-up cannot fall between a thread
deciding to wait and its waiting.

**A lock per thing, not one for everything.** A channel has its own, as a
`TVar` does, and its handle holds the address of it, so a send and the receive
it answers touch that channel and nothing else; the scheduler's lock is taken
only to put a woken thread back on the queue. The order is always the
channel's or the `TVar`'s first and the scheduler's after, never the other way
round, which is the whole of what keeps them from holding each other up --
so a worker files a parked thread before it takes the scheduler's lock, not
under it.

**Transactions** keep a version per `TVar`, which a commit bumps. A read of
one written since the transaction began is a conflict, and the transaction
runs again; a commit checks that every version it read is as it was and
publishes its writes, having locked the cells it touches -- in address order,
so two commits cannot hold each other up. Transactions over different `TVar`s
never meet at all.

A `TVar`'s handle holds the address of the cell, so reading or writing one is
no lookup; a handle copied to another thread carries the same address. What it
holds, when that is a word and nothing more -- an `Int`, a `Bool` -- is kept
as it is rather than as a parcel, so a transaction over numbers allocates
nothing to commit.

**Stacks are pooled.** A segment's or a thread's stack is reserved memory with
guard pages, which is most of what making one costs, so a finished one is kept
to be used again: a few per thread for segments, a couple per core for
threads. A program whose every transaction is a `handle` spent eight times
longer making stacks than working before this.

What a thread's context holds, it reaches through one accessor that is never
inlined (`ctx::get`). A thread that waits can carry on on another OS thread,
and an address that a compiler kept from before the wait would be the old OS
thread's. For the same reason the emitted code asks the runtime for the spill
area rather than reading a thread-local of its own.

`meadow run -j N` (`--threads`) sets how many workers there are, for either
runtime; unset, there is one per core. `MEADOW_THREADS` says the same, and
`meadow run --leaks` asks Silo for what the program left behind
at exit, as `--gc-stats` asks the VM about its collector.

**Preemption.** A thread that neither waits nor finishes gives way at a
**safe point**: a byte the emitted code reads at the entry of every definition
and every method, which is to say on every loop, since a loop in AxCut is a
jump to one of them. A timer sets the byte every few milliseconds, and only
while a thread is waiting for a core, so the usual answer is a load and a
branch not taken; the thread that acts on it clears it, and goes to the back
of the queue. The load is `volatile`: LLVM turns a self-tail-call into a loop,
and an ordinary load of a byte nothing in the loop writes would be hoisted out
of it, which is exactly the loop that has to be interrupted.

The thread that starts the program waits for `main`'s answer rather than
running threads itself. Otherwise a worker still inside a thread that never
waits would hold the answer up, and the program would end no sooner than that
thread did.

## A program that cannot spawn pays for none of this

Whether a program can ever have a second thread is plain in the program: it
spawns one somewhere, or it never does, and `meadow_llvm::emit` looks before
it writes anything. A program that never spawns gets:

- **no safe points** -- nothing can be waiting for a core, so the check and
  its branch are not emitted at all;
- **a spill area of its own**, an ordinary global, rather than something the
  runtime looks up per thread;
- **one context in a static**, with no thread-local and no lookup: reading it
  is a load, and it can be inlined, since nothing can move between OS threads;
- **no scheduler**: `main` runs on the thread that called it, and no worker,
  timer or coroutine is made for it.

The emitted module says which it is in `@meadow_threaded`, and the runtime
reads it.

Counting itself is never atomic, in either kind of program: a block belongs to
one thread's heap, and what crosses between threads is copied, so there is no
block two threads can count at once. The only atomics are the scheduler's own
bookkeeping -- what `await` has to hand back, what a channel holds, what a
`TVar` holds -- which is shared by nature.

## Units: a module at a time, and kept

A program is compiled as many LLVM modules, **a unit for each module of the
program** -- `Std.Collections.Vector`, `MeadowBoot.Infer` -- and several for a
module with more than two megabytes of functions (`meadow_llvm::UNIT`). One
LLVM module is what LLVM handles worst, and units compile on every core. Every
definition says which module it was written in (`core::Def::module`), and what
a pass makes of one -- a specialized copy, a lifted lambda, a join point -- is
of the module that one is. The files are named for what they hold:
`MeadowBoot.Std.Collections.Vector.ll`, `MeadowBoot.MeadowBoot.Infer.3.ll`.
The first unit, `MeadowBoot.ll`, holds the tables and the entry points.

**An object is kept, and compiled again only when its unit's text has
changed** (`buildtools/meadow/src/aot.rs`): beside the objects, `<name>.units`
says what text each was compiled from. So a unit's text has to be the same
whenever its module is, which is why nothing in it is numbered across the
program:

- a function is named for its definition, `@mw.L.<module>.<name>.<k>`, the
  `k`th block of that name in its module, and its methods and frames after
  it (`@mw.M…`, `@mw.F…`);
- a string is named for what it says, `@mw.s.<hash>`, and a unit declares the
  ones it names and no others;
- a closure's method-table index is read from a constant the first unit
  defines, `@mw.mi…`, since the index is the method's place among every
  method of the program;
- and the compiler orders interned strings by their text. By the order they
  were first seen, which with several threads is a different order each run,
  two builds of one program differed in where a record kept its fields and in
  which of two definitions came first.

With that, a build of MeadowBoot from unchanged sources reuses every object
but a handful, and one after an edit to a function in one module compiles
that module's units and the first: 25 s where a build from nothing is 78.

What is still numbered across the program is a **constructor's tag**, in the
order constructors are first used, so a change that uses a constructor
earlier than any use before it renumbers the ones after. And the text of a
module depends on what was inlined into it and which copies of its generic
functions the program asked for, so a change elsewhere can change it for
that reason too.

`MEADOW_SIZES=N` says, as a program is compiled, the `N` modules and the `N`
definitions that came to the most LLVM IR.

## Testing

- **Differential:** every program the backend compiles is run by the VM as
  well, and must answer alike -- the same harness `meadow-glade` has, on the
  standard library's tests and the examples.
- **Leaks:** a debug runtime counts live objects, and a program without
  `Ref`s, mutable arrays or `TVar`s must end with none. It also says how many
  blocks were acquired at all, which is how the tests see reuse.
- **A/B:** `meadow build --release --runtime glade` against `--runtime silo`, on
  the benchmarks.

## Stages

1. **The core, single-threaded:** linearization; data, closures, frames on the
   native stack; `Int`, `Float` and immediates; the arithmetic and comparison
   primitives; printing an answer.
2. **The rest of the values:** strings, arrays, records, `BigInt`, `Ref`s, and
   the primitives over them.
3. **Effects:** stack segments, `Enter`/`Detach`/`Reattach`, and the effects
   that reach the world (`Console`, `Fs`, `Process`, `Time`, `Random`,
   `Test.fail`).
4. **Threads:** spawning, channels, STM, the scheduler.
5. **Parity:** the standard library's tests and every example agree with the
   VM; then the A/B against `--runtime glade`.

All five are done: the standard library's 281 tests pass under
`meadow test --std --release --runtime silo`, and the examples print what they
print under `--runtime glade`, but for timings and the size of a compact region.

A program is emitted as several LLVM modules of about 2 MB each (see
`emit::Module::units`), compiled in parallel and linked: as one module, the
standard library's tests took clang tens of gigabytes.
