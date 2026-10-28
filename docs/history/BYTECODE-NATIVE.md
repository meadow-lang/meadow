# The native runtime: layout and contract

> **History.** This describes the bytecode runtime that had Glade's name until
> the merged runtime took it (`docs/RUNTIME-MERGE.md`, R7). Its code is gone;
> this is kept for the reasoning behind what replaced it.

What machine code has to look like to run on the Meadow runtime: how an
executable is put together, what a native function is handed and must keep
true, where every field it may touch lives, and how objects, addresses and the
frame stack are laid out in memory.

[BYTECODE-RUNTIME.md](BYTECODE-RUNTIME.md) explains how the runtime _works_ and why. This is the
other document: the **contract** a code generator is written against -- the
one in `glade/src/codegen` today, and one written in Meadow later. Every number
here is a constant in the source named beside it, and the source is normative.
The layout is **not stable across runtime builds**; [§9](#9-versioning) says
how that is enforced.

1. [An executable](#1-an-executable)
2. [The object file](#2-the-object-file)
3. [Block functions](#3-block-functions)
4. [The machine, by offset](#4-the-machine-by-offset)
5. [What native code must keep true](#5-what-native-code-must-keep-true)
6. [Values, addresses and objects](#6-values-addresses-and-objects)
7. [The frame stack](#7-the-frame-stack)
8. [Calls, returns and method entries](#8-calls-returns-and-method-entries)
9. [Versioning](#9-versioning)
10. [The smallest correct code generator](#10-the-smallest-correct-code-generator)

## 1. An executable

Three things, linked by the system C compiler:

```text
  <name>.o / .obj     machine code + the bytecode image        (§2)
  <name>-main.c       eleven lines: hands the two symbols to the runtime
  libmeadow_glade.a     the runtime: interpreter, collector, scheduler, primitives
```

`main` is:

```c
extern const unsigned char meadow_code[];
extern const unsigned char meadow_data[];
extern const unsigned char meadow_glade_<hash>;            /* §9 */
const unsigned char *const meadow_runtime = &meadow_glade_<hash>;
extern int meadow_glade_main(const unsigned char *code, const unsigned char *data,
                           int argc, char **argv);
int main(int argc, char **argv) {
    return meadow_glade_main(meadow_code, meadow_data, argc, argv);
}
```

`meadow_glade_main` (`glade/src/aot.rs`) decodes the image, builds the table of
native functions from the block table, runs the program's entry point on the
scheduler with every worker thread, prints the answer unless it is `()`, and
returns `0` -- or prints the failure and returns `1`. `argv` after the program
name is what `Process.argv` answers.

**The executable always contains the image and the interpreter.** Native code
is an acceleration of the bytecode, block by block, never a replacement for
it: any instruction may be handed back to the interpreter (§3), and any pc
without native code is interpreted. A code generator is therefore correct if
it compiles _nothing_, and gets faster as it compiles more (§10).

The system libraries to link are `Format::system_libs` in
`glade/src/codegen/object.rs`; on Windows `main` is compiled `/MD`.

## 2. The object file

Mach-O, ELF or COFF, with **two symbols and no relocations**:

| symbol        | section        | contents                                  |
| ------------- | -------------- | ----------------------------------------- |
| `meadow_code` | executable     | every block's function, one after another |
| `meadow_data` | read-only data | the header, the image, and the two tables |

(Mach-O adds its leading underscore.) `meadow_data`, little-endian:

```text
  offset  0   u64   image length, in bytes
          8   u64   block count
         16   u64   offset of the block table, from the start of meadow_data
         24   u64   method-entry count
         32   u64   offset of the method-entry table
         40         the image: exactly the bytes of the .mbc (BYTECODE-IMAGE.md)
                    zero padding to a multiple of 4
  table       (u32 entry pc, u32 offset into meadow_code)   per block
  table       (u32 entry pc, u32 offset into meadow_code)   per method entry
```

The method-entry table follows the block table directly. The code needs no
relocations because it reaches everything through the machine it is handed
(§4), branches only within `meadow_code`, and calls the runtime through a
pointer the machine holds. The same bytes therefore run from an object file or
from memory a JIT made executable.

## 3. Block functions

A **block** is the code starting at a pc control can enter from elsewhere:
every member of the image's `entries`, its `entry`, every method-table entry,
and every target of `Jump`, `JumpUnless`, `JumpUnlessTag`, `JumpUnlessPrim`,
`JumpUnlessPrimK`, `BrI`, `BrIK` and `BrF` (`abi::block_entries`). Each block
that has native code has one function:

```c
uint32_t block(Vm *vm);
```

in the **System V AMD64** convention on _every_ x86-64 platform, Windows
included, and **AAPCS64** on arm64. It runs from its pc until control leaves
the block, makes `vm->pc` say where control went, and returns a status:

| value | status      | meaning                                                               |
| ----- | ----------- | --------------------------------------------------------------------- |
| 1     | `JUMPED`    | control left the block; `vm->pc` says where                           |
| 2     | `HALTED`    | the program finished; the runtime has the value                       |
| 3     | `FAILED`    | it failed; the runtime has the error                                  |
| 4     | `REQUESTED` | a thread operation is waiting for the scheduler; `vm->pc` is after it |

Native code produces only `JUMPED` itself. The rest come back from the
interpreter and are passed on unchanged.

**Handing an instruction back.** For any instruction it does not do inline,
native code calls

```c
uint32_t meadow_exec(Vm *vm, uint32_t pc);   /* through vm->exec, same convention */
```

which runs the instruction at `pc` exactly as the interpreter would and
returns `0` (`CONTINUE`: control falls through to `pc + 1`) or one of the
statuses above, which the block function must return at once. Before the call
native code must have made the machine true (§5); after it, anything it had
cached from the machine -- a fixed register, an address, the block tables --
is stale and must be loaded again, because the instruction may have collected.

**Bounded.** A block function must return within a bounded number of loop
iterations and chained jumps so the scheduler can preempt: the generator here
counts both and returns `JUMPED` after `BACK_EDGES` (4096) back edges or
`CHAINS` (256) chained jumps in one call.

## 4. The machine, by offset

`Vm` is `repr(C)`, and its first fields, then the first fields of the `Heap`
inside it, are what native code may touch (`codegen::layout`). Offsets in
bytes; everything is 8 bytes unless it says otherwise.

| offset | name             | type                   | native code                                                        |
| ------ | ---------------- | ---------------------- | ------------------------------------------------------------------ |
| 0      | `REGS`           | `*mut u64`             | the register file: 256 registers, then 256 scratch slots           |
| 8      | `LIVE`           | `u64`                  | registers `0..live` are the collector's roots -- §5                |
| 16     | `PC`             | `u64`                  | write before returning `JUMPED`                                    |
| 24     | `AT`             | `u64`                  | the pc being executed; `meadow_exec` sets it                       |
| 32     | `STEPS`          | `u64`                  | instructions retired -- §5                                         |
| 40     | `EXEC`           | function pointer       | `meadow_exec`                                                      |
| 48     | `METHOD_PCS`     | `*const u32`           | every method table's pcs, in a row                                 |
| 56     | `METHOD_STARTS`  | `*const u32`           | where table `t` starts in that row; `t + 1` is where it ends       |
| 64     | `NATIVE_TABLE`   | `*const *const u8`     | per pc: its block function, or null                                |
| 72     | `NATIVE_METHODS` | `*const *const u8`     | per pc: its method entry (§8), or null                             |
| 80     | `BASE`           | `*mut u64`             | the nursery's first slot                                           |
| 88     | `CAP`            | `u64`                  | the nursery's size, in slots                                       |
| 96     | `TOP`            | `u64`                  | the next free nursery slot: allocation bumps it                    |
| 104    | `ALLOCATED`      | `u64`                  | slots allocated, ever: add what you bump                           |
| 112    | `REGION_GROWTH`  | `u64`                  | if `> CAP`, the heap wants a collection: allocate on the slow path |
| 120    | `TABLES`         | `[*const *mut u64; 2]` | block table of generation 0 and of generation 1 -- §6              |
| 136    | `FSP`            | `u32`                  | frame stack top, a heap address -- §7                              |
| 140    | `FLIM`           | `u32`                  | end of the current chunk; `0` when there is no chunk yet           |
| 144    | `FCUR`           | `u32`                  | the current chunk's base address                                   |
| 152    | `FBASE`          | `*mut u64`             | the current chunk's first slot, as a machine address               |

A native function entered through `NATIVE_TABLE` starts at its first byte. One
**chained** to (§8) is entered `WARM` bytes in, past the part of the prologue
that builds the machine frame, which every function builds identically:
`Emit::WARM` is 33 on x86-64 and `48 + 4 × 13` on arm64. A generator that does
not chain can ignore this entirely.

Inside a block function the generator here keeps: the `Vm` in `rbx` / `x19`;
the register file in `r12` / `x20`; uncounted steps in `r13` / `x21`; back
edges and chains in `r14` / `x22`. Those are its own choices, except that a
function chained to must find them as the function chaining left them.

## 5. What native code must keep true

The interpreter, the collector and the scheduler read the machine whenever
native code is not running: at every `meadow_exec`, and at every return. At
those points:

- **Registers.** Every bytecode register the program can still read holds its
  value in the register file. A generator may keep registers in machine
  registers between those points and must write them back before them. (The
  one here keeps `r0`–`r12` in fixed machine registers on arm64, `r0`–`r4` on
  x86-64; BYTECODE-RUNTIME.md has the table.)
- **`live`** is at least one past the highest register written since the block
  was entered, and never lower than the block's entry requires. It only bounds
  the collector's scan -- each pc's `gc_map` says which of those registers hold
  addresses -- so declaring more than necessary is always safe, and declaring
  less loses objects. `Jump` carries the target's count in `a`; `Invoke` sets
  it to captures plus arguments.
- **`steps`** has had one added for every instruction native code retired
  itself. `meadow_exec` counts the ones it runs; an instruction attempted
  inline and then handed back must not be counted twice.
- **`pc`** is where control is, when returning `JUMPED`.
- **Nothing else is held.** No heap address lives in a machine register, on
  the machine stack, or in a scratch slot across a `meadow_exec` or a return:
  a collection moves nursery objects and rewrites only the registers the map
  names. An address loaded again afterwards is the right one.
- **The block tables (`TABLES`) and `FBASE`** are republished by the runtime
  before it returns into native code, so they are safe to load without a
  check, and must not be kept across a `meadow_exec`.

The scratch slots -- the 256 words after the register file -- are native
code's to use within one instruction (the generator here parks `Invoke`'s
arguments and the thin steps' temporaries there). The collector never reads
them.

## 6. Values, addresses and objects

**A register or a field is a 64-bit word with no tag.** What it is comes from
the instruction (typed opcodes), the image's operand descriptors, the pc's
collector map, or the object's header. Words: an `Int` is its two's-complement
bits; a `Float` its IEEE-754 bits; a `Float32` its bits in the low half; `Bool`
is `0` or `1`; unit is `0`; a `Char` is its scalar value; a sized integer is
its value wrapped to its width; an interned name is its key; everything else
is a **heap address**.

**A heap address is a 32-bit slot number**, zero-extended, in one of three
ranges (`old::OLD_BASE`, `region::REGION_BASE`):

| range          | space                                            | how native code reaches slot `a`                |
| -------------- | ------------------------------------------------ | ----------------------------------------------- |
| `0 .. 2^30`    | the thread's nursery                             | `BASE + 8·a`, or through the tables as below    |
| `2^30 .. 2^31` | the thread's old generation, and its frame stack | through the tables                              |
| `2^31 ..`      | a compact region, shared between threads         | not from native code: hand the instruction back |

The two-level lookup (`codegen::addr`; 64 KiB blocks of 8192 slots):

```text
  table = TABLES[a >> 30]                     generation
  block = table[(a >> 13) & 0x1FFFF]          the block's first slot, a machine address
  word  = block[a & 0x1FFF]
```

An object is contiguous in memory, so one lookup finds its first word and the
rest is offsets from it -- including an object bigger than a block.

**An object** (`glade/src/object.rs`), at slot `a`:

```text
  a+0   len (bits 63‥32) | reserved | uniform (bit 12) | uniform desc (bits 11‥8) | kind (bits 7‥0)
  a+1   descriptors of fields 0‥7, 4 bits each (bits 63‥32) | meta (bits 31‥0)
  a+2   descriptors of fields 8‥23, 16 per word          -- only when len > 8 and not uniform
  ...
  a+h   field 0
```

Descriptors are those of [BYTECODE-IMAGE.md](BYTECODE-IMAGE.md#descriptors). A **uniform** object
(an array, a mutable array, a `BigInt`, a string) has one descriptor for every
element and always a two-word header. Otherwise the header is two words up to
eight fields and one more per sixteen after that.

Kinds (`heap::Kind`), by number, and what `meta` is:

| #   | kind       | meta                     | fields                                                        |
| --- | ---------- | ------------------------ | ------------------------------------------------------------- |
| 0   | `Data`     | constructor tag          | the constructor's fields; a tuple is the constructor `#tuple` |
| 1   | `Array`    | 0                        | elements; uniform                                             |
| 2   | `Record`   | --                       | `label, value, …` sorted by label                             |
| 3   | `Closure`  | method table             | captures -- a closure, a continuation or a handler            |
| 4   | `Ref`      | see BYTECODE-RUNTIME.md §5        | one: the value                                                |
| 5   | `MutArray` | 0                        | elements; uniform                                             |
| 6   | `BigInt`   | sign: 0, 1 plus, 2 minus | base-2³² digits, least significant first; uniform             |
| 7   | `Resume`   | --                       | one: the one-shot flag                                        |
| 8   | `Compact`  | region                   | one: the compacted value                                      |
| 9   | `Channel`  | its number               | none                                                          |
| 10  | `Task`     | its number               | none                                                          |
| 11  | `TVar`     | its number               | none                                                          |
| 12  | `Str`      | length in bytes          | UTF-8, eight bytes to a word, little-endian, spare bytes zero |
| 13  | `Bytes`    | length                   | an `Array` of `UInt8`, laid out as `Str`                      |
| 14  | `Frame`    | **return pc**            | captures, as a `Closure` -- §7                                |
| 15  | `Stack`    | --                       | `[top chunk, fsp]`: a detached stack segment                  |

A first word whose kind byte is `0xFF` is a **forwarding** word -- the object
moved, to the address in bits 63‥32 -- and native code never sees one, since
it holds no address across a collection.

**Allocating** an object of `n` words inline: if `TOP + n > CAP` or
`REGION_GROWTH > CAP`, take the slow path (`meadow_exec`, which collects and
allocates). Otherwise the object is at slot `TOP`: write its header and
fields, add `n` to `TOP` and to `ALLOCATED`, and the address is the old `TOP`.
A header can be written inline only when every field's descriptor is known at
compile time (the image's operand descriptors are all below 16 for that
instruction); otherwise hand the instruction back.

**Writing into an existing object** needs the write barriers unless the value
is not an address or the object is in the nursery (`a < 2^30`). Native code
does those two cases and hands everything else back.

## 7. The frame stack

The continuation of a call that is not a tail call is a `Frame`, made by
`Op::Frame` and pushed on the thread's frame stack rather than allocated.

The stack is a chain of **chunks**. A chunk is one 64 KiB block of the old
generation (8192 slots) in state `STACK`, so a frame's address is an ordinary
old-generation address and everything in §6 applies to it. A chunk begins with
a three-slot header (`heap::CHUNK_HEADER`): slot 0 is the address of the chunk
below, or `0`; slot 1 is that chunk's `fsp` when this one was entered; slot 2
is the serial number the chunk was entered with, which is how a `handle` names
its chunk. Frames follow, growing upward: everything in a stack chunk from
slot 3 up to `FSP` is a frame.

**Pushing** a frame of `n` words inline: if `FSP + n > FLIM` -- which is also
true when there is no chunk yet, since `FLIM` is then `0` -- hand the
instruction back, and the runtime starts a chunk. Otherwise the frame's first
word is at machine address `FBASE + 8·(FSP − FCUR)`; write the header (kind
`Frame`, `meta` the return pc: method 0 of the table the instruction names)
and the captures; set `FSP += n`; the frame's address is the old `FSP`.

**Returning** through a frame is `Invoke` on it (§8). If its address `a`
satisfies `FCUR <= a < FLIM` it is in the current chunk: its first word is at
`FBASE + 8·(a − FCUR)`, its kind needs no test, its entry pc is its `meta`,
and popping it -- and everything pushed after it -- is `FSP = a`. A frame in a
chunk below has chunks to release on the way down: hand the instruction back.

Every `handle` starts a chunk of its own (`Prim::Enter`), and a resumption is
captured by unlinking whole chunks (`Prim::Detach`, `Prim::Reattach`); all
three are primitives native code hands back. Frames are collector roots, which
the runtime walks; native code does nothing for that.

## 8. Calls, returns and method entries

`Invoke a b c imm` enters method `b` of the object in `r[a]` with `imm`
arguments from `r[c]`. There is no other call and no return instruction. Its
meaning, which the interpreter implements and any native path must reproduce
exactly:

```text
  n    = the object's len                          captures
  pc'  = methods[meta][b]   (a Closure)   or   meta   (a Frame)
  r[0 .. n]        = the object's fields
  r[n .. n + imm]  = the arguments, in order       (they may overlap r[0..n]: move them out first)
  live = n + imm
  pc   = pc'
  if the object is a Frame: pop it (§7)
```

`methods[t][b]` is `METHOD_PCS[METHOD_STARTS[t] + b]`, valid when
`b < METHOD_STARTS[t + 1] − METHOD_STARTS[t]`.

The **general path** does exactly that in the register file in memory and
returns `JUMPED` -- or, chaining, jumps to `NATIVE_TABLE[pc']`'s warm entry if
it is not null. It is always correct.

The **fast path** avoids the memory traffic. A method whose captures fit a
two-word header (at most 8) and whose block's registers all fit the fixed
machine registers gets a **method entry**: a stub, listed in the object file's
second table and found at run time through `NATIVE_METHODS[pc']`. The caller
puts the `imm` arguments in the fixed registers for `r[0 .. imm]`, the address
of the object's first word in `x12` (arm64), and jumps to the entry. The entry
knows its shape from the image -- `method_captures[t]` captures,
`method_params[t][m]` registers in all -- so it moves the arguments up past the
captures, loads the captures from the object, sets `live`, and falls into the
block's warm entry. x86-64 has too few fixed registers and emits no method
entries; its calls take the general path.

## 9. Versioning

Generated code hard-codes every offset in §4 and every layout in §6 and §7, so
it runs correctly against exactly one build of the runtime. `glade/build.rs`
hashes the runtime's sources, with `meadow-bytecode` and `meadow-core`, and the
library exports one symbol named for the hash: `meadow_glade_<hash>`
(`object::runtime_symbol`). The generated `main` refers to it, so **a runtime
built from other sources fails to link** instead of corrupting a heap. A code
generator outside this repository must be built against the same sources, take
its constants from them, and emit the same reference.

The image has its own version, in its magic ([BYTECODE-IMAGE.md](BYTECODE-IMAGE.md#versioning)).

## 10. The smallest correct code generator

Because every instruction can be handed back, a generator can start trivial
and stay correct at every step:

1. **Emit nothing.** An object file whose block table is empty runs the whole
   program in the interpreter. This alone makes a Meadow-written compiler able
   to produce executables: it only has to write an image (BYTECODE-IMAGE.md) and this
   container (§2).
2. **One function per block that hands every instruction back:** a prologue;
   for each instruction, `meadow_exec(vm, pc)` and return the status unless it
   is `0`; at the block's end, set `pc` and return `JUMPED`. Already faster
   than interpreting, because dispatch is gone.
3. **Do the word-level instructions inline**, on the register file in memory:
   `Move`, `Const` of an immediate, the typed `Int` and `Float` arithmetic,
   compares and branches. Count a step for each (§5). No heap knowledge is
   needed, and this is where most of the speed is.
4. **Then the heap fast paths** with a slow half each -- `Field`,
   `JumpUnlessTag`, allocation, `Frame`, `Invoke` -- every check that fails
   branching to a label that undoes the step count, calls `meadow_exec` for
   that one instruction, and jumps back.
5. **Then** registers in machine registers, chaining, method entries and
   loops, each of which changes what it costs and nothing about what it does.

`glade/tests/differential.rs` and `buildtools/meadow/tests/vm.rs` run the
standard library's tests on the interpreter and on native code and require the
same answer from both, word for word. A generator is right when they pass.
