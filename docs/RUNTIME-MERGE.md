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
| memory | `--gc rc` (counting, with reuse), `tracing`, or `generational` (a nursery, on the VM) | rc |
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
it waited on, and records a frame's borrowed values too. Native code cannot
yet, since its SSA values hold addresses across calls, so under
`generational` it traces without moving.

Every example prints the same with the smallest nursery (32K words). On the
VM, binarytrees spends 7.7% of its run collecting, against 10.6% traced. The
run itself is about the same, since interpreting it costs most. A larger
nursery (`MEADOW_GC_NURSERY`, in words) spends less: 4.8% at 16M words.

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
| a generational, moving collector | on the VM: `--gc generational`; native code traces without moving |
| a JIT of hot blocks | not planned: the LLVM path compiles a whole program |

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
