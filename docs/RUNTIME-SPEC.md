# Glade, the merged runtime: specification

Status: in progress on branch `runtime-merge`, not merged. The whole project
is experimental until Chris says to merge it.

This is the specification of the merged runtime, written so that tests can be
made from it directly. Each requirement has an identifier, says what must hold
in terms a test can observe, says how to check it, and says where it stands.
[RUNTIME-MERGE.md](RUNTIME-MERGE.md) is the design log, with the reasoning and
measurements behind each step. The module docs in the code are the detailed
reference for how each requirement is met.

**Contents**

- [0. How to read this](#0-how-to-read-this)
- [1. What it is](#1-what-it-is)
- [2. The pipeline](#2-the-pipeline)
- [3. Configuration](#3-configuration) -- `CFG`
- [4. Behaviour across configurations](#4-behaviour-across-configurations) -- `DIFF`
- [5. The object layout](#5-the-object-layout) -- `LAY`
- [6. Memory](#6-memory) -- `MEM`
- [7. Stacks](#7-stacks) -- `STK`
- [8. The VM](#8-the-vm) -- `VM`
- [9. Native code](#9-native-code) -- `AOT`
- [10. Effects and threads](#10-effects-and-threads) -- `EFF`
- [11. Hot loading](#11-hot-loading) -- `HOT`
- [12. The debugger](#12-the-debugger) -- `DBG`
- [13. The REPL](#13-the-repl) -- `REPL`
- [14. Profiling, coverage and statistics](#14-profiling-coverage-and-statistics) -- `PROF`, `COV`, `GCS`
- [15. Images and running them](#15-images-and-running-them) -- `IMG`
- [16. Performance](#16-performance) -- `PERF`
- [17. The test matrix](#17-the-test-matrix)
- [18. Roadmap](#18-roadmap) -- `R1` to `R9`, with requirements to come
- [19. Open questions](#19-open-questions)
- [Appendix A. Where the code is](#appendix-a-where-the-code-is)
- [Appendix B. Limits and constants](#appendix-b-limits-and-constants)

## 0. How to read this

**A requirement** looks like this:

> **MEM-GEN-4.** A minor collection rewrites every reference to a block it
> moves. *Check:* … *Status:* done; `test_name`.

- **Its identifier** is the area, a sub-area where there is one, and a
  number. Identifiers are never reused; a requirement that is dropped is
  marked so.
- **The words** "must" and "must not" are the requirement. "Should" is a
  target that a test may report on without failing.
- **"Check"** says how a test observes it: a command and what it prints, or a
  property over runs.
- **"Status"** is one of:
  - *done*, with the tests that cover it, if any;
  - *done, no test*: met, but nothing checks it yet, so a test is to be
    written;
  - *planned* (`R1`…`R9`): not met, and the roadmap item that meets it.

**Notation:**
- `meadow` is the driver, and `vm` stands for `meadow-silo-vm`.
- *A configuration* is a choice on each axis of [§3](#3-configuration).
- *Every configuration* means every combination [§17](#17-the-test-matrix)
  lists.
- A message is quoted exactly where a test may match it; `{x}` stands for a
  part that varies.

## 1. What it is

Meadow had two runtimes:
- **Glade:** a bytecode VM, a block JIT and AOT executables, with a
  generational collector, a debugger and a REPL.
- **Silo:** native code through LLVM, with reference counting, reuse and the
  native stack.

The merged runtime is one runtime, named Glade, the next release of Glade;
Silo is retired (Chris, 2026-10-24). Its code is Silo's for now (`silo/`,
`--runtime silo`), and takes Glade's name when the old Glade is retired
([R7](#r7-retire-the-old-glade-and-rename)).

Its principles:
- **Maximum configurability.** The object representation, memory management,
  the use of the native stack, and the engine (interpreted, JIT or AOT) are
  each a choice, and every feature of either old runtime is an option on every
  engine.
- **One interface.** Linear AxCut is the interface every engine runs.
- **Silo's speed is kept.** Counted native code is Silo's IR exactly.
- **Glade's tools are kept.** The debugger, the REPL and introspection, with
  nothing worse.
- **Clean or not at all.** Mechanisms are small and contained.

## 2. The pipeline

```text
front ends ──► Cut ──► AxCut ──► (versions, when reloadable)
                                   │
                         memory pass: linear AxCut
                                   │
                  ┌────────────────┴────────────────┐
              an image                         LLVM IR
                  │                                │
          the VM (JIT: R2)                 native executable
                  └──────────► one runtime library ◄┘
```

## 3. Configuration

### 3.1 The axes

| axis | options | default today | target default |
|---|---|---|---|
| engine | interpret, `jit` ([R2](#r2-a-jit)), aot | aot; `--interpret` or `--backend vm` chooses the VM, `--jit` the VM with its JIT | debug: interpret or jit; release: aot |
| memory | `rc`, `tracing`, `generational` | `rc` | `rc` |
| stack | `fixed`, `growable`, `heap` | debug: `growable`; release: `fixed` | the same |
| layout | `silo`, `glade` ([§5](#5-the-object-layout)) | `silo` | `silo` |
| reloading | on, off | on (the VM) | on |
| nursery | a size | 2 MB (2^18 words) | the same |

### 3.2 Requirements

**CFG-1.** `--gc rc|tracing|generational|copying` chooses the memory strategy:
`copying` means `generational`, and `MEADOW_GC` says the same when the flag is
absent. Without either, it is `rc`, or `generational` under a layout with no
count (`--layout glade`), as Glade's default is.
- *Check:* run a program with `--gc-stats` under each name; the first `gc:`
  line begins `gc: counted:` for `rc` and `gc: traced:` for the others.
- *Status:* done; `driver::gc_stats_say_what_memory_did_on_the_merged_runtime_s_vm`.

**CFG-2.** `--stack fixed|growable|heap` chooses the stack. `MEADOW_STACK`
and `stack = "…"` in a profile say the same, with the flag winning. Without
any of them, a debug build has `growable` and a release build `fixed`.
- *Check:* the depth tests of [§7](#7-stacks), under each way of saying it.
- *Status:* done, no test for the precedence.

**CFG-3.** `--interpret`, or `MEADOW_INTERPRET`, with `--runtime silo` builds
an image instead of an executable, and `meadow run` runs it on the VM.
- *Check:* `meadow build --runtime silo --interpret P` writes
  `P/target/debug/native/silo/{Name}.image`.
- *Status:* done; `hot_reload::*` builds images this way.

**CFG-4.** `--no-reload` builds what is interpreted with nothing versioned,
and so do `reload = false` in a profile and `MEADOW_RELOAD` set to `0`,
`false` or `no`. See HOT-30 and HOT-31.
- *Status:* done; `hot_reload::a_build_without_reloading_is_not_reloaded`
  (the flag only).

**CFG-5.** `--nursery SIZE` (for example `8M`, between 256K and 1G) sets the
young generation's size. `MEADOW_GC_NURSERY`, in words, says the same; the
runtime clamps it to between 2^15 and 2^27 words.
- *Check:* binarytrees under `generational` with `MEADOW_SILO_GC=1` logs fewer
  `aot: minor gc` lines with a larger nursery.
- *Status:* done, no test.

**CFG-6.** `meadow run --watch` chooses Silo and `--interpret` itself. With
`--no-reload` or `reload = false` it fails with exit status 2 and the message
``--watch` loads each new build into the program, which `--no-reload` (or
`reload = false`) builds it not to take``.
- *Status:* done, no test for the refusal.

**CFG-7.** A configuration is part of what makes an executable stale: building
again with another memory strategy or stack builds anew, and does not reuse
the executable built for the last one.
- *Check:* build release with `--gc rc`, then `--gc tracing`; the second build
  links anew, and its `--gc-stats` says `traced`.
- *Status:* done, no test.

**CFG-8.** MeadowBoot's command line takes every flag `meadow`'s does,
command for command, including `--no-reload` and `run --watch`, and its
version number is `meadow`'s.
- *Status:* done; `bootstrap::a_command_takes_what_meadow_s_takes`.

**CFG-9.** A combination that cannot be met is refused with a message, never
run as another combination silently. One exception is allowed while native
code is the case: none at present, since `generational` moves blocks on both
engines.
- The one refused today is the `glade` layout with `rc`. `meadow` exits with
  status 2 and `error: the `glade` layout has no count, so its memory is
  traced: build with `--gc tracing` or `--gc generational``.
- *Status:* done, no test for the refusal; must hold for every option added
  later.

**CFG-10.** `--layout silo|glade`, `MEADOW_LAYOUT`, or `layout = "…"` in a
profile, chooses the object layout ([§5](#5-the-object-layout)), with the flag
winning. The image's configuration carries it. Building again with the other
layout builds anew (CFG-7).
- *Check:* DIFF-1 with `--layout glade`. A package whose profile says
  `runtime = "silo"` and `layout = "glade"`, run with no flags, runs under
  `generational` (CFG-1).
- *Status:* done, no test.

**CFG-11.** `--backend vm|jit|aot` (or `--jit`, `--aot`), or `backend = "…"`
in a profile, chooses the engine for Silo:
- `vm` is its interpreter, as `--interpret` is;
- `jit` is the interpreter with its JIT;
- `aot` is LLVM ahead of time, which Silo is unless one is named.

The JIT's threshold is `MEADOW_JIT_THRESHOLD`.
- *Check:* `driver::the_merged_runtime_s_jit_runs_what_its_vm_runs`.
- *Status:* done; `--jit-tiers` waits for a second tier (R2).

## 4. Behaviour across configurations

These are the differential properties. They are the strongest tests the
runtime has, since they need no expected output written by hand.

**DIFF-1.** A program that is deterministic prints the same, byte for byte,
in every configuration.
- *Check:* `examples/` `tour`, `effects`, `concurrency`, `MiniML`,
  `streams` and `euler`, with stdin closed, compared across:
  - VM and native;
  - `rc`, `tracing` and `generational`;
  - `fixed`, `growable` and `heap`;
  - debug and release;
  - the default nursery and the smallest;
  - the `silo` and `glade` layouts.
- *Status:* done as a manual check, no test. A test is to be written over
  these examples. The script is `scratchpad/laydiff.sh`.

**DIFF-2.** Std's 350 tests pass in every configuration of [§17](#17-the-test-matrix).
- *Status:* done; run on Jarvis at 09a91d0.

**DIFF-3.** A failure says the same thing in every configuration: the same
message on stderr, and exit status 1.
- *Check:* a program that divides by zero prints `division by zero` and exits
  1, on the VM and natively, under each strategy.
- *Status:* done, no test across configurations.

**DIFF-4.** What a program prints of a value (`show`, `println`) is the same
on the merged runtime as on the old Glade.
- *Status:* done for Std's tests, which compare printed values.

## 5. The object layout

### 5.1 The description

The layout is a value, `meadow_rt::layout::Layout`. Every reader and writer of
a block takes its facts from it. There are two:
- `SILO`, Silo's, with a count;
- `GLADE`, Glade's as near as machine pointers allow, with no count.

The runtime library and the VM are each built for one, by `meadow-silo`'s
`glade-layout` feature (LAY-20). The LLVM back end takes it from the
configuration.

**LAY-1.** `meadow_rt::layout::SILO` describes Silo's layout exactly:

| field | word | shift | bits |
|---|---|---|---|
| count | 0 | 0 | 32 |
| length | 0 | 32 | 32 |
| kind | 1 | 0 | 8 |
| element descriptor | 1 | 8 | 4 |
| uniform | 1 | 12 | 1 |
| colour | 1 | 13 | 2 |
| mutable | 1 | 18 | 1 |
| meta | 1 | 32 | 32 |

It also fixes:
- 2 header words;
- 4-bit descriptors, 16 to a word;
- immediates `(payload << 1) | 1`, told by `w & 1 == 1`, with null 0;
- a region floor of 2^29 and a sticky count of 2^30;
- the kinds data 1, closure 2, string 3, array 4, big integer 5, cell 6,
  record 7, mutable array 8, once 9, compact 10, channel 11, task 12, `TVar`
  13, stack 14 and region 15.

Under tracing, the count is the mark. A moved block has a count of
`u32::MAX - 1`, with where it went in word 1. A free block's word 1 is zero,
and word 0 links it.
- *Status:* done; `meadow_rt::layout::tests::silo_s_header_is_where_its_heap_says`.

**LAY-2.** `first_field(n, uniform)` answers `header_words` for a uniform
block, and otherwise `header_words + ceil(max(n - inline_descs, 0) /
descs_per_word)`. `desc_at(i)` answers `(inline_word, inline_shift + i *
desc_bits)` for `i < inline_descs`. Otherwise, with `j = i - inline_descs`, it
answers `(header_words + j / descs_per_word, (j % descs_per_word) *
desc_bits)`.
- *Check:* for `SILO`, `first_field(3, false) == 3`,
  `first_field(17, false) == 4`, `first_field(17, true) == 2`, and
  `desc_at(17) == (3, 4)`.
- *Status:* done; the same test, and LAY-6's for `GLADE`.

**LAY-3.** No code in the runtime, the VM or the LLVM back end writes a header
word index, shift, mask or kind number as a literal. Each comes from the
description.
- *Check:* a grep over `silo/src`, `silo/vm/src` and
  `compiler/meadow-llvm/src/emit.rs` finds none outside `heap.rs`'s
  accessors. The exceptions are those documented: the collector's free-block
  test reads the kind's word, and the shadow record's own layout.
- *Status:* done, no test; a lint test is to be written.

**LAY-4.** Changing the description to an equivalent one changes no program's
behaviour. Moving the LLVM back end onto it changed no IR: the IR is
identical byte for byte (PERF-1).
- *Status:* done.

**LAY-5.** The LLVM back end refuses at start a description its IR cannot
serve. `emit::suits` requires of every layout:
- a one-bit immediate tag equal to its mask;
- the uniform bit and the element descriptor in one word;
- descriptors per word and descriptor bits that are powers of two.

And of a layout that counts, what counting's code assumes:
- the count at word 0, shift 0, 8, 16, 32 or 64 bits wide;
- the length and the count in one word;
- the meta, uniform bit, element descriptor, colour and mutable bit in the
  kind's word;
- no descriptors in the header;
- the length and the meta at the top of their words.

A field is read with a shift unless it is at the bottom of its word, and a
mask unless it is at the top (`emit::read_field`).
- *Status:* done, no test.

**LAY-6.** `meadow_rt::layout::GLADE` describes the Glade-like layout:

| field | word | shift | bits |
|---|---|---|---|
| kind | 0 | 0 | 8 |
| element descriptor | 0 | 8 | 4 |
| uniform | 0 | 12 | 1 |
| colour, buffered, dead, moved, mutable, cyclable, deferred | 0 | 13 to 20 | 8 in all |
| mark | 0 | 21 | 11 |
| length | 0 | 32 | 32 |
| meta | 1 | 0 | 32 |
| descriptors of fields 0 to 7 | 1 | 32 | 4 each |

It also fixes:
- no count;
- 2 header words, and descriptors of fields 8 and up in the words after
  the header, 16 to a word;
- immediates as Silo's;
- a region's block has the mark 2047 (the region floor), which no collection
  reaches, since collections are numbered below 2047;
- a moved block has kind `0xFF`, with where it went in word 1;
- a free block's word 0 is zero, and word 1 links it;
- the kinds as Silo's.

Every bit of word 0 belongs to exactly one field.
- *Check:* `first_field(n, false)` is 2 for `n <= 8`, 3 for `9 <= n <= 24`
  and 4 for `n = 25`. `desc_at(0) == (1, 32)`, `desc_at(8) == (2, 0)` and
  `desc_at(24) == (3, 0)`.
- *Status:* done;
  `meadow_rt::layout::tests::glade_s_header_is_two_words_up_to_eight_fields`.

**LAY-7.** A block of 1 to 8 fields takes one word fewer under `GLADE` than
under `SILO`: a cons cell is 4 words rather than 5.
- *Status:* done.

### 5.2 One layout per runtime

**LAY-20.** The runtime library and the VM are built for one layout:
`SILO`, or `GLADE` with the `glade-layout` feature. A checkout builds the
second with `--features meadow-silo/glade-layout --target-dir
silo/target/glade-layout` (the VM), or `--features glade-layout` (the
library). `meadow` looks for a `glade` layout's VM and library in
`glade-layout` directories:
- in a checkout, under `silo/target`;
- beside `meadow`, both in `lib/{TRIPLE}/` and where the binary is.

No `glade` library is embedded in `meadow`.
- *Status:* done.

**LAY-21.** A library built for `GLADE` exports `meadow_silo_{hash}_glade`,
and a program emitted for it refers to that symbol. So a program links only
with a library built for its layout.
- *Status:* done.

**LAY-22.** The VM refuses an image built for another layout than its own,
with exit status 1 and
`error: the program is built for the `{a}` layout, and this interpreter for the `{b}` one: run it with the interpreter built for its layout`.
- *Status:* done, no test.

**LAY-23.** A runtime built for a layout with no count stops a counted
program at start with exit status 2.
- *Status:* done, no test. `meadow` never builds one (CFG-9).

### 5.3 Descriptors

**LAY-10.** A descriptor is a small integer: `REF` 0, `INT` 1, `FLOAT` 2,
`STR` 3 (an interned name, not a `String`), `UNIT` 4, `BOOL` 5, `CHAR` 6,
`FLOAT32` 7, the sized integers from `WORD` 8, and `ANY` 15. Only `REF` is an
address.
- *Status:* done.

**LAY-11.** Wherever a frame holds a value of a type variable's type, the
record made of it describes it by the descriptor that frame holds for the
variable, never by `ANY`.
- *Check:* `livecheck` over the IR of a program built at `-O1` under
  `generational` finds no frame capture passed through a call without being
  recorded, other than integers.
- *Status:* done; `emit::capture_desc`, met by Std's Json tests at `-O1`
  under `generational` with the smallest nursery. A focused test is to be
  written.

## 6. Memory

### 6.1 All strategies

**MEM-1.** Under every strategy, nothing a program can still reach is freed,
reused or moved without every reference to it following.
- *Check:* DIFF-1 and DIFF-2 with the smallest nursery and the most frequent
  collection (`MEADOW_GC_NURSERY=32768`).
- *Status:* done.

**MEM-2.** The runtime's own code never collects. Allocation only marks a
collection due, and collections run at safe points in the program's code.
- *Status:* done.

**MEM-3.** A runtime frame that holds a block across a call into the
program's code links a record of it on the shadow chain, and reads it back
after the call. `meadow_enter` does, for the continuation a `handle`'s value
goes to.
- *Status:* done.

### 6.2 Counting

**MEM-RC-1.** A counted native program's emitted code is Silo's before the
merge (PERF-1), and its memory behaviour with it:
- shares and erases where the memory pass says;
- reuse;
- lazy freeing;
- the cycle collector for mutable kinds.

- *Status:* done.

**MEM-RC-2.** `--leaks` (or `MEADOW_SILO_LEAKS`) reports, at exit, the
blocks still held and what held them.
- *Status:* done, inherited from Silo.

### 6.3 Tracing

**MEM-TR-1.** Under `tracing`:
- nothing is shared, erased or reused;
- a collection runs only at a safe point (a function's start) and only when
  one is due;
- it frees exactly the blocks no root reaches.

- *Check:* DIFF-2 under `tracing`.
- *Status:* done.

**MEM-TR-2.** The roots are:
- every record on every shadow chain: the running segment's, the outer ones
  `drive` keeps, parked segments' and suspended segments';
- the context's top-level values, string literals and kept values;
- a transaction's writes.

- *Status:* done.

**MEM-TR-3.** A heap collects again once it has allocated twice the words its
last collection kept, and never before 2^20 words.
- *Check:* `MEADOW_SILO_GC=1` lines are spaced so.
- *Status:* done, no test.

**MEM-TR-5.** Nothing a traced program runs changes a block's count. In
particular, `@mw.field`, which reads a field by number or a record's by
label, shares nothing under tracing. Under Silo's layout the count is the
mark, and a share would make a block look marked in the next collection, so
that its fields were never scanned.
- *Status:* done, since the second layout's work; no focused test.

**MEM-TR-4.** A block in a compact region is never marked or swept. The heap
holds one reference to each region its last collection found a pointer into.
- *Status:* done.

### 6.4 Generational

**MEM-GEN-1.** Under `generational`, a block smaller than 128 words is made
in its thread's nursery while there is room. A larger one, or any while the
nursery is full, is made in old space and remembered.
- *Status:* done.

**MEM-GEN-2.** When the nursery fills, a collection is due. At the next safe
point a minor collection copies out what is still reached, and the nursery is
used again from its start.
- *Check:* `MEADOW_SILO_GC=1` prints `aot: minor gc: {n} blocks copied out,
  {t} ms` for each.
- *Status:* done.

**MEM-GEN-3.** A minor collection reaches from the roots of MEM-TR-2,
including every suspended segment's chain, and from every remembered block's
fields.
- *Status:* done.

**MEM-GEN-4.** A minor collection rewrites every reference to a block it
moves:
- in the roots, where they are;
- in remembered blocks' fields;
- in the fields of the blocks it copies.

- *Check:* MEM-1 with the smallest nursery.
- *Status:* done.

**MEM-GEN-5.** A young block that dies and held something outside the heap
lets it go, as a sweep does: a stack segment, a thread's task, a `TVar`'s
cell, a region.
- *Check:* a program that makes and drops many continuations under
  `generational` leaves no live segments at exit (`MEADOW_SILO_LEAKS`).
- *Status:* done, no test.

**MEM-GEN-6.** The write barrier. A store of a reference into a block that
may be older than it remembers the block, wherever the store is:
- `heap::set_field` (`setRef`, `setField`, a record updated in place);
- a mutable array's store (`note_stored`);
- an inline store in native code (`meadow_remember`).

A store of a non-reference needs no barrier.
- *Check:* a test that stores a new young value into an old `Ref` and an old
  array, forces a minor collection, and reads the values back.
- *Status:* done, no focused test.

**MEM-GEN-7.** A full collection empties the nursery first, by a minor
collection, and then marks and sweeps as `tracing` does.
- *Status:* done.

**MEM-GEN-8.** The VM:
- records borrowed values under `generational`;
- reads back what its records held after a safe point and after a call it
  waited on, before using any of it.

- *Status:* done.

**MEM-GEN-9.** Native code under `generational`:
- reads back its parameters after its poll;
- reads back its closure's loaded captures after a method's poll;
- reads back what a waiting frame held after the call, before entering the
  frame.

None of this is written under `rc` or `tracing` (PERF-1).
- *Status:* done.

**MEM-GEN-10.** `generational` speeds up allocation-heavy native programs.
- *Check (should):* binarytrees natively under `generational` runs faster
  than under `tracing`: 1.13 s against 1.57 s measured.
- *Status:* done.

## 7. Stacks

**STK-1.** Under `fixed`, the main thread has a 1 GB stack. A recursion that
exceeds it stops the program with a stack overflow.
- *Status:* done.

**STK-2.** Under `growable`, a recursion runs as deep as memory allows, on
both engines.
- *Check:* a non-tail recursion 20 million deep finishes:
  - on `main`, in a spawned thread, and inside effect handlers whose
    continuations are resumed or abandoned;
  - on both engines, under every strategy.
- *Status:* done, measured; a test is to be written.

**STK-3.** Under `growable`, a function that begins with less than its limit
runs on a new 64 MB piece of stack and returns to the old one. Pieces are
kept for reuse, up to 4.
- *Status:* done.

**STK-4.** Under `heap`:
- no continuation is a frame;
- every non-tail call's continuation is an object;
- every call is a tail call;
- a recursion runs as deep as memory allows on both engines.

- *Check:* a recursion 10 million deep finishes on the VM (measured: 0.74 s).
- *Status:* done.

**STK-5.** A segment keeps its own stack limit and frame records: a
continuation captured deep in a growable stack and resumed elsewhere runs
correctly.
- *Status:* done, measured.

## 8. The VM

### 8.1 Code

**VM-1.** Every instruction is eight bytes, which the code asserts when it is
compiled.
- *Status:* done; a `const` assertion in `interp/code.rs`.

**VM-2.** The instruction set is:
- `Share`, `Erase`, `Rename`, `Let`, `Clean`, `DropReuse`;
- `Switch`, `New`, `Jump`, `Invoke`, `Call`, `Prim`;
- `Bin`, `BinK`, `BinI`, `IfBin`, `IfBinI`, `Branch`;
- `Wide`, `Segment`, `Error`.

An operand that does not fit takes the wide form. `MEADOW_VM_WIDE` lays every
instruction down wide, and a program behaves the same either way.
- *Check:* DIFF-1 with `MEADOW_VM_WIDE=1`.
- *Status:* done, no test.

**VM-3.** `vm --dis IMAGE` prints every procedure, by name, register count and
parameters, then its instructions, each with its pc and source location, and
runs nothing.
- *Status:* done, no test.

**VM-4.** A procedure's name is `{Module}.{definition}`, with a suffix
(`/method0`, `/frame0`, `/after{n}`) for what is made inside it. A block no
person wrote is named after the definition that jumps to it: a join point is
`{Module}.{definition}/<join>`, and likewise `<letrec>` and `<perform …>`.
- *Check:* a profile's folded stacks (PROF-3) name a function's join points
  as the function.
- *Status:* done, no focused test.

### 8.2 Running

**VM-10.** Under a debugger, a profile or coverage, every activation keeps a
record linked to its caller's, and the VM runs the loop that checks for
stops. Without them it runs the loop that does neither, and costs no more
than that.
- *Status:* done.

**VM-11.** The VM's speed holds: fib about 335 ms and matmul about 2.1 s on
the Windows desktop. Every change is timed against these.
- *Status:* done; see PERF-3.

### 8.3 Command line

**VM-20.** `vm IMAGE [ARGS…]` runs the program with `ARGS` as
`Process.argv`.
- *Status:* done.

**VM-21.** `vm TESTS.image N` runs test `N` of a program of tests. Given no
number, or one out of range, it prints `usage: a test's number, below {n}`
and exits 2.
- *Status:* done; used by `meadow test --interpret`.

**VM-22.** `vm --answer FILE IMAGE` runs the program, writes what it answers,
shown, to `FILE` (`()` included), and prints nothing of it on stdout.
- *Status:* done; REPL-2.

**VM-23.** `vm --profile FILE [--profile-every N | --profile-hz HZ] IMAGE`
writes a profile at exit (PROF-1).
- *Status:* done.

**VM-24.** `vm --watch IMAGE` reloads the image whenever it is written again
(HOT-1).
- *Status:* done.

**VM-25.** `vm --debug PORT IMAGE` connects to the adapter on `PORT` and runs
the program under the debugger (§12).
- *Status:* done.

**VM-26.** A malformed command line prints a `usage:` line on stderr and exits
2. An image that cannot be read prints `error: {path}: {why}` and exits 2.
- *Status:* done, no test.

## 9. Native code

**AOT-1.** `meadow build --runtime silo` compiles a package to a native
executable through LLVM, linked with the runtime library built from the same
sources. A runtime library from other sources fails to link: the fingerprint
symbol `meadow_silo_{hash}` is missing.
- *Status:* done.

**AOT-2.** `--target TRIPLE` builds for another system, with the runtime found
at `lib/{TRIPLE}/libmeadow_silo.a` beside `meadow`, or where a checkout
builds it (`scripts/silo-runtime.sh {TRIPLE}`). The triples include
`aarch64-linux-android`, `x86_64-linux-android`, `aarch64-apple-ios` and
`aarch64-apple-ios-sim`.
- *Status:* done; the `android` and `ios` workflows, and v0.4.0.

**AOT-3.** A package with `@export` functions builds to a C library: `.so`,
`.dylib`, `.dll`, or `.a` for iOS. C can call each export.
- *Status:* done; the `android` and `ios` workflows.

**AOT-4.** `MEADOW_KEEP_IR=1` keeps the LLVM modules beside the executable:
`{name}.ll`, and `{name}.{unit}.ll` for each unit.
- *Status:* done.

## 10. Effects and threads

**EFF-1.** A `perform` finds the innermost handler of its operation by the
key `{Effect}.{op}`, or `{Effect}#{version}.{op}` when the effect has a
version (HOT-12). A `perform` no handler takes goes to the runtime's world
(`Console`, `Fs`, …).
- *Status:* done.

**EFF-2.** A handler's body runs on a stack segment of its own. A
continuation captured there can be resumed once, or abandoned; an abandoned
one lets go of what its frames held.
- *Status:* done.

**EFF-3.** Threads are green threads with a heap each. What crosses between
them is copied, and a `TVar` holds a copy of its value. One thread's
collection never touches another's heap.
- *Status:* done.

## 11. Hot loading

### 11.1 Reloading

**HOT-1.** `vm --watch IMAGE` checks the image's modification time every
100 ms. When it changes, it reads the image and loads it into the running
program. On stderr it then prints either
`reloaded {path}: {r} definitions replaced, {a} added` or
`not reloaded: {why}`.
- *Status:* done; `hot_reload::*`.

**HOT-2.** After a reload, a call by name runs the new version of the
definition it calls.
- *Status:* done;
  `hot_reload::a_call_by_name_runs_the_new_version_and_what_runs_finishes_as_it_was`.

**HOT-3.** A call already under way when the reload comes finishes in the
version it began in. What it calls by name afterwards is the new version.
- *Status:* done; the same test.

**HOT-4.** A closure made before a reload runs the version it was made of,
including a local function the compiler lifted out of its definition.
- *Status:* done; the same test (`old-closure`).

**HOT-5.** A top-level value read by the new code is computed again by the
new code. Old code keeps reading its own.
- *Status:* done; the same test (`100` from the new `scale`).

**HOT-6.** Reloads follow one another: a third build loads over a second.
- *Status:* done; `hot_reload::reloads_follow_one_another`.

**HOT-7.** A program takes at most 1,023 reloads. The next one prints
`not reloaded: the program has been reloaded as often as it can be`.
- *Status:* done, no test.

### 11.2 Versions

**HOT-10.** A type's version is a hash of its definition (its constructors,
their fields' types and labels) and of the definitions of every type those
reach. A build that changes nothing reachable from a type keeps its version.
- *Check:* `meadow_core::versions::Versions::of` on two builds of the same
  source gives equal versions; changing one constructor's field changes the
  versions of that type and of every type that mentions it.
- *Status:* done, no unit test; a test is to be written.

**HOT-11.** A definition's key is its module, its name, its type written with
versions, and its parameters' and answer's representations. A definition
whose key is unchanged takes its old slot, and one whose key changed is a new
definition.
- *Status:* done.

**HOT-12.** An effect's version is a hash of the versioned types its handlers'
and `perform`s' types carry. An effect that carries none has no version.
- *Status:* done.

**HOT-13.** A value of a type the new build changed reaches only code made
for its version: old code describes old values, and new code makes and
describes new ones.
- *Status:* done;
  `hot_reload::a_value_of_a_changed_type_goes_to_the_code_made_for_it`.

**HOT-14.** A definition no person wrote (a join point, a group of local
functions), a lifted local function, and two definitions with the same key
take no key. Each build's version is its own.
- *Status:* done.

**HOT-15.** Constructors are matched by name and type version, and a changed
constructor takes a new tag. Both versions print by their names.
- *Status:* done.

### 11.3 Refusals

Each of these must leave the running program as it was. The message follows
`not reloaded: ` on stderr.

| id | when | message |
|---|---|---|
| HOT-20 | the build manages memory or the stack otherwise | `it was built to manage memory or the stack otherwise` |
| HOT-21 | it is a program of tests | `it is a program of tests` |
| HOT-22 | the running program was built with `--no-reload` | ``the program running was built without hot loading (`--no-reload`)`` |
| HOT-23 | the new build was built with `--no-reload` | ``it was built without hot loading (`--no-reload`)`` |
| HOT-24 | the roles are other constructors | `its built-in types are other constructors than they were` |
| HOT-25 | a constructor the runtime builds by name changed | ``constructor `{Type.Ctor}`, which the runtime builds by name, changed`` |
| HOT-26 | it starts threads and the running program does not | `it starts threads, which the program running was not set up for` |
| HOT-27 | it detaches continuations and the running program cannot | `it detaches continuations, which the program running was not set up for` |
| HOT-28 | its caches cannot be numbered apart | `its top-level values cannot be numbered apart from the running program's` |
| HOT-29 | the reload limit (HOT-7) | `the program has been reloaded as often as it can be` |

- *Status:* done. HOT-23 is tested
  (`hot_reload::a_build_without_reloading_is_not_reloaded`); the others are to
  be tested.

### 11.4 Building for reloading

**HOT-30.** An interpreted build writes each definition's versioned type
(`Program::types`) and each constructor's version (`Program::ctor_types`),
unless it was built with `--no-reload`. A native build writes neither.
- *Status:* done.

**HOT-31.** `--no-reload` makes an image smaller (8% for MiniML), and makes
the program no slower and no faster.
- *Status:* done, measured.

**HOT-32.** Images are written whole: to `{name}.image.part`, then renamed into
place. A watching VM never reads a partial image.
- *Status:* done.

**HOT-33.** `meadow run --watch` rebuilds the package whenever a file under it
changes, other than under `target/` or a dot-directory. It prints
`Rebuilding` and then the build's own output. A build that fails leaves the
program running as it was. When the program ends, `meadow` exits with its
status.
- *Status:* done, checked by hand; a test is to be written.

## 12. The debugger

The adapter is `meadow dap` with `"runtime": "silo"` in the launch request,
and the VM is the debuggee.

**DBG-1.** The VM and the adapter speak JSON, one message a line, over TCP:
the VM connects to the adapter's port.
- **The adapter sends:**
  - `{"setup": {"boundary": [...], "keys": [...]}}`;
  - `{"breakpoints": [pc, ...]}`;
  - `{"run": mode}`;
  - `{"stack": max}`;
  - `{"children": handle}`;
  - `{"pause": true}`;
  - `{"terminate": ...}`.
- **The VM sends:**
  - `{"ready": description}`;
  - `{"stopped": reason, "pc": pc}`, where `reason` is `breakpoint`, `step`,
    `pause` or `entry`;
  - `{"stack": frames}`;
  - `{"children": ...}`;
  - `{"exception": message}`;
  - `{"exited": {"ok": shown}}`.

- *Status:* done.

**DBG-2.** A breakpoint on a line stops at the first instruction of that
line. A line with no code moves the breakpoint to the next line that has
some.
- *Status:* done;
  `dap_silo::a_line_with_no_code_moves_to_the_next_one_that_has_some`, and
  others.

**DBG-3.** Stepping in, over and out behaves as on Glade:
- in enters a call;
- over stays in the function, on a later line;
- out returns to the caller.

- *Status:* done; `dap_silo::stepping_over_a_call_stays_in_the_function_and_into_it_goes_inside`.

**DBG-4.** A recursive function has a frame per call, innermost first, and
each frame's locals are its own.
- *Status:* done; `dap_silo::a_recursive_function_has_a_frame_per_call`.

**DBG-5.** A frame shows only the locals that hold a value at its pc: those
some path from the pc reads before writing. A waiting frame is judged as it
will be after its call. A name used for the last time is not shown.
- *Status:* done; the stack tests, on Linux, where showing a dead slot
  crashed.

**DBG-6.** A value opens into its parts: a constructor's fields, a record's
labels, an array's elements, a closure's captures.
- *Status:* done; `dap_silo::a_value_opens_into_its_parts`.

**DBG-7.** A failure stops where it happened, with its message.
- *Status:* done; `dap_silo::a_failure_stops_where_it_happened`.

**DBG-8.** Every requirement of this section holds with `"stack": "heap"`,
where the debugger follows continuations on the heap.
- *Status:* done; `dap_silo` under `MEADOW_STACK=heap`.

**DBG-9.** (planned, R5) The debugger stops in reloaded code, by line, in
every segment.

## 13. The REPL

**REPL-1.** `:silo` in `meadow`'s REPL makes each later entry run on the
merged runtime's VM, and prints `(evaluating on the merged runtime's VM)`. If
no VM is found it prints `(no VM to evaluate on: {why})` and changes nothing.
`:vm`, `:jit` and `:cek` choose Glade's engines again.
- *Status:* done; `driver::the_repl_evaluates_on_the_merged_runtime_s_vm`.

**REPL-2.** On the VM, an entry prints what it printed, then `= {value}`, as
Glade's REPL does. A function's value is `<closure>`.
- *Status:* done; the same test.

**REPL-3.** On the VM, an entry that fails prints its message and
`the program failed (exit status 1)`, and the session goes on.
- *Status:* done, no test.

**REPL-4.** (planned, R7) The REPL runs on one VM for the whole session, each
entry loaded into it as a reload.

## 14. Profiling, coverage and statistics

### 14.1 Profiling

**PROF-1.** `meadow run --profile-to FILE` on the VM writes a profile when the
program ends. `profile = true` in a profile asks for one at
`target/{profile}/profile.folded`.
- *Status:* done.

**PROF-2.** By time (the default, `--sample-hz HZ`, 1000 by default), samples
are taken `HZ` times a second of the program's run, within a thousand
instructions. By work (`--sample-every N`), a sample is taken every `N`
instructions, and two runs of the same program give the same profile.
- *Status:* done;
  `driver::a_profile_of_a_program_on_the_merged_runtime_s_vm_names_its_functions`
  checks the work case.

**PROF-3.** The profile is folded stacks: a line a stack, outermost first,
frames joined by `;`, then a space and the count. Lines are sorted by count,
most first. A frame is named by its definition: a procedure's name up to the
first `/`.
- *Status:* done; the same test.

**PROF-4.** The runtime prints `profile: {n} samples of time` or
`profile: {n} samples of work`, or
`profile: no samples -- the program did not run long enough`.
- *Status:* done.

### 14.2 Coverage

**COV-1.** `Process.covered True` forgets what the thread has reached and
answers 0. `Process.covered False` answers how many places the thread has
reached since. The count only grows, and grows when the program reaches a
place it had not.
- *Check:* `use Std.Process as P` … `P.covered True` (`0`), `pick 1`,
  `P.covered False` (`> 0`), `pick 20` (a new branch), `P.covered False`
  (more again). This prints `0 True True` on Glade's VM and on the merged
  runtime's.
- *Status:* done;
  `driver::the_merged_runtime_s_vm_says_what_a_program_reached_as_glade_does`.

**COV-2.** A native program is told 0 by `Process.covered`.
- *Status:* done; the same test (`0 False False`).

**COV-3.** `Std.Test.Fuzz`'s steered tests pass on the VM.
- *Status:* done; Std's `Test.Fuzz` tests on the VM.

### 14.3 Statistics

**GCS-1.** `--gc-stats`, on either engine, prints after the program's output:
- traced:
  `gc: traced: {n} collection{s}, {t} ms paused ({p}% of {r} ms), longest {l} ms`;
- counted: `gc: counted: {n} blocks acquired on the main thread`, then
  `gc: {n} cycle collection{s}, {m} blocks freed, {t} ms ({p}% of {r} ms), longest {l} ms`;
- then `gc: heaps {size} at the end`.

On Glade's engines it prints Glade's own statistics.
- *Status:* done; `driver::gc_stats_say_what_memory_did_on_the_merged_runtime_s_vm`.

**GCS-2.** `MEADOW_SILO_GC=1` prints a line for each collection:
`aot: gc {n}: {w} words live, {t} ms` for a full one, and
`aot: minor gc: …` for a minor one.
- *Status:* done.

## 15. Images and running them

**IMG-1.** An image begins with the bytes `meadow axcut image 5\n`, followed by
an `Image` in postcard: `program`, `config`, `tests` and `linear`. An image
with any other beginning is refused.
- *Status:* done.

**IMG-2.** `meadow exec FILE.image [-- ARGS…]` runs the image on the VM with
the arguments, and exits with the program's status. `meadow exec FILE.mbc`
runs a Glade bytecode image as before.
- *Status:* done; `driver::exec_runs_an_image_of_axcut_on_the_merged_runtime_s_vm`.

**IMG-3.** The VM finds itself, for `meadow`, through `MEADOW_SILO_VM`, then
beside `meadow`, then where a checkout builds it.
- *Status:* done.

## 16. Performance

**PERF-1.** A counted native program's LLVM IR is the same as before the
merge, byte for byte. Only two things may differ: the runtime fingerprint's
name, and the order of the method table and the method numbers, which varies
from build to build (PERF-5).
- *Check:* snapshot `MEADOW_KEEP_IR=1` builds of fib, matmul, binarytrees,
  rbtree, wordfreq, `tour`, `effects`, `concurrency` and `MiniML`, clean,
  before and after a change. Compare with the fingerprint masked, and sorted
  where the method table differs.
- *Status:* done; checked by hand at each change (the script is
  `scratchpad/ir-snap.sh`); a test is to be written.

**PERF-2.** With code layout held still (functions and loops aligned to 64
bytes, `-C llvm-args=-align-all-functions=6
-C llvm-args=-align-all-nofallthru-blocks=6`), the branch is as fast as master
or faster on every benchmark.
- *Status:* done, measured on an EPYC 7543:

  | benchmark | branch | master |
  |---|---|---|
  | matmul | 27.2 ms | 29.1 ms |
  | binarytrees | 1078 ms | 1077 ms |
  | fib | 18.2 ms | 18.2 ms |
  | rbtree | 3316 ms | 3577 ms |

**PERF-3.** On the VM, fib takes about 335 ms and matmul about 2.1 s on the
Windows desktop, and every change keeps them so within noise, about 3%.
- *Status:* done.

**PERF-4.** The VM runs a debugger-free, profile-free, coverage-free program
without paying for records (VM-10), and slot lookups for reloading cost
nothing measurable.
- *Status:* done, measured.

**PERF-5.** (should) The method table's order is the same from one build to
the next. It is not, today.
- *Status:* planned, a smaller item.

## 17. The test matrix

Run on Jarvis Labs (Linux, x86-64) before a step is called done. Std's 350
tests run in each of these 16 configurations:

| engine | memory | stack | profile | nursery |
|---|---|---|---|---|
| native | rc | default | release | |
| native | tracing | default | release | |
| native | rc | default | debug | |
| native | tracing | default | debug | |
| VM | rc | default | release | |
| VM | tracing | default | release | |
| native | rc | heap | release | |
| native | tracing | heap | release | |
| VM | rc | heap | release | |
| VM | generational | default | release | default |
| VM | generational | default | release | smallest |
| VM | generational | heap | release | smallest |
| native | generational | default | release | default |
| native | generational | default | release | smallest |
| native | generational | default | debug | smallest |
| native | generational | heap | release | smallest |

Also run:
- the full suite (`scripts/check-parallel.sh`);
- `dap_silo`, under both the growable and heap stacks;
- `hot_reload`;
- `driver`;
- the LLVM back end's native tests.

The job script is `scratchpad/merge-job8.sh`, which adds nine configurations
with `--layout glade` and the examples' differential under it. The last full
run, at 4011547 (the suite at 8da0687), passed everything.

To add:
- VM debug configurations;
- `MEADOW_VM_WIDE`;
- the examples' differential (DIFF-1);
- the IR snapshot (PERF-1).

## 18. Roadmap

In order, unless Chris reorders it. Each item lists the requirements it is to
meet, numbered now so that tests can be planned.

### R1. The Glade-like layout

Begun 2026-10-25: `GLADE` and `--layout` (LAY-6 to LAY-23).

- **LAYG-1.** A second instance of the layout description, with no count in
  the header. Its first word holds the collector's mark, and a moved block
  says so in its kind.
  - *Status:* done (LAY-6).
- **LAYG-2.** Region blocks are told apart without a count. Changed from "by
  address range": they are told by a mark no collection gives, the same test
  under both layouts (`heap::in_region`). An address range would need memory
  reserved for regions, which a phone limits.
  - *Status:* done.
- **LAYG-3.** `--layout silo|glade` (CFG-10). The LLVM back end takes the
  layout from the configuration. The runtime and the VM are each built for
  one, rather than reading the layout from a variable, so that a header read
  costs what it did (LAY-20).
  - *Status:* done.
- **LAYG-4.** The Glade-like layout with `rc` is refused, since it has no
  count, with a message (CFG-9).
  - *Status:* done.
- **LAYG-5.** Std and the suite pass with the Glade-like layout on both
  engines under `tracing` and `generational`.
  - *Status:* done. On Linux at 4011547, Std passes in nine configurations
    with the Glade-like layout:
    - on the VM: `tracing`, `generational`, and `generational` with the
      smallest nursery, with heap frames too;
    - natively: the same, and in a debug build.

    The six examples print the same in five configurations each, and the
    suite passes at 8da0687.
- **LAYG-6.** Silo's layout's native counted IR is unchanged (PERF-1).
  - *Status:* done, checked by snapshot.
- **LAYG-7.** A value crossing between two layouts is converted. Dropped:
  one runtime holds one layout, so no value meets the other. Compact regions
  and parcels copy within it.
- **LAYG-8.** (should) The Glade-like layout is no slower than Silo's under
  tracing or `generational`.
  - *Status:* faster on binarytrees under both and on rbtree under tracing,
    by 5 to 7%; rbtree under `generational` is 6% slower, yet to be
    profiled.
- **LAYG-9.** A profile key, `layout = "glade"`, and the release archives
  ship the second layout's runtime and VM.
  - *Status:* the profile key is done (CFG-10); the archives are planned.

### R2. A JIT

A third engine, interpreted first and compiled when hot, as configurable as
the others. AOT and JIT are both options, and either may be mixed with the
VM.

- **JIT-1.** `--jit` runs a program on the VM, compiling a procedure to
  machine code once it is entered more than a threshold of times, or once a
  loop head in it is passed more than a threshold of times. Both thresholds
  are options.
  - *Status:* done for entries (`MEADOW_JIT_THRESHOLD`, 1000 by default).
    A loop of tail calls counts each call as an entry. A loop within one
    procedure is not counted yet.
- **JIT-2.** Tier 1 is a baseline compiler. Each VM procedure becomes one
  native function, instruction for instruction, over the same registers.
  An instruction it does not compile is run by the interpreter, and the
  native code carries on.
  - *Status:* begun, x86-64 only (`silo/vm/src/interp/jit.rs`), with
    `MEADOW_JIT` and `MEADOW_JIT_THRESHOLD` and no driver flag yet. Inline:
    - the integer arithmetic and comparisons, and the double arithmetic;
    - reading and writing elements of mutable arrays that are no
      references;
    - returns to the null continuation;
    - tail calls into compiled procedures.

    Every other instruction goes through the interpreter's `step`. Glade's code generator did not
    fit, so tier 1 has a small encoder of its own; Glade's executable memory
    is shared (`compiler/meadow-exec`).
- **JIT-3.** Tier 2 is an optimizing compiler from the lower-level IR (R3),
  in process (LLVM's ORC, or Cranelift; §19). `--jit-tiers 1|2` chooses
  how far to go.
- **JIT-4.** A compiled procedure keeps the VM's frames, records, line table
  and slots. Calls by name go through slots (HOT-2), so a reload replaces
  compiled code as it replaces interpreted code.
- **JIT-5.** A procedure goes back to the interpreter at a safe point when a
  debugger attaches or sets a breakpoint in it, when a reload replaces it,
  and when profiling or coverage begins.
  - *Status:* done for recording: the loop that records (a debugger, a
    profile, coverage) never enters compiled code. A reload's procedures are
    a new segment's, compiled anew.
- **JIT-6.** Hot code can enter its compiled form at a loop head, without
  waiting for the procedure to be entered again (on-stack replacement).
- **JIT-7.** Under every memory strategy, compiled code meets the VM's
  obligations: MEM-GEN-8 under `generational`, and records around waited
  calls under tracing.
  - *Status:* done by construction. Compiled code keeps no value in a
    machine register across a call of `step`, and reads the slots' address
    again after each one.
- **JIT-8.** DIFF-1 and DIFF-2 hold under `jit` in every configuration, with
  the thresholds at 0 (compile everything) and at their defaults.
  - *Status:* DIFF-1 holds with the threshold at 0 under `rc`, `tracing` and
    `generational`, and DIFF-2 under `rc` and `generational` (Windows). The
    script is `scratchpad/jitdiff.sh`.
- **JIT-9.** (should) A long-running loop under `jit` runs within a factor of
  the AOT build's speed, to be set once tier 1 is measured.
  - *Status:* matmul under `jit` takes 912 ms, against 2,097 ms
    interpreted, on the Windows desktop.
- **JIT-12.** (should) A program the JIT cannot speed up runs no slower
  under `jit` than interpreted.
  - *Status:* not met yet: fib, binarytrees and rbtree are 2 to 4% slower.
    That is the driver's lookup at each procedure entry.
- **JIT-11.** The VM without the JIT is no slower for its being there
  (PERF-3).
  - *Status:* done; measured with code layout held still (functions and
    blocks aligned to 64 bytes).
- **JIT-10.** Mixed engines are options:
  - an AOT executable that carries the VM and the JIT, for code loaded into
    it by a reload (hot loading native programs);
  - an AOT program that runs on the VM only what a debugger is stopped in.

### R3. A lower-level IR

- **LIR-1.** An IR between linear AxCut and the back ends, in which a
  layout's arithmetic and a strategy's operations are explicit: loads and
  stores at offsets, counts, polls, records, reloads and barriers.
- **LIR-2.** The LLVM back end reads only the LIR, with PERF-1 holding.
- **LIR-3.** The JIT's tier 2 reads the LIR.
- **LIR-4.** A layout is lowered once, in the LIR, for every back end.

### R4. Cheaper heap frames

- **HF-1.** Under `--stack heap`, frames live on a stack of chunks, as
  Glade's do. A frame that is captured is copied to the heap.
- **HF-2.** fib on the VM under `heap` is within 20% of the growable stack's
  time (today 563 ms against 345 ms).

### R5. The debugger and reloading together

- **DBG-9.** The debugger stops in reloaded code, by line, in every segment,
  and names each frame's segment.
- **DBG-10.** `meadow dap` can watch sources and reload, as `meadow run
  --watch` does.

### R6. Hot loading, further

- **HOT-40.** When a reload is refused, `meadow run --watch` offers to start
  the program again.
- **HOT-41.** A function from a changed type's old shape to its new shape can
  be given. It is applied to old values, which then reach new code.
- **HOT-42.** A native program can be reloaded, through mixed engines
  (JIT-10).

### R7. Retire the old Glade, and rename

- **RET-1.** The REPL runs on one VM for its session (REPL-4).
- **RET-2.** Procedural macros, MeadowBoot's among them, run on the merged
  runtime.
- **RET-3.** `meadow cut` runs on the merged runtime.
- **RET-4.** `meadow exec` of a `.mbc` image is converted, or retired with a
  message.
- **RET-5.** The old Glade's engine, collector and JIT are deleted. Nothing
  builds them.
- **RET-6.** The merged runtime takes Glade's name, directory and
  `--runtime glade`. `--runtime silo` stays for a while as another name for
  it.
- **RET-7.** The CEK machine (`eval/`) stays, as the reference semantics.

### R8. The final report

- **REP-1.** A full account, for Chris, of what was done and how Silo's speed
  was kept, before the branch is offered for merging.

### R9. The runtime in Idyll

The runtime rewritten in Idyll's low-level fragment: C's memory model as the
`Mem` effect (Idyll's `docs/LOW.md`), compiled to C, and proved memory-safe
and then sound. It goes a module at a time, behind the C interface the Rust
runtime already has. The plan, with the compiler's half and the order, is
`docs/MEADOW-IN-IDYLL.md` on master. It waits on Idyll's
`Mem`, its solver and its extraction, and it begins once R1 to R4 have
settled the design here (MI-0).

- **IDY-1.** The layout is an Idyll module. `SILO` and `GLADE` are values,
  and these invariants are theorems: no two fields of a word overlap; every
  kind's number is distinct; the region floor is above every collection's
  mark; the epochs stay below the floor. The table the runtime and the LLVM
  back end read is generated from it.
  - *Check:* the generated table is byte for byte today's
    `meadow_rt::layout` constants, and LAY-1 and LAY-6 hold.
- **IDY-2.** A module in Idyll lands only with every proof obligation in it
  discharged. The only trusted primitives are those listed in the one
  module that names them, each with its specification: the stack switch,
  the atomics, the system's calls, the instruction-cache flush.
  - *Check:* the build fails on an undischarged obligation, and the list is
    one file.
- **IDY-3.** A module replaced in Idyll keeps everything this specification
  already requires:
  - DIFF-1 and DIFF-2 in every configuration of §17;
  - the debugger's and hot loading's tests;
  - PERF-1, since the emitted code does not change;
  - PERF-2 and PERF-3 within noise, measured with code layout held still.
- **IDY-4.** Each module in Idyll also runs on `Mem`'s model, a handler in
  pure Idyll, and agrees with its C on the same tests.
- **IDY-5.** The allocator (`heap::acquire`, `trace::Space::take`, the
  nursery's bump) hands out blocks that are live, of the size asked for, and
  disjoint from every other live block. *(Proved.)*
- **IDY-6.** Tracing frees only what no root reaches. After each operation
  the heap is well formed: every header agrees with the layout, and every
  reference field of a live block is an immediate or the address of a live
  block. *(Proved; MEM-TR-1 to MEM-TR-5.)*
- **IDY-7.** The copier leaves no reference to a moved block's old place,
  and the barrier's obligation is the store's precondition. *(Proved;
  MEM-GEN-4 and MEM-GEN-6.)*
- **IDY-8.** Given that the memory pass consumes each name once, share,
  erase and reuse keep a block's count equal to the references it accounts
  for, so nothing still held is freed. *(Proved; MEM-RC-1.)*
- **IDY-9.** The rest of the runtime is in Idyll: segments, the scheduler,
  regions, parcels, STM, the primitives, printing and big integers. The big
  integers are proved against their arithmetic. Done when `silo/src` has no
  Rust.
- **IDY-10.** The VM's loop is in low-level Idyll, and the JIT's encoder is
  in high-level Idyll, proved against a decoder of the same instructions,
  with VM-11 and PERF-3 holding. Done when `silo/vm` has no Rust.

### Smaller items, any time

- Phone runtimes in the experimental desktop MeadowBoot archives (macOS
  Intel, Linux ARM).
- PERF-5: the same method table order every build.
- The examples' differential as a test (DIFF-1), and the IR snapshot as a
  test (PERF-1).
- Tests for the "done, no test" requirements above.

## 19. Open questions

- **The JIT's tier 2:** LLVM's ORC in process, which is the AOT path's
  optimizer at a large dependency and a slow compile, or Cranelift, which
  compiles fast and optimizes less.
- **What the Glade-like layout's references are:** machine pointers with
  forwarding, or Glade's slot numbers, whose range says the space.
- **Reuse with a barrier:** reuse writes into a block in place, which under a
  young generation is a store into a possibly old block. Either reuse stays
  counting-only, as now, or it takes the barrier.
- **How far layout is configurable** beyond its first two instances: header
  size, inline descriptors, immediates.

## Appendix A. Where the code is

| part | where |
|---|---|
| runtime interface | `compiler/meadow-rt`: `desc`, `roles`, `layout`, `look`, `world` |
| AxCut, its configuration, images, the memory pass | `compiler/meadow-axcut`: `config`, `image`, `linear` |
| lowering, reloadable builds | `compiler/meadow-seq`: `lower_program_with` |
| versions | `compiler/meadow-core/src/versions.rs`, `signature` |
| LLVM back end | `compiler/meadow-llvm/src/emit.rs` |
| runtime library | `silo/src`: `heap`, `trace` (with the nursery), `cycles`, `stack`, `segments`, `shadow`, `frames`, `sched`, `prims`, `show`, `coverage`, `stats`, `profile` |
| the VM | `silo/vm/src`: `interp`, `interp/code`, `interp/debug`, `interp/live`, `interp/profile`, `main` |
| the driver | `buildtools/meadow/src`: `aot`, `dap`, `repl`, `watch`, `runtime` |
| tests | `buildtools/meadow/tests`: `dap_silo`, `hot_reload`, `driver`, `bootstrap`; `compiler/meadow-llvm/tests/native.rs` |
| old Glade | `glade/` (to be retired, R7) |

## Appendix B. Limits and constants

| what | value | where |
|---|---|---|
| reloads a program takes | 1,023 (1,024 segments) | `interp::SEGMENTS` |
| definitions over all loads | 2^18 | `interp::DEFS` |
| segment bits in an id | 22 and up | `interp::SEG` |
| size classes | 128 words | `trace::CLASSES`, `heap::CLASSES` |
| least words between traced collections | 2^20 | `trace::LEAST` |
| growth before the next traced collection | twice what was kept | `trace::GROWTH` |
| tracing epochs before renumbering | 2^27 | `trace::EPOCHS` |
| nursery, default and bounds | 2^18 words; 2^15 to 2^27 | `trace::nursery_words` |
| a growable stack's piece | 64 MB; up to 4 kept | `stack::CHUNK`, `stack::POOL` |
| `main`'s fixed stack | 1 GB | `lib::MAIN_STACK` |
| VM instruction | 8 bytes | `interp/code.rs` |
| image magic | `meadow axcut image 5\n` | `meadow_axcut::image` |
| watching interval | 100 ms (VM); 250 ms (sources) | `silo/vm/src/main.rs`, `watch.rs` |
