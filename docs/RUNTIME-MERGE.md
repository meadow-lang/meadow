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

**Its name is Glade** (Chris, 2026-10-24). The merged runtime is Glade's next
release. Silo is retired, and Glade gains everything Silo has, so that it
replaces Silo in full: native code through LLVM, counting with reuse, the
native stack and its segments, Silo's speed. Much of the merged runtime's
code is Silo's today (`silo/`, the `meadow-silo` crates, `--runtime silo`).
The names follow when the old Glade's parts are retired (phase 6): the
merged runtime takes Glade's name and directory, `--runtime glade` selects
it, configured along the four axes below, and `--runtime silo` stays for a
while as another way to say the same.

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

**The layout is a description now** (2026-10-24). `meadow_rt::layout`
describes a layout as data:
- the word that is nothing, and how a value with no block is encoded and
  told from an address;
- each header field as a word, a shift and a width: the count, the length,
  the kind, the element descriptor, the uniform bit, the colour, the mutable
  bit and `meta`;
- how descriptors are packed, and where fields begin;
- the region floor;
- the number of each kind.

`SILO` is its one instance. Silo's heap takes its constants and accessors
from it. The interpreter and the runtime's printing, arrays, big integers,
segments, parcels, regions and collectors read headers only through those
accessors. The LLVM back end writes its IR from the description too
(`Module::layout`), and asks of it only what that IR's shape assumes
(`suits`). The IR of nine programs, counted and traced, is byte for byte
what it was -- for `concurrency` in one build of four, since the order of
its method table varies from one build to the next, as it did before this.

**The second layout, Glade's** (Chris: "something like glade's", with more
of the layout to be configurable later). What sets it apart, and so what it
needs, in the order it can be built:
1. **No count in the header, and blocks that move.** (Begun 2026-10-24: a
   young generation on the VM, below.) A young generation
   copied as Glade's nursery is, and an old one evacuated from sparse blocks.
   The header gives the count's place to the collector: a mark and an age,
   and a forwarding address in the header of a block that has moved.
2. **Precise roots that are read back.** Everything that holds a reference
   across a safe point must hold it where the collector can rewrite it:
   - The interpreter's registers, which its activation records already
     reach.
   - The runtime's code, which may hold no address across an allocation.
     This is Glade's rule, `Vm::ensure`.
   - The native code's shadow records. Today they are written and never
     read back: every reference held across a call is to be reloaded from
     its record after it.
3. **Regions told by address range, not by count.** A region's blocks lie in
   memory of their own, as Glade's do above `REGION_BASE`.
4. **Values with no block kept.** Odd words survive moving as they are, so
   the Glade-like layout keeps Silo's encoding of them, which Glade itself
   does not have.

The interpreter comes first, since its roots are already precise. Native
code comes second: its frames need reloading after every safe point, which
costs a load per live reference across a call, so it is the strategy's
choice and not paid under counting.

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

### The end state: one runtime, four choices (Chris, 2026-10-23)

There is no "Glade for debug, Silo for release". There is one runtime,
configured per build along four axes. A profile picks the defaults:

| axis | options | what it means |
|---|---|---|
| engine | bytecode VM, or native | a register VM in Glade's manner (a pc, debugging, introspection, hot loading), or LLVM |
| memory | counting with reuse, tracing (later generational) | who frees a block |
| stack | native fixed, native growable, heap frames | where activations live: the native stack, as Silo's do, or frames on the heap, as Glade's |
| object representation | Silo's layout; others behind the same interface | how a block is laid out: its header, its fields, how a value is told apart from a block |

The first three exist today for the native engine. Of the last, Silo's
layout is the only one. "One layout for now, kept open" now means the layout
becomes a real choice. For the VM, everything that reads a block goes through
one interface (`meadow_rt::look` and `world` are the start). For native code,
the lowering takes the layout's description: header size, where the length
and kind are, and how a field is addressed.

**The second layout is Glade-like** (Chris, 2026-10-23), and more of the
layout may become configurable later. So the interface is a description of
a layout, which the runtime, the VM and the lowering all read, not two
layouts written in by hand. Silo's layout and a Glade-like one are its first
two instances. A description has to say:
- **References:** what a reference is (a machine pointer, or a slot number in
  a heap of slots).
- **Small values:** how a value with no block is told from one that has a
  block.
- **Headers:** what the header holds (count or mark, length, kind, meta), and
  where.
- **Fields:** how a field and its descriptor are found.
- **Collection:** whether blocks may move. A moving layout needs forwarding,
  and so the moving collector it was made for.

That last point ties the layout to the memory axis: a Glade-like layout
comes with a generational, moving collector on the shared runtime.

### The VM: a register machine with a pc (Chris, 2026-10-23)

Silo's interpreter becomes a register VM in Glade's manner, with Glade's
debugging and introspection, and hot loading, which Glade does not have.

**What Glade has, to match.**
- **Bytecode:** 53 opcodes of 8 bytes each (`compiler/meadow-bytecode`), over
  one flat file of 256 registers. There is no call or return instruction:
  every call is a tail call, and a non-tail call first pushes a continuation
  onto a frame stack on the heap.
- **Debug information:** one source location per instruction, which name is
  in which register at each pc, and each function's region. It is built only
  for debug builds, and never written into an image.
- **Debugger** (`meadow dap`): line breakpoints, step in, over and out,
  pause, stack traces, scopes, locals, and `evaluate` of a name in scope. A
  frame is told by its return continuation; stepping is by line.
- **Introspection:**
  - `meadow dis`;
  - coverage (`Process.covered`);
  - a sampling profiler (`--profile-to`);
  - GC statistics.

  There is no stack trace on failure outside the debugger.
- **No hot loading.** The REPL compiles every line as a new whole program and
  runs it on a fresh VM. A redefinition shadows the old one.

**The design.**
- **Code.** Each linear block compiles to a run of fixed-size instructions in
  one array, addressed by a pc.
  - Registers are the block's slots, in a window per activation.
  - Lists of arguments and a switch's arms are in side tables, so every
    instruction is the same size.
  - What the current interpreter learned stays: no frame is made for a call
    whose frame only continues it, inline arithmetic and branches, and
    descriptors resolved at load.
- **Debug information.** The memory pass keeps AxCut's `Mark`s, which become a
  line table, one location per pc. A register-name table says which name each
  register holds. Both come from what a debug build already marks.
- **Frames.** One interface, two homes:
  - the native stack, as now;
  - heap frames, as Glade's, where a frame is an object the debugger and hot
    loading can walk without the native stack's help.
- **Debugger.** The VM runs in its own process. `meadow dap` drives it over a
  debug channel, and the protocol code moves to a crate both share. The
  session logic (breakpoints to pcs, stepping by frame, stack traces, locals)
  is written once against the VM's frames, and reads values through
  `meadow_rt::look`, not a heap layout.
- **Hot loading** (Chris, 2026-10-23). New, since Glade has none. Names have
  versions:
  - A reload adds a version of each definition it changes.
  - Newly loaded code is used only by new invocations. A call by name
    resolves to the newest version at the moment it is made.
  - Nothing already running changes. An activation finishes in the version it
    began in. A closure or continuation already made keeps its version, since
    its captures are laid out for that version's code.
  - A top-level value is computed again by the new code. The design said
    "kept unless its definition changed"; see "Hot loading, as built" for
    why it is not.
- **Order of work:**
  1. the register VM on the native stack, at least as fast as the tree it
     replaces;
  2. the line table and `dis`;
  3. the debugger;
  4. heap frames;
  5. hot loading.

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
| memory | `--gc rc` (counting, with reuse), `tracing`, or `generational` (with a nursery) | rc |
| stack | `--stack fixed` (Silo's) or `growable` (Glade's) | growable in a debug build, fixed in a release one; `stack = "…"` in a profile |

`--gc` is one option for both runtimes. Glade's names for its two collectors,
`generational` and `copying`, choose tracing on Silo.

**Heap frames** (`--stack heap`). The front end marks which continuations
are *frames*: made for one non-tail call, and entered once. Those, and only
those, live on the native stack:
- the memory pass lets them borrow;
- both engines turn their call into a native one.

With no name a frame, every continuation is an ordinary closure on the heap,
and every call is a tail call. This is Glade's model, on both engines, with
no mechanism of its own: the frame set is emptied before the memory pass
(`Config::without_frames`).

The debugger follows return continuations through the heap, as Glade's does,
and tells a frame in a step by the continuation it returns to. On this
machine:
- A recursion 10 million deep runs in 0.74 s on the VM, against 8.4 s with a
  growable native stack, and in 0.39 s native.
- Each non-tail call allocates. Fib takes 563 ms on the VM, against 345 with
  a native stack and 474 on Glade's VM; native fib takes 88 ms, against 49.
- Loops cost nothing more: matmul is unchanged.
- Glade puts its frames on a stack of chunks rather than the general heap,
  which is cheaper. Doing the same is a later step.

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

### Hot loading, as built (2026-10-23)

`meadow run --watch` runs a program on the VM. Whenever the package's
sources change, it builds the package again and loads the new build into the
program while it runs.
- The VM does the loading: `meadow-silo-vm --watch IMAGE` reloads the image
  whenever it is written anew.
- `meadow` writes an image whole, by renaming it into place, so the VM never
  reads half of one.
- Each reload prints on stderr what it replaced, or why it was refused.
- A build that fails to compile leaves the program running as it was.

**Segments.** Each load's code is a segment of its own: its procedures, its
instructions and its method table.
- A procedure's or a method's number carries its segment in its top bits.
  So the header of a closure, or the return of a frame, says which version it
  belongs to.
- Nothing in a segment changes once it is in place.
- A program takes up to 1,024 loads.

**Slots.** A call by name goes through a slot, one per definition. This
covers a jump, a direct call and the program's entry. Each reads its slot's
procedure when it is made.
- A reload points each slot it has a new version for at that version, with a
  release store.
- A call already under way runs the code it began in. So does a closure or a
  continuation already made.
- Whatever any of them calls by name next is the newest version.

The first load numbers its slots as its procedures, so a program that is
never reloaded runs as before: fib takes 337 ms and matmul 2.12 s, against
333 ms and 2.11 s before the change.

**What a definition is.** A slot is found by a key with four parts:
- the module;
- the name;
- the type, as the type checker gave it, written the same in every build
  (`meadow_core::signature`, carried in AxCut's `Program::types`), with the
  version of every type and effect in it;
- how its parameters and its answer are represented.

A definition whose type changed is therefore a new definition. Old code that
calls it by name still reaches the old version, with arguments laid out as
that version expects.

**Versions of types** (Chris, 2026-10-24). A type's version is a hash of its
definition: its constructors, their fields' types, and the versions of the
types those mention, so that `Scene`, holding a `List Shape`, is new when
`Shape` is (`meadow_core::versions`). A type the new build changed is a new
type, and everything whose type mentions it is a new definition, even where
its text did not change. So a value of a changed type never reaches code
written for the other version:
- old code holding an old `Shape` calls the old `area`, and new code holding
  a new one calls the new;
- code whose type does not mention `Shape` is shared, and builds and reads
  only the new;
- generic code passes a `Shape` through without looking into it, and a
  dictionary for `Show Shape` is a definition with a type like any other.

A constructor of a changed type gets a tag of its own, so old and new values
are told apart, and each prints with its name.

**Versions of effects.** A handler is found by its operation's name, not by a
call whose type says anything, so an effect has a version too: the versions
of the types its handlers and its `perform`s carry. An effect whose
declaration mentions a type mentions it at every handler and `perform` of
it, whatever its parameters are instantiated at, so when that type changes
the effect does, and the evidence key is new: an old handler is not what a
new `perform` finds, which fails as unhandled rather than misreading a
value. What an effect's parameters are instantiated at is in the effect rows
of the functions that perform it, which the signature writes with versions.

A loop whose own parameters are of a changed type stays on its version,
since it calls itself with old values: it takes new code when it is started
again.

Some definitions have no key, so each build has its own:
- **What no person wrote**, such as join points.
- **Local functions the compiler lifted** out of the definition they were
  written in (`meadow_core::lift::is_lifted`). Such a function is part of
  that definition's code, so a closure made of it keeps its version.
- **Definitions alike in all four parts.**

**Constructors.** Tags are renumbered to the running program's by what the
constructor is: its name and its type's version (`Program::ctor_types`).
New constructors, and those of changed types, get new tags. A reload is
refused, and nothing changes, when:
- a constructor the runtime builds by name changed: a role's (`Maybe`,
  `List`, a result) or one of `meadow_rt::roles::RECORDS`, since the runtime
  could not tell which build's it meant;
- the built-in roles are played by other constructors;
- it was built to manage memory or the stack another way;
- it starts threads, or detaches continuations, and the running program was
  not set up for that.

**Tables.** The runtime's names tables are replaced whole
(`show::reinstall`), and each new table extends the one before. Labels the
runtime numbered for itself become the program's, at the numbers they
already had.

**Top-level values.** A reload's caches are numbered after the running
program's. So the new code computes each value again the first time it reads
it, and the old code goes on reading its own. Keeping the values whose
definitions did not change would mean knowing what each value depends on.
Values are pure, so computing them again only costs time.

**Opting out** (Chris, 2026-10-24): `--no-reload`, `reload = false` in a
profile, or `MEADOW_RELOAD=0` builds what is interpreted with nothing
versioned and no types written down, and the VM refuses to reload into it or
from it. What it saves is at build time: MiniML's image is 8% smaller (713 KB
against 778 KB) and builds about 1% sooner. A call by name costs the same
either way: reading a definition's slot measured nothing on fib (325 ms) or
matmul (2.11 s against 2.12 s with calls made direct). A native build is never
versioned, so its IR is what it was.

**Not yet:**
- The debugger knows only the first load's code, and does not stop in
  reloaded code.
- A watching run only reloads. It does not offer a restart when a reload is
  refused.

Tests: `buildtools/meadow/tests/hot_reload.rs`, among them a changed type
with old and new values alive at once, and a build that opted out.

### A young generation (2026-10-24)

`--gc generational` makes small blocks in a nursery of each heap's own, from
the front. When it fills, the next safe point copies what is still reached
out of it into the pages the tracing collector keeps (`silo/src/trace.rs`,
`minor`), and a full collection empties it first and then marks and sweeps
as before. Under tracing nothing counts, so a block's count is free to say
that it moved, and the header word its kind was in says where.

Blocks move, so everything that points at the nursery must be where the copy
can rewrite it:
- the records on every chain;
- the context's top-level values, literals and kept values;
- a transaction's writes;
- the older blocks a young one was stored into.

A store through `heap::set_field`, or into a mutable array, notes the block
stored into (`trace::remember`, the write barrier). A block made while the
nursery is full counts as one, since it may be given young fields as it is
built.

The VM reads back what its records held, after a safe point and after a call
it waited on, and records a frame's borrowed values too. So does native code
under `generational`, and only there:
- a function's poll is followed by loads of what its record held, joined by
  a phi;
- a method's record holds the captures it loaded as well;
- a frame that waits on a call is entered with what its record holds after
  the call;
- a `Ref` or a hole stored into inline calls `meadow_remember`.

Counted and traced programs' IR is as it was.

Every example prints the same with the smallest nursery (32K words), and the
standard library's 350 tests pass under `generational` on both engines --
the default nursery and the smallest, debug and release, and heap frames --
as under counting and tracing (Linux, 2026-10-24, at 09a91d0, with the full
suite). Natively, binarytrees takes 1.13 s against 1.57 s
traced, and 0.91 s with an 8 MB nursery (`--nursery 8M`). On the VM, binarytrees spends 7.7% of its run collecting, against 10.6% traced. The
run itself is about the same, since interpreting it costs most. A larger
nursery (`MEADOW_GC_NURSERY`, in words) spends less: 4.8% at 16M words.

### The second layout (2026-10-25)

`--layout glade` lays blocks out as Glade does, as near as machine pointers
allow (`meadow_rt::layout::GLADE`).
- **The header.** It has no count, so word 0 holds the kind, the uniform
  descriptor, the cycle collector's bits, the collector's mark and the
  length. Word 1 holds the `meta` and the first eight fields' descriptors.
  A block of up to eight fields has two words of header, where Silo's layout
  gives it three: a cons cell is four words rather than five.
- **Memory.** It is traced only: counting has nothing to count, and
  `--layout glade --gc rc` is refused.

The description grew what a collector keeps, so that one runtime serves
both:
- **the mark**: Silo's count, or a field of its own;
- **a region's blocks**, told by a mark no collection reaches: Silo's sticky
  count, or the mark's top value;
- **a moved block**: Silo's count of `u32::MAX - 1`, or Glade's kind `0xFF`;
- **a free block's link word**;
- **descriptors kept in a header word.**

Everything that read Silo's numbers now reads these instead:
- the heap's accessors and its builders (`heap::build` had its offsets
  written in);
- the collectors' mark, sweep, copy and walk;
- copies into a region, another heap or a record updated by copy
  (`heap::copy_header`);
- the VM's `New`.

The runtime and the VM are built for one layout, by the `glade-layout`
feature, so that reading a header costs no more than it did. A library built
for it exports a symbol of its own (`meadow_silo_{hash}_glade`), so a program
links only with the library made for its layout. The VM refuses an image of
the other layout. The image's configuration carries the layout, and so its
format is version 5.

The LLVM back end takes the layout from the configuration. Each place it
assumed Silo's shape now asks the description:
- a field is read with a shift only if it is not at the bottom of its word,
  and a mask only if it is not at the top;
- a block's header and descriptors are written word by word, in Silo's
  order;
- a closure's method index is or-ed in with the descriptors that share its
  word;
- `setRef`'s descriptor goes where `desc_at` says;
- `setField`, the hole tail recursion modulo cons fills, finds its field
  past only the descriptors the header does not hold.

That last was the one bug the work found: lists came back with their first
element only. `suits` asks Silo's shape only of a layout that counts.

**A bug, found on the way.** Under tracing, `@mw.field` (a field read by
number, or a record's by label) shared the value it read. That added to a
count which, under Silo's layout, is the collector's mark. A block marked by
one collection and read once before the next looked marked by it, so its
fields were never scanned, and could be freed while it was live. A traced
program now has no counting helpers at all. Its IR differs from before only
in what was removed. Counted IR is the same byte for byte, for the nine
programs that are checked (`concurrency` up to its method table's order).

**Checked** (Windows, 2026-10-25):
- **The examples.** All six print the same, on both engines, under tracing
  and generational, with the smallest nursery, as under counting with Silo's
  layout.
- **Std.** All 354 tests pass under generational with the smallest nursery,
  on the VM and natively. The 354 are Std's 350 and the test package's own
  four.

**Measured** natively, best of five:

| benchmark | strategy | silo | glade |
|---|---|---|---|
| binarytrees | tracing | 1600 ms | 1495 ms |
| binarytrees | generational | 1154 ms | 1100 ms |
| rbtree | tracing | 10343 ms | 9727 ms |
| rbtree | generational | 2033 ms | 2157 ms |

rbtree under generational is the exception:
- it runs fewer minor collections with the smaller blocks (4,872 against
  5,577), but spends longer in them (870 ms against 803);
- that leaves about 50 ms of the gap in the program's own code. One
  explanation is the mask the bottom-of-word `meta` now needs at every
  `switch`; another is code layout, which has moved these benchmarks by as
  much before.

It is yet to be profiled.

### A JIT: the design (2026-10-25)

The engine becomes a choice of three, as Chris asked ("I want aot and jit to
be options. the idea is maximum configurability"):
- `interpret`, the VM as it is;
- `jit`, the VM compiling what runs often;
- `aot`, LLVM ahead of time.

`docs/RUNTIME-SPEC.md` has the requirements (JIT-1 to JIT-10). This is how
the first tier meets them.

**Where it starts: the VM's own registers.** A procedure the VM runs keeps
its slots in an `Env`, a vector of `V`s (a word, a frame not yet built, or
nothing) in the native frame of the call that runs it. Tier 1 compiles a
procedure to a native function over that same `Env`, so that compiled and
interpreted code see one state:
- `V` gets a fixed layout (`repr(u64)`: a tag word, then the word or the
  frame's pointer), which compiled code reads and writes.
- Any instruction can be left to the interpreter. One that tier 1 does not
  compile becomes a call to `step`, which runs that one instruction as the
  interpreter's loop does and answers where to go next: another pc of the
  procedure, which compiled code jumps to through a table of its labels, or
  a transfer out of it (a `Jump` or `Invoke` to another procedure, a return).
  That is JIT-2's "an instruction it does not compile is run by the
  interpreter, and the native code carries on", and it lets tier 1 begin
  with very little compiled and grow an instruction at a time, checked
  against the interpreter at each step.
- What leaves a procedure goes back to the driver loop (`run`), which enters
  the next procedure compiled if it is, and interpreted if not. Calls by name
  go through the slots hot loading uses, so a reload replaces compiled code
  as it replaces interpreted code (JIT-4): compiled code is per segment and
  procedure, and a new segment's procedures start interpreted.

**What is compiled first:**
- the arithmetic and comparisons (`Bin`, `BinK`, `BinI`, `IfBin`,
  `IfBinI`), on words, with the tag checked inline and `step` taking any
  other case;
- branches and jumps within the procedure, so that a loop is native;
- moves (`Rename`).

The rest calls `step`, through which the procedure stays whole. A `Switch` on
a tag, allocation, and calls come next, measured one at a time.

**Memory.** Compiled code keeps no value in a machine register across
anything that may collect: every helper it calls may, and it reads its
operands from `Env` again after each one. So the VM's records, which already
reach `Env`, stay all the roots there are, and the young generation needs
nothing new (JIT-7).

**When.** A procedure is compiled once it has been entered `--jit-threshold`
times (1000 by default; 0 compiles each one when it is first entered), or
once a backward jump in it has been taken as often. A loop entered once is
compiled at its head, and the running activation carries on in the compiled
code at that pc (JIT-6), which the label table makes a lookup.

**Not compiled.** While a debugger, a profile or coverage records, the VM
runs the loop that records, and compiled code is not entered: a procedure
goes back to the interpreter at its next entry (JIT-5).

**The machine code.**
- Executable memory is Glade's JIT's `Arena`, moved into a crate both use
  (`compiler/meadow-exec`): never writable and executable at once, `MAP_JIT`
  on macOS, two views of one mapping elsewhere.
- The instructions are x86-64 first, then AArch64. Glade's encoders did not
  fit, being written for its own machine state (pinned registers, its
  "thin" steps), so tier 1 has a small encoder of its own
  (`silo/vm/src/interp/jit/x64.rs`).

**Tier 2** is the open question in the spec (§19): LLVM's ORC or Cranelift,
from the lower-level IR (R3). Tier 1 does not wait for it.

**Built: the first step** (2026-10-25). `MEADOW_JIT=1` on the VM, and
`MEADOW_JIT_THRESHOLD`:
- **What is compiled.** Every procedure is compiled once entered often
  enough. `Bin`, `BinK`, `BinI`, `IfBin` and `IfBinI` are compiled inline,
  for the integer operations and comparisons; everything else calls `step`.
- **One source for the semantics.** `step` and the interpreter's loop are
  one macro (`instruction!`), each giving it its own way of going on, so
  that what the interpreter does is written once. A first try, the loop
  calling `step` as a function, cost fib 15% even with `#[inline(always)]`,
  and was taken back.
- **The interpreter pays nothing for it.** `exec` and `call` carry whether
  the JIT is on as a constant, so the interpreter without the JIT calls only
  code without it. Measured with functions and blocks aligned, fib takes
  335 ms against 336 to 342 ms before, and matmul 2,127 ms against 2,144 to
  2,160 ms. Unaligned, fib reads 3% slower, which is code layout.
- **Checked.** With everything compiled at first entry, the six examples
  print as without it, under counting, tracing and `generational` with the
  smallest nursery; Std's tests pass under counting and `generational`
  (Windows).
- **Not faster yet.** fib takes 445 ms, against 355 ms interpreted, since a
  call goes through `step` and then the driver, and the arithmetic it saves
  is little. matmul is level. Calls and returns compiled natively come next,
  and they are what will make it pay.

**The second step: tail calls stay native** (2026-10-25).
- **Tail calls.** A `Jump` or `Invoke` is made by a helper, `transfer`. When
  the procedure it enters is compiled, the function jumps into that
  procedure's body, past the prologue every function shares, with the same
  context and native frame. A loop of tail calls through compiled
  procedures never goes back to the driver, and takes no native stack.
- **Doubles.** Floating-point arithmetic is inline (`FAdd`, `FSub`, `FMul`,
  `FDiv`).
- **Measured.** matmul takes 2,047 ms against 2,132 ms interpreted; fib takes
  406 ms against 346 ms.
- **Checked.** The examples' differential holds, Std's tests pass under all
  three strategies, and a recursion 20 million deep finishes on the growable
  stack and on heap frames, everything compiled at first entry.
- **What is left.** The rest goes through the interpreter's `step` still:
  the array primitives, calls, switches, sharing and erasing.

**The third step: arrays and returns** (2026-10-25).
- **Array elements.** `stGetArray` and `stSetArray` on a mutable array whose
  elements are no references are inline. The fast path checks that the
  value is a block, that it is a mutable array, its elements' descriptor,
  and the bounds, as an unsigned compare. It reads the header through the
  layout's fields, so it serves Glade's layout too. Anything else, an index
  out of bounds included, is the interpreter's, which fails with the same
  message.
- **Returns.** An answer to the null continuation, which is how a call
  returns, is inline too.
- **Measured.** matmul takes 904 ms with the JIT, against 2,107 ms
  interpreted: 2.3 times as fast. fib is level, at 355 ms against 346 ms.
- **Checked.** The differential, Std under all three strategies, the deep
  recursions, and an index out of bounds: the same message and exit status
  with the JIT as without.

**The fourth step: counting, and what not to compile** (2026-10-25).
- **Counting inline.** Under counting, sharing and erasing a reference are
  inline: a count up or down, and the cycle collector's candidate tested as
  the emitted code's helper tests it. The last reference and a region's
  block are the interpreter's. With everything compiled, a leak check of
  `tour` reports what it reports interpreted: 623 blocks acquired, none
  live at exit.
- **What not to compile.** binarytrees and rbtree ran 17% and 16% slower
  with the JIT. Most of their instructions (allocation, switches, calls) go
  through `step`, and through a call each costs more than in the
  interpreter's own loop. So a procedure is compiled only if at most 30% of
  its instructions would be left to `step` (`MEADOW_JIT_STEPPED`).
- **The driver's overhead.** The driver's lookup at each procedure entry
  went from a dynamic call to a flag, and its count from a locked increment
  to a load and a store.

Measured, best of two, on the Windows desktop:

| program | interpreted | with the JIT |
|---|---|---|
| fib | 340 ms | 355 ms |
| matmul | 2,097 ms | 912 ms |
| binarytrees | 9,269 ms | 9,501 ms |
| rbtree | 58,263 ms | 60,295 ms |

What is left of the JIT's cost on code it does not compile is the lookup
at each procedure entry, 2 to 4%. Compiling the switches, allocation and
calls is what will turn that around.

**The fifth step: switches, allocation, moves and calls** (2026-10-27).
`MEADOW_JIT_LOG=steps` counts, by instruction, what compiled code leaves to
`step`. With everything compiled, binarytrees left 134 million calls, 101
million renamings, 67 million switches and 67 million `Let`s; rbtree left
714 million renamings, 636 million switches, 414 million `Let`s, 168
million literals and 109 million closures. Now inline:
- **Renamings**, as a parallel move through a scratch area in the context.
  A word is copied and a frame not built is moved, as `take_val` does.
- **Literals** that are words.
- **Switches.** The tag is an immediate's payload or a block's `meta`. The
  arm's fields go into their slots, and the value is released: the last
  reference's block is cleaned or kept as the arm's reuse token, and any
  other's count goes down and its fields are shared. An arm of no fields
  erases the value, as `release` does, since it is mostly no block. The
  interpreter takes a block in a region, a scrutinee that is a frame not
  built, and a destination holding one. Each of those is ruled out before
  anything changes but slots written with what the interpreter would write
  in them again, so its slow path is to run the whole instruction again.
- **`Let` and closures.** When the descriptors are known, the header is
  computed when compiling. The block is a reuse token's, or a new one from
  `heap::acquire`, which never collects (a collection only becomes due),
  so nothing read before it moves.
- **Branches** on a word's truth, and on a comparison done inline.
- **Division and remainder**, with nought the interpreter's (which fails
  with the same message) and -1 a negation or nought, where the machine
  would trap on the least integer.
- **The wide forms** of the arithmetic, the compares, sharing and erasing.
- **Calls by name**, through a helper that enters the callee's compiled code
  at once, rather than going through `step`, `call`, `exec` and `run`.

`MEADOW_JIT_SKIP=switch,let,...` leaves the named instructions to `step`,
which is how a fault in one of them is found: rbtree's crash was the arms of
no fields, read as blocks.

**JIT-5 for coverage.** Std's steered fuzzer failed with everything compiled,
an older fault that compiling more brought out: it turns coverage on as it
runs, and a loop of compiled tail calls never went back to the loop that
records. Now an entry in the JIT's loop, and a tail call made by compiled
code, look whether records have begun, and if so go to the loop that keeps
them.

**Loops.** Control never joins inside a procedure, so a loop is a tail call
to its own head and its passes are entries. The entry threshold is the loop
threshold, and the pass that crosses it goes on in compiled code (JIT-1,
JIT-6). `--jit-threshold N` sets it.

Measured with the machine loaded by another build, best of five:

| program | interpreted | with the JIT |
|---|---|---|
| fib | 373 ms | 398 ms |
| matmul | 2,113 ms | 909 ms |
| binarytrees | 9,762 ms | 8,080 ms |
| rbtree | 60,448 ms | 46,561 ms |

fib's calls still cost more through the helper than in the interpreter's
loop: each makes an `Env`, and the helper, the driver and the compiled
function's prologue each cost a few instructions the interpreter does not
pay. That is what tier 2 is for: a call there is a native call.

### The LIR (2026-10-27)

R3's lower-level IR is `compiler/meadow-lir`. It is LLVM's instructions as far
as Meadow's code uses them -- integer and floating arithmetic, compares,
casts, loads and stores at offsets, calls and tail calls, branches,
switches and phis -- on a few scalar types, with each operand written as
LLVM writes it (`%t3`, a number, `@name`). A module is functions,
declarations, globals with what they hold, and notes, which are what only
LLVM reads (comments, branch weights).

**How it was made without changing a byte.** The back end's functions were
LLVM text, one `format!` per instruction. They became LIR in three steps,
each checked against a snapshot of the IR of nine programs in four
configurations (counted, traced, generational, Glade's layout: 432
modules):
1. **Raw lines.** Bodies held the instructions as raw lines of text, printed
   as before.
2. **Typed instructions.** A converter read each `format!`, matched its text
   against the LIR's forms, and rewrote the call as the typed instruction:
   246 of 253. The other seven were rewritten by hand: calls whose
   arguments were a joined string, the `switch`, and the spill area's two
   lines.
3. **Modules.** The back end's product became LIR modules, which it prints.
   The fixed parts -- the runtime's declarations, the helpers that share and
   erase, the tables, the ways in -- are written as LLVM text, which is the
   clearest way to write fixed code, and read back by `meadow_lir::parse`.

The parser is the printer's inverse, and a test holds them to it: every one
of the snapshot's 81,737 functions, and every module whole, reads back and
prints as it was written.

**What it is for.** The lowering is written once, for every back end that
makes machine code (LIR-4): LLVM prints it, and the JIT's second tier
compiles it.

### Tier 2: the definition, as an executable has it (2026-10-27)

**Design.** The second tier compiles what runs often the way an executable
is compiled. A definition entered `MEADOW_JIT_TIER2` (10,000) times more
than tier 1's threshold is lowered by the LLVM back end's own lowering,
with its methods and frames, to the LIR, and Cranelift compiles that in the
VM's process (`silo/vm/src/interp/tier2.rs`). Its code is an executable's:
- frames are the native stack, a tail call is a tail call (Cranelift's
  `tail` convention), and values are in registers;
- what the layout and the strategy decide is what the LIR says;
- the records a collection needs are an executable's, on the shadow chain.

**What differs from an executable.** The lowering has a JIT mode
(`meadow_llvm::emit::Jit`), which numbers what the VM numbers as the VM did
when it loaded the program:
- a closure's methods, by the address of the `new` that makes them, which
  is how the VM keyed them;
- tags, after a reload's renumbering;
- interned names, and a reload's caches.

So a closure compiled code builds is one the interpreter can call, and the
other way. A method's function is asked of the VM (`@meadow_jit_method`),
since the VM's method numbers are by segment.

**Calls between the two.**
- *A call by name* goes through a table by the definition's slot, which a
  reload clears (JIT-4).
- *A callee not compiled* is the VM's. A call that waits for its answer asks
  the VM, which runs the callee on the native stack, as an interpreted call
  does.
- *A tail call cannot do that*, or a loop between compiled and interpreted
  code would take stack at each turn. It leaves the call pending on its
  thread and answers a sentinel, which travels back up the tail calls to
  whoever waits: compiled code that called and waits, which asks the VM to
  run what is pending, or the VM's loop, which runs it as its next
  procedure.

A real answer equal to the sentinel is told from it by nothing being
pending.

**Faults found on the way.**
- *Dangling blocks.* The lowering wrote a method's block from a copy, so a
  `new` inside one was at another address than the VM's. It now refers to
  the program's blocks, and keeps the few of its own making -- a segment
  primitive's packed code -- until the definition is done. Before that,
  what was queued from inside one pointed into freed memory, which showed
  as names out of nowhere and crashes.
- *Frame pointers.* Cranelift's tail calls need them on x86-64.
- *Addresses out of reach.* The JIT's memory can be further than a 32-bit
  displacement from the executable's code, so addresses are loaded through
  a table (`is_pic`), as cranelift-jit asks on x86-64.

**Measured.** With the machine loaded by another build, best of five:

| program | interpreted | tier 1 | tier 2 |
|---|---|---|---|
| fib | 357 ms | 427 ms | 61 ms |
| matmul | 2,113 ms | 909 ms | 228 ms |
| binarytrees | 9,762 ms | 8,080 ms | 932 ms |
| rbtree | 60,448 ms | 46,561 ms | 4,810 ms |

These are a whole process's times, its start and its compiling included.

### Heap frames on the VM's own stack (2026-10-28)

**Before.** Under `--stack heap`, the VM's image dropped the program's
frames, so every continuation was a closure: built on the heap at each
non-tail call, and released when it was entered. fib took 563 ms against
345 ms on the growable stack.

**Now.** The image keeps the frames, and the VM runs them on a stack of its
own (HF-1).
- **A call that waits** does not call natively. The caller's resumption goes
  on the loop's stack, and the loop goes on in the callee. A resumption is
  the rest of the procedure that called -- its procedure, the pc after the
  call, its slots, where the answer goes -- or a frame not built that was
  handed the call as its continuation.
- **A return** to the null continuation goes on with the last resumption,
  and leaves the loop only when there is none.
- **A frame used as a value** is built on the heap, as on the other stacks.

**Making it cheap took three steps:**
1. The callee's slots are boxed from a pool, so that neither side's are
   copied or cleared.
2. The records a frame keeps on the shadow chain -- a program that detaches
   continuations keeps them for every frame -- are in chunks that do not
   move, so a call allocates nothing. Boxing each record was a `malloc` and
   a `free` a call, which `perf` in WSL showed as a quarter of the time.
3. The two hooks the loop gives `instruction!`, for a call that waits with a
   frame and for one that holds slots, are where the stack of frames comes
   in, so nothing runs before the other instructions.

fib now takes 356 ms against 352 ms (HF-2).

**Records begun.** When a debugger, a profile or coverage begins, the loop
that keeps no records hands its stack of frames to the loop that keeps
them, at the next procedure entry. Std's steered fuzzer is what found that
it must.

**The interpreter's own regression, found on the way.** fib on the VM had
gone from 345 ms to 398 ms over the JIT's work. The JIT does not run there:
the loop's code had changed with the code around it. Passing a call's slots
to `exec` and `run` by reference rather than by value -- 264 bytes copied
twice a call -- brings it back to 348 ms, level with before, with matmul and
binarytrees level too.

### The debugger and reloading together (2026-10-28)

**Before.** The debugger knew only the first load's code. Code a reload
brought ran past every breakpoint, and a step through it saw nothing.
`meadow dap` did not reload at all.

**Now.** Each load is a segment with its own pcs and procedures, and the
debugger is told of each (DBG-9).
- **The VM** keeps boundaries, line keys and breakpoints per segment. A frame
  names its segment, with its procedure and pc numbered within it. A
  breakpoint is a segment and a pc.
- **A reload under a debugger** says `loaded`, with the new segment's
  description, before anything can call the new code. It then waits for
  the segment's setup. So a breakpoint in the new code is in place from
  the first call, and nothing runs the new code unseen.
- **The adapter** works each segment out as it works out the first, from
  the build that made it: that build's sources, lines and variables. Its
  tables run every segment's pcs one after another. A breakpoint is set in
  each segment's code for its line: the old code's as well as the new,
  since a call under way runs on in the old. A frame of reloaded code is
  named for its reload: `greet [reload 1]`.

**Watching** (DBG-10). Launched with `"watch": true`, `meadow dap` looks at
the package's sources as `meadow run --watch` does. When they change, it
builds the program again and writes the image, and the VM, started with
`--watch`, loads it. The VM then says `reloaded` or `refused`, and the
adapter shows what the VM printed of it. A build that fails says why, and
the program runs on as it was. A session builds reloadably, as an
interpreted build does, unless reloading is turned off.

**Found on the way.** Std's steered fuzzer kept an input whenever the count
of places reached grew. The fuzzer's own code reaches new places as its
draws vary, so it kept inputs for nothing the property did. How far that
noise carried it depended on how an engine numbers its places. On the
growable stack it found `fuzz` only after 3,381 tries, where the test allows
1,000. It now counts only what the property reaches, and every engine finds
it on the same try, the 82nd.

**Not done.** A line whose only code begins a continuation -- `m` alone,
after a call -- is never a boundary, so a step goes past it. That was so
before reloading, and is so in every segment.

### Hot loading, further (2026-10-28)

**A reload refused** (HOT-40). The interpreter writes how each reload ended
to a file `meadow run --watch` names. When one is refused, `meadow` says why
and offers a start over, and saving again takes it: the program is stopped,
built, and run again from the start.

The offer is taken by saving, not by a key, because the program owns the
terminal. Reading a key would take input the program was waiting for, and
handing the program input through `meadow` would stop it from being a
terminal.

**Migrations** (HOT-41). Before this, a value of a type the new build
changed stayed with the old code, and the old code went on calling the old
versions of everything that took it. Now a new build can say what an old
value becomes:

```
data Shape = Circle String | Square Int

@migrate(Circle)
fun fromOld (r : Int) : Shape = Circle (S.repeat "x" r)
```

The function takes the old constructor's fields, as its version wrote them
(`Shape#…(Int)`). The reload checks that some earlier `Circle` had them, and
refuses if none did.
- **The other constructors.** Each one the new build kept as it was is
  migrated by keeping its fields under its new tag. One that changed with
  no `@migrate` refuses the reload.
- **Old code meets new.** An old definition whose type differs from a new
  one's only in the migrated types' versions sends its calls to the new
  one.
- **The conversion is lazy.** A `switch` in the new code that finds no arm
  for a value asks whether its tag is a migrated one. If it is, the
  function makes the new value, and the `switch` takes that apart instead.
  Nested values are migrated as they are reached, and nothing walks the
  heap. While the function runs, everything the frame holds is on the
  shadow chain, as for any call that waits, so a collection, moving or
  not, finds it.
- **Two prices.** The new build's code is interpreted, so that every
  `switch` in it can migrate; a JIT-compiled `switch` cannot. A migration
  is given no handlers, so it may not perform effects.
- **Not done.** A new value that reaches old code which takes it apart
  meets that code's `match` as a constructor it does not know.

**Keys, found on the way.** A definition's slot was found by its module,
name, versioned type and representations. The memory pass's copies -- a
block that owns what its definition borrows -- had no key at all, so a
reload never replaced them; the migration test's `describe` was called
through one. The key now includes which parameters the block borrows, and a
copy is keyed by its definition. That fixes a hazard too. A new version
that owned a parameter the old one borrowed would have been called by old
code still holding it, and given it up twice.

**A native program, reloaded** (HOT-42). JIT-10 asks for an executable that
carries the VM and the JIT for what a reload brings. What is built instead
gets the same result another way. Under `--watch`, a program not asked to
be interpreted runs on the VM with the JIT's second tier compiling each
definition the first time it is called (`MEADOW_JIT_THRESHOLD=0`,
`MEADOW_JIT_TIER2=0`). The code is compiled as an executable's definitions
are, from the same LIR, so it runs natively from its first call. A reload's
definitions are compiled the same way, and what tier 2 does not compile
runs on tier 1 or the interpreter.

An executable linked ahead of time would need its calls by name to go
through the slot table, with the runtime's addresses fixed at link time
rather than at load. Tier 2 has the table and makes calls the same way
already, which is why it is the route taken.

### Glade's differential cases, on the merged VM (2026-10-28)

Glade's `tests/differential.rs` -- 119 cases, each run by the CEK machine,
the AxCut machine and Glade's VM at every optimization level -- is the
merged runtime's now (`buildtools/meadow/tests/differential.rs`). The third
engine is the merged VM, interpreted and with its JIT compiling everything
at first entry, at one worker and at eight. Moved over, 13 cases disagreed.
- **Deep programs.** A list literal of a few hundred elements nests its
  term as many times, and the VM's main thread overflowed its megabyte
  decoding the image. The VM now decodes and loads on a thread with a
  gigabyte of stack, which is address space until it is used.
- **The spill area.** A call's arguments past the registers go through the
  thread's spill area, 256 words in the VM. A definition of three hundred
  parameters passes far more, and tier 2's code wrote past the area:
  valgrind under WSL found it. The area now holds twice the widest
  procedure's parameters, and tier 2 asks the emitter how many words a
  definition's code passes and leaves to the interpreter one that would
  need more.
- **Compiling on a fresh stack.** Cranelift's passes recurse as deep as a
  function is large, and tier 2 compiled on whatever stack the program was
  on. It compiles on a new piece of stack now.
- **`compactSize`.** The CEK and AxCut machines counted a region in Glade's
  layout. They count the merged runtime's now: its header, the fields, an
  array's room to grow, a word for the region, and nothing for a
  constructor of no fields, which is a word and not a block.
- **Waiting with no scheduler.** A program that starts no thread runs
  without the scheduler, and a transaction that waits there failed as "a
  thread operation ran outside every thread". It is the deadlock the CEK
  machine reports, in the words the runtimes share, which the scheduler
  now uses too.
- **Not done.** A function that captures nothing is a word on the merged
  runtime, which a `TVar` cannot tell from a constructor's, so the VM
  keeps one in a `TVar` that the CEK machine refuses. One that captures
  something is refused on every engine. And threads are cooperative: on
  one worker (`MEADOW_THREADS=1`), a thread that never waits starves the
  rest, where Glade's VM switched threads on a budget of instructions. The
  case that shows it is `#[ignore]`d until a poll in compiled code, on
  every engine, lets the scheduler take a thread off its worker.

### Replacing Glade in full (2026-10-24)

The merged runtime is to be Glade's next release, so it has to do all that
Glade's VM does. Where it stands:

| Glade has | on the merged runtime |
|---|---|
| a debugger (`meadow dap`) | yes, on the VM |
| a REPL | `:silo` runs each entry on the VM, a fresh machine a line as Glade's |
| a sampling profiler (`--profile-to`) | yes, by time or by work, as folded stacks |
| coverage (`Process.covered`) | yes: `Std.Test.Fuzz` steers on it |
| GC statistics (`--gc-stats`) | yes, on both engines, under both strategies |
| `meadow exec` of an image | yes, of an image of AxCut |
| growable stacks, heap frames | yes, on both engines |
| a generational, moving collector | `--gc generational`, on both engines |
| a JIT of hot blocks | planned: R2 in `docs/RUNTIME-SPEC.md`, beside the VM and AOT |

A block no person wrote -- a join point, a group of local functions, a
`perform`'s search -- is named at load after the definition that jumps to
it, `Fib.fib/<join>`, so a debugger's frames and a profile's samples say
whose code it is, as Glade's do.

### Silo's speed, kept (measured 2026-10-23)

Counting on the LLVM path compiles to the same code as before the merge. For
a counted program, the LLVM IR of every benchmark is master's, except for
an unused declaration, one constant and the runtime's fingerprint. What
could differ is the runtime library:
- **Allocation:** a counted program calls `meadow_acquire`, which is master's
  code word for word. A traced program calls `meadow_acquire_traced` instead.
- **Elsewhere:** the counting paths gained one predictable branch on a
  constant.

Interleaved on one machine, master against the branch, four rounds:
- **EPYC 9554:** every task matched or improved once allocation was split.
  The one regression, matmul's 18%, came from the tracing branch moving how
  the allocator compiled, and splitting the allocation path removed it.
- **EPYC 7543:** matmul showed 8% and binarytrees 3% with the runtime as
  built, while fib and rbtree were 6% faster.

To tell layout from cost, both runtimes were rebuilt with every function and
loop aligned to 64 bytes:

| task | master | branch |
|---|---|---|
| matmul | 29.05 ms | 27.23 ms |
| binarytrees | 1,077 ms | 1,078 ms |
| fib | 18.19 ms | 18.16 ms |
| rbtree | 3,577 ms | 3,316 ms |

With layout held still, the branch is as fast as master or faster. The
differences in the plain builds were layout.

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
