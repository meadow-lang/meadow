# The `.mbc` image format

> **History.** This describes the bytecode runtime that had Glade's name until
> the merged runtime took it (`docs/RUNTIME-MERGE.md`, R7). Its code is gone;
> this is kept for the reasoning behind what replaced it.

A **bytecode image** is a whole program as the runtime loads it: the
instructions, and every table an instruction's operand indexes. `meadow build`
writes one to `target/<profile>/bytecode/<name>.mbc`; `meadow exec` runs one;
`meadow link` compiles one to a native executable, which carries it (see
[NATIVE.md](BYTECODE-NATIVE.md)). A compiler that writes this format needs nothing else
from the Rust toolchain to have its output run.

This document is the format. The implementation is
`compiler/meadow-bytecode/src/image.rs` (`encode`, `decode`), and
`a_golden_image_is_these_bytes` in the same file pins the bytes below; where the
two disagree, the code is right and this is a bug. What the instructions
_mean_ is documented on `meadow_bytecode::Op`; the machine that runs them is
described in [BYTECODE-RUNTIME.md](BYTECODE-RUNTIME.md).

- [Conventions](#conventions)
- [The file](#the-file)
- [Instructions](#instructions)
- [Constants](#constants)
- [Descriptors](#descriptors)
- [Collector maps](#collector-maps)
- [What a valid image satisfies](#what-a-valid-image-satisfies)
- [Versioning](#versioning)
- [A worked example](#a-worked-example)

## Conventions

Everything is **little-endian**. There is no alignment and no padding: fields
follow one another byte for byte.

| notation | meaning                                                          |
| -------- | ---------------------------------------------------------------- |
| `u8`     | one byte                                                         |
| `u16`    | two bytes                                                        |
| `u32`    | four bytes                                                       |
| `u64`    | eight bytes                                                      |
| `str`    | `u32` length in bytes, then that many bytes of UTF-8             |
| `vec<T>` | `u32` count, then that many `T`                                  |
| `bytes`  | `u32` count, then that many `u8` (that is, `vec<u8>`)            |
| `pc`     | a `u32` index into the code: instruction number, not byte offset |

## The file

In this order, and nothing after the last field -- trailing bytes are an error.

| #   | field             | type                         | what it is                                                                                          |
| --- | ----------------- | ---------------------------- | --------------------------------------------------------------------------------------------------- |
| 0   | magic             | 8 bytes                      | `MDWIMG06` (ASCII)                                                                                  |
| 1   | `code`            | `vec<instr>`                 | the instructions, 8 bytes each -- [below](#instructions)                                            |
| 2   | `consts`          | `vec<const>`                 | what `Const`, `PrimK`, `JumpUnlessPrimK` load -- [below](#constants)                                |
| 3   | `methods`         | `vec<vec<pc>>`               | method tables: entry pcs. `Closure` and `Frame` name a table by index; `Invoke` names a method      |
| 4   | `method_captures` | `bytes`                      | per method table: how many captures an object made with it holds                                    |
| 5   | `method_params`   | `vec<bytes>`                 | per method table, per method: how many registers its block takes, captures included                 |
| 6   | `shapes`          | `vec<vec<str>>`              | per `MakeRecord` shape: field names, in argument order                                              |
| 7   | `labels`          | `vec<str>`                   | field names for `Select` and `Extend`                                                               |
| 8   | `prims`           | `vec<u16>`                   | which primitive each `Prim*` instruction's index means: `meadow_rt::Prim::code`                   |
| 9   | `ops`             | `vec<(str, str)>`            | `(effect, operation)` for `Native`                                                                  |
| 10  | `ctors`           | `vec<str>`                   | constructor name per tag (canonical, `Type.Ctor`)                                                   |
| 11  | `roles`           | `vec<(str, str)>`            | the constructor playing each runtime role: the role's name, then the constructor -- [below](#roles) |
| 12  | `ctor_fields`     | `vec<(str, vec<str>)>`       | named-field order per constructor, **sorted by constructor name** (byte order)                      |
| 13  | `messages`        | `vec<str>`                   | what each `Error` says                                                                              |
| 14  | `entries`         | `vec<pc>`                    | entry point per top-level definition, in the compiler's label order                                 |
| 15  | `entry`           | `u8`, then `pc` if it is `1` | the program's entry point: `0` for none, `1` followed by the pc                                     |
| 16  | `regs`            | `u16`                        | the most registers any block needs; at most 256                                                     |
| 17  | `gc_maps`         | `vec<gcmap>`                 | the distinct register maps -- [below](#collector-maps)                                              |
| 18  | `gc_at`           | `vec<u32>`                   | per instruction: its map's index, or `0xFFFFFFFF` if it cannot collect                              |
| 19  | `operands_at`     | `vec<u32>`                   | per instruction: where its operand descriptors start in `operands`, or `0xFFFFFFFF` for none        |
| 20  | `operands`        | `vec<u16>`                   | operand descriptors -- [below](#descriptors)                                                        |
| 21  | `sources_at`      | `vec<u32>`                   | empty, or per instruction: where its field registers start in `sources`, or `0xFFFFFFFF`            |
| 22  | `sources`         | `bytes`                      | field registers -- [below](#field-registers)                                                        |
| 23  | `results`         | `bytes`                      | per entry of `entries`: the descriptor of what it answers                                           |
| 24  | `entry_result`    | `u8`                         | the descriptor of what `entry` answers                                                              |

Debug information (`meadow_bytecode::DebugInfo`) is **not** in the image. A
debugger and a profiler build their own image in memory, from source.

## Instructions

Eight bytes, always:

```text
  byte 0   opcode
  byte 1   a
  byte 2   b
  byte 3   c
  byte 4‥7 imm, u32
```

A few instructions read `b` and `c` together as one `u16`, `b` the low byte
(`JumpUnlessTag`'s tag). An immediate that is a number (`AddIK`, `CmpIK`) is
the `u32` read as a signed 32-bit integer; `BrIK`'s `b` is a signed byte.

Opcodes are dense from zero, in the order of `meadow_bytecode::Op::ALL`. An
unknown opcode is an error at load.

| #   | op              | #   | op                | #   | op      | #   | op       |
| --- | --------------- | --- | ----------------- | --- | ------- | --- | -------- |
| 0   | `Nop`           | 13  | `Extend`          | 26  | `DivI`  | 39  | `BrIK`   |
| 1   | `Move`          | 14  | `Closure`         | 27  | `ModI`  | 40  | `BrF`    |
| 2   | `Const`         | 15  | `Invoke`          | 28  | `AddIK` | 41  | `ShlI`   |
| 3   | `Jump`          | 16  | `Prim`            | 29  | `SubIK` | 42  | `ShrI`   |
| 4   | `JumpUnless`    | 17  | `Prim1`           | 30  | `MulIK` | 43  | `AndI`   |
| 5   | `JumpUnlessTag` | 18  | `Prim2`           | 31  | `AddF`  | 44  | `ShlIK`  |
| 6   | `Halt`          | 19  | `PrimK`           | 32  | `SubF`  | 45  | `ShrIK`  |
| 7   | `Error`         | 20  | `JumpUnlessPrim`  | 33  | `MulF`  | 46  | `AndIK`  |
| 8   | `MakeData`      | 21  | `JumpUnlessPrimK` | 34  | `DivF`  | 47  | `UshrI`  |
| 9   | `MakeArray`     | 22  | `Native`          | 35  | `CmpI`  | 48  | `UshrIK` |
| 10  | `MakeRecord`    | 23  | `AddI`            | 36  | `CmpIK` | 49  | `PopI`   |
| 12  | `Field`         | 24  | `SubI`            | 37  | `CmpF`  | 50  | `ItoF`   |
| 13  | `Select`        | 25  | `MulI`            | 38  | `BrI`   | 51  | `Frame`  |

What each does with `a`, `b`, `c` and `imm` is on the variant in
`compiler/meadow-bytecode/src/lib.rs`, and `meadow dis` prints any image in
those terms. The comparison a typed compare or branch makes is a `Cond`:
`0` `==`, `1` `!=`, `2` `<`, `3` `<=`, `4` `>`, `5` `>=`.

**Primitives.** `prims` holds `Prim::code()` values. The numbering is in
`compiler/meadow-core/src/lib.rs` and is append-only: `0` `Add` … `121`
`StringCompare`, `122` `Enter`, `123` `Detach`, `124` `Reattach`, `125`
`SetField`. `ToWord(w)` is
`28 +` the width's place in `Width::ALL` (`Int8`, `Int16`, `Int32`, `UInt8`,
`UInt16`, `UInt32`, `UInt64`). An unknown code is an error at load.

## Constants

A tag byte, then the payload:

| tag | constant  | payload                                   | loads as                                                  |
| --- | --------- | ----------------------------------------- | --------------------------------------------------------- |
| 0   | `Unit`    | --                                        | the unit word                                             |
| 1   | `Bool`    | `u8`, `0` or `1`                          | a `Bool`                                                  |
| 2   | `Int`     | `u64`, two's complement                   | an `Int`                                                  |
| 3   | `BigInt`  | `u64`, two's complement                   | a `BigInt` of that value, made on the heap at load        |
| 4   | `Float`   | `u64`, IEEE-754 bits                      | a `Float`                                                 |
| 5   | `Word`    | `u8` width (place in `Width::ALL`), `u64` | a sized integer, its bits already wrapped to the width    |
| 6   | `Float32` | `u32`, IEEE-754 bits                      | a `Float32`                                               |
| 7   | `Str`     | `str`                                     | an **interned name** (a symbol): an effect key, a label   |
| 8   | `Char`    | `u32`, a Unicode scalar value             | a `Char`                                                  |
| 9   | `Text`    | `str`                                     | a `String`: an object on the heap, made when it is loaded |

`Str` and `Text` are different things. A program's string literals are `Text`.

## Descriptors

A **descriptor** says how a word is represented, for whoever has to know: the
collector, `show`, equality, a generic primitive. They are
`meadow_core::desc`:

| value    | name                                                                      | the word is                                               |
| -------- | ------------------------------------------------------------------------- | --------------------------------------------------------- |
| 0        | `REF`                                                                     | a heap address: data, closure, array, record, `String`, … |
| 1        | `INT`                                                                     | an `Int`                                                  |
| 2        | `FLOAT`                                                                   | a `Float`'s bits                                          |
| 3        | `STR`                                                                     | an interned name's key                                    |
| 4        | `UNIT`                                                                    | unit                                                      |
| 5        | `BOOL`                                                                    | `0` or `1`                                                |
| 6        | `CHAR`                                                                    | a scalar value                                            |
| 7        | `FLOAT32`                                                                 | a `Float32`'s bits                                        |
| 8–14     | `WORD+k`                                                                  | a sized integer, `k` its width's place in `Width::ALL`    |
| 16 `ANY` | unknown: ask the value (only in programs lowered without representations) |

`results` and `entry_result` are descriptors, one byte each.

An entry of `operands` is a **descriptor source**, a `u16`: a value below 16 is
the descriptor itself; `16 + r` means "the descriptor held in register `r`" --
which is how code generic over a type variable, in a debug build, is told what
its values are. An instruction's descriptors start at `operands[operands_at[pc]]`
and run for as many operands as that instruction reads, in the order it reads
them.

## Field registers

`MakeData`, `Closure` and `Frame` build an object from `c` registers. They are
the window `r[b .. b+c]`, unless `sources_at[pc]` is not `0xFFFFFFFF`: then
they are the `c` bytes of `sources` from there, one register each, in field
order, and `b` means nothing. `sources_at` is empty in an image that lists
none.

A compiler never has to list: it can always copy the registers into a
window first. The list is there so that it need not -- where register reuse
has left an object's fields out of order, those copies were most of what a
closure-heavy program executed.

## Collector maps

A `gcmap` is `vec<(u8 register, held)>`, sorted by register, where `held` is a
tag byte:

| tag | held     | payload       | meaning                                       |
| --- | -------- | ------------- | --------------------------------------------- |
| 0   | `Ref`    | --            | the register holds a heap address             |
| 1   | `Scalar` | --            | it does not                                   |
| 2   | `Var`    | `u8` register | whatever the descriptor in that register says |
| 3   | `Any`    | --            | ask the value                                 |

`gc_at[pc]` names the map in force while instruction `pc` runs, for every
instruction that can allocate. A register below the machine's high-water mark
that the map does not list holds nothing anyone will read again.

## What a valid image satisfies

`decode` checks only that the bytes parse. The machine assumes the rest, and a
compiler has to guarantee it:

- `gc_at` and `operands_at` have exactly one entry per instruction, and
  `sources_at` either none or one per instruction. A listed instruction's `c`
  registers are inside `sources`.
- Every jump target, method entry, `entries` member and `entry` is a pc inside
  `code`.
- Every index an instruction carries is inside its table: constants, method
  tables, shapes, labels, prims, ops, messages, and a tag inside `ctors`.
- `method_captures` has one entry per method table and `method_params` one list
  per table with one entry per method. An object made with table `t` has
  exactly `method_captures[t]` captures, and method `m`'s block takes
  `method_params[t][m]` registers: the captures first, then the arguments.
- No block uses a register at or past `regs`, and `regs <= 256`. Register 255
  is the machine's, for a fused compare-and-branch.
- Every instruction that can allocate has a map, and the map is right: a
  register that holds an address and is read again must be listed as one.
- The runtime builds and recognizes a few values by itself -- what a native
  answers, an optional value, a result, a list, a vector, a tuple -- and asks
  `roles` which constructor plays each part (see [Roles](#roles)). Every
  constructor `roles` names, and `#tuple`, must have a tag in `ctors`, whether
  or not the program builds one itself.

## Roles

A runtime builds some values itself -- `Some line` for a line it read,
`Ok bytes` for a file, a list or a vector a primitive made -- and recognizes
some to print them as the sequence they are. Which of the program's
constructors plays each part is the front end's to say, not the runtime's, so
`roles` lists them: each entry is a role's name, as `meadow_rt::roles::Role`
writes it, and the constructor that plays it. A role's shape -- how many fields,
and what each holds -- is the same in every language:

| role            | fields                                                      |
| --------------- | ----------------------------------------------------------- |
| `none`          | none: the empty optional value                              |
| `some`          | the value                                                   |
| `ok`            | the value                                                   |
| `err`           | the error                                                   |
| `nil`           | none: the empty list                                        |
| `cons`          | the first element, and the rest                             |
| `vector-empty`  | none                                                        |
| `vector-single` | an array of the elements                                    |
| `vector-full`   | seven: see `Std.Collections.Vector`                         |
| `vnode-leaf`    | an array of elements                                        |
| `vnode-branch`  | an optional size table, and an array of children            |
| `false`         | none: `False` built as data, which a branch treats as false |
| `file-meta`     | `isFile`, `isDir`, `len`, `readonly`, `modified`            |

A role the program declares no constructor for is one the runtime builds no
value of: an operation that would have to fails, saying so. An unknown role
name is an error.

## Versioning

The last two bytes of the magic are the version, in ASCII decimal: this is
`06`. There is no compatibility between versions: a loader refuses any magic
but its own, and the message says so. The version changes whenever a field is
added, removed or reordered, an opcode or constant tag is renumbered, or a
descriptor changes meaning. Appending a primitive or an opcode does not change
it -- an old loader refuses the image at the unknown code, which is the right
answer.

## A worked example

The image of a program whose only instruction is `halt r0`, with nothing in any
table, no entry point, and one register:

```text
4D 44 57 49 4D 47 30 36   magic "MDWIMG06"
01 00 00 00               code: 1 instruction
06 00 00 00 00 00 00 00     Halt a=0
00 00 00 00               consts: 0
00 00 00 00               methods: 0
00 00 00 00               method_captures: 0
00 00 00 00               method_params: 0
00 00 00 00               shapes: 0
00 00 00 00               labels: 0
00 00 00 00               prims: 0
00 00 00 00               ops: 0
00 00 00 00               ctors: 0
00 00 00 00               roles: 0
00 00 00 00               ctor_fields: 0
00 00 00 00               messages: 0
00 00 00 00               entries: 0
00                        entry: none
01 00                     regs: 1
00 00 00 00               gc_maps: 0
01 00 00 00 FF FF FF FF   gc_at: [NO_MAP]
01 00 00 00 FF FF FF FF   operands_at: [NO_OPERANDS]
 00 00 00               sources_at: none
00 00 00 00               sources: 0
00 00 00 00               results: 0
04                        entry_result: UNIT
```

`meadow dis` on a package prints the code of a real one; `meadow build --emit
bytecode` writes the same text beside the image.
