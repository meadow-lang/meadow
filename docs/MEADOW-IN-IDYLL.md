# Meadow in Idyll

*A plan, not yet begun. It depends on work in Idyll's roadmap that is
designed but not built (Idyll's `docs/LOW.md`), and it is ordered so that
each step ships on its own.*

## Why

Meadow's runtime is where its speed and its worst bugs both live. It is
Rust, and much of it has to be `unsafe`: headers read at offsets, blocks
moved under live pointers, counts that must never drop to zero early. Tests
and differential runs catch many mistakes there. They cannot show that none
are left.

Idyll can give it both. Its low-level fragment, the `Mem`
effect extracted to C, writes code as close to the machine as the runtime's
Rust and as fast. Its proofs state what that code must keep true, such as
"this is a live block of this layout" or "nothing reachable was freed",
and check it before the code runs. Written in Idyll, the runtime can be at
least as fast as today, faster where a proof removes a check, and known
safe where today it is only tested. The compiler follows for the same
reason, and so that Meadow is written in one language with one toolchain.

## What it means

Meadow stays Meadow: the language its users write, its Std, its tools. What
changes is what Meadow's implementation is written in. There are two halves,
and they use two different parts of Idyll.

- **The runtime is written in low-level Idyll.** This covers the heap, the
  collectors, counting, stack segments, the scheduler, the primitives and
  the VM. It is written in Idyll's `@extract` fragment, with C's memory model
  as the `Mem` effect, and it compiles to C with no runtime of its own (see
  Idyll's `docs/LOW.md`). Every module is proved memory-safe: indices in
  bounds, buffers live, writes disjoint where they must be. The proofs are
  erased before code is made, so the C has no checks they paid for. This
  half replaces `silo/`, `silo/vm/`, `compiler/meadow-rt` and what Glade
  still contributes. Today that is Rust, with a large `unsafe` core and no
  proofs.
- **The compiler and the tools are written in high-level Idyll.** This
  covers the front end, the passes, Cut and AxCut, the memory pass, the LLVM
  emitter, the JIT's code generator, the driver, and the debugger's and
  language server's protocol code. They are garbage-collected code with
  effects, running on the runtime above as any program does, natively
  through Silo. Proofs go where they pay: the memory pass produces linear
  AxCut, a layout's fields do not overlap, an encoder's bytes decode to the
  instruction asked for. The rest is ordinary, tested code. This half
  replaces `compiler/`, `buildtools/` and, as its source, MeadowBoot
  (`bootstrap/`), which already has the compiler pass by pass in an
  ML-family language and checked against the Rust one.

Cut stays the contract between the halves and between the languages (Goal 3
in `ROADMAP.md`). A program of Meadow, of Idyll, or of anything else that
lowers to Cut runs on the same runtime, now written in Idyll.

## Why the runtime first

- **The runtime is where safety is weakest.** The runtime is where nearly
  all of the `unsafe` is: raw words read as pointers, headers read through
  offsets, blocks freed and reused, stacks switched. A memory-safety proof
  there removes the class of bug the merge has spent its debugging on (a
  stale reference after a move, a field read past a header, a dead slot
  shown to a debugger).
- **The runtime is where performance is decided.** Low-level Idyll compiles
  to C the way the Rust compiles to machine code, with the checks proved
  away rather than paid for.
- **It is the easier half to replace a piece at a time.** The runtime meets
  the emitted code at a C interface, the `meadow_*` entry points, so a
  module can be swapped behind it while everything else stays (see *The
  strangler*). The compiler is one pipeline, and its halves replace less
  cleanly.
- **The compiler is already garbage-collected code.** A bug there gives a
  wrong program, not a corrupt heap. It gains less from proofs, and it can
  wait for high-level Idyll to mature.

## What Idyll must have first

Idyll's roadmap has the base: `Std.Low` and the heap model, bounds by
types, the solver (Z3 or cvc5 through SMT-LIB), `@extract` and its C back
end, and a real library proved in it (SHA-256). These are steps 1 to 5 of
Idyll's `docs/LOW.md`, under *Low-level code* in its Phase 7.

A runtime needs more than a hash function does. These are the extensions to
ask of Idyll, in the order the plan below needs them:

1. **Words and tagged values.** A `Word` (`U64`), with immediates told from
   pointers by a tag bit, and a pointer read as a word and back again, with
   proofs that a word read as a pointer is one: the layout's `is_block`.
2. **Blocks typed by their headers.** A block is a header, then descriptors,
   then fields whose types the descriptors give at run time. Its model is a
   buffer of words with a ghost map from each field to what it holds, so
   that "this field is a reference to a live block" is a proposition the
   solver can use.
3. **Offsets within an allocation**, and a pointer to the middle of one.
   For example, a region's block sits behind the word that names its region.
   Each offset carries a proof that it lands inside the allocation.
4. **Function pointers**: tables of C functions, and calls through them.
   This is the method table the runtime calls emitted code through (the
   emitted code's own `ghccc` adapters stay in its IR).
5. **Structs laid out as C lays them out**, with alignment, together with
   globals, and a thread-local for the running thread's context.
6. **Atomics**: C11's, with a stated memory model. The scheduler, the
   cross-thread counts on regions and STM's commit need them. To start with,
   the proofs are sequential, and the atomics are trusted primitives with
   simple specifications (see *What is trusted*).
7. **Trusted primitives with specifications**, for what has no model:
   switching a stack (today `corosensei`), the system's calls (`mmap`,
   `VirtualAlloc`, threads, files, sockets, signals), and flushing the
   instruction cache. Each is a few lines of C or assembly per architecture,
   with a specification stated as an axiom. They are listed in one module,
   so that what is trusted can be read in one place.
8. **A static library**: exported names, separate compilation, `inline` and
   `cold` where asked, and LTO with the emitted program. The fast paths
   cross the boundary today (`mw.share`, the allocation fast path,
   `meadow_gc_poll`).
9. **The arithmetic big integers and hashing need**: 128-bit products and
   overflow-detecting arithmetic. Today the runtime takes these from
   `num-bigint`. They become Idyll code, with the big-integer operations
   proved against their mathematical specification.

How each is designed is Idyll's to decide; Idyll's roadmap lists them under
Phase 7, and `ROADMAP.md`'s *Meadow in Idyll* section keeps track of which have landed.

## The strangler: replacing the runtime a module at a time

The Rust runtime is not rewritten all at once. Each module is replaced
behind the C interface it already has, and each replacement is a release
that keeps every test passing.

- **The seams.** Each Rust module the runtime is built of becomes a set of
  `extern "C"` functions, if it is not one already, with nothing shared
  across the seam but those functions and the layout. The layout description
  (`meadow_rt::layout`) is the first thing written in Idyll. It becomes the
  one source both sides are generated from.
- **A mixed library.** Through the transition, `libmeadow_silo` holds both
  kinds of module: the Rust ones, and C compiled from Idyll. The runtime's
  fingerprint covers the Idyll sources as it covers the Rust ones.
- **The gates.** A replacement lands only when all of these hold:
  - every proof in it is discharged, with no new trusted primitive left
    unlisted;
  - the differential checks of `docs/RUNTIME-SPEC.md` hold: Std in every
    configuration of its matrix, the examples' differential, the debugger's
    and hot loading's tests;
  - the benchmarks are within noise of the Rust module's, measured with
    code layout held still (PERF-2 and PERF-3).
- **Running without C.** Low-level Idyll's `Mem` has a handler that runs
  the heap model in pure Idyll. So each module also runs on the reference
  interpreter, against its specification, in the differential style the
  compiler's tests use. A test of the collector is a test of its model and
  of its C.

## The order

Each step has a *done when*. Steps MI-1 to MI-3 wait on Idyll's extensions
1 to 5; the rest come in turn.

| step | what | done when |
|---|---|---|
| **MI-0** | The design settles in Rust first: the runtime merge's roadmap R1 to R4 (layout, JIT tier 1, LIR, heap frames). Porting a design while it moves is paying for it twice. | `docs/RUNTIME-SPEC.md`'s R1 to R4 are done |
| **MI-1** | The layout, in Idyll: `SILO` and `GLADE` as values, with their invariants as theorems (no two fields overlap; every kind's number distinct; the region floor above every collection's mark). The Rust and the LLVM back end read a table generated from it. | the layout's unit tests are proofs, and the generated table is byte for byte today's constants |
| **MI-2** | The allocator: the size classes, pages, free lists and the nursery's bump, proved to hand out disjoint, live blocks of the size asked for. | `heap::acquire` and `trace::Space::take` are Idyll; the gates hold |
| **MI-3** | Tracing: marking and sweeping, proved to free only what no root reaches, and to keep the heap well formed (every reference field of a live block points to a live block or is an immediate). | `trace::collect` is Idyll; the gates hold under `tracing` |
| **MI-4** | The young generation: the copier, proved to rewrite every reference to a block it moves, with the barrier's obligation stated as the store's precondition. | `trace::minor` and the barrier are Idyll; the gates hold under `generational` with the smallest nursery |
| **MI-5** | Counting: share, erase, reuse, lazy freeing and the cycle collector. Each operation is proved to keep the count equal to the references it accounts for, given what the memory pass guarantees: that each name is consumed once. | `heap.rs` and `cycles.rs` are Idyll; the gates hold under `rc`, and the leak check reports nothing |
| **MI-6** | The rest of the runtime: stack segments and the scheduler (on the trusted stack switch and atomics), regions, parcels, STM, the primitives and printing, big integers. | `silo/src` has no Rust left |
| **MI-7** | The VM and the JIT: the interpreter's loop in low-level Idyll, and the JIT's encoder in high-level Idyll, proved against a decoder of the same instructions. | `silo/vm` has no Rust left; the VM's speed holds (PERF-3) |
| **MI-8** | The compiler, ported pass by pass from MeadowBoot, each checked against the Rust compiler as MeadowBoot's are. The memory pass is written so that its output is linear by Idyll's grades, which the type checker then proves. | `meadow` builds itself with no Rust in it |
| **MI-9** | Retire the Rust: the bootstrap chain below, and Glade's and Silo's crates removed. | a release builds from Idyll and C alone |

## What is proved, and what is trusted

The proofs, from the cheapest to the most valuable:

1. **Memory safety** of every low-level module: in bounds, live, disjoint
   where required. This is required of every module, and Idyll's solver
   discharges most of it.
2. **A well-formed heap**: headers that agree with the layout, and
   reference fields that point at live blocks or are immediates. Every
   exported runtime operation preserves it.
3. **The collectors' soundness**:
   - marking reaches everything the roots reach;
   - sweeping frees only what marking did not reach;
   - the copier leaves no reference to a moved block's old place.
4. **Counting's soundness**: given the memory pass's linearity, a count is
   the number of references its block has, so nothing is freed while it is
   still held.
5. **The compiler's invariants where they are cheap**: linear AxCut out of
   the memory pass, a layout's arithmetic in the emitter, an encoder against
   its decoder.

Proving the whole compiler correct, CompCert-style, is not a goal.

What is trusted, and listed in one place:
- the C compiler and LLVM;
- Idyll's type checker and its extraction to C;
- the SMT solver, unless its answers come with certificates (an open
  question of Idyll's);
- the trusted primitives of extension 7, each with its specification;
- the system's own libraries.

## Performance

The aim is speed at least today's, and better where the proofs let it be.

- **No check the proofs make unnecessary.** A bounds check is proved, not
  run; a header read is an offset, as now.
- **Fast paths stay in the emitted code.** The inline `share`, the
  allocation fast path and the safe point are emitted LLVM IR today and stay
  so. What changes is the runtime behind them.
- **One optimizer over both.** The runtime's C and the emitted program are
  optimized together (LTO), as the Rust and the emitted program are today.
- **Every step meets the speed gates** of `docs/RUNTIME-SPEC.md`: no step
  lands slower, measured with code layout held still.
- **The proofs buy freedoms Rust does not allow.** One example: a block
  reused in place with its unchanged fields left unwritten, as the LLVM
  back end does now and as the Rust runtime cannot do without `unsafe`.

## Bootstrapping

- **The runtime needs no bootstrapping.** It is C, and a C compiler builds
  it.
- **The compiler does.** The chain is:
  - Idyll's compiler (`front/`, written in Meadow, or the Rust `idyllic`)
    builds Meadow's compiler, written in Idyll;
  - that compiler runs on the runtime above and builds Meadow programs,
    Idyll's `front/` among them.
- **Each release is built by the last.** A pinned stage 0, release *N*,
  builds release *N + 1*, as Rust's and GHC's are built. The Rust compiler
  stays as stage 0 until a release built without it has passed the full
  matrix once.
- **Idyll's own compiler** being written in Idyll would shorten the chain.
  That is Idyll's roadmap's to decide.

## Risks and open questions

- **Idyll is the critical path.** None of this starts before Idyll's
  `Mem`, its solver and its extraction exist, and the extensions above go
  past what Low\* has. Steps MI-1 and MI-2 are the test: if a layout and an
  allocator cannot be written and proved at today's speed, the plan is
  re-thought there, not at MI-6.
- **Proof effort.** HACL\*'s experience is about a line of specification and
  proof for each line of code, for verified crypto. A runtime's invariants
  are harder than a hash's. So the order puts memory safety, which the
  solver mostly does, before soundness, which needs lemmas written by hand.
- **Concurrency.** Proofs about atomics, the scheduler and STM are research
  territory. The plan proves the sequential parts, and trusts the atomics'
  specifications until there is a way to do better.
- **Compile times.** A solver in the build loop is slower than `rustc`.
  Proofs are checked once per change of a module, and cached as Lingua's
  build state is.
- **The platforms.** C reaches every target Meadow has (Windows, Linux,
  macOS, Android, iOS), but the trusted primitives (the stack switch, the
  executable memory) are per architecture, as they are in Rust today.
- **Two moving targets.** The runtime merge is still moving, and so is
  Idyll. MI-0 exists so that the first is settled before it is ported.
