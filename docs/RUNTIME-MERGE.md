# One runtime: merging Glade and Silo

Status: **design, in progress** on branch `runtime-merge`. Phase 1 has started.

Meadow has two runtimes. **Glade** is a bytecode VM, with a JIT and AOT
executables, and a generational tracing collector (`docs/RUNTIME.md`).
**Silo** compiles to native code through LLVM, with reference counting, reuse
and the native stack (`docs/SILO.md`). Both run the same AxCut, which Cut
lowers to (`docs/CUT.md`), so a difference between them is a runtime
difference.

The aim is **one runtime** with two engines: an interpreter for bytecode,
and native code through LLVM. Collection becomes a choice of strategy that
works with either engine. It is not a property of the engine.

## What has to survive

- **Glade's fast build.** Debug builds go from AxCut to bytecode in one pass,
  with no LLVM, so they compile in a fraction of a Silo build's time. This
  stays the default for `meadow build` and `meadow run` in debug.
- **Glade's introspection.** The debugger (DAP), the REPL, stepping,
  breakpoints at any instruction, and reading every live value through its
  descriptor. None of this may get worse.
- **Silo's speed.** Release builds through LLVM keep reference counting with
  reuse, borrowing, frames on the native stack, stack segments for effects,
  and the `ghccc`/`tailcc` calling convention. The benchmarks in
  `benchmarks/` are the gate. A phase that slows a Silo row by more than
  noise does not land.
- **Every collection strategy on every engine.** Reference counting with
  reuse and cycle collection (Silo's today), and generational tracing
  (Glade's today), each with the interpreter and with LLVM. The defaults stay
  what they are, interpreter with tracing and LLVM with reference counting,
  but all four combinations are supported and tested.

## Where the code splits today

Everything above AxCut is shared: the front ends, Core, Cut, AxCut, the
reference interpreters, and `meadow-rt`, the runtime interface (primitives,
descriptors, numbers, text, hashing, roles, sockets, FFI). The split is below
AxCut, along four lines.

| | Glade | Silo |
| --- | --- | --- |
| lowering | `meadow-codegen`: AxCut → bytecode, one pass | `meadow-llvm`: `linear` (ownership, borrowing) then `emit` → LLVM IR text |
| memory | no counting; GC maps per instruction; nursery (Cheney) + Immix old generation, marked concurrently | `share`/`erase`/reuse emitted inline; lazy freeing; trial-deletion cycle collector |
| addresses | slot numbers; the range says the space (nursery, old, regions) | machine pointers; odd words are blockless objects |
| object header | `len │ uniform │ desc │ kind`, `field descs │ meta` | `extra refs │ len`, `kind │ flags │ meta`, field descs |
| frames | a per-thread stack of 64 KiB heap chunks | the native stack; a frame used once is never built |
| effects | `handle` enters a chunk of its own; detach and reattach chunks | stack segments (`corosensei`) |
| native code | blocks of bytecode, any instruction may go back to the interpreter (`docs/NATIVE.md`) | whole functions; there is no interpreter |
| scheduler | M:N, work stealing, a heap per green thread | M:N, a context per green thread |
| natives | `glade/src/native.rs` | `silo/src/native.rs`, the same operations again |

Two things hold the engines apart, and neither is the bytecode itself:

1. **Memory management lives in different places.** Silo's ownership
   analysis is inside `meadow-llvm`, so no other back end can use it.
2. **Glade's native-code contract.** Because any instruction may hand control
   back to the interpreter, the interpreter's state must be exact at every
   instruction. So Glade's native code cannot keep values in registers, use
   the native stack, or see SSA. That, more than the instruction set, is the
   gap between Glade's native code and Silo's.

## The shape

```text
Cut ─► AxCut ─► memory pass ─► AxCut-M ─┬─► bytecode ─► interpreter  (debug default)
                (per strategy)          │              (JIT, later)
                                        └─► LIR ─► LLVM ─► native    (release default)

                      one runtime under both engines:
   object model · heaps (RC, tracing) · scheduler · segments · natives ·
   primitives · regions · STM · FFI · printing
```

### AxCut-M: AxCut with its memory operations written down

The memory pass is Silo's `meadow_llvm::linear`, moved up from `meadow-llvm`
into a pass over AxCut. For reference counting it writes out what `emit`
works out internally today:

- `dup x` and `drop x`;
- `reset x` / `reuse x in let ...` for building into a block that was just
  taken apart (`docs/SILO.md`, "Reuse");
- borrowed parameters, and the set of pure definitions;
- which continuations are frames.

This is the design of Lean 4's λRC IR, which is both interpreted and compiled
to C.

For tracing, the pass writes no counting. It marks the stores that need a
write barrier, and what is live at each point that can collect. Those are the
GC maps the bytecode carries today, worked out once for both engines.

Both engines consume AxCut-M. The bytecode gains `Dup`, `Drop`, `Reset` and
`Reuse`, and an image records which strategy it was built for. A runtime
refuses an image made for another.

### LIR: the low level, made explicit

`meadow-llvm`'s `emit` today goes from AxCut and the linear analysis
straight to LLVM text. Pulled apart, it is:

1. a lowering to a low-level IR: SSA over 64-bit words, explicit block
   layouts and header words, calls into the runtime by name, and frames
   marked;
2. a printer of that IR as LLVM.

The LIR is where a JIT would attach later, through Cranelift or Glade's own
x86-64 and aarch64 emitters (`glade/src/codegen`). It is also where a
strategy's code differs on the LLVM path: inline counting under RC, barriers
and frame records under tracing.

### One operation, one meaning

Each bytecode operation is defined once, as a function in the runtime. The
interpreter calls it. The LIR lowering either expands the operation inline,
for the ones LLVM can do better (arithmetic, field access, counting), or
calls that same function. Silo already works this way: `meadow_prim` is the
fallback beside inline primitives. So bytecode can keep large operations that
are good for an interpreter, and LLVM still sees small ones.

### Tracing on the LLVM path

A moving, generational collector needs its roots and its write barriers.

- **Roots: precise frame records.** Around each call that can collect, and
  each allocation, the lowering keeps the frame's live references in a small
  record on the native stack. It links the record into a per-segment chain
  and unlinks it on return. Code reads those values back from the record
  after the call, so the collector may move what they point to. This is
  LLVM's own "shadow stack" strategy. Silo already does it, for effects, in
  `silo/src/shadow.rs`, where it costs "a few stores and a load around each
  non-tail call". A frame that holds no reference across a call makes no
  record.
- **Barriers.** Every store of a reference into an existing object is a
  barrier under tracing: `Ref` and `TVar` writes, mutable arrays, `SetField`
  (tail recursion modulo cons), in-place record updates. The memory pass
  marks them, and the lowering emits the check inline: an old object, a young
  value, then remember it. Under RC they cost nothing.
- **Allocation.** A bump pointer and a limit, inline. The slow path is a
  runtime call, which is a point that can collect.

Two alternatives, kept for later:
- **LLVM statepoints**, for precise stack maps with no records. They are
  faster at run time, but intrusive, and they slow code generation down.
- **Conservative scanning of the native stack**, with a precise heap. This is
  what WebKit and Bartlett's mostly-copying collectors do: anything a stack
  word might point to is pinned. It is the fallback if records prove too
  slow.

### The interpreter on the native stack

Glade's frame chunks go. The interpreter runs on the native stack:

- A non-tail call is a recursive call of the interpreter's loop.
- A tail call is a jump within the loop.
- Effects use Silo's stack segments, so a `handle` enters a segment and a
  resumption reattaches it.
- Deep recursion has the room it has on Silo: segments reserve a large stack
  and commit it as it is touched.

Each activation's registers are a window in its Rust frame. The window, with
its current pc and that pc's GC map, is linked into the same chain of frame
records that native code uses. One chain then serves three uses:
- the collector's roots under tracing;
- the debugger's backtrace and its reading of locals;
- the discard of an abandoned continuation's values under RC.

Later, interpreted and native functions can call each other through one
calling convention. Then a program can run part interpreted and part
compiled, and a JIT can compile what is hot.

### One object model

Both heaps use **Silo's layout**: machine pointers, and odd words for
blockless objects (a nullary constructor, a closure capturing nothing).
Headers are two words:

```text
word 0   strategy word: u32 │ length: u32
word 1   kind: u8 │ flags: u8 │ spare: u16 │ meta: u32
words    field descriptors, four bits each (absent for uniform blocks)
fields
```

The strategy word is the extra-reference count under RC. Under tracing it
holds the mark, the age, and a forwarded flag; a forwarded block keeps its
new address in word 1. The nursery and the old generation become address
ranges reserved per heap, so "is this young?" stays one compare, as it is
for Glade today with slot numbers.

So primitives, printing, the natives and FFI read every block the same way,
and only the heap differs.

**Under reference counting nothing changes.** LLVM with RC is Silo exactly as
it is: the same header, odd words, size classes, lazy freeing, cycle
collection. It is the baseline the rest is measured against.

**One layout for now, kept open** (Chris, 2026-10-22). The shared code reads
values through traits (`meadow_rt::world::World`, `meadow_rt::look::Look`,
and those that follow). It never reads a header directly. So a strategy that
needs a layout of its own can have one. What would then be per layout is the
LIR lowering's description of a block (header size, where the length and
kind are, how a field is addressed), and converting values that cross
between two layouts in a compact region or a parcel.

### Debugging native code

The interpreter remains the debugger's engine: debug builds use it, and it
loses nothing. For native code, a build with debug information emits:
- DWARF line tables;
- frame records at every non-tail call, under RC as well as tracing.

That gives backtraces and locals in the debugger at a cost the debug build
chooses to pay.

## Phases

Each phase lands on its own, and is checked against the reference semantics
(the CEK machine, the Cut and AxCut interpreters) and the benchmarks.

1. **One runtime library, no behaviour change.** Move what both runtimes
   implement twice into one place, behind a small trait for each runtime's
   values:
   - the natives (Console, Fs, Process, Random, Time, Log, Net, FFI);
   - printing;
   - the primitives' semantics;
   - STM's bookkeeping;
   - the scheduler's policy.

   Done when each exists once and the standard library's tests pass on both
   runtimes. Started: the natives, in `meadow_rt::world`.
2. **The memory pass.** `meadow_llvm::linear` becomes an AxCut → AxCut-M
   pass, which `meadow-llvm` consumes. Done when the LLVM IR for the
   benchmarks and the standard library is the same as before, byte for byte.
3. **The LIR.** `emit` splits into lowering and printing. Done when the
   output is unchanged again.
4. **Tracing on the LLVM path.** On the shared object model, using Glade's
   nursery and Immix code:
   - a tracing heap in the runtime;
   - frame records, barriers and inline allocation in the LIR lowering.

   Done when the standard library's tests pass with `--gc tracing` on the
   LLVM path, and both strategies are benchmarked there.
5. **The interpreter on the shared runtime.**
   - Bytecode from AxCut-M, run on the native stack with segments, under both
     strategies.
   - The debugger and the REPL ported.

   Done when everything Glade does today, the debugger included, works on it,
   and debug builds are no slower to make.
6. **One runtime.** Glade's chunk stack, its own heap layout and the old
   `silo/` and `glade/` crates go. What remains is one runtime crate and two
   engines, with the JIT reattached through the LIR.

## Status (2026-10-22, branch `runtime-merge`)

**Phase 1, started.** Two things that both runtimes implemented now exist once:
- what an unhandled effect operation does, in `meadow_rt::world`;
- how a value is shown, in `meadow_rt::look`.

Each runtime says only what shape a value of its own has.

**Phase 2, done.** `linear`, the memory pass, lives in `meadow-axcut` beside
AxCut. `meadow-llvm` re-exports it, and so does every engine that counts
references.

**Phase 4, done in a simpler form than planned.** `--gc tracing` on the LLVM
path:
- emits no counting, no reuse and no cycle collection;
- polls at definition and method entries, recording the parameters on the
  shadow chain;
- collects with a non-moving mark-sweep over size-class pages
  (`silo/src/trace.rs`).

Nothing moves, so no barriers are needed and the layout is Silo's unchanged.
The roots are the shadow records, which are already made for effects, the
thread's context, and suspended segments.

The heap grows to three times what the last collection kept before it
collects again. The standard library's 350 tests pass under it, release and
debug.

What is not done is the plan's generational collector: a nursery, and
barriers at the few stores into existing objects. Tracing pays for its
absence on programs that build something large:
- rbtree is reuse's best case, which tracing gives up;
- binarytrees is a nursery's best case, which this collector does not have.

Programs that build nothing large cost about what counting does.

**Phase 5, the engine, done; the debugger, not yet.** `meadow-silo-vm`
(`silo/vm`) is an interpreter on Silo's runtime, under both strategies:
`meadow run|build|test --runtime silo --interpret`.

- **What it runs.** `meadow` writes an image (`meadow_axcut::image`) that
  holds the program after the memory pass. The interpreter compiles each
  linear block once, at load, to code over numbered slots, with descriptors
  and primitives resolved. It is not a separate bytecode: AxCut-M is close
  enough to an instruction set that it is the interpreter's.
- **Calls.** A non-tail call is a native call of the interpreter's loop, and
  a tail call is a jump within it.
- **Continuations.** A continuation that only continues one call is not built
  as an object. Its code is compiled into the caller's slots after the call,
  as native code would place it.
- **Effects.** Effects run on Silo's stack segments.

Against Glade's bytecode interpreter (`meadow exec --backend vm`) on the
benchmarks, measured on one Windows machine:

| task | Glade VM | Silo VM |
|---|---|---|
| fib | 474 ms | 329 ms |
| binarytrees | 12.0 s | 8.7 s |
| matmul | 2.06 s | 1.96 s |
| rbtree | 56.9 s | 61.7 s |
| wordfreq | 33 ms | 40 ms |

Building MiniML takes:
- Glade's bytecode: 0.34 s;
- an image to interpret: 0.59 s;
- a native executable through LLVM: 4.4 s.

Every example prints the same as native Silo does. The standard library's
350 tests pass under both strategies.

Glade is unchanged. It is still the engine for the debugger, the REPL and
introspection. Porting those to the new interpreter is what phase 5 has
left, along with frame records that a debugger can read.

### Every feature an option (Chris, 2026-10-22)

The aim is not to choose between the runtimes' features. What either had
becomes an option on both engines. AxCut, after the memory pass, is the
interface:
- run as it is, by the interpreter, as a bytecode would be;
- or lowered, by the LLVM back end.

How it is run is a configuration the program carries
(`meadow_axcut::config::Config`), which both engines read. So far it has two
choices:

| choice | options | default |
|---|---|---|
| memory | `--gc rc` (counting, with reuse) or `tracing` | rc |
| stack | `--stack fixed` (Silo's) or `growable` (Glade's) | growable in a debug build, fixed in a release one; `stack = "…"` in a profile |

`--gc` is one option for both runtimes. Glade's names for its two collectors,
`generational` and `copying`, choose tracing on Silo.

**Growable stacks** (`silo/src/stack.rs`):
- Every stack the runtime runs code on has a limit: `main`'s, a thread's, and
  a handler's segment.
- Where a function begins, a growable program compares the stack pointer
  with that limit.
  - In native code this is two loads and a compare, on a cold branch.
  - In the interpreter it is the same check, where each native call of its
    loop begins.
- Past the limit, the function runs on a new 64 MB piece of stack and comes
  back when it returns. Pieces are kept for reuse.
- A native function gets there through a small trampoline, emitted beside
  it, that calls it again with its arguments from a buffer.
- The limit is the thread's, kept in its context. A segment takes its own
  back each time it is resumed, so continuations captured deep and resumed
  later work across pieces, as do abandoned ones.

What it costs and allows, on this machine:
- A recursion 100 million deep runs in 5.9 s native.
- 20 million levels, inside a spawned thread and inside handlers whose
  continuations are resumed or abandoned, pass on both engines and under both
  strategies.
- With a fixed stack the same programs stop.
- The check costs nothing measurable on fib, and 30% on a recursion that does
  nothing but call.

One thing is lost: a piece in use by a continuation that is abandoned, rather
than finished, is not given back.

## Open questions

- **Interpreter speed on the native stack.** It was a question, and it is
  answered: see the status above. A recursive interpreter matches Glade's,
  because the continuation of a call is code after the call, not a frame
  rebuilt. The Rust frame around each call costs less than the register
  file Glade rebuilds.
- **The cost of frame records under tracing on LLVM.** It is a few stores
  per non-tail call. Measure it against reference counting on the same
  benchmarks.
- **A barrier with reuse.** Reuse writes into a block in place, which under
  tracing is a store into a possibly old object. Either reuse is RC only, or
  it takes the barrier.
- **Glade's block JIT.** It relies on the interpreter's state being exact at
  every instruction, which phase 5 removes. It keeps running until then. A
  function-at-a-time JIT on the LIR replaces it, or the LLVM path does.
- **Compact regions and parcels.** Both runtimes have them, with different
  addresses. They move to the shared object model with phase 4.
- **The CEK machine's natives.** `eval/` has a third copy of the natives. It
  can use `meadow_rt::world` as well, but it is the reference semantics, so
  it moves last and on purpose.

## Precedents

- **Lean 4**: one reference-counting IR with `inc`, `dec`, `reset` and
  `reuse`, both interpreted and compiled to C, and the two mix.
- **GHC**: STG lowers to Cmm, then to its own code generator or LLVM.
  GHCi's bytecode shares the stack and heap with compiled code.
- **OCaml**: bytecode and native code share the front end and the collector,
  but cannot be mixed at run time.
- **HotSpot and V8**: native code falls back to the interpreter at chosen
  points, through deoptimisation metadata, not at every instruction.
- **LLVM's `shadow-stack` GC strategy**, and Silo's own frame records.
