# Meadow against other languages

Eight tasks, twelve languages, one program each — and Meadow in both of its
runtimes. Five of the tasks are single-threaded and three use every core the
machine has.

This is not `benches/`. That directory times Meadow against _itself_ — the
bytecode VM against the CEK machine — and is the right tool for asking whether
a change to the compiler helped. This directory asks a different and less
comfortable question: given a task, how long does Meadow take compared to the
languages someone might otherwise have written it in.

```
./run.py                      every task, every language whose toolchain is installed
./run.py --task matmul        one task
./run.py --lang meadow --lang rust --lang c
./run.py --reps 10            more repetitions; the minimum is reported
./run.py --list               what is here, and what is missing
./run.py --json out.json      the raw numbers as well as the table
./run.py --arena              the same work with the allocator taken out
```

Meadow needs a release build of its own compiler first:

```
cd ../buildtools && cargo build --release -p meadow
```

## The rules

**The same algorithm everywhere.** Not the same code — that would be a
translation exercise and would flatter whichever language the original was
written in. The loop structure, the data layout and the amount of work are
fixed; how each language expresses them is left to that language.

**Idiomatic, lightly optimized.** What a competent person would write knowing
it should be quick, and would not be embarrassed to show a colleague. Not the
benchmark-game entry with hand-unrolled SIMD and a custom allocator. So: `ikj`
loop order in the matrix multiply, because everyone knows to do that;
`STUArray` rather than a list in Haskell, because nobody would write the list;
but no intrinsics, no arena allocators, no unsafe tricks.

**Every program prints one line, and they must all agree.** The harness
compares the checksums before it reports any timing, and refuses to time a task
whose languages disagree. A benchmark that is quietly computing something else
is not a benchmark, and this is the only defence against it.

**The minimum of several runs**, not the mean. The work is deterministic, so
the spread is the operating system, and the shortest run is the one it
interfered with least.

**Wall clock for the whole process**, startup included. That is unfair to the
runtimes that have one, so the table's last row is the same measurement for a
program that only prints — the floor each language cannot go below. Java's 65ms
and Node's 37ms are in every number in their columns.

## The tasks

|     | task          | what it is for                                                               |
| --- | ------------- | ---------------------------------------------------------------------------- |
|     | `fib`         | recursive calls and nothing else: `fib 32`, 64-bit integers, no allocation   |
|     | `binarytrees` | allocation and collection: 67 million short-lived nodes, one long-lived tree |
|     | `matmul`      | 256×256 double matrix multiply, `ikj` order: flat arrays and tight loops     |
|     | `wordfreq`    | a 3MB file, 400k words, a hash map and a sort                                |
|     | `rbtree`      | 4.2 million keys into a functional red-black tree: reuse, nested patterns    |
| ⇉   | `mandelbrot`  | data parallelism: a 2000×2000 grid of independent float work                 |
| ⇉   | `contention`  | eight threads moving money between sixteen accounts, atomically              |
| ⇉   | `pipeline`    | message passing: four producers, one channel, 200k messages                  |

A few of these deserve a note about why they are shaped as they are.

**`binarytrees` gives every tip the iteration number it was built in.** Without
that the tree is the same on every pass of the loop, GHC floats it out and
builds it once, and the benchmark finishes in a fiftieth of the time having
measured nothing. This was not a hypothetical: the first version of this task
had Haskell at 23ms against C's 1.4s, with the right answer. Making each tree
depend on its iteration is the fix, and it is applied in all eight languages so
that none of them is being handicapped.

**`matmul` uses only whole numbers small enough to be exact in a double.** Every
product and every partial sum is an integer below 2^53, so fusing a multiply
and an add gives bit-identical results to doing them separately. Without that
the task would be measuring which compilers emit FMA on this hardware rather
than which ones are fast, and the checksums would not agree. C is built with
`-ffp-contract=off` as a second belt.

**`contention` is transfers, not a counter.** A shared counter is an atomic
add, which several of these languages would do without any mutual exclusion at
all, and then the comparison with Meadow's transactions would be meaningless.
Moving money needs two reads and two writes to happen as one step, which is the
thing transactions are for and the thing atomics cannot do. Because a transfer
is unconditional, the final balances do not depend on the order the threads got
their turns — so the sixteen printed balances are a real check that each of the 150,080
transfers landed exactly once and none of them twice, and not merely a timing.

**`mandelbrot` splits rows between threads by taking every Nth one**, so no
thread gets all the cheap ones, and the total does not depend on how many
threads there were. Every language may therefore use as many as it likes and
still has to produce the same number.

**`rbtree` is Koka's benchmark, not ours.** Four point two million keys, in
descending order, each inserted into a persistent red-black tree that nothing
else holds, then a fold counting the tenth that are flagged. It is the
benchmark the Perceus paper made its case with: a tree only ever held once can
be rebuilt in the nodes it is taken apart from, and a runtime that counts
references can see that and a tracing collector cannot. The Koka, Haskell,
OCaml and Java programs are Koka's own (`test/bench/`, at `facb793`), unchanged
but for Java's class name. The C++ is Koka's too, and is not the same
algorithm: it is `std::map`, a red-black tree mutated in place, which is what
Koka's papers measure against -- the thing a functional tree is trying to get
close to. The Meadow program is Koka's written as Meadow would write it, with
nested patterns; Rust, Go, Python and JavaScript are ports of the same.

## The languages

| language   | how it is built                                                                                                                                                          |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Meadow     | `meadow build --release` — `-O2`, compiled ahead of time to a native executable                                                                                          |
| Meadow/aot | `meadow build --release --runtime silo` — the same program against the runtime of its own: counted by reference, cycles found by concurrent tracing (trial deletion when these were measured), on the native stack |
| Rust       | `rustc -C opt-level=3`, what `cargo build --release` uses                                                                                                                |
| C          | `cc -O3 -ffp-contract=off` — `-O3` to match Rust's `opt-level=3`; `-ffp-contract=off` so `matmul` measures the loop and not who emits a fused multiply-add               |
| C++        | `clang++ -O3 -std=c++17` — only `rbtree` has a C++ program, Koka's                                                                                                       |
| Go         | `go build`                                                                                                                                                               |
| Haskell    | `ghc -O2 -threaded -with-rtsopts=-N`                                                                                                                                     |
| Java       | `javac`, default JVM settings                                                                                                                                            |
| OCaml      | `ocamlopt -O3`, native code                                                                                                                                              |
| MLton      | `mlton`, whole-program compilation                                                                                                                                       |
| Koka       | `koka -O2`, Perceus reference counting                                                                                                                                   |
| Python     | CPython, no flags                                                                                                                                                        |
| JavaScript | Node, no flags                                                                                                                                                           |

OCaml, MLton and Koka are here because they are the company Meadow is trying to
keep: compiled, garbage-collected or reference-counted functional languages
rather than C and Rust. Koka is the most pointed comparison of the three --
its Perceus reference counting is the nearest thing in production to the memory
discipline AxCut describes, and `binarytrees` is where that shows.

Three of the eight tasks are missing for all three, and for a reason worth
recording rather than hiding. **OCaml 4.14 has no multicore** (OCaml 5 does);
**MLton has no parallel runtime** (MPL is the fork that does); **Koka's
concurrency is `async`, not threads**. None of them can express the parallel
tasks the way the others do, and a `fork`-and-pipe imitation would measure the
imitation. `wordfreq` is also missing for MLton and Koka: SML's Basis Library
has no hash table, and Koka's `std/data/map` and `std/data/dict` are both stubs
marked "Todo".

Meadow is measured on its **ahead-of-time native backend**, which is what
`--release` selects. It also has a bytecode interpreter and a JIT; neither is
measured here, and `benches/` is where those get compared.

It has **two runtimes**, and both are in the table. `meadow` is the default:
the runtime the JIT and the debugger share, with a tracing collector and green
threads. `meadow-silo` is the same program compiled against the runtime
described in `docs/SILO.md` — memory counted by reference as the AxCut paper
does, the native stack, no bytecode and no interpreter in the executable —
which `--runtime silo` selects. They are the same compiler up to the point
where AxCut is lowered, so a difference between the columns is a difference
between the runtimes and what each backend makes of them.

## Results

On a 28-core Windows 11 machine, minimum of five runs. The figure in brackets
is how many times slower than the fastest in that row. `meadow` is the default
runtime and `meadow-silo` the one of its own; OCaml, MLton and Koka have no
toolchain on this machine, and C's parallel tasks want a `pthread.h` it has
not got, so those are absent rather than carried over from other hardware --
see [Caveats](#caveats-in-order-of-how-much-they-matter).

| task         | meadow        | meadow-silo   | rust         | c            | go           | haskell       | java          | python         | js            |
| ------------ | ------------- | ------------- | ------------ | ------------ | ------------ | ------------- | ------------- | -------------- | ------------- |
| fib          | 30ms (4.4×)   | 14ms (2.0×)   | **7ms**      | 7ms (1.0×)   | 12ms (1.8×)  | 16ms (2.4×)   | 73ms (10.7×)  | 216ms (31.9×)  | 52ms (7.7×)   |
| binarytrees  | 2.36s (8.1×)  | 1.28s (4.4×)  | 1.74s (6.0×) | 1.56s (5.4×) | 1.32s (4.6×) | **290ms**     | 524ms (1.8×)  | 6.34s (21.8×)  | 1.31s (4.5×)  |
| matmul       | 122ms (22.2×) | 16ms (3.0×)   | 6ms (1.1×)   | **6ms**      | 16ms (2.9×)  | 31ms (5.7×)   | 77ms (14.0×)  | 716ms (129.9×) | 57ms (10.4×)  |
| wordfreq     | 446ms (32.7×) | 323ms (23.7×) | 15ms (1.1×)  | **14ms**     | 25ms (1.8×)  | 119ms (8.7×)  | 154ms (11.3×) | 69ms (5.1×)    | 108ms (7.9×)  |
| ⇉ mandelbrot | 85ms (6.5×)   | 34ms (2.6×)   | **13ms**     | ✗            | 18ms (1.4×)  | 33ms (2.5×)   | 128ms (9.8×)  | 684ms (52.3×)  | 131ms (10.0×) |
| ⇉ contention | 89ms (7.9×)   | 59ms (5.2×)   | 13ms (1.1×)  | ✗            | **11ms**     | 27ms (2.4×)   | 89ms (7.9×)   | 75ms (6.7×)    | 126ms (11.2×) |
| ⇉ pipeline   | 62ms (4.3×)   | 42ms (2.8×)   | **15ms**     | ✗            | 16ms (1.1×)  | 730ms (49.8×) | 98ms (6.7×)   | 218ms (14.9×)  | 143ms (9.8×)  |
| _startup_    | 5ms           | 4ms           | 3ms          | 3ms          | 6ms          | 10ms          | 65ms          | 27ms           | 37ms          |

The default runtime's `fib`, `binarytrees`, `wordfreq` and `rbtree` have moved
since this table was taken -- see [the tenth
round](#the-tenth-round-x86-64-and-the-nurserys-bet). In a run of the whole
suite afterwards, `binarytrees` was 769ms on `meadow` against GHC's 349ms.

**Absolute times move about 20% between runs of the whole suite on this
machine**, so do not read one run against another, and do not read this table
against the one that was here before it: that was a 10-core Apple silicon
laptop and this is a 28-core Windows desktop, which is why `binarytrees` is
slower here in every column and the parallel tasks are quicker. Ratios _within_
a row are taken in the same conditions and are the trustworthy part. Where a
change to Meadow is claimed below, it was measured by alternating two compilers
back to back on an otherwise idle machine rather than by comparing two runs of
this table.

Where Meadow was when this suite was written, and where it was after the rounds
of work [What was fixed](#what-was-fixed) describes -- both measured the same
way on the Apple silicon machine those rounds were done on, so read the two
columns against each other and not against the table above:

| task        | at the start | after those rounds | faster by |
| ----------- | ------------ | ------------------ | --------- |
| fib         | 50ms         | **26ms**           | 1.9×      |
| binarytrees | 3.77s        | **1.39s**          | 2.7×      |
| matmul      | 2.79s        | **14ms**           | 199×      |
| wordfreq    | 1.44s        | **384ms**          | 3.8×      |
| mandelbrot  | 433ms        | **83ms**           | 5.2×      |
| contention  | 683ms        | **483ms**          | 1.4×      |
| pipeline    | 56ms         | 53ms               | —         |

`contention` and `pipeline` move ±20% between runs of the same binary, so
nothing under that is claimed for them.

### `rbtree`

Added after the table above, and measured on its own, on the same machine:

| task   | meadow        | meadow-silo  | c++       | java         | haskell      | js           | rust         | go           | python         |
| ------ | ------------- | ------------ | --------- | ------------ | ------------ | ------------ | ------------ | ------------ | -------------- |
| rbtree | 8.97s (13.9×) | 1.54s (2.4×) | **646ms** | 836ms (1.3×) | 1.13s (1.7×) | 1.64s (2.5×) | 3.15s (4.9×) | 4.26s (6.6×) | 18.98s (29.4×) |

The first time it ran, `meadow-silo` took 22 seconds and acquired 1.27
billion blocks, and `meadow` took 15. Reuse was there, and was reaching almost
nothing: a nested pattern compiled to a chain of failure objects, each holding
the scrutinee, so no `switch` ever saw a last reference; a node built after a
call returned could not be built in the one taken apart before it; and every
cell tail modulo cons built filled its hole with a runtime call. With those
fixed (`docs/SILO.md`, "Reuse") the same program acquires 8.4 million blocks
-- two per insertion -- and runs in 1.54s, ahead of Node and GHC and behind
only the JVM and the tree C++ mutates in place. The decision trees are
`meadow`'s too, which took it from 15.2s to 9.0s.

## What this says

**The two runtimes are the first thing in the table.** `meadow-silo` is ahead
of the default on every row: 2.1× on `fib`, 7.6× on `matmul`, 2.5× on
`mandelbrot`, 1.8× on `binarytrees`, and 1.4–1.5× on `pipeline`, `wordfreq`
and `contention`. It is the same compiler as far as AxCut and the same programs;
what differs is everything below that — LLVM rather than the JIT's own code
generator, counting by reference rather than a tracing collector, and the
native stack. The rows where it wins most are the ones where a native compiler
has most to say: `matmul`'s three nested loops and `fib`'s calls.

The spread inside a Meadow column — 2.0× on `fib` against 23.7× on `wordfreq`
for `meadow-silo` — matters far more than where the column sits on average.

**The `meadow-silo` column now includes a cycle collector**, which counting
needs and which this runtime cannot get from a tracing backstop: there are no
stack maps, so the roots cannot be enumerated, and what works without roots is
trial deletion (`silo/README.md`). None of these programs makes a cycle, so what
the column shows is purely what it costs a program that does not: the inline
test before each candidate is one mask and one compare, and a program that
cannot tie a knot at all has neither. Measured the way this file requires --
the runtime built with and without it, alternated back to back on an idle
machine -- `binarytrees` is unchanged at 1.30s against 1.29s and `wordfreq` is
312ms against 327ms, which is 5%. The arena rows are where it should cost most,
since they write a mutable array once per node and a mutable array is one of
the two kinds that get buffered; that was not measured by alternation, so the
347ms above should not be read against the 317ms of an earlier run.

Of the compiled functional languages with a managed heap, which is what Meadow
is, only **Haskell** is installed on this machine; OCaml and Koka, the two
most pointed comparisons, are not. Against GHC, `meadow-silo` is **ahead on
`fib` (14ms against 16ms) and on `matmul` (16ms against 31ms), level on
`mandelbrot`**, 4.4× behind on `binarytrees`, and 2.7× behind on `wordfreq`.

`pipeline` at 2.8× the fastest is among the strongest results. It is behind Go,
the language whose reputation rests on this one thing, and behind Rust's
`mpsc`, but ahead of Java and Node, and seventeen times ahead of GHC's `Chan`.
Handing a value between green threads is what the scheduler was built for — a
thread woken by a message goes to the receiving worker rather than the back of
a shared queue, so a send and the receive answering it happen on one core back
to back. It was 3.4× until each channel got a lock of its own: 400,000 sends
and receives were taking the one lock the whole scheduler uses, so four
producers and a consumer queued up behind each other on it rather than on the
channel they were actually using.

`fib` at 2.0× says the calling convention and the native backend are sound. It
is the one task with no allocation, no arrays and no runtime services, so it is
the closest thing here to a measurement of the compiler on its own. It was
5.2× on the default runtime until [the sixth
round](#the-sixth-round-arguments-in-registers) passed arguments in registers
and let a method load its own captures, which is what the fourth round had said
was left: the continuation on a stack moved `fib` not at all, and the
convention around the call moved it 1.8×. On `meadow-silo`, where the calls are
LLVM's in a convention with no callee-saved registers, it is 14ms against
Rust's and C's 7ms and ahead of GHC.

`mandelbrot` at 2.6× on `meadow-silo` is a good number, and close to GHC. Its
inner loop is float arithmetic on eight registers and nothing else, and since
[the fifth round](#the-fifth-round-registers-and-the-inliner) that arithmetic
happens in registers rather than through a register file in memory. The
default runtime's 6.5× on this machine against 2.6× on the Apple silicon one
is the clearest sign of what the rest of this table says quietly: the JIT's own
code generator has had far more attention on aarch64 than on x86-64, and
`meadow-silo`, which hands the loop to LLVM, does not care which it is on.

`matmul` at 3.0× has been 366×, 146×, 56×, 36× and 21×, each with a specific
cause. `St.get`
and `St.set` are primitives, and native code did not implement primitives: it
handed them back to the interpreter through `meadow_exec`, and sampling put 62%
of the run inside that call. [The third round](#the-third-round-the-native-back-end)
compiled them inline, and the same sampling put 0.6% there — and the benchmark
barely moved, because `St.get a i` was still a _call_: a one-line wrapper around
the primitive, and a call from a machine with no call stack is a continuation
built, a jump and a return through the generic `invoke`. Three of those per
element. [The fifth round](#the-fifth-round-registers-and-the-inliner) inlines
the wrappers, and the inner loop is now straight-line: two shifts, two adds, two
reads, a multiply, an add, a write, then one closure call and one return per
element for the loop itself; the sixth round made that call and return cheap,
[the eighth](#the-eighth-round-loops) removed them -- `St.forRange` with a
lambda is now a loop in the caller, and the loop no longer writes the
collector's high-water mark on every trip round -- and [the
ninth](#the-ninth-round-two-at-a-time) does the loop two elements at a time
on the vector unit, with every check on the arrays made once before it.

That last round is aarch64's, which is why the default runtime is 122ms here
and was 14ms on the machine it was written on, while `meadow-silo` — the same
Meadow, lowered to LLVM IR and vectorized by the same compiler that does it
for the C — is 16ms, level with Go and 3.0× off C. It is the row where having
a second backend pays for itself most plainly.

`contention` at 5.2× is a comparison of software transactional memories: GHC's
runs the identical algorithm in 27ms, Go's mutex in 11ms. (This row moves ±20%
between runs of the same binary.) It was 97× when this suite was written, and
what fixed it was the same lesson in both runtimes: the cost was not the
conflicts but what every transaction did on the way through, much of it a
write to a word all the threads share — a lock over the whole table of
`TVar`s, an `Arc` cloned per primitive, a region made and discarded for every
`Int` written, one lock over all waiters at every commit. The note in
`glade/src/stm.rs` records those for the default runtime; `meadow-silo` took the
same medicine later (`silo/src/sched.rs`): a `TVar`'s handle carries the
address of its cell, a value that is one word is kept as a word rather than
copied, and a commit locks the cells it touches in address order instead of
taking one lock for all commits. Transactions over different accounts now
proceed at once, which is what makes this row scale with the cores rather than
against them.

The other half of it was not STM at all. `atomically` is a handler, and a
handler in this runtime ran on a stack segment of its own — but a segment that
did not end when the `handle` did, so a program that handled in a loop kept one
unfinished segment per turn. Ending the segment where its `handle` ends
(`docs/SILO.md`, "Effects") took a benchmark of 250,000 handlers from 116
seconds to 53ms, and it is the reason this row is a number rather than a
scandal.

`wordfreq` at 23.7× is the worst row in either Meadow column, and the one
place where the second backend barely helps: 323ms against the default
runtime's 446ms, where every other row is a factor or more. That says the cost
is not code generation but the work each word makes the runtime do. It was 68×
with a persistent HAMT doing the work a mutable hash table does everywhere
else; it now uses `Std.Collections.HashTable`, which is that table, and the
same shape of program as the Rust. What is left is seven runtime calls per
word — two `stringIndexOf` and a slice from `split`, a hash, an equality, and
the store of the new count: 3.5 million calls into the runtime for 400,000
words. Giving the six of them an entry each, taking their arguments in
registers rather than through the array every other primitive is marshalled
into, took 350ms to 313ms — which says the marshalling was never the main
cost. Each is a Rust function that will stay Rust; what would actually move
this row is a `split` primitive, which takes three of the seven calls away. Python's four lines of glue over `str.split` and
`collections.Counter` do it in 69ms, and that is the number to be embarrassed
by.

**The default runtime's `binarytrees` on this machine looked like the
nursery, and was not.** `MEADOW_GC_NURSERY` at 32MB took it from 2.41s to
1.44s, and the copying collector, which has no nursery to size, got 1.45s --
but because both promote fewer trees, and on x86-64 every read of a promoted
node was going to the interpreter. With that fixed ([the tenth
round](#the-tenth-round-x86-64-and-the-nurserys-bet)) the 2MB default runs it
in 749ms, 16MB in 565ms, and 32MB in 909ms: a nursery that has not reached
its largest promotes nothing, and copies the long-lived tree at every
collection while it grows. `wordfreq` is 303ms at the default and 240ms at
16MB. The nursery is 2MB, and each green thread has one, so it is sized for
having many of them and for `benches/Latency`'s pauses; a nursery that grows
with what a thread actually allocates and keeps is still what this wants, and
the column above is the default, untuned.

`binarytrees` is worth reading for its own sake. It is nothing but
allocate-walk-discard, and the ordering is not the one the rest of the table
has: GHC's generational collector wins outright at 290ms, Java's is next, and
then — level with Node and ahead of Go, Rust's `Box` and C's `malloc`/`free` —
comes `meadow-silo` at 1.28s, counting by reference and reusing the block it has
just freed. Every collector with a bump allocator beats `malloc` here by two to five
times, and the received wisdom that reference counting cannot compete on
allocation-heavy functional code is not what this row says either. (Koka, whose
Perceus is the nearest thing in production to what `meadow-silo` does, is the
comparison this row wants and this machine cannot give.)

**Go is the most consistent column.** It wins `contention` outright, is never
worse than 4.6×, and has none of the one-task disasters every other column
contains somewhere.

**Two results are about the other languages, not Meadow.** Python's `wordfreq`
at 69ms beats Haskell, Java and Node, because `str.split` and
`collections.Counter` are C and the Python is four lines of glue. And GHC's
`Chan` on `pipeline` is a boxed linked structure with an `MVar` per cell doing
what a ring buffer does elsewhere; it is also the noisiest measurement in the
suite.

## With arenas

A language without a bump-allocating collector has an answer to this: stop
allocating. Build into one flat block, index it rather than point into it, and
throw the block away whole. `./run.py --arena` runs the same four
single-threaded tasks written that way -- same algorithm, same checksums, the
allocator taken out -- and the question it asks is how much of the gap a
programmer can close by hand.

A node in `binarytrees_arena` is three slots (value, left, right) in one array,
a child is an index, building a tree is a bump of a counter and discarding one
is setting the counter back to nought. In `wordfreq_arena` a word is a pair of
numbers into the corpus, so no word is ever copied, and the table is three flat
arrays probed in place; only the ten words reported at the end become strings.
`fib` allocates nothing and `matmul` is flat arrays already, so those two are
the same programs -- which is the answer for them, and the rows are there to
say so rather than to be left out.

On the same machine, minimum of five runs:

| task                | meadow        | meadow-silo   | rust         | c         | go           | haskell       | java         | python         | js           |
| ------------------- | ------------- | ------------- | ------------ | --------- | ------------ | ------------- | ------------ | -------------- | ------------ |
| fib_arena           | 29ms (4.4×)   | 13ms (2.0×)   | 7ms (1.0×)   | **7ms**   | 12ms (1.8×)  | 16ms (2.4×)   | 73ms (10.9×) | 218ms (32.5×)  | 51ms (7.6×)  |
| binarytrees_arena   | 2.48s (11.2×) | 347ms (1.6×)  | 239ms (1.1×) | **222ms** | 240ms (1.1×) | 2.95s (13.3×) | 292ms (1.3×) | 11.98s (54.0×) | 628ms (2.8×) |
| matmul_arena        | 122ms (21.5×) | 16ms (2.9×)   | 6ms (1.0×)   | **6ms**   | 16ms (2.8×)  | 32ms (5.5×)   | 77ms (13.5×) | 721ms (126.5×) | 58ms (10.1×) |
| wordfreq_arena      | 329ms (25.9×) | 212ms (16.7×) | 13ms (1.0×)  | **13ms**  | 15ms (1.1×)  | 23ms (1.8×)   | 89ms (7.0×)  | 613ms (48.2×)  | 55ms (4.3×)  |
| binarytrees_compact | 2.42s (1.9×)  | **1.25s**     | —            | —         | —            | —             | —            | —              | —            |
| _startup_           | 4ms           | 3ms           | 3ms          | 3ms       | 6ms          | 10ms          | 67ms         | 27ms           | 37ms         |

**Against the table above**, on `binarytrees`, which is the row this is for:

| language    | as written | with an arena |              |
| ----------- | ---------- | ------------- | ------------ |
| rust        | 1.74s      | **239ms**     | 7.3× faster  |
| c           | 1.56s      | **222ms**     | 7.0× faster  |
| go          | 1.32s      | **240ms**     | 5.5× faster  |
| meadow-silo | 1.28s      | **347ms**     | 3.7× faster  |
| js          | 1.31s      | **628ms**     | 2.1× faster  |
| java        | 524ms      | **292ms**     | 1.8× faster  |
| meadow      | **2.36s**  | 2.48s         | no different |
| python      | **6.34s**  | 11.98s        | 1.9× slower  |
| haskell     | **290ms**  | 2.95s         | 10.2× slower |

**The arena is worth most to the languages whose allocator is worst.** C and
Rust gain seven times over `malloc` and `Box`, Go five, and all three land
within a factor of the fastest — which is the finding the row was written to
get: a programmer who cares can take the allocator out of the comparison
entirely, and what is left is much closer together than the table above.

**It is worth nothing to a language whose allocator is already a bump.** GHC is
_ten times slower_ with the arena than without it: its generational collector
allocates a node in a few instructions and never looks at it again, while every
write into an `STUArray` goes through the ST monad. Python is twice as slow with
`array('q')` as with tuples. Neither result is a failure of the arena; both say
the same thing from the other side, which is that these languages already had
one.

**Writing this row is what made it pay for Meadow.** The first measurement had
`meadow-silo` a shade _slower_ with the arena than without it -- alone among the
compiled languages here, and an odd enough result to be worth chasing rather
than reporting. `MEADOW_SILO_PRIMS` said why in one line: 67.6 million calls
into the runtime for `setRef`, one per node, because the bump counter is a
`Ref` and reading or writing one was a call while `St.get` and `St.set` had
long been emitted inline. Emitting those two inline as well -- a `Ref` is a
block of one field, so its value is a load and a store at a known offset --
took this row from 1.36s to 317ms when it was measured, and the arena went
from costing Meadow nothing to being worth **3.7×** of it. The benchmark did
not measure a thing that was slow; it found one.

Part of what is left of the gap to C is the bounds check Meadow emits on every
array read and write, which C does not do; how much of it, this row does not
say.

`wordfreq_arena` gains for a different reason, and gained before the fix:
446ms to 329ms on the default runtime and 323ms to 212ms on `aot`, because
there the arena deletes a hash table and a string per word rather than
replacing an allocation that was already cheap.

**Compact regions are not an arena** (`binarytrees_compact`, Meadow only).
`Compact.make` copies a value into memory the collector never looks inside,
which is what the long-lived tree wants: one mark instead of 22 MiB traced at
every cycle. Measured, it is worth nothing here either -- 2.42s against 2.36s
on the default runtime -- because the collector was not spending its time on
that tree; it was spending it on the churn.

**On `meadow-silo` it cannot help at all, by construction**, and the row is
flat for a different reason: 1.25s against 1.28s. Counting references, there
is nothing to compact -- so a compact there is a box around a value and not a
copy of it (`silo/src/prims.rs`, `Compact`), and a box changes neither how the
67 million nodes were allocated nor how they were counted. What _would_ help
is the other half of the idea: allocating new values **inside** a region
rather than copying finished ones into it, so the churn is a pointer bump, no
node is ever counted, and the whole region goes at once. That is region
allocation rather than compaction, it needs the escape rule `runSt` already
has (`lib/Std/src/St.mw`) extended from cells and arrays to everything
allocated in the scope, and `binarytrees_arena` at 347ms against 1.28s is the
size of the prize. It is not implemented.

Putting the churn in regions as things stand is
far worse: compacting every tree as it is built takes **224 seconds**, ninety
times the plain program, because a region costs a copy of everything put into
it and these trees die immediately. A region is for data that is large, shared
and long-lived, and this benchmark's data is none of those.

## What was fixed

Changes that came out of profiling this suite, in rounds. Each was measured
before and after, and the numbers are in [Results](#results).

### The tenth round: x86-64, and the nursery's bet

`binarytrees` on the default runtime was 2.36s on this machine against
GHC's 290ms, and this file put that down to the nursery. A profile said
otherwise. Two fifths of the run was the interpreter: 35 million tag tests
and field reads handed back by native code (`MEADOW_TRAPS=1`), every one of
them on a tree that had lived long enough to be promoted. The third round
taught native code to read the old generation through the block tables --
on arm64, where those rounds were done. On x86-64 a tag test or a field of
an old object still went to `meadow_exec`, and a bigger nursery only helped
because it promoted fewer trees. Now x86-64 walks the tables too.

The sixth round's calling convention had not come across either. On x86-64
every chain wrote the fixed registers back to memory and every warm entry
read them again, and every call and every return rebuilt the register file
in memory in a loop. It now does what arm64 does: `r0`–`r4` stay in
`r8`–`r11` and `r15` from one function to the next, and a call or a return
puts its arguments there and jumps to the method's entry. Arithmetic,
comparisons and moves are done in those registers rather than through
`rax`, a frame's slot is one load and an add, and a tag test and the field
reads after it find the object once (`Emit::KEEPS_FOUND`) -- for an old
object that was three dependent loads per instruction.

Then the collector. A survivor is kept young for one collection before it
is promoted, which is a bet that it is about to die, and a tree bigger than
the nursery loses the bet every time: everything survives that collection
and the next, and was copied twice. The heap now watches the bet
(`Heap::tenure`) and promotes at once while it keeps losing. Promoting an
object is one copy rather than a block lookup per slot, and promotion asks
for the cache line it is about to write, since the old generation's free
lines were last touched long ago.

Where `binarytrees`' processor time went, by `perf` on the Linux build of
the same code (the marking threads' share is on other cores), and what
`meadow run --gc-stats` says of the collections:

```text
                                  before      after
  interpreter, for native code      38%         0%
  native code                       35%        55%
  nursery collections               17%        29%
  marking (other threads)            7%        13%

  run, on the JIT                 1976 ms     784 ms
    paused for collections        344 ms     248 ms
  collections                       1100        921
  copied within the nursery      402 MiB     46 MiB
  promoted                       337 MiB    370 MiB
  longest pause                  1.95 ms    1.69 ms
```

Alternated, the two compilers' executables run in turn on this machine:

| task        | before | after      |
| ----------- | ------ | ---------- |
| fib         | 34.6ms | **20.5ms** |
| binarytrees | 1.96s  | **752ms**  |
| matmul      | 128ms  | 127ms      |
| wordfreq    | 373ms  | **302ms**  |
| rbtree      | 8.18s  | **3.58s**  |
| mandelbrot  | 103ms  | 100ms      |
| contention  | 101ms  | 97ms       |
| pipeline    | 74ms   | 61ms       |

In the suite's own run of every language afterwards:

| task        | meadow       | meadow-silo  | rust         | c            | go           | haskell   | java         | python        | js           |
| ----------- | ------------ | ------------ | ------------ | ------------ | ------------ | --------- | ------------ | ------------- | ------------ |
| binarytrees | 769ms (2.2×) | 670ms (1.9×) | 1.93s (5.5×) | 1.70s (4.9×) | 1.42s (4.1×) | **349ms** | 538ms (1.5×) | 7.57s (21.7×) | 842ms (2.4×) |

`benches/Latency`, which is what the nursery's size is for, kept its tail --
the slowest request about a millisecond, the 99th-percentile pause 0.92ms
either way -- and spent 310ms paused where it spent 400.

What still separates `binarytrees` from GHC is the convention and the
layout, not the allocator, which is a bump in both. Counted by callgrind,
Glade's native code spends about 190 instructions building and walking each
node -- 300 before this round -- where GHC's whole program, collector and
all, spends about 40: every call pushes a frame with two header words, a return looks its
target's entry up in a table, a tag test checks the generation and for an
old object walks the block tables where GHC reads a tagged pointer, and the
step counter and `live` are kept up throughout -- on five fixed registers,
where arm64 has thirteen. Every object has a header of two words where
GHC's has one, so the same trees are 1.8 GiB of allocation against 1.3 GiB,
and the collector copies that much more. And the nursery is bounded for
short pauses, so a tree bigger than it is promoted into the old generation
and marked there, where GHC's 4 MiB nursery feeds a copying second
generation collected rarely, for as long as it takes.

### The ninth round: two at a time

A loop whose body is index arithmetic, reads and writes of arrays that do not
change during it, and float arithmetic on what it read -- `matmul`'s inner
loop, after the eighth round -- is done two elements per trip on the vector
unit (`glade/src/codegen/vector.rs`; `docs/RUNTIME.md`, "Vector loops"). The
plan is made from the bytecode: the induction register and its bound, which
registers are the arrays, that every index is the induction register plus
something invariant (stride one, so the element after is the next word), and
that nothing is carried from one iteration to the next through a register.
Before the loop, a _preheader_ checks once what the scalar code checked on
every access -- that each array is the kind expected with `Float` elements,
that every index the whole loop will use is inside its array (the body's own
index arithmetic, run at the first and last values of the induction
register), and that an array written is not one read at a different index
under another name -- and then runs the body on `q` registers while two
iterations remain, falling into the ordinary scalar loop for the odd one out,
or for all of them if a check fails. The scalar loop is never changed, so the
vector version is only ever a faster way of doing the same thing.

Two things had to be true underneath. An object bigger than a block is now
contiguous in memory -- the old generation lays it out in a run of blocks in
one allocation -- so an array is one base and an offset, and the thin steps
reach an element that way (one block-table walk per access, not two). And
the checks had to be made where the access is, not after the body's
arithmetic: lowering reuses the index register for the store's argument
window, and a check made at the end compared an address with a length and
sent every loop to the scalar path.

`matmul` went from 104ms to 14ms: level with Go, ahead of OCaml, 2.8× from C.

### The eighth round: loops

Two things, and the second was found by the first.

`St.forRange lo hi (\i -> ...)` was a recursive function called with a
closure: a call, a frame and a return per iteration, for a body of a few
instructions. The inliner now **specialises** such a definition at a call
that gives a lambda for a parameter it passes through unchanged -- the
lambda substituted, the parameter dropped from the recursion -- and, where
every recursive call is a tail call, the copy is a _join point that jumps to
itself_: lowering makes that a block and a jump, and the loop's body is the
lambda's body. `while` and the folds qualify the same way. The simplifier
also applies a lambda where it stands (`(\i -> e) x` is `let i = x in e`),
which is what lets the substituted lambda disappear. `matmul`'s inner loop
became eight instructions and a jump -- and ran no faster.

The reason was the collector's high-water mark. `live`, the count of
registers a collection scans, was raised at every join, and a loop's header
is a join: five instructions per iteration, a load and a store of the same
word, which serialised every trip round on the one before. A jump into a
loop now declares `live` for the whole loop (the most registers anything in
it writes; raising it is safe, since the collector reads each pc's map for
what a register holds and `live` only bounds it), every other way in raises
to that, and the header knows it. `matmul` went from 178ms to 104ms.

### The seventh round: a mutable table, and the long tail

`wordfreq` was the row the sixth round had not touched, and profiling it said
why: the program was not the same program as the others'. Every other
language counts words in a mutable hash table; Meadow was path-copying a
persistent trie per word. `Std.Collections.HashTable` is the mutable table --
open addressing, linear probing, tombstones, cached hashes, living inside a
`runSt` like a `StArray` -- and `insertWith` is one probe and one write.
That alone took 816ms to 648ms, which was less than it should have been, and
the trap counter said the rest: 8.9 million instructions handed back to the
interpreter, twenty-two per word.

Most were one thing. A closure or a frame with more than eight captures has a
header longer than two words (the field descriptors no longer fit the second
one), and native code neither built such objects nor entered them: every
`insertWith` continuation captured ten to twelve values, so its allocation
_and_ its invocation went to the interpreter. The native allocator, the frame
push, the general `invoke` path and the method entries now handle a header of
any length. With that, `getRef`, the array and string lengths as thin steps,
a bare store for a reference into a _young_ array (no barrier is needed for
one), and `String.split` cutting into an array of the right size instead of
growing a persistent vector a piece at a time, the count is 2.7 million:
seven primitive calls per word, each one real work in Rust. That is the
long tail, and the shape of the next step -- a direct calling convention for
primitives, so a hash of five bytes does not cost a trip through the
interpreter's dispatch.

### The sixth round: arguments in registers

The calling convention, which the fourth round had measured as the cost of
`fib` and the fifth had left alone.

First a fixed assignment: bytecode registers `r0`–`r12` live in the same
thirteen machine registers in every native function, at every level, instead
of each function choosing which few to pin. Control passing from one function
to another then moves nothing, and the register file in memory is touched
only where the interpreter could look at it. On its own this moved nothing
either -- `fib` stayed at 47ms -- because `Invoke` still rebuilt that file in
memory on every call, word by word, and now reloaded thirteen registers from
it afterwards.

Then the call itself. Every method block gets a native **entry** that knows
its own shape -- the compiler now records, per method table, how many
captures the object holds and how many registers the block takes -- so a call
puts its arguments in `r0..argc` and jumps there, and the entry moves them
past the captures, loads the captures from the object, sets `live` and falls
into the block. A return is the same thing for a frame, which is recognised
by its address alone (everything in a stack chunk is a frame), popped, and
entered through the entry of the block it names. Nothing goes through memory.
The interpreter's `Invoke` did not change, and the two agree by construction
(`docs/RUNTIME.md`, "Method entries and the fixed registers").

`fib` went from 47ms to 26ms, `binarytrees` from 1.70s to 1.28s, `matmul`
from 280ms to 186ms. The bug this round found was not in native code: the Std
tests had never been run at `-O2`, and doing so showed the inliner losing a
value's type when it inlined a release-mode specialized copy, whose type
binders are all `#Ref`; lowering now reads the field types a pattern was
checked at when the value's own type says nothing.

### The fifth round: registers, and the inliner

Two changes, one on each side of the bytecode.

Native code now does its arithmetic _in_ the machine registers a function pins,
instead of moving each operand into `x9`, operating there and moving the result
back: `add x3, x4, x5` where there were four instructions. A bytecode register
the function only ever does float arithmetic on is pinned to one of
`d16`–`d31` and stays there, so a `mulf` on two pinned registers is one `fmul`;
and the pool of general pins grew from seven to thirteen (`docs/RUNTIME.md`,
"Bookkeeping native code keeps up"). `mandelbrot`, whose inner loop is float
arithmetic on eight registers and nothing else, went from 179ms to 74ms: its
escape test is now `fmul, fmul, fadd, fcmp, b.le` on `d16`–`d19`. `matmul`
moved 4% on this alone, which said its cost was somewhere else.

It was. The inliner in `compiler/meadow-core/src/inline.rs` — written, tested
and left out of the pipeline because it broke two differential tests — is in
the pipeline for release builds (a debug build keeps every call a call, so the
debugger has frames to show). Neither failure was its own. A record accessor inlined at a record
literal is a `match` on a literal, whose fields lowering typed as nothing; and a
jump to a join point whose argument has to be evaluated first was not keeping
the join's environment alive meanwhile. Both were lowering bugs no program had
produced before, and both are fixed where they belonged. It also had to learn
that a call to a specialized copy carries the original definition's type
arguments. With `St.get` and `St.set` inlined, `matmul`'s inner loop has no
call in it but the loop's own, and 695ms became 280ms — from 146× C to 56× —
with nothing changed in the machine code for any of the instructions involved.
`wordfreq` moved 10% and `pipeline` 14% from the inliner alone.

`fib` and `binarytrees` did not move, as the fourth round said they would not:
their cost is the calling convention itself, which this round did not touch.

### The fourth round: a frame stack

A function's continuation was a heap object; it is now a frame on a per-thread,
chunked stack (`docs/RUNTIME.md`, "The call stack is a frame stack"), with
effect handlers capturing frames by unlinking chunks rather than copying them.
All of it is exercised by the differential suite against the CEK machine, which
never changed.

The measured result is the table above, and it settles a question this suite
had been assuming an answer to. `fib` is 46ms against OCaml's 12ms, and the
working theory was that the gap was the four-word continuation allocated per
non-tail call. It is not: with the continuation on a stack and returned through
by a direct jump, `fib` does not move. A nursery bump was already as cheap as a
push; the collections it caused were 0.5% of the run. What `fib` pays for is
the convention around the call — a register file that lives in memory, so every
argument and every returned value is a store and a load; a generic `invoke` that
checks the object's kind and copies its captures into that file one word at a
time; and a step counter kept per instruction for preemption. None of that is
touched by where the continuation lives. The stack is the prerequisite for
fixing it — a return that leaves its value in a machine register needs a frame
to return _to_ — but it is not the fix.

### The third round: the native back end

Everything here came out of one question, answered by measurement instead of
inference: _which instructions does native code hand back to the interpreter,
and how often?_ Static counts said 3–9%. A sampler on the executable said
`matmul` spent 62% of its time in `meadow_exec`. A per-pc counter in the
runtime then said exactly which ones.

**Native code could not reach the old generation.** The nursery is one flat
array, so a heap word was one load off its base; the old generation is Immix
blocks, each a separate allocation, so a promoted object's fields were
unreachable from machine code and every instruction touching one bailed to the
interpreter. `binarytrees` keeps a long-lived tree and many medium-lived ones,
all promoted: **55 million** `Field` and `JumpUnlessTag` traps on trees that
native code itself had built. The heap now publishes a **block table** per
generation (`Heap::tables`, at `layout::TABLES`); a heap word is
`tables[a >> 30][(a >> 13) & 0x1FFFF][a & 8191]`, three dependent loads. The
object instructions branch on the generation bit, so a young object still costs
one load. 55 million traps became 4,467; `binarytrees` 2.98s → 1.89s.

**Arrays went the same way**, and were the matmul number. `stGetArray`,
`arrayGet` and `stSetArray` expand into `codegen::thin` steps that the back end
emits inline, with the nursery-only guard replaced by "not a compact region",
and the emitter tightened to use immediate forms instead of building each
constant. `matmul` 1.78s → 692ms; 62% in the interpreter → 0.6%. A store is
only taken for an array whose elements are not references: that is what lets
it skip the marker's mutation lock, the snapshot barrier and the remembered
set, and the reasoning is written beside the expansion.

**Four instructions the machine did not have.** `toFloat` on an `Int` was
16 million traps a run of `mandelbrot` (twice per pixel, for the coordinates);
`popCount` and `>>>` are what a hash trie finds a child with. They are one
instruction each on every machine this targets and now are here too, with
`Ushr`'s folded-constant form.

**Two bugs the tests caught.** The block table is a `Vec` that moves as blocks
are added, and it was republished only when the _nursery_ moved: `wordfreq`
read a freed copy and crashed. It is now republished at the single point
control returns to native code, `meadow_exec`, with a regression test that
promotes trees and closures and reads them natively. And a wrong encoding for
`addv` was an illegal instruction — caught by assembling every new encoding
with the system assembler and comparing bytes, which is how they are all
checked now.

### A change to the language: `Int` is the default

An integer literal nothing pins down was a `BigInt`; it is now an `Int`. This
suite is unmoved by it, because every program here annotates its numbers -- the
benchmark was written to measure the compiler, not the default. Unannotated
code is another matter. A program of the shape a person writes first --

```meadow
fun fib n = if n < 2 then n else fib (n - 1) + fib (n - 2)
fun sumTo n acc = if n == 0 then acc else sumTo (n - 1) (acc + n)
def work = [30, 30, 30, 30]
```

-- ran in **6.3s** under the old default and runs in **0.09s** under the new
one: 70×, from having machine words instead of heap-allocated numbers and typed
instructions instead of primitive calls. `benches/`, the VM-against-CEK package,
is unannotated too, and its ratios rose 20–80× for the same reason. What was
given up is that `2 ^ 64` is now `0` unless the program says `BigInt`, which
`docs/TUTORIAL.md` now says in the same place it used to say the opposite.

### The second round: the compiler

**Registers were never reused.** A name got a register when it was bound and
kept it for the rest of the block, so a straight-line block climbed through the
register file even where most of what it held was dead — and every transfer out
of it ended in a permutation putting things back. `move` was the most-retired
instruction in the machine: 27% of `fib`, 35% of `binarytrees`, 18% of
`mandelbrot`. The sequent IR already says where a value dies, so this was a
matter of reading it (`meadow_seq::still_used`) rather than computing liveness.
The case that matters is that **capturing a name is a use of it** — once a
closure holds a copy, what its methods do later is the methods' business — so a
continuation built out of the environment now lands on top of what it closed
over. `fib`'s recursive call went from five instructions to three:

```text
before                          after
closure r2 <- m7 [r0..+2]       closure r1 <- m7 [r0..+2]
subik   r3 <- r0 - 1            subik   r0 <- r0 - 1
move    r0 <- r3                jump    @169
move    r1 <- r2
jump    @169
```

**There was no simplifier.** `meadow_core::simplify` now does case of known
constructor (a `Maybe` built and matched in one expression is neither
allocated nor tested), case of case (the outer alternatives named once in a join
point and jumped to, rather than copied into each branch), and the algebraic
identities. On this suite the first two are **neutral**: these are numeric loops
and channels, not constructor-heavy code. They are kept because they are correct
and because the code they remove is the code inlining produces.

**The machine had no shift instruction**, and that made strength reduction a
pessimization. Turning `i * 256` into `i << 8` looks free — a multiply is three
to five cycles and a shift is one — but `Shl` was not in the typed instruction
table, so it lowered to a generic primitive, and a generic primitive is not
compiled natively: it traps back into the interpreter. `matmul` got **45%
slower** (1.84s to 2.66s). `ShlI`, `ShrI`, `AndI` and their folded-constant forms
now exist in the bytecode, the VM and both native back ends, and with them
strength reduction is neutral on `matmul` and worth 11% on `wordfreq`, which
hashes. The general lesson is the one worth keeping: an instruction is only
cheaper if the back end can see it.

### The first round: the runtime

**The nursery was 256 KB** (`glade/src/heap.rs`), so `binarytrees` collected about
eight thousand times. It is now 2 MB. That is a real trade and not a free win:
a nursery collection costs what _survives_ it, so a bigger nursery buys
throughput and spends tail latency.

```text
  nursery   Latency p99   over 1 ms   binarytrees   copied
   256 KiB       115 us           0        3.77 s   926 MiB
     2 MiB       459 us           0        2.75 s     —
     8 MiB      1.31 ms           0        2.14 s   450 MiB
    32 MiB      5.24 ms          79        3.40 s   5.3 GiB
```

2 MiB is the largest that keeps every pause well under a millisecond, which is
the property `docs/RUNTIME.md` claims. At 32 MiB the collector copies 5.3 GiB out
of 8 GiB allocated, because a long-lived object sitting in a large nursery is
recopied at every collection instead of being promoted out of it.
`MEADOW_GC_NURSERY` moves it, in slots.

**Every collection zeroed the whole to-space.** A copying collector writes every
slot below `top` and nothing reads above it, so the memset was pure loss — and it
made a pause cost what the nursery _is_ rather than what survived it. Removing it
cut median pause time by a quarter.

**Hashing allocated twice per call.** `Vm::hash_value` built a `Vec` work stack
for values that have no parts, and hashed a heap string by copying its bytes into
a fresh `Vec` first. A `Kind::Str` is already laid out exactly as the hasher
consumes a string — packed eight bytes to a little-endian word — so its words now
go straight in. `wordfreq` hashes a string twice per word; that was two million
allocations it did not need.

**A top-level `def` of a literal was a jump.** A mention of a global lowers to a
jump to its definition, and lowering only fuses a comparison into the branch
that tests it when the comparison cannot transfer control. So a bound written
`def limit : Int = 100` and read in a loop condition cost _two heap-allocated
continuations and an unfused compare per iteration_, where the same loop over a
parameter cost a single `bri`. `globals::inline_literals` puts the literal at
the mention. This was the largest single change in the suite's history, and it
moved every benchmark that reads a size or a bound from a top-level `def`.

**Each `St` array access read the object header three times** — once for the
kind, once for the length, once for the field. Now once. Worth 2%, and worth
less than it looks, because the cost of an array access is the round trip and
not the memory.

**The STM serialised every commit on one mutex**, and looked up each `TVar`
through an `RwLock` and an `Arc` clone on every read and every commit.
`World::tvar` was the largest single cost of a contended transaction. Commit
already locks the union of the read and write sets in sorted order, which is
two-phase locking and is all serializability needs, so the global lock was
redundant.

## Caveats, in order of how much they matter

1. **One machine, one run of the suite.** These numbers are from a 28-core
   Windows 11 desktop that was not otherwise idle. Ratios within a row are
   more trustworthy than absolute times, and nothing here is a claim about
   other hardware. The whole table moves about 20% between runs, so a cell
   compared against the same cell in an older version of this file says
   nothing: every claim in [What was fixed](#what-was-fixed) comes from
   alternating two compilers back to back instead. `contention` and `pipeline`
   vary by ±20% on their own, and `pipeline` in Haskell moves by an order of
   magnitude, so read that cell as one.
2. **OCaml, MLton and Koka are written but were not run here.** None of the
   three toolchains is on this machine, so their columns are absent rather
   than carried over from the machine that had them: a number measured on
   other hardware in a table of this one's would be worse than no number.
   Install any of them and `./run.py` fills the column back in.
3. **The arena tasks are written for the eight languages installed here.**
   `tasks/*_arena/` has no OCaml, MLton or Koka, for the same reason their
   columns are absent above: a program nothing has run is not a measurement.
   The shape to follow is in any of the eight, and `./run.py --arena` will
   pick a new one up.
4. **C has no `pthread.h` here.** The three parallel tasks are written against
   POSIX threads, and this machine's clang targets MSVC, which has none — so
   C's `mandelbrot`, `contention` and `pipeline` did not build. The column is
   marked `✗` for those, which is a fact about the toolchain and not about C.
5. **Startup is inside every number.** Java pays 65ms, Node 37ms and Python
   27ms before their first instruction. On `fib`, where the fastest is 7ms,
   that is most of what separates the columns. Subtract the last row before
   drawing conclusions about short tasks.
6. **`matmul` in Python is lists, not numpy.** Anybody multiplying matrices in
   Python calls numpy and is then timing a BLAS written in C and Fortran. That
   is a fair thing to know and not a thing this suite measures, so the Python
   entry is the loops written out.
7. **`contention` in JavaScript is not idiomatic.** JavaScript has no shared
   mutable object across workers, so the accounts are a `SharedArrayBuffer` and
   the atomic step is held by a lock built from `Atomics.wait`. It is the only
   way to write the task and no working JavaScript programmer would enjoy it.
   The absence is the finding.
8. **`tasks/startup/` is generated** by the harness and overwritten on each run.
   `work/` holds build outputs, the generated corpus and `results.json`, and is
   not checked in.
