# How Meadow runs a program: Glade

Meadow has two runtime systems. This is **Glade**, the default, in `glade/`:
the one the interpreter, the JIT and the debugger share. The other, **Silo**,
is compiled all the way down by LLVM and counts references instead of
collecting; [SILO.md](SILO.md) is its design.

This is a tour of everything below the type checker: Glade's three backends, the
memory model they share, how effects are compiled and run, and how threads run
in parallel. It describes the code as it is, and names the files to read for
the rest. The module docs in those files are the detailed reference; this is
the map.

- [1. One program, three backends](#1-one-program-three-backends)
- [2. From core to bytecode](#2-from-core-to-bytecode)
- [3. The backends](#3-the-backends)
- [4. The memory model](#4-the-memory-model)
- [5. Effects](#5-effects)
- [6. Concurrency and parallelism](#6-concurrency-and-parallelism)
- [7. Where to read next](#7-where-to-read-next)

## 1. One program, three backends

Every backend runs the **same bytecode**. Native code isn't a second compiler
with its own view of the language. It translates the bytecode's instructions
into machine code, does the simple ones itself, and hands the rest back to the
interpreter one instruction at a time. So the heap, the collector, effects and
the scheduler work the same way whichever backend is running. The only
difference is how much of the work happens in machine code.

| backend | what runs                                                                                         | default for |
| ------- | ------------------------------------------------------------------------------------------------- | ----------- |
| `vm`    | the bytecode interpreter                                                                          |             |
| `jit`   | the interpreter, compiling each block to machine code once it has been entered 16 times           | `--debug`   |
| `aot`   | an executable: machine code for every block, the bytecode image, and the runtime, linked together | `--release` |

`--backend`, `--jit` and `--aot` choose a backend for one command, and
`backend = "..."` under `[profile.<name>]` in `Meadow.toml` chooses one for a
package. The CEK machine (`--cek`, in `eval/`) is separate. It is the
reference semantics, and every backend is tested against it.

```text
  source ─► parse, rename, infer ─► core
                                     │  meadow-core: specialize, joins, globals, lower
                                     ▼
                                   AxCut (meadow-seq)       seven statements, no expressions
                                     │  meadow-codegen
                                     ▼
                                   bytecode (meadow-bytecode)   8-byte instructions, 256 registers
                                     │
               ┌─────────────────────┼──────────────────────┐
               ▼                     ▼                      ▼
          interpreter        JIT: hot blocks →        AOT: all blocks →
          (glade/src/vm.rs)    machine code in memory   object file + runtime.a
                             (glade/src/jit.rs)         (glade/src/codegen/object.rs)
               └──────── same heap, same scheduler, same natives ────────┘
```

## 2. From core to bytecode

Three decisions made before the runtime sees anything shape everything after.

### The call stack is a frame stack

The backend IR is **AxCut** (`compiler/meadow-seq/src/lib.rs`), from Schuster,
Müller, Ostermann and Brachthäuser, _Compiling Classical Sequent Calculus to
Stock Hardware_ (OOPSLA 2025).

AxCut is a classical sequent calculus in a normal form, and it does **not**
eliminate cuts — it restricts them. A cut is a redex, so cut elimination is the
reduction relation itself and a program with no cuts has already run. What the
normal form does is push every cut until a variable — an axiom — stands on one
side, which is what the name says. There is no `Cut` node in the grammar because
each of the seven statements _is_ a cut with the rule it meets folded in: `let`
and `new` are cuts against an activation rule, `switch` and `invoke` cuts against
an axiom.

In the paper that pairing is also the memory discipline — activation acquires,
deactivation releases, and `substitute` adjusts reference counts — so an AxCut
program frees its own memory. Meadow does not take that half: it has a
generational collector (section 4), so `switch` and `invoke` release nothing and
the linear environment is not enforced. What Meadow takes is the _shape_.

Before the lowering, `meadow_core::joins` names the continuations that several
places share. A `let f = \x -> … in …` where every mention of `f` is a
saturated call in tail position is not a function -- nothing can hold it,
nothing can pass it anywhere, and nothing runs after it answers. It becomes a
`Term::Join`, and lowering gives it a labelled block that captures nothing:
what it needs from around it is still in the environment where it is entered,
so it is passed at the jump. Both branches of an `if` then _jump_ to one block
instead of each building an object and invoking it.

The saving today is that object. The reason to have the form is case-of-case,
which has to put the context it pushes inwards somewhere: copied into every
branch a program can square in size, and built as a closure it allocates.
Naming it is the third answer, and it is why GHC has join points.

In AxCut a function receives the continuation to answer, and _returning is
invoking that continuation_. A closure, a continuation and an effect handler are
all the same kind of heap object: codata with a method table and captured
values.

So the bytecode has no `call` or `ret`. `Op::Invoke` is the whole calling
convention: rebuild the register file as the object's captures followed by its
arguments, then jump to the method. Every call is a tail call.

What a non-tail call needs is a continuation, and where that continuation
lives is the one thing here that changed. It used to be a heap object -- four
words allocated per call, collected later. It is now a **frame** (`Op::Frame`,
`Kind::Frame`): the same object, laid out as a closure so that `Invoke` enters
it exactly as it enters a closure, but written into a per-thread **frame
stack** and reclaimed by moving the stack's top back when the function returns
through it. Lowering knows which `new`s are these -- `meadow_seq::Program::frames`
-- because a continuation made for a call is entered once, by that call
returning, and everything pushed after it is dead by then.

The stack is chunked, as GHC's is: 64 KiB chunks that are old-generation
blocks in state `STACK`, linked through a three-slot header, so a deep
recursion grows a chain and never copies a frame, and a frame's address is an
ordinary old address that every part of the runtime already knows how to
reach. A frame in the current chunk is one load from the chunk's published
base (`layout::FBASE`); returning through one is a jump to the pc the frame
carries as its `meta`, with no method table to look up. Frames are collector
roots, walked by the heap that owns them at every nursery collection, at the
start of every marking cycle, and at every evacuation -- a frame is pushed
with a bare store, so no pointer in one is ever recorded as pointing into a
block chosen to move, and evacuation has to rewrite them as it does the
registers. The marker never reads one while the program pushes and pops.

A nursery collection does not walk the whole stack. Only the current chunk is
ever pushed into or popped from, so a chunk below it that a collection has
found to hold nothing young goes on holding nothing young until it is the
current chunk again, and is skipped (`Heap::clean_chunks`; GHC keeps a dirty
bit per stack chunk for the same reason). Without that, a deep recursion that
allocates rescans every frame at every collection and is quadratic. A
survivor can stay young for one collection, so a chunk may take two scans to
come clean; a pop, a `detach` or a `reattach` un-cleans exactly the chunks it
makes current or frees; and a whole collection, which also marks the regions
frames point into, scans them all.

Every `handle` enters a chunk of its own, and that is what makes effects work
on a stack -- see [section 5](#5-effects). The stack alone moved `binarytrees`
11% and the call-bound benchmarks not at all, which said where the remaining
cost was: the convention around the call, where arguments travelled through
the register file in memory and every `Invoke` rebuilt that file one word at a
time. Native code no longer does that -- see [Method entries and the fixed
registers](#method-entries-and-the-fixed-registers) -- and `fib` is 1.8x
faster for it. The interpreter still does, and the two agree by construction:
the method entry lays out exactly the registers `Op::Invoke` would.

### Tail recursion modulo cons

`compiler/meadow-core/src/trmc.rs`.

Every tail call is a jump for free -- there is no `call` to optimise away --
but `f x :: map f rest` is not a tail call: the cons waits for the recursive
call, so a `map` over a million elements pushes a million frames. The stack
never overflows, being chunks on the heap, but those frames are live until the
end, and each one is a push, a return and a scan at every collection.

From O1, after `simplify`, `meadow_core::trmc` rewrites every top-level function
whose tail position is a constructor with a saturated call of itself in one
field (the last such field, with only values after it, so that nothing is
reordered). It makes a twin in destination-passing style, `map_dps dst i f xs`,
which writes what `map f xs` would have answered into field `i` of `dst`: at the
constructor it builds the cell with a placeholder in that field, writes the
cell into `dst`, and continues with `map_dps cell k …` -- a tail call, so the
whole list is built front to back in a loop. A tail call of `map` itself
continues with the same destination; anything else is written into it. `map`
itself changes only at the constructor, which builds the first cell, hands it
to the twin, and answers it.

The write is `Prim::SetField`, which on the bytecode machine is the heap's
barriered field store (`Heap::set_field`): a cell that was promoted while the
recursion ran is remembered like any old object that comes to point at a young
one, and the store takes the mutation lock while a marking cycle runs. The
placeholder is a value of the field's own type, so that the cell has the
representation its type says at every moment and nothing downstream has to know
about holes: in the twin it is `dst`, which costs nothing; for the first cell,
a nullary constructor of the type (`Nil`), and a type with none is not
rewritten. Nothing can observe the write: until the outermost call answers,
the chain of cells is reachable only from its frame, and resumptions are
one-shot. The sequent machine lets a constructor's fields be written for this
one primitive; the CEK machine never sees it, since it runs core as lowering
wrote it.

On a `map` of a million-element list: 3.0s → 1.07s on the interpreter, 856ms →
655ms on the debug JIT, and even in a release executable. `MEADOW_NO_TRMC=1`
turns the pass off.

### A register is a word, and types become descriptors

Registers and object fields hold 64-bit words with no tags. An `Int`, a `Float`
and a heap address look the same. Whoever needs to know what a word is gets
told:

- **Typed instructions** (`AddI`, `CmpF`, `BrIK`, …) are emitted where the
  types are known, and their opcode says what the operands are.
- **Operand descriptors** (`Program::operands`) are recorded beside the
  instructions that need them, such as a generic primitive, `show`, or a
  `halt`.
- **GC maps** (`GcMap`) at every instruction that can collect say which
  registers are live and what each holds: a reference, a scalar, or "whatever
  the descriptor in register _n_ says".
- **Object headers** carry a 4-bit descriptor per field (see
  [the object layout](#objects)).

Code that is generic over a type variable doesn't know at compile time whether
an `a` is an address or a number. So in a **debug** build a generic definition
takes one hidden _descriptor_ argument per type variable
(`meadow_core::desc`: `REF`, `INT`, `FLOAT`, `STR`, …), passed at each
instantiation. A **release** build specializes instead
(`meadow_core::specialize::release`). It makes one copy of generic code per
_representation_: one for `Int`, one for `Float`, and one shared by every
reference type. After that no descriptors are passed, and arithmetic in
formerly generic code becomes typed instructions. This matters for native code
further down: typed instructions and objects with headers known at compile time
are what the code generator can do inline.

### Traits are dictionaries, and then they are not

`compiler/meadow-infer/src/traits.rs`, `compiler/meadow-core/src/dictionaries.rs`.

A `trait` is lowered to a **dictionary**: one constructor, holding the
dictionaries of the traits it requires and then its methods. An `impl` is a
top-level value of that type -- or, with a `where` of its own, a function from
dictionaries to one -- a method is the function from a dictionary to its
field, and a function with a `where` takes a dictionary per trait before its
other arguments. Inference works out which dictionary every mention is applied
to (`InferResult::evidence`); lowering writes the applications. An associated
type is one more type parameter of the dictionary's type, settled by the
`impl`, so nothing below inference knows there are associated types. That is
the whole of what traits are at run time, and it needs nothing new from any
backend: data, closures and calls.

`meadow_core::dictionaries` then takes most of it away again. It is the first
pass of `lower_program`, from O1, while calls still say exactly which types
they are at. Where a function of dictionaries is applied to **known** ones --
an `impl`'s own value, or the copy of a parameterized `impl` at known ones --
it is copied for those types and those dictionaries, and the call becomes a
call of the copy; in the copy a method selected from a known dictionary is the
`impl`'s method itself, a direct call the inliner can see. Copies ask for
copies until nothing is left to ask for; the generic original stays for a
caller that does not know its types, as it does under `specialize`. A debug
build makes at most 16 copies of one function; a release build is bounded only
in depth, which only polymorphic recursion reaches. `MEADOW_KEEP_DICTIONARIES=1`
turns the pass off. On a loop that does nothing but call methods: 1.97s → 0.41s
on the debug JIT, 5.9s → 2.0s on the interpreter, 676ms → 277ms as a release
executable.

The operators go through the same machinery. `Std.Ops` defines `+`, `==`, `<<`
and the rest as methods of traits whose `impl`s are a primitive of their
parameters and nothing else (`fun (+) x y = _primAdd x y`), so the pass also
rewrites a saturated call of such a method -- once a known dictionary has
turned the selection into a direct call -- into the primitive itself, carrying
the `impl`'s types: `x + y` at `Int` reaches bytecode as the `AddI` it was when
`+` was built in. `impl Eq a`, the one `impl` for every type, is a dictionary
with type parameters and no dictionary parameters, copied at known types like a
function and known from then on. A program that does not depend on `Std` has
no operator traits at all: the resolver falls back to the `_prim` primitive an
operator names where no definition of it is in scope.

The CEK machine runs the program as lowering wrote it, dictionaries and all,
which is what every differential test compares the copies against.

### Effects are gone before codegen

`handle` and `perform` lower to evidence passing: ordinary objects, data and
jumps. The bytecode has exactly one effect instruction, `Op::Native`, for an
operation that no handler answers. [Section 5](#5-effects) covers the details.

## 3. The backends

### The interpreter

`glade/src/vm.rs`. The machine is a program counter, a boxed array of 256
registers plus a scratch area, `live` (how many registers are roots), a heap,
and a pointer to the program. It has no stacks of any kind. Instructions are a
fixed 8 bytes: opcode, three register operands, and a 32-bit immediate.

`Vm::step` decodes and runs one instruction. `Vm::advance`, which is what the
scheduler calls, checks for native code first:

```rust
pub fn advance(&mut self) -> Result<Option<Value>, Error> {
    match self.native.and_then(|n| n.at(self.pc)) {
        Some(f) => crate::abi::enter(self, f),   // run a whole block natively
        None => self.step(),                     // or one instruction
    }
}
```

That one `match` is the whole integration between interpreted and native code.

### Native code: blocks and block functions

`glade/src/abi.rs`, `glade/src/codegen/mod.rs`.

A **block** starts at any pc that control can enter from elsewhere: a
definition, a method entry, or the target of a jump or branch
(`abi::block_entries`). Each block compiles to one native function:

```c
u32 block(Vm *vm);   // System V on every x86-64 OS, AAPCS64 on arm64
```

The function runs from its pc until control leaves. Then, from O1, it jumps
straight into the native function for the new pc if there is one (_chaining_,
below); otherwise, or at O0, it sets `vm->pc` and returns a status:

| status      | meaning                                               |
| ----------- | ----------------------------------------------------- |
| `JUMPED`    | control left the block, and `pc` says where           |
| `HALTED`    | the program finished, and the value is in the machine |
| `FAILED`    | it failed, and the error is in the machine            |
| `REQUESTED` | a thread operation is waiting for the scheduler       |

Native code reaches the machine only through fixed offsets into the `repr(C)`
`Vm` and `Heap` (`codegen::layout`). It reaches the interpreter through a
function pointer stored in the `Vm` (`vm->exec`). It branches within its own
function, or, chaining, to another function: through the table the `Vm` points
at (`vm->native_table`), or, in an AOT object, with a jump whose offset is
known because every function is in the same code. So **the code needs no
relocations**. The same bytes work
whether they sit in an object file or were just copied into memory made
executable. That's what lets the JIT compile one block at a time and put it
anywhere.

#### What is done inline, and what is handed back

| inline, in machine code                                                                                                                                                                     | handed to the interpreter via `meadow_exec(vm, pc)`                                                                 |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `Move`, and `Const` for immediates                                                                                                                                                          | `Const` for strings and `BigInt`s                                                                                   |
| typed `Int`/`Float` arithmetic, shifts and comparisons; `popCount`, `>>>` and `toFloat` on an `Int`                                                                                         | generic `Prim`, `PrimK`, `JumpUnlessPrim(K)`                                                                        |
| `Jump`, `JumpUnless`, `BrI`/`BrIK`/`BrF`                                                                                                                                                    | `Ref` operations, `compact`, STM and thread primitives                                                              |
| `JumpUnlessTag`, `Field` and `Invoke` on a nursery **or old-generation** object                                                                                                             | the same on a region object; `Field` of a data object with more than 8 fields                                       |
| `stGetArray`, `arrayGet`, `stSetArray` (non-reference elements, or a **young** array), `getRef`, `stArrayLen`, `arrayLen` and `stringByteLength`, through the thin steps of `codegen::thin` | `stSetArray` of a reference into an old array, which needs the write barriers; `setRef`                             |
| `MakeData`/`MakeArray`/`Closure` with a **static header**, if the nursery has room -- a header of any length                                                                                | the same with descriptors from registers, or a full nursery                                                         |
| `Frame` with a static header, if the chunk has room; `Invoke` of a frame in the current chunk                                                                                               | a `Frame` that overflows its chunk, and a return through a frame in a lower chunk, which releases chunks on the way |
|                                                                                                                                                                                             | `MakeRecord`, `Select`, `Extend`, `Native`, `Halt`, `Error`                                                         |

The heap instructions use a **fast path with a slow half**. For `field`, for
example, the aarch64 code reaches the object -- a nursery address is one load
off the nursery base; an old one is two more, through the per-generation
**block table** the heap publishes at `layout::TABLES` (`Heap::tables`), chosen
by the address's generation bit -- loads the header, checks the kind and the
bounds, and loads the word. An array element is reached the same way once,
for the object, and then by an offset: every object is contiguous in memory,
including one bigger than a block, which the old generation lays out in a
run of blocks in one allocation (`old::Mem::Part`). Any check that fails branches to a slow label placed
after the function body. There it undoes the step count, calls `meadow_exec`
for that one instruction, and jumps back. The block table is a `Vec` that moves
as blocks are added, so `meadow_exec` republishes it before it returns: there
is no other way back into native code, and that is what makes the table safe to
read without a check.
Allocation works the same way: bump `TOP` by the object's size, and if that
passes `CAP` (or the heap has put enough into regions that it wants a
collection first), take the slow path, where the interpreter collects.

`meadow_exec` runs the instruction exactly as the interpreter would and returns
`CONTINUE` if control falls through. Any other status makes the native function
return it at once.

#### Method entries and the fixed registers

Bytecode registers `r0` to `r12` live in machine registers in every native
function alike -- the **fixed registers**: `x2`–`x8` and `x23`–`x28` on arm64,
`r8`–`r11` and `r15` for `r0`–`r4` on x86-64 -- and the rest live in the
register file in memory. They are loaded from the file where the machine
enters native code (the prologue), written back before every call into the
interpreter and every return, and loaded again after every call; a function
going on to another (chaining, or a call) moves nothing. On arm64 a register a
function only ever does float arithmetic on lives in one of `d16`–`d31` for
that function's duration, moved in at its warm entry and back out before it
goes on, so `fmul d17, d17, d18` is what a `mulf` on two of them is.

A call passes its arguments in them. `Op::Invoke`'s fast path puts the `argc`
arguments in `r0..argc` and jumps to the method's **entry** -- a stub after
the method's block function, found through a second table the `Vm` points at
(`vm->native_methods`, by pc) -- with the object's first word's address in a
scratch register. The entry knows its own shape, which the compiler records
per method table (`Program::method_captures`, `Program::method_params`): it
moves the arguments up past the captures, loads the captures from the object,
sets `live` to what the block takes, and falls into the block's warm entry.
Nothing goes through memory. A **return** is the same thing for a frame: a
frame in the current chunk is recognised by its address alone -- everything
in a stack chunk is one -- popped by moving the stack's top back to it, and
entered through the entry of the block its `meta` names.

A method gets an entry when its captures follow a two-word header (at most
`compact::INLINE_DESCS`, 8, since past that the descriptors spill into further
header words) and its block's registers all fit the fixed ones. Any other
method, an `Invoke` whose pc has no native code yet, a frame in a lower chunk,
and a chain that has run out of budget take the **general path**, which
rebuilds the register file in memory as the interpreter does and goes on to
the block's warm entry -- or the interpreter, exactly as before. The
interpreter's `Op::Invoke` never changed; native code's fast path lays out the
same registers it would.

#### Vector loops

A loop the compiler has made a jump -- `St.forRange lo hi (\i -> ...)` and
its like, after `meadow_core::inline`'s loop specialisation -- is looked at
by `codegen::vector` at O2. Where its body is index arithmetic on invariants
and the induction register, reads and writes of arrays that never change
during the loop, and float arithmetic on what was read, it gets a **plan**:
the arrays, the induction register and its bound, each instruction of the
body in vector form, and what has to be true first. The architecture's code
emits the plan as a _preheader_ and a loop that does two iterations a trip on
`q` registers, placed just before the scalar loop's header, which it falls
into for the rest -- the last iteration of an odd count, or every one if a
check fails. Control from outside the loop enters through the preheader; the
loop's own jump back does not.

The preheader checks, once, what the scalar code checks on every access: each
array is the kind expected, uniform, with `Float` elements; every index the
loop will use is within its array, found by running the body's index
arithmetic at the induction register's first and last values and checking at
each access (an index is the induction register plus an invariant, so those
are its extremes, and an unsigned compare rejects a negative one); and an
array written is not the same object as one read at a different index under
another register. Then each array's base address is kept in `x12`–`x17`
(free inside the body: nothing in it walks a table or looks up a method),
its length in a `d` register, invariant floats are broadcast into vector
registers, and the body runs with lanes in `v0`–`v7`. The scalar loop is
never changed. `MEADOW_VECTOR_DEBUG=1` at compile time prints every loop
considered and, for one refused, the line of `vector.rs` that refused it.

#### Bookkeeping native code keeps up

Native code updates the machine state an instruction changes: the register it
writes, `live` when that write raises it, `pc` when control leaves, and `steps`.
The code generator follows the compiler's `OptLevel`, and every level has the
same observable behavior:

- **O0** is a literal translation, one instruction at a time. Use it to bisect
  a suspected miscompilation.
- **O1** (debug) settles `steps` and `live` only where they can be observed:
  before a call into the interpreter, a return, a branch, or a join
  (`codegen::Book`). A run of arithmetic between two such points costs one
  addition to `steps` and at most one write to `live`. O1 also uses short
  encodings for constant operands, and **chains**: where control leaves a
  function for a pc that has native code -- a `Jump`, a branch or fall-through
  out of the function, or `Invoke`'s fast path -- it writes pinned registers
  back and jumps to that function's _warm entry_, just past the prologue that
  builds the frame, instead of returning to `advance`. The frame, `steps` and
  the loop count carry on. The target is a direct jump in an AOT object, and a
  load from `vm->native_table` otherwise (null: return as before). A call and
  its return therefore cost a few jumps rather than two trips through the
  scheduler loop: `fib 35` runs 1.9x faster for it.
- **O2** (release) adds **regions**: the loops a block belongs to are pulled
  into its function, so a loop that the bytecode lays out as several blocks
  goes round in machine code instead of returning to `advance` at every block.

At every level the fixed registers (above) are where `r0`–`r12` live, and
arithmetic, comparisons and branches on them are done in place -- `add x3, x4,
x5`, `fmul d17, d17, d18` -- with only a register that lives in memory going
through a scratch register.

A loop pays for `live` once, not per trip round. A jump to an earlier pc
declares `live` for the whole loop -- one past the most registers anything
from the target to the jump writes (`codegen::loop_live`) -- and every other
way into the loop's header raises `live` to that, so the header knows it and
nothing inside raises it again. Declaring more than the compiler said is
safe: the collector reads each pc's map for which registers hold addresses,
and `live` only bounds it.

A loop inside one function counts its back edges, and after `BACK_EDGES`
(4096) iterations the function returns anyway. Chained jumps are counted with
them, and after `CHAINS` (256) the chain returns too. That keeps any single call
into native code bounded, so the scheduler always gets control back
([section 6](#preemption)).

Machine registers inside a block function:

| role                                                      | arm64                                           | x86-64                              |
| --------------------------------------------------------- | ----------------------------------------------- | ----------------------------------- |
| the `Vm`                                                  | x19                                             | rbx                                 |
| its register file                                         | x20                                             | r12                                 |
| steps not yet added to the `Vm`                           | x21                                             | r13                                 |
| back edges and chained jumps this call                    | x22                                             | r14                                 |
| scratch for the current instruction                       | x9–x17, d0, d1                                  | rax, rcx, rdx, rsi, rdi, xmm0, xmm1 |
| fixed bytecode registers `r0`–`r12` (`r0`–`r4` on x86-64) | x2–x8, x23–x28; d16–d31 for a function's floats | r8–r11, r15                         |

### The JIT

`glade/src/jit.rs`. `Native::jit(program, threshold, opt)` starts with an empty
table: one `AtomicPtr` per pc. Each time `advance` reaches a block entry with
no function yet, it bumps that pc's counter. On the `threshold`th entry
(`MEADOW_JIT_THRESHOLD`, default 16) it compiles the block:

1. It takes the arena lock and checks the table again, since another thread may
   have compiled the block meanwhile.
2. It runs `codegen::compile_block(program, host arch, pc, opt)`.
3. It copies the bytes into executable memory and publishes the address into
   the table with an atomic store.

Code that runs only once, which is most of what a program does at startup, is
never compiled. A loop is compiled within a few iterations. The `opt` level is
the build profile's: O1 for debug, O2 if you run `--release --jit`.

Compiling a block needs a few facts about the whole program -- which blocks
loop, which instructions are jumped to, what each method takes. Those are
worked out once, when the JIT starts (`codegen::Whole`), not per block: with
the standard library linked in a program has tens of thousands of
instructions, and recomputing them for every hot block made closure-heavy code
two and a half times slower than it is.

Functions are **packed together** in executable memory, as a linker would lay
them out. Giving each its own mapping started every one on a page boundary --
a 64 KiB one on Windows -- so every function's entry had the same low address
bits and they all competed for the same few ways of the instruction cache and
the branch predictor; that made the JIT half again slower than the same code
ahead of time. No address is ever both writable and executable:

- **macOS** uses one `MAP_JIT` mapping, 1 MiB at a time. Only the thread writing
  it sees it as writable, and only while it writes
  (`pthread_jit_write_protect_np`), so other threads keep running the code
  already there. `sys_icache_invalidate` then flushes the processor's
  instruction cache.
- **Linux** maps each 1 MiB chunk twice from a `memfd`: once writable, for the
  compiler, once read+execute, for the program, with `__clear_cache` on arm64.
- **Windows** does the same with a section backed by the page file and two
  `MapViewOfFile`s, then `FlushInstructionCache`.

Where the system will not make the pair of views, each function gets pages of
its own after all, written and then made executable.

Code is never patched or freed while the program runs, so a thread executing a
function never races with anything.

#### Guessed calls

The types say what kind of thing an `invoke` enters, never which: a function
value is whatever was passed in, and a return goes wherever the caller said.
The generic `invoke` finds everything out at run time -- the object's kind,
its length, its method table, where the target's code is -- and rebuilds the
register file in a loop. Most call sites only ever enter one thing, though:
three quarters of the calls through closures in parsing code.

So while a block is still interpreted, each `invoke` in it notes what it
entered (`Native::observe`): one word per site, holding the first thing seen,
and "more than one" for good once a second is. When the block is compiled, a
site that saw one thing gets a guard on it (`Emit::invoke_known`): the object
is a closure made with that method table, or a frame returning to that pc. If
the guard holds, everything the generic path computes is a constant -- how
many captures, how long the header, where the target is -- and the call is a
few unrolled moves and a jump. If it fails, the generic `invoke` follows,
still native. A call through a closure in a tight loop runs about a fifth
faster for it.

`MEADOW_JIT_GUESS=0` turns guessing off, for telling whether a guess is what
broke something. x86-64 has guarded calls; on arm64 every call is generic for
now.

#### Profile-guided `aot` builds

An `aot` build compiles before anything has run, so it has nothing to guess
from unless a run leaves what it saw behind. `meadow run --train` runs on the
JIT and writes every call site that entered one thing to
`target/<profile>/calls.pgo`; an `aot` build of the same image reads it and
guards those calls as the JIT does, and says so (`Guided by …`). The file names
a hash of the image it was made from, since its pcs mean nothing in any other:
after the source changes the profile is ignored, with a note, until the
program is trained again. See `buildtools/meadow/src/pgo.rs`.

```text
meadow run --release --train     # run on the JIT, write target/release/calls.pgo
meadow build --release           # Guided by target/release/calls.pgo
```

### AOT executables

`buildtools/meadow/src/aot.rs`, `glade/src/codegen/object.rs`, `glade/src/aot.rs`.

`meadow build --release` (or `--aot`) compiles every block with
`codegen::compile` and writes an object file (Mach-O, ELF or COFF) with two
symbols:

```text
  meadow_code    the machine code for every block, one after another
  meadow_data    u64 image length │ u64 block count │ u64 table offset
                 the bytecode image
                 (u32 entry pc, u32 code offset) per block
```

A generated `main.c` calls `meadow_silo_main(meadow_code, meadow_data, argc,
argv)`, and the system C compiler links it with `libmeadow_glade.a`.
`meadow_silo_main` decodes the image, fills a `Native` table from the block list
(`Native::ahead_of_time`), and runs the same scheduler as `meadow run`.

The image holds **only what `main` reaches** (`meadow_core::prune`): every
definition of the package and its dependencies is in the program the front end
links, `Std` included, and pruning drops the ones no path from the entry point
names before anything is lowered. A program that prints `fib 30` is a 1 MB
executable with a 1 KB image rather than 23 MB and 3 MB, and builds in half the
time. `meadow run` and `meadow dis` prune the same way, and so does every REPL
entry. `prune = false` in a `[profile.<name>]`, or `--no-prune`, keeps
everything; `meadow test` and the debugger never prune, since they start from
more places than one. The runtime builds a few constructors by name
(`Maybe.Just`, `Result.Ok`, tuples, `List` and `Vector` shapes); lowering gives
those a tag in every program (`RUNTIME_CTORS`), so a pruned program that never
names them can still receive them.

So an executable **still contains the interpreter and the bytecode**. Native
code needs them for every instruction it hands back, and for any pc that isn't
a block entry, such as the instruction after a thread operation returns (see
[below](#thread-operations-from-native-code)). What AOT removes is compiling
at run time and interpreting the common instructions.

**The runtime library has to match the code generator.** The generated code
hard-codes the `Vm` layout, so a runtime built from different sources would
corrupt memory. `glade/build.rs` hashes the runtime's sources (plus
`meadow-bytecode` and `meadow-core`), and the library exports a symbol named
`meadow_glade_<hash>`. The generated `main.c` refers to that symbol, so a
mismatched library fails at link time, and `meadow` then tries the next place a
runtime library could be. A release `meadow` embeds its own runtime library and
writes it to `~/.meadow/lib/meadow_glade_<hash>/` the first time it links, so
nothing else needs to be installed.

## 4. The memory model

`glade/src/heap.rs`, `object.rs`, `old.rs`, `mark.rs`, `evacuate.rs`,
`region.rs`.

### One heap per green thread

Every green thread has its **own heap**, and only the OS thread currently
running that green thread touches it. So there is no locking in the allocator
or the collector, and a collection pauses only the thread that owns the heap
([section 6](#no-shared-mutable-heap)). A spawned thread starts with a small
heap (`THREAD_INITIAL`, 1,024 slots) that grows as needed.

A `String` is an ordinary object on the heap, collected like any other: its
UTF-8 bytes packed eight to a word, with its length in the header
(`glade/src/text.rs`). An array of `UInt8` is kept the same way, a byte to an
element (`Kind::Bytes`), whenever it has elements: every array primitive reads
either kind, and the one that builds an array packs it if its elements are
bytes. A string literal is made the first time a thread loads it,
and that thread keeps it for every later load.

Two kinds of memory are outside every heap:

- **Interned names**, kept for the life of the process: record labels and the
  keys effect operations are dispatched on. Their word is the intern id. There
  are only as many as the program's source names.
- **Compact regions**, which are shared, read-only and reference-counted
  ([below](#compact-regions)).

### Values and addresses

A `Word` is 64 bits. An address is a **slot number**, not a machine pointer,
and its range says which space it points into:

```text
  0 ............ 2^30   nursery          (moves)
  2^30 ......... 2^31   old generation   (Immix blocks, doesn't move while marking)
  2^31 ......... ∞      shared regions   (never move; the same address in every heap)
```

Native code turns a nursery address into memory as `base + addr × 8`, and
checks `addr < 2^30` before using its fast paths.

In Rust, a `Value` is a word together with what it is (`Value::from_bits(word,
desc)` / `value.bits()`). The primitives, `show` and the natives work with
`Value`s. The machine itself stores only words.

<a name="objects"></a>

### Objects

Every slot is one word. An object is a header followed by its fields, and a
field holds only a value's bits:

```text
  a+0   len (32) │ reserved (19) │ uniform (1) │ uniform desc (4) │ kind (8)
  a+1   descriptors of fields 0..8 (32)        │ meta (32)
  a+2   descriptors of fields 8..24, 16 per word -- only when needed
  …
  a+h   field 0
  …
```

`kind` is one of Data, Array, Record, Closure, Ref, MutArray, BigInt, Resume,
Compact, Channel, Task and TVar. What `meta` holds depends on the kind: a
constructor tag, a method table index, a sign, or a region or channel number.

Arrays, mutable arrays and `BigInt`s are **uniform**: one descriptor covers
every element, so the header is two words at any length. Other objects have a
descriptor per field, so the header is two words up to eight fields, plus a
word per sixteen fields after that. Because the collector finds references by
descriptor, one scan loop serves every kind of object. There are no per-kind
tracing rules.

### The one rule: never hold an address across an allocation

Nursery objects move. An address kept anywhere the collector can't see (a Rust
local, or a machine register) is wrong after a collection. So every operation
that allocates follows the same three steps:

1. Measure how much it will allocate.
2. `Vm::ensure(slots)`, which may collect and move everything.
3. Re-read its arguments from registers, which the collector has updated, and
   only then allocate.

The **roots** are registers `r0..live`, filtered by the GC map of the
instruction being run, plus the per-thread globals cache (top-level `def`s
evaluated once per thread, `meadow_core::globals`) and anything a debugger
pinned. `MEADOW_GC_VERIFY=1` overwrites every register the map leaves out, so a
map that forgets a live register fails immediately instead of rarely.

### The nursery

New objects are bump-allocated into the nursery: a bounds check and an
increment. When the nursery fills, it is collected with **Cheney's
algorithm**. The roots are copied to the other half-space, and the copies are
scanned as a queue, copying whatever they reach. The cost is proportional to
what survives, which is usually very little.

- Until a heap reaches its **largest nursery** (`MEADOW_GC_NURSERY`, default
  32,768 slots, or 256 KiB), the nursery simply grows. Most green threads never
  need an old generation and never pay for one.
- After that, an object that has already survived one nursery collection is
  **promoted** to the old generation at the next one. The fixed nursery size
  bounds the nursery pause.
- An object bigger than the whole nursery goes straight into the old
  generation.

### The old generation: Immix, marked concurrently

Promoted objects go into **Immix** blocks of 8,192 slots (64 KiB), each cut into
256 lines of 32 slots (`old.rs`). Space is reclaimed a _line_ at a time: after a
marking cycle, any line with no marked object in it is free. Allocation bumps a
cursor through runs of free lines. No object is freed individually, and nothing
is copied to reclaim space.

Each line records an **epoch**, the number of the cycle that last marked it or
allocated into it, instead of a mark bit that would need clearing before every
cycle. So both starting and finishing a cycle cost O(1).

Marking runs **on other OS threads while the program keeps running**
(`mark.rs`), using snapshot-at-the-beginning:

```text
  program thread:  ──run──┤pause 1├──────run──────────────┤pause 2├──run──
                          │ empty nursery,                │ adopt result:
                          │ hand roots to a Job           │ unmarked lines free
  marker pool:            └──────── mark ─────────────────┘
```

- **Allocation is black.** Anything promoted or allocated in the old generation
  during a cycle is marked when it's created.
- **Overwrites are logged.** Only three things are ever mutated: a `Ref`, a
  mutable array element, and a resumption's one-shot flag. Only those writes
  run the SATB barrier, which logs the old value for the marker. Meadow data is
  otherwise immutable, so the barrier almost never runs.
- The marker reads a mutable object's fields under the heap's mutation lock.
- The marker pool is shared by every heap in the process
  (`MEADOW_GC_MARK_THREADS`, default one per four cores). With no pool, a heap
  marks a slice during each nursery pause.

A cycle starts when the old generation has grown by as much as the previous
cycle found alive, or by `MEADOW_GC_TRIGGER` slots (default 262,144), whichever
is larger.

**Evacuation** (`evacuate.rs`) fixes the fragmentation that non-moving marking
leaves behind. After a cycle, the sparsest blocks are chosen, and during the
next cycle every pointer into them is recorded, by the marker and by the
barriers. Between cycles, a pause copies a chosen block's survivors into dense
blocks, rewrites the recorded pointers, and gives the block back.
`MEADOW_GC_EVACUATE=0` turns this off.

**The remembered set.** A nursery collection doesn't scan the old generation,
so it has to be told about old-to-young pointers. There are only three ways to
create one: promoting an object whose field stays young, allocating a large
object directly into the old generation, and writing a `Ref` or mutable array
element or a resumption flag. Each of those records the field.

`MEADOW_GC=copying` swaps in a single growing Cheney space with no generations,
kept as a baseline to compare against.

### Native code and the collector

The JIT and AOT code don't need stack maps, safepoint polls, or any
cooperation with the collector beyond what the interpreter already does:

- **Collection only happens inside the interpreter.** Native allocation bumps
  `TOP` if there's room and otherwise takes the slow path, which calls
  `meadow_exec`, and that call is where a collection can happen. Inline code
  never collects.
- **Before every call into the interpreter**, native code writes the fixed
  registers back to the register file and brings `live` up to date. The GC map
  for that pc then describes the roots exactly.
- **After the call**, the fixed registers are reloaded from the register file,
  which the collector has updated. Scratch machine registers are never live
  across a call.
- **No native frame survives a block.** Control leaving a block function either
  returns to `advance` or jumps into the next function, which reuses the same
  frame. So the native stack is one frame deep, and a deep recursion grows only
  the heap.

The inline fast paths only read nursery objects and only allocate in the
nursery, so they never need the old generation's mutation lock or barriers.
Anything that writes a mutable object goes to the interpreter, which runs the
barrier.

<a name="compact-regions"></a>

### Compact regions

`Std.Compact` (`region.rs`) copies a structure out of the heap into a
**region**. No heap owns a region, and no collector copies or scans one.
Regions are **closed** (nothing in them points outside) and **immutable once
published**: new objects are only appended, under the region's lock, past
anything an existing address can reach. So any number of threads can read one
without locks or copying.

A heap holds a reference count on each region it has addresses into, and drops
it when a collection finds none left. A region is freed when the last count
goes. `Ref`s, mutable arrays and functions can't be compacted.

### Tuning

| variable                 | default        | effect                                                  |
| ------------------------ | -------------- | ------------------------------------------------------- |
| `MEADOW_GC`              | `generational` | `copying` for the single-space baseline                 |
| `MEADOW_GC_NURSERY`      | 262144         | largest nursery, in slots, which bounds nursery pauses  |
| `MEADOW_GC_TRIGGER`      | 262144         | old-generation growth, in slots, before the first cycle |
| `MEADOW_GC_MARK_THREADS` | cores / 4      | marker pool size; 0 means mark in pauses                |
| `MEADOW_GC_EVACUATE`     | on             | `0` turns evacuation off                                |
| `MEADOW_GC_VERIFY`       | off            | check every map and every marking cycle (slow)          |
| `MEADOW_JIT_THRESHOLD`   | 16             | block entries before the JIT compiles a block           |
| `MEADOW_THREADS`         | cores          | OS threads running green threads                        |

`meadow run --gc-stats` reports collections, pause percentiles, promotion and
marking.

The nursery's limit is also `--nursery <SIZE>` on the command line and
`nursery = "…"` in a `[profile.<name>]` of `Meadow.toml`, as a size in bytes
-- `8M`, `512K`, `1G` -- from 256K to 1G; the default is 2M, which is the
262144 slots above. A larger nursery is collected less often and promotes
less: `binarytrees`, which spends two fifths of its time in collections, runs
a quarter faster at 8M. Its nursery pauses are longer for it, and a program
that keeps most of what it allocates gains nothing. Like `-j`, it is what
`meadow` runs a program with: an executable run by hand reads
`MEADOW_GC_NURSERY`.

## 5. Effects

`compiler/meadow-seq/src/lower.rs` ("Effects: evidence passing").

**No backend knows effect handlers exist.** The runtime has no handler stack
and no special instruction for `handle` or `perform`. The compiler lowers both
into closures, data constructors, `Ref`s and jumps, plus three primitives that
cut and rejoin the frame stack at chunk boundaries where a resumption is
captured ([below](#5-effects): `Enter`, `Detach`, `Reattach`). So the
interpreter, the JIT and AOT code all run effects with the same instructions
they use for everything else, and native code accelerates handlers exactly as
much as it accelerates ordinary closures.

### Evidence

Every function takes one hidden parameter: the **evidence**, the list of
handlers in scope where it was called, newest first.

```text
  #ev (key, clause, target, rest)     an ordinary handler clause
  #evt(key, clause, target, rest)     a tail-resumptive clause (see below)
  #evnone                             the empty list
```

- `key` is the operation's name as a string constant, such as `"Log.log"`.
- `clause` is a closure whose one method is the clause body.
- `target` is a `Ref` holding the continuation that the whole `handle`
  expression's value goes to.
- `rest` is the evidence outside this handler.

A continuation doesn't take evidence as a parameter. It captures the evidence
it needs like any other variable, which is why resuming a continuation needs no
evidence restored.

### `handle body with { clauses }`

Answering continuation `k`, this builds:

```text
  target = Ref k                         where the handle's value goes
  for each clause c:
      obj_c   = closure(c's body, capturing its free variables)
      ev      = #ev(key_c, obj_c, target, ev)    (or #evt for a tail clause)
  H = closure: read target; run the return clause (or pass x through) there
  run body with evidence ev, answering H
```

The clauses and `H` capture the evidence from _outside_ the handler, so an
effect performed inside a clause skips past this handler, as it should.

### `perform E.op x`

Each `perform` site gets a small lifted block that walks the evidence:

```text
  <perform Log.log>(ev, x, k):
      switch ev
        #ev (key, clause, target, rest) | #evt(…) →
            if key == "Log.log" then dispatch
            else <perform Log.log>(rest, x, k)
        #evnone → Native Log.log x, answering k       ← no handler: the runtime's
```

The innermost handler is almost always the one that matches, and it is one tag
test and one comparison away. The search allocates nothing.

### Dispatch: tail-resumptive clauses

Most clauses resume straight away with a value computed without using the
resumption, as in `ask () k -> k 10`. `tail_resumptive` recognizes the shape
`op x k -> k e`, where `k` isn't free in `e`. For those clauses the entry is
`#evt`, and dispatch simply **invokes the clause with `x` and the performing
code's own continuation `k`**. The clause computes `e` and returns it to `k`.
No resumption is built, no flag is set, and nothing is written, so this costs
the same as calling a closure.

This is sound because `k e` would have sent its result to wherever the clause's
value goes, and that's the same place.

### Dispatch: general clauses and one-shot resumptions

For any other clause, such as `log m k -> 1 + k ()`:

```text
  flag   = Once ()                          a Kind::Resume object: [taken = false]
  R      = closure [flag, k, target] with the method "resume" below
  kh     = GetRef target                    the handle's current destination
  invoke clause(x, R) answering kh
```

and `R`'s method, called as `k v` with its own continuation `c`, does:

```text
  if TakeOnce flag has already been taken → fail "continuation resumed more than once"
  SetRef target := c                        the handle's value now goes back to this resume site
  invoke k(v)                               continue the performing code
```

Writing `c` into `target` is what makes handlers **deep** and makes `k v`
_return_ inside the clause.

With continuations on a stack, capturing `k` means capturing the frames
between the `perform` and the handler. Three primitives do it, all at chunk
boundaries and none of them copying a frame: `Enter` at every `handle` starts
a fresh chunk and writes its tag into the handle's `target` `Ref` (in the
`Ref`'s otherwise unused `meta`), so the body's frames never share a chunk with
the handler's own and the boundary is known from where the handler was
_entered_; `Detach`, where a general clause is entered, unlinks that chunk and
every one above it and names the segment with a `Kind::Stack` object the
resumption captures alongside `k`; `Reattach`, in the resumption, links the
segment back on top of the stack before continuing into `k`, and refuses to do
it twice. The boundary has to come from `Enter` rather than from where the
handler's continuation lives, because that continuation need not be a frame at
all: a `handle` in tail position of a function called from another handler's
body answers with a heap closure, and cutting by its position would take the
outer handler's frames along -- which is how `Std.Stream`'s `map` inside
`toVec` found the bug. A perform to a handler whose chunk is no longer on the
stack -- a closure that escaped its `handle` and kept the evidence -- is the
runtime error "an effect was performed after its handler had finished". A segment nobody
resumes is freed at the next nursery collection if its naming object died
young, or by the marker if it was promoted. This is OCaml 5's fiber design, and
one-shot resumption is exactly what makes it O(1). When the resumed body eventually finishes, it
invokes `H`, and `H` reads `target`. That now holds `c`, the rest of the clause
after its `k v`, so `1 + k ()` gets its answer and the clause carries on. This
is safe only because a resumption runs **at most once**. Resuming twice is a
runtime error, and multi-shot continuations aren't supported.

A worked trace for the `counted` handler in the tutorial:

```text
  handle work () with { log m k -> 1 + k (), return x -> 0 }

  target := main's continuation
  work () → perform log "step one"
      → evidence hit (#ev) → R1, clause("step one", R1) answering main
      clause computes 1 + R1 ()  → the "1 + _" continuation c1 is built
          R1: flag taken; target := c1; resume work
  work () → perform log "step two"
      → R2, clause("step two", R2) answering c1 (read from target)
          clause: 1 + R2 () → c2; R2: target := c2; resume work
  work () returns 42 → H: read target = c2, return clause gives 0
      → c2: 1 + 0 = 1 → c1: 1 + 1 = 2 → main
```

The flag and `target` are mutable, so writing them goes through the same
barrier as any `Ref` write ([section 4](#the-old-generation-immix-marked-concurrently)).
That's one of the three kinds of write that ever run it.

### Operations nobody handles

When the evidence runs out, the site falls into `Op::Native`: `r[a] ←
ops[imm](r[b])`. The runtime answers it in `glade/src/native.rs` and `vm.rs`.

| effect      | answered by the runtime                                     |
| ----------- | ----------------------------------------------------------- |
| `Console`   | stdin and stdout, or a debugger's protocol stream           |
| `Fs`        | files, directories and metadata                             |
| `Process`   | running commands, `argv`, the environment, `exit`           |
| `Random`    | a splitmix64 generator per OS thread, seeded from the clock |
| `Time`      | wall and monotonic clocks, `sleep`                          |
| `Test.fail` | a failure with the shown message                            |

Anything else is the run-time error `unhandled effect E.op`. A native's result
is often a tree, such as `Ok(Just(#[1, 2, 3]))`, and building one object at a
time would hold addresses across allocations. So a native describes its result
as a `Build` (plain Rust data), and `Vm::build` measures the whole tree, makes
room once, and materializes it bottom-up.

Mutable references are primitives (`NewRef`, `GetRef`, `SetRef`), not handled
operations. `Mut` appears only in types.

### Effects on native code

Everything above is ordinary instructions, so the native backend runs effects
with the fast paths from [section 3](#what-is-done-inline-and-what-is-handed-back):

- The evidence walk's `switch` on `#ev`/`#evt`/`#evnone` is an inline
  `JumpUnlessTag` when the evidence is in the nursery.
- The key comparison is a `JumpUnlessPrimK` against a string constant, which
  goes to the interpreter.
- Building evidence entries, clause closures and resumptions uses inline
  nursery allocation when their headers are static.
- Invoking a clause, `H` or a resumption is an inline `Invoke`.
- `NewRef`/`GetRef`/`SetRef`, `Once`/`TakeOnce` and `Native` go to the
  interpreter.

So a tail-resumptive operation costs, in native code, the evidence walk, one
interpreted string comparison and one closure call. A general clause adds the
flag, the resumption closure, and three interpreted `Ref`-style primitives.
Doing the key comparison and the `Ref` primitives inline would be the obvious
next steps for effect-heavy code.

### Effects and threads

A spawned thread starts with `#evnone` evidence. Its type allows only the
effects the runtime answers (`Console`, `Fs`, `Process`, `Random`, `Time`,
`Test`, `Mut`, `Thread`), and it must handle any others itself. A resumption
can't cross threads: it captures its `Resume` flag first, and sending or
compacting a value refuses a `Resume` object as a continuation.

**STM** (`lib/Std/src/Stm.mw`, `glade/src/stm.rs`) is the same idea split in two.
`atomically`, `retry` and `orElse` are handlers written in Meadow. The runtime
supplies what a handler can't: `TVar`s every thread can see (their values live
in shared regions), a per-transaction read/write log, and a commit that checks
a global clock and publishes all the writes under one lock.

## 6. Concurrency and parallelism

`glade/src/sched.rs`, `lib/Std/src/Thread.mw`.

### A green thread is a value

A green thread is a `Vm`: registers, a pc, `live`, a heap, and its globals
cache. Its call stack is the frame stack
([section 2](#the-call-stack-is-a-frame-stack)), which is chunks of that heap
and not the OS thread's stack, so that is **all** of it. A thread
that isn't running is a boxed struct sitting in a queue, and any OS thread can
pick it up and continue. Nothing about a thread is tied to the OS thread that
last ran it: no native stack, no thread-local storage, and no suspended native
frames. This holds equally for native code, since a block function has always
returned before its thread is suspended.

### The scheduler: M:N with work stealing

- **Workers.** `MEADOW_THREADS` OS threads (default: one per core). The calling
  thread is worker 0. The others start at the first `spawn`, so a program that
  never spawns never creates an OS thread.
- **Run queues.** Each worker has its own queue. A thread that a worker spawns
  or wakes, or one that used up its time slice, goes on that worker's queue, so
  its heap stays in that core's cache.
- **The `next` slot.** A thread woken by a message or an `await` result goes
  into the worker's unstealable `next` slot and runs as soon as the current
  thread stops. A send and the receive it satisfies then run back to back on
  one core.
- **Stealing.** An idle worker takes half of a random victim's queue, from the
  end that runs last. A worker is woken to steal only when some queue holds more
  than its owner is about to run.
- **Fairness.** A shared global queue is checked first every `GLOBAL_EVERY`
  (61) turns.
- **Locks** are per queue, per channel and per task handle. Workers that aren't
  touching the same things never wait on each other.

```text
  worker 0                 worker 1                 worker 2 (idle)
  ┌────────┐ next: T7      ┌────────┐               ┌────────┐
  │ T1 ▶   │               │ T4 ▶   │               │ steal  │── takes half of
  ├────────┤               ├────────┤               └────────┘   worker 0's queue
  │ T2 T3 T5 T6 …          │ T8
  └──────────────          └────                     global: T9 (every 61 turns)
```

<a name="preemption"></a>

### Slices and preemption

A worker runs a thread for `SLICE` (2048) calls to `Vm::advance`, then puts it
at the back of its queue. On the interpreter, a call runs one instruction. On
native code, a call runs one block function and whatever it chains into: at
least one instruction, and at most `CHAINS` (256) functions and `BACK_EDGES`
(4096) loop iterations in all. Preemption therefore happens only at block boundaries, never in the middle
of machine code. A native slice can run far more instructions than an
interpreted one, but it is always bounded, so a loop that never waits can't
starve other threads.

<a name="thread-operations-from-native-code"></a>

### Thread operations, from any backend

`spawn`, `await`, `yieldNow`, `newChannel`, `send`, `receive`, STM commit and
STM wait are **primitives**. A primitive can't carry out a thread operation
itself, because the channels and other threads belong to the scheduler. So it
leaves a `Request` in the machine and stops:

```text
  native block ──► meadow_exec(pc of `receive`) ──► primitive sets vm.request
       ◄── REQUESTED ◄──────────────────────────────┘
  block function returns REQUESTED ──► advance returns ──► run_slice sees the request
  worker settles it:
      send / spawn / newChannel / commit  → keep running the same thread
      receive on empty channel / await on unfinished thread / retry
                                          → park it with what it waits on
  whoever wakes it attaches a Wake (a parcel, a handle, a Bool) that is
  written into the thread's destination register when it runs again
```

Native code needs no special support for this. The thread operation is just
another instruction handed to the interpreter, and `REQUESTED` is just another
reason to return. When the thread resumes, its pc is the instruction after the
primitive. If that isn't a block entry, the interpreter runs from there until
it reaches one, and native code takes over again.

**Every thread shares one native table.** The JIT's `Native` is borrowed by
every `Vm` in the run, and its table entries are atomics. A block compiled
because one thread made it hot becomes native for all threads, and it is
compiled only once however many threads reach the threshold together.

<a name="no-shared-mutable-heap"></a>

### No shared mutable heap: parcels

Heaps are never shared, so a value moves between threads in exactly three
ways: the function a thread is spawned with, a message on a channel, and the
result `await` returns. Each is a **copy**:

1. The sending thread **exports** the value into a `Parcel`
   (`Heap::export`). The export walks everything reachable, like a Cheney scan,
   and lays the objects out contiguously with addresses relative to the
   parcel. Sharing inside the value is preserved.
2. A parcel holds no heap addresses, so it can sit in a channel or a task
   result without belonging to any heap.
3. The receiving thread **imports** it when it next runs
   (`Heap::import`): one block copy, then rebasing the references.

The export refuses a `Ref`, a mutable array or a continuation (a `Resume` flag)
with a runtime error. Immutable data copies invisibly. **Compact regions and
`TVar` values aren't copied at all**: the parcel keeps their addresses, which
are the same in every heap, and holds a reference count on the region. This is
how `examples/Parallel` lets eight threads read one prime table.

The rule has three consequences:

- **No locks in the machine.** Allocation, field reads and writes, and `Ref`
  updates never contend.
- **Independent collection.** Each worker collects the heap of the thread it is
  running, whenever that heap needs it, without stopping other threads. The only
  process-wide GC state is the marker pool, which takes jobs from any heap, and
  region reference counts.
- **The same semantics everywhere.** The CEK machine shares immutable values
  instead of copying them, which no program can distinguish, and every engine
  refuses the same values with the same messages (`meadow_core::thread`).

### Ending, failing and deadlock

- The program ends when `main` does. Threads still running then are dropped.
- A failure in a thread fails only that thread. `await` on it fails with the
  same message, and it can be awaited any number of times.
- The scheduler counts threads that are running or queued. Only a running
  thread can wake a waiting one, so if the count reaches zero while `main` is
  unfinished, the scheduler reports a deadlock.
- A panic inside the runtime stops every worker instead of hanging the run
  (`StopOnPanic`).

### Parallelism, summarized

| layer                             | how it uses cores                                                         |
| --------------------------------- | ------------------------------------------------------------------------- |
| green threads                     | M:N over `MEADOW_THREADS` workers, with work stealing                     |
| native code                       | shared and immutable once published; every worker runs the same functions |
| JIT compilation                   | on the thread that makes a block hot, under one lock                      |
| allocation and nursery collection | per thread, no locks                                                      |
| old-generation marking            | a process-wide marker pool, concurrent with the program                   |
| shared data                       | compact regions and `TVar`s, read in place by every thread                |
| communication                     | copy-on-send parcels over channels, `await` results                       |

## 7. Profiling

```sh
meadow run --backend vm --profile-to out.folded --sample-every 5000 mypkg
```

writes **folded stacks** -- one line per stack, outermost frame first, frames
separated by `;`, then a count -- which `flamegraph.pl` and speedscope both
read, and which is legible on its own:

```text
row;escape 323904
band;row 25001
row 4812
```

### Dumping a definition's core

```sh
MEADOW_DUMP_CORE=main meadow run -O2 --backend vm Main.mw
```

Prints the named definition's core term twice on stderr, before and after
the passes lowering runs on it (inlining, join points, simplification), at the
optimization level in use. This is the first thing to reach for when a
program is right at one level and wrong at another: the bytecode says what
was compiled, but this says what the passes did to get there.

### Counting what native code hands back

```sh
MEADOW_TRAPS=1 ./target/release/native/MyProgram
```

An ahead-of-time program run with `MEADOW_TRAPS` set counts every instruction
native code handed to the interpreter, by pc, and prints the top of the list
when it finishes -- with the opcode, so a line reads against `meadow dis`. This
is the number to look at before the sampler: a program that is "all native"
by its static instruction count can still spend most of its time in
`meadow_exec`, because the instructions that trap sit in the innermost loop.
`matmul` was 7.5% trapping instructions and 62% trapping time. The counter
costs a lock per trap and is off unless asked for.

### A native program, profiled by itself

```sh
MEADOW_SILO_PROFILE=prof.txt ./target/release/native/silo/MyProgram
scripts/silo-profile.py ./target/release/native/silo/MyProgram prof.txt
```

A program built for Silo and run with `MEADOW_SILO_PROFILE` set interrupts
itself once for every millisecond of processor time it uses, on every thread,
and writes where it was -- the return addresses on the stack -- to the file
when it ends. `scripts/silo-profile.py` puts those against the executable's
symbols and prints the busiest: by the function running, by the function of
the program itself it was in or called from (so the runtime's work is charged
to whoever asked for it), and by kind of cost. It counts time, where the
sampler above counts instructions; and it is for a machine where nothing
outside the process may look -- a container allows neither `perf` nor
`ptrace`. On macOS `sample <pid>` does as much unasked.

### What a native program made and let go of

```sh
MEADOW_SILO_PRIMS=1 ./target/release/native/silo/MyProgram
```

Prints, when the program ends: each primitive that went through the runtime's
shared entry and how often; the top-level values read most; how many blocks
of each kind -- each constructor, each closure -- were erased, over every
thread; of the blocks let go of whole, which function dropped them; and of
the arrays made, how long and by which function. The last two are from one
in every 1,024. Two builds of one program, set side by side, say where one
does more than the other: MeadowBoot built by itself made half as many
arrays again as MeadowBoot built by the Rust compiler, and the line that
differed was the function that reads a tree's node off its children.

`MEADOW_SILO_MEMORY=1` says four times a second, on stderr, what the
program holds: how much its threads' heaps have from the system, how much of
that is in blocks still in use, and how much its compact regions hold and
how many there are -- each line with the milliseconds since it started, to
set against whatever else the program prints of where it has got to. A
process's resident memory says how much; this says of what. It is how a
compiler was found to keep every file's tree to the end of the run: the
regions' line never fell.

To measure what a pass or a rule is worth, leave it out and time what is
built: `MEADOW_SKIP_PASSES=simplify,joins` (of `release`, `inline`, `joins`,
`simplify`, `lift`, `trmc`) for the core passes, `MEADOW_NO_REUSE` and
`MEADOW_NO_BORROW` for the native back end's, `MEADOW_CUT_GENERIC` and
`MEADOW_CUT_IMPURE` for what a program of Cut is given.

### Where a profile's stack comes from

The machine has no `call` or `ret`, and nothing that is a stack pointer to
unwind from ([section 2](#the-call-stack-is-a-frame-stack)). What it has is a
chain of continuations: the function running holds the one it will answer,
that one captured the one _its_ caller will answer, down to the `halt`. Most
links are frames on the frame stack; one made in tail position of a `handle`,
or captured by something that outlives its call, is a heap closure. The two
are laid out alike, so the walk follows the chain from link to link and does
not care which it is on -- or where one chunk of the stack ends and the next
begins.

Three things make the walk possible, and all three were already there for the
debugger: `DebugInfo::env_of`/`envs` say which name is in which register at a
pc, `returns` says which name is a function's _own_ return continuation (as
against `continuations`, the ones it makes for calls it makes), and a closure
keeps its captures in fields `0..len` in the order its method takes them -- so
the register a name sits in at a method's entry is the field it was captured
into. Each link's entry pc is a return address, which is the frame below: a
frame carries it as its `meta`, and a closure's is method 0 of its table
(`Vm::stack`).

### What it measures, and what it does not

Samples are taken where the machine enters a block, so what is counted is
instructions rather than seconds; time in the collector is not here, and
`--gc-stats` is where that lives.

**Use `--backend vm` to profile.** Native code goes from one block to the next
without returning to the machine -- that is the point of chaining -- so a JIT or
ahead-of-time run is sampled far too rarely to say anything. The interpreter
enters the machine at every instruction, and what is hot in a program is the
same either way: a profile answers _which of my functions_, not _which of my
machine instructions_.

A package can ask for this instead of remembering the flags, in its
`Meadow.toml`:

```toml
[profile.debug]
profile = true
```

which keeps the debug info and samples every run into
`target/debug/profile.folded`.

A profile needs debug info to name anything, so `--profile-to` compiles its own
image with it. The code is the same instruction for instruction
(`meadow_codegen::compile_with_debug_info`), so what is measured is the program
that would have run.

### Where the garbage comes from

```sh
cargo build --features profile-alloc      # in glade/
```

charges every allocation to the instruction that asked for it
(`profile::Sites`). It is off by default and compiled out entirely when it is:
the accounting sits on the bump allocator, which is the fast path the
generational collector exists to have.

## 8. Where to read next

| topic                                            | file                                                                                           |
| ------------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| AxCut, the IR                                    | `compiler/meadow-seq/src/lib.rs`                                                               |
| lowering, evidence passing, descriptors          | `compiler/meadow-seq/src/lower.rs`, `describe.rs`                                              |
| register allocation, GC maps, typed ops          | `compiler/meadow-codegen/src/lib.rs`                                                           |
| the instruction set and image format             | `compiler/meadow-bytecode/src/lib.rs`, `image.rs`; [IMAGE.md](IMAGE.md) is the format's spec   |
| specialization                                   | `compiler/meadow-core/src/specialize.rs`                                                       |
| the thin instruction set                         | `glade/src/codegen/thin.rs`                                                                    |
| the interpreter                                  | `glade/src/vm.rs`, `prims.rs`                                                                  |
| the native ABI                                   | `glade/src/abi.rs`; [NATIVE.md](NATIVE.md) is the contract a code generator is written against |
| the code generator                               | `glade/src/codegen/mod.rs`, `a64.rs`, `x64.rs`                                                 |
| object files                                     | `glade/src/codegen/object.rs`                                                                  |
| the JIT                                          | `glade/src/jit.rs`                                                                             |
| AOT entry and linking                            | `glade/src/aot.rs`, `buildtools/meadow/src/aot.rs`, `glade/build.rs`                           |
| heap, nursery, remembered set                    | `glade/src/heap.rs`, `object.rs`                                                               |
| old generation, marking, evacuation              | `glade/src/old.rs`, `mark.rs`, `evacuate.rs`                                                   |
| regions and STM                                  | `glade/src/region.rs`, `stm.rs`                                                                |
| the scheduler                                    | `glade/src/sched.rs`                                                                           |
| profiling, and the stack the machine has not got | `glade/src/profile.rs`, `buildtools/meadow/src/samples.rs`                                     |
| join points                                      | `compiler/meadow-core/src/joins.rs`                                                            |
| unhandled effects                                | `glade/src/native.rs`                                                                          |
| what may cross threads                           | `compiler/meadow-core/src/thread.rs`, `compact.rs`, `stm.rs`                                   |
