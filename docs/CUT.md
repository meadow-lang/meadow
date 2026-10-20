# The cut IRs: what a front end hands the back end

**Status: draft v0, second revision, for review.** Nothing here is frozen. The
Idyll session reviews it from a front end's side before it becomes v0. Where a
choice is still open it says so, under [Open questions](#open-questions).

This is the contract between a language's front end and Meadow's back end:
the IRs a front end produces, as text, and what every runtime agrees they mean.
Meadow and Idyll both compile to it. Neither front end's types reach the back
end. What does reach it is described here, and nothing else is.

## Contents

1. [Two IRs](#two-irs)
2. [Representations](#representations)
3. [Names and symbols](#names-and-symbols)
4. [A program](#a-program)
5. [The declaration table](#the-declaration-table)
6. [Cut: the upper IR](#cut-the-upper-ir)
7. [Primitives](#primitives)
8. [Effects](#effects)
9. [AxCut: the lower IR](#axcut-the-lower-ir)
10. [Evidence](#evidence)
11. [Versions](#versions)
12. [Open questions](#open-questions)

## Two IRs

- **Cut** is the upper IR. It is a sequent calculus (λμμ̃, in the style of
  Sequent Core) typed by representations. A front end produces Cut. Most
  optimisation happens here, and the back end's reference interpreter runs Cut.
  That interpreter's answer is what a program means.
- **AxCut** is the lower IR: Cut focused and linearised until every cut has a
  variable on one side (Schuster et al., _Compiling Classical Sequent Calculus
  to Stock Hardware_, OOPSLA 2025). It keeps only machine-level passes, and the
  runtimes compile it: Glade to bytecode, Silo to LLVM.

A front end produces Cut only. AxCut's text form exists for the back end's own
tests and for debugging: what Cut lowered to, written down. A front end may
emit it while Cut is being built, but must not depend on it afterwards.

| crate          | holds                                                                                                   |
| -------------- | ------------------------------------------------------------------------------------------------------- |
| `meadow-rt`    | the runtime interface: primitives, descriptors, the numeric tower, text and hash rules, roles, evidence |
| `meadow-axcut` | AxCut, its abstract machine, and (to come) its text format                                              |
| `meadow-cut`   | Cut, its text format, its reference interpreter, and its lowering to AxCut                              |

None of them depends on any front end.

`meadow cut FILE` runs a program of Cut's text: lowered to AxCut and run on
Glade, or, with `--runtime silo`, built as an executable and run. The lowering
takes everything here but a `μ` where a value is wanted other than as a
definition's argument, and refuses a program with one, saying so.

`meadow cut FILE --emit axcut` prints what the program is lowered to, and
`--emit llvm` the LLVM IR a Silo build of it compiles, each unit after the
first under a `; ---- unit NAME ----` line; neither runs it. They are for a
front end's tools to show the back end's lowering beside its own, as a REPL's
viewer does. Like AxCut's text form, what they print is not a contract.

## Representations

A value's _representation_ is what a register holding it contains: how many
bits it is, and whether a collector follows it. Representations are the only
types either IR has.

| rep    | what it is                                                                            |
| ------ | ------------------------------------------------------------------------------------- |
| `i64`  | a 64-bit integer, two's complement, wrapping; untagged, in a register                 |
| `f64`  | an IEEE double                                                                        |
| `f32`  | an IEEE single                                                                        |
| `i8`   | sized integers, wrapping at their width: also `i16`, `i32`, `u8`, `u16`, `u32`, `u64` |
| `bool` | `false` or `true`                                                                     |
| `char` | a Unicode scalar value                                                                |
| `unit` | the one value of the unit type: no bits                                               |
| `sym`  | an interned name: a record label, an effect operation's key                           |
| `str`  | an immutable string, on the heap                                                      |
| `ptr`  | any other heap object: data, codata, an array, a record, a big integer, a `Ref`       |
| `'a`   | a representation variable: whatever its descriptor says at run time                   |
| `any`  | not known when compiled: boxed, and self-describing at run time                       |

`i64` is Meadow's `Int` and Idyll's. A language's arbitrary-precision integer
is a `ptr` to a big integer, which the numeric primitives handle (see
`meadow_rt::num`).

### Descriptors and representation variables

Code generic over a type that is not erased takes the type's representation as
a run-time value, its _descriptor_ (`meadow_rt::desc`), which names one of the
representations above. A definition that abstracts over representations names
its variables and, for each, the parameter that carries its descriptor:

```text
def meadow:Std/Collections.List.map <'a = da, 'b = db> (da: desc, db: desc, f: ptr, xs: ptr; k: ptr) = ...
```

`desc` is the representation of a descriptor parameter: an `i64` that a
linker and the back end know to be one. Inside the definition, a value of rep
`'a` is whatever `da` says. A call passes descriptors like any other argument:
one at a known representation passes a constant (`desc(i64)`, `desc(ptr)`), and
one at a variable passes the descriptor it was given. The back end's _specialise
on known arguments_ pass makes a copy for each constant it sees. Code it does
not specialise still runs, by reading the descriptor, which is what a debug
build and Glade's hot-patching rely on.

A front end decides which of its types are erased and which become
representation variables. Idyll erases everything of grade 0, which never
reaches the IR at all.

`any` is for a value whose representation depends on data the front end
erased, such as a type computed from a proof. Such a value is boxed, and a
runtime reads what it is from the value. It is part of v0's grammar but not yet
implemented: a lowering that meets one reports it as unsupported.

## Names and symbols

Within a program, a definition, a constructor and an effect are each named by
a **symbol**: a flat, globally unique name. The IRs have no namespaces. A front
end flattens its module paths into symbols, and two front ends' symbols never
collide:

```text
symbol   ::= lang ":" package "/" path
lang     ::= "meadow" | "idyll" | ...         -- the front end
package  ::= name ("@" version)?               -- the package, and its version
path     ::= segment ("." segment)*            -- module path, then the name
segment  ::= ident | quoted
ident    ::= [A-Za-z_][A-Za-z0-9_]*
quoted   ::= '"' (any character but '"' and '\', or '\"', '\\')* '"'
```

For example:

```text
meadow:Json@1.2.0/Value.parse
idyll:Json@1.2.0-alpha.1/parse
idyll:App/double
idyll:Prelude/List."::"
idyll:Prelude/Nat."x'"
meadow:Std/Maybe.Just
```

`/` ends the package, so a version's dots and hyphens, and a lower-case name in
a package's root module, are never mistaken for one another. Within the path,
segments are separated by `.`, as Meadow writes paths. A segment that is not
a plain identifier, such as an operator, a constructor like `(::)`, or a name
with a prime, is quoted. A language's standard library has no version.

A constructor is named after its type, as a segment of the type's path:
`meadow:Std/Maybe.Just`. Within one program, constructors also have tags,
which the back end gives them. A front end writes symbols, never tags.

**Exported symbols.** A definition its language makes public is _exported_.
Its symbol is visible to every program it is linked into, with its signature
(see [Exports and imports](#exports-and-imports)). Other definitions are local
to their package, and the back end may rename them.

## A program

A program is one text: a version line, then its declarations and definitions
in any order. `--` starts a comment that runs to the end of the line.

```text
cut 0
entry idyll:App/main
answer none

data idyll:App/Nat { Z; S(ptr) }
roles { ... }
native { ... }
val idyll:App/greeting : str = <"hello" | halt>
def idyll:App/double (n: i64; k: ptr) = prim add(n, n; k)
def idyll:App/main (; k: ptr) = ...
```

### Entry and answer

`entry` names the definition that runs: one with no parameters but its
continuation. `answer` says what becomes of its result:

- `answer none`: nothing is printed. Meadow's `main` and Idyll's `idyll run`
  are this.
- `answer str`: the result is a string, and is printed. A REPL's trailing
  expression is this: the front end wraps it in its own to-string first, since
  the runtimes turn no value but a string into text (see
  [Primitives](#primitives)).

A program with no `entry` is a library: it is linked into others, and runs
nothing of its own.

### Top-level values

```text
val <symbol> : <rep> = s
```

A `val` is computed once, when the program starts, before `entry` runs. `s`
gives its value to the continuation `halt`, and what it gives is the value.
Vals are computed in the order the program lists them. A front end lists them
so that every val comes after the vals it reads, across packages as OCaml
initialises modules. A val may perform effects, and they happen then, once. A
use of a val reads the value. It does not compute it again.

A `def` is a statement with parameters and runs each time it is called. A
top-level value is a `val`, never a `def` without parameters.

## The declaration table

What a runtime knows about the program's own types and effects is said here.

### Data

Every data type a program uses is declared, with its constructors in order
and each constructor's fields' representations:

```text
data idyll:App/Nat { Z; S(ptr) }
data idyll:Prelude/List <'a> { Nil; "::"('a, ptr) }
data meadow:Std/Maybe <'a> { None; Just('a) }
```

A constructor's symbol is its type's followed by its name:
`idyll:Prelude/List."::"`. The back end gives the constructors tags in the
order declared, and knows from this which constructors a `case` must cover to
be exhaustive. A field of a type's representation variable has that variable's
representation, with the descriptor whatever the value was built with.

The names are for diagnostics: a run-time error that has to say what it was
given names the constructor. How a language prints its values, such as
Idyll's `S (S Z) : Nat`, is its own business, done by a to-string it writes in
the language and compiles like any other code.

A tuple is the IR's own data, `#tuple`, and needs no declaration.

### Roles

A runtime builds some values itself, and recognises some. A native answers
with an optional value or a result, and `argv` makes a sequence. Which
declared constructor plays each
part is the front end's to say (`meadow_rt::roles::Role`):

```text
roles {
  none = meadow:Std/Maybe.None
  some = meadow:Std/Maybe.Just
  ok   = meadow:Std/Result.Ok
  err  = meadow:Std/Result.Err
  nil  = meadow:Std/List.Nil
  cons = meadow:Std/List.Cons
}
```

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
| `vector-full`   | seven: Meadow's `Std.Collections.Vector` tree               |
| `vnode-leaf`    | an array of elements                                        |
| `vnode-branch`  | an optional size table, and an array of children            |
| `false`         | none: `False` built as data, which a branch treats as false |
| `file-meta`     | `isFile`, `isDir`, `len`, `readonly`, `modified`            |

A role's shape is the same in every language: what the table says, in that
field order. A program declares only the roles it uses. An operation that would
build an undeclared role fails, saying so. `file-meta` is a record the runtime
builds field by field. A declared constructor with those fields, in that order,
plays it.

### Native operations

An effect operation that no handler in the program answers is performed by
the runtime. A language keeps its own effects and operation names. The
`native` table binds each operation a program may leave unhandled to the
runtime operation that performs it:

```text
native {
  idyll:Prelude/Console.putStr  = Console.writeOutput
  idyll:Prelude/Console.readLine = Console.readLine
  meadow:Std/Console.writeOutput = Console.writeOutput
}
```

The payload and answer must have the representations the runtime operation
takes and gives, from the table below. **Native also means unhandled:** an
operation that is not bound here, and that no handler answers, is an error
when the program is lowered, not when it runs.

The runtime's operations, as Glade and Silo perform them today. An operation
takes one value: one that takes several takes them as a tuple (`(str, str)`).
A front end performs its own operation with its arguments as they are,
`perform idyll:Prelude/Fs.writeString(path, text; c)`, and the back end packs
them into the tuple the runtime operation it is bound to takes. An optional
answer is the `none`/`some` roles, and a fallible one the `ok`/`err` roles with
a `str` error.

| operation                                          | takes                        | answers                                                     |
| -------------------------------------------------- | ---------------------------- | ----------------------------------------------------------- |
| `Console.writeOutput`                              | `str`                        | `unit`                                                      |
| `Console.writeError`                               | `str`                        | `unit`: to standard error, after standard output is flushed |
| `Console.readLine`                                 | `unit`                       | optional `str`                                              |
| `Console.readExact`                                | `i64`, a byte count          | optional `str`                                              |
| `Fs.readToString`                                  | `str`, a path                | fallible `str`                                              |
| `Fs.readBytes`                                     | `str`                        | fallible array of `u8`                                      |
| `Fs.writeString`, `Fs.appendString`                | `(str, str)`                 | fallible `unit`                                             |
| `Fs.writeBytes`                                    | `(str, ptr)`, an array       | fallible `unit`                                             |
| `Fs.removeFile`, `Fs.createDir`, `Fs.createDirAll` | `str`                        | fallible `unit`                                             |
| `Fs.removeDir`, `Fs.removeDirAll`                  | `str`                        | fallible `unit`                                             |
| `Fs.rename`                                        | `(str, str)`                 | fallible `unit`                                             |
| `Fs.copy`                                          | `(str, str)`                 | fallible `i64`, the bytes copied                            |
| `Fs.readDir`                                       | `str`                        | fallible sequence of `str`, the entries' names              |
| `Fs.metadata`                                      | `str`                        | fallible `file-meta`                                        |
| `Fs.exists`, `Fs.isFile`, `Fs.isDir`               | `str`                        | `bool`                                                      |
| `Process.spawn`, `Process.status`                  | a command: see `Std.Process` | fallible result                                             |
| `Process.exit`                                     | `i64`                        | does not return                                             |
| `Process.currentPid`                               | `unit`                       | `i64`                                                       |
| `Process.currentExe`                               | `unit`                       | `str`                                                       |
| `Process.isTerminal`                               | `i64`, a stream              | `bool`                                                      |
| `Process.argv`                                     | `unit`                       | a sequence of `str`: see below                              |
| `Process.getEnv`                                   | `str`                        | optional `str`                                              |
| `Process.setEnv`                                   | `(str, str)`                 | `unit`                                                      |
| `Process.removeEnv`                                | `str`                        | `unit`                                                      |
| `Random.nextInt`, `Random.nextSeed`                | `unit`                       | `i64`                                                       |
| `Random.nextFloat`                                 | `unit`                       | `f64`, in `[0, 1)`                                          |
| `Random.intBetween`                                | `(i64, i64)`                 | `i64`                                                       |
| `Time.now`                                         | `unit`                       | `i64`, milliseconds                                         |
| `Time.monotonic`                                   | `unit`                       | `i64`, nanoseconds                                          |
| `Time.sleep`                                       | `i64`, milliseconds          | `unit`                                                      |
| `Test.fail`                                        | `str`                        | does not return: stops with the message, status 1           |
| `Log.logAt`                                        | `(i64, str)`, a level's rank | `unit`: a line to standard error, by `MEADOW_LOG`'s level   |
| `Net.tcpListen`, `Net.tcpConnect`                  | `(str, i64)`, host and port  | fallible `i64`, a socket's handle                           |
| `Net.tcpAccept`                                    | `i64`, a listener            | fallible `(i64, str)`: a stream and its peer's address      |
| `Net.tcpRead`                                      | `(i64, i64)`, at most so many| fallible array of `u8`; empty at the end of the stream      |
| `Net.tcpWrite`                                     | `(i64, ptr)`, an array       | fallible `i64`, the bytes written                           |
| `Net.tcpClose`                                     | `i64`                        | `unit`                                                      |
| `Net.tcpTimeout`                                   | `(i64, i64)`, milliseconds   | fallible `unit`                                             |
| `Net.tcpLocalPort`                                 | `i64`                        | fallible `i64`                                              |
| `Net.resolve`                                      | `str`, a host's name         | fallible sequence of `str`, its addresses                   |
| `Net.tlsConnect`, `Net.tlsAccept`                  | `(i64, str, str)`: see `Std.Tls` | fallible `i64`, the session that takes the stream's place |

A sequence a runtime makes -- `Process.argv`'s, and any other an operation
answers as a sequence -- is a vector if the program declares the vector roles,
and a plain array of its elements if it does not. So a language without
Meadow's vector gets arrays, which the array primitives (`arrayLen`,
`arrayGet`) take apart.

A program that gives up says why with `Console.writeError` and stops with
`Process.exit`: a language's own way of giving up, an `Abort` effect say, is
its code over those two. `Test.fail` is for a failed assertion, which a test
runner reads. The IR's `error` takes only a literal, and is for the back end's
own use.

This table is checked against the runtimes before v0 is frozen. `Std`'s
`println` and similar conveniences are Meadow code over these; a
language binds the operation it means to the runtime's that does it.

### Exports and imports

```text
export meadow:Json@1.2.0/Value.parse : (str; ptr) -> ptr ! { }
import idyll:Prelude/List.map : <'a = da, 'b = db; e: Effect> (da: desc, db: desc, f: ptr, xs: ptr; k: ptr) ! { e }
```

A signature gives, in order:

- the representation variables, each bound to the descriptor parameter that
  carries it;
- the effect variables, each with its kind;
- the parameters with their representations;
- the operations the definition may perform, and the effect variables it
  passes on, after `!`.

The effect part is what lets an importer's lowering check that every operation
is handled or native, and pass evidence. An import is checked against the
export it names when programs are linked.

Values cross between languages as representations only. A value of a type the
other language does not declare crosses as a `ptr`, opaque to it. Roles are
per language, so one language's list is not the other's.

## Cut: the upper IR

Cut has three sorts:

- **producers** `p`, which make a value;
- **consumers** `c`, which receive one;
- **statements** `s`, which are a producer meeting a consumer and do not return.

Data is built by producers and taken apart by consumers. Codata is the other
way round. A function is codata with one method, `apply`, and a continuation is
a consumer. So the IR has no separate notion of calling or returning.

```text
p ::= x                                     a variable
    | symbol                                a val's value (never a def's)
    | lit                                   42, 1.5, 'c', "text", true, unit
    | desc(rep)                             a descriptor, as a constant
    | K(p, ...)                             a declared constructor, applied
    | μ k. s                                the value s gives to k
    | cocase { m(x: rep, ...; k: ptr, ...) => s; ... }
                                            codata: an object with methods
    | record { l = p, ... }                 a record
    | [p, ...]                              an array

c ::= k                                     a continuation variable
    | halt                                  the end: a val's value, the entry's answer
    | μ̃ x: rep. s                           bind the value to x, then s
    | case { K(x: rep, ...) => s; ...; _ => s }
                                            take data apart
    | m(p, ...; c, ...)                     call method m, with arguments and continuations

s ::= <p | c>                               the cut: give p to c
    | f(p, ...; c, ...)                     call a top-level definition
    | prim op(p, ...; c, ...)               a primitive: see Primitives
    | let x: rep = p in s
    | handle ... | perform ...              effects: see Effects
    | error "message"
```

Every binder carries a representation, a continuation's included (`k: ptr`):
`μ̃ x: i64. s`, `case { "::"(h: 'a, t: ptr) => ... }`, `cocase { apply(x: i64;
k: ptr) => s }`. A val's symbol is a producer, and reads the value. A def's is
not: a def is a statement with parameters, so a front end that needs one as a
value wraps it in a `cocase` whose method calls it.
A definition is a statement with parameters, values first and then
continuations:

```text
def meadow:Std/List.length <'a = d> (d: desc, xs: ptr; k: ptr) =
  <xs | case { Nil => <0 | k>;
               Cons(h: 'a, t: ptr) =>
                 meadow:Std/List.length(d, t; μ̃ n: i64. prim add(n, 1; k)) }>
```

A front end lowers its functions into definitions like this one. Its closures
become `cocase` with an `apply` method. Its pattern matches become `case`, and
its effects `handle` and `perform`. Cut has no source types, no modules, no
implicit arguments, and nothing erased.

**Cut has no local recursion.** A recursive definition inside another is
lifted to the top level by the front end, with what it closes over as extra
parameters.

**Reserved hints.** A binder may carry hints the back end may use and must not
need, written after its representation with `@`: `x: ptr @once`,
`x: ptr @0..1`. Idyll's grades are the intended source, for Silo's
linearisation to skip uniqueness checks or place drops. v0 parses and ignores
them.

The **reference interpreter** runs Cut directly, by cut elimination: a cut of a
constructor against a `case` picks the arm, a cut of a `μ` against a consumer
substitutes, and so on. Its answer and what it prints define the program. The
AxCut machine, Glade and Silo must all agree with it.

## Primitives

`prim op(args; continuations)` runs a primitive of `meadow_rt::Prim`. Most
take one continuation, which receives the result. A test takes two, false
first:

```text
prim if(b; c_false, c_true)          branch on a bool
prim lt(x, y; c_false, c_true)       a comparison, branched on
prim lt(x, y; c)                     the same comparison, as a bool to c
```

`prim if` is how a `bool` is branched on. `case` is for data only. A literal
pattern is a comparison: `prim eq(n, 0; c_other, c_zero)`. A test's
continuations receive `unit`, so a branch is written `μ̃ u: unit. s`: no
consumer is value-less.

**A primitive's answer given straight to a continuation's name has no
representation written**, since a continuation is a `ptr`. Where the
primitive says what it answers -- a comparison a `bool`, `arrayLen` an `i64`,
arithmetic its operands' -- the back end takes that. Where the answer is
whatever was stored -- `arrayGet`, `getRef`, `select` -- it is taken to be a
`ptr`, which it is only if what was stored is. Anything else must be bound
at its representation first:

```text
prim arrayGet(xs, i; μ̃ x: 'a. <x | k>)    an element of rep 'a, to k
```

Glade's values carry their own tags, so a wrong guess there goes unnoticed;
Silo's do not, and a word taken for a pointer is followed.

The `i64` arithmetic is what `meadow_rt::num` does, and what every runtime and
both front ends' interpreters agree on:

- `add`, `sub` and `mul` wrap.
- `div` rounds toward zero. `MIN / -1` wraps to `MIN`.
- `mod` is the remainder of that division, with the dividend's sign.
- `div` or `mod` by zero is a run-time error, "division by zero" or "modulo by
  zero". It is never undefined.

A record is built by `record { l = p, ... }` and taken apart a field at a
time, by its label -- written as a string literal, since it is a name the back
end is given and not a value the program computes:

```text
prim select(r, "x"; c)              the field `x` of `r`
prim extend(r, "x", p; c)           `r` with its `x` set, or added, to `p`
```

Reading a field a record does not have is a run-time error.

Text and arrays have their own primitives, among them `concatStrings` (an
array of strings, joined), `stringByteLength`, `stringByteAt` (the byte at an
index, as a `u8`; out of bounds is an error), `stringSlice`, `stringCompare`,
`arrayLen`, `arrayGet`, `arraySlice` and `arrayConcat`.

**The runtimes turn no value into text but a string.** There is no generic
`show`: a language converts its values to strings in its own code, an
integer's digits included (`/`, `%`, digit literals and `concatStrings`), and
prints the string with `Console.writeOutput`. What a value looks like as text
is the language's to decide, and is the same on every runtime because it is
ordinary code.

The sized integers do the same at their width. A conversion to one is named
for its type, as it is in source: `toUInt8`, `toInt32`. The full list of primitives,
with what each takes and answers, is `meadow_rt::Prim`. The spec will quote it
as a table before v0 is frozen.

## Effects

### Declaring an effect

```text
effect meadow:Std/Console { writeOutput(str) -> unit }
effect idyll:Search/Choose { @many choose(unit) -> bool }
effect idyll:Prelude/Fs { writeString(str, str) -> unit }
```

An operation may take several arguments. It is resumed **at most once**
unless it is marked `@many`. An effect with a `@many` operation has kind
`ManyEffect`; any other has kind `Effect`, which is the default. `Effect ≤
ManyEffect`: an effect of kind `Effect` may stand wherever a `ManyEffect` is
expected, never the reverse.

### Handling and performing

```text
s ::= handle { E.op(x: rep, ...; r: ptr, k: ptr) => s; ...
               return(x: rep; k: ptr) => s }
      in μ b. s ; c
    | perform E.op(p, ...; c)
```

- The body is `s`, under `μ b`: `b` is the body's own continuation, which
  passes what the body gives to the `return` clause.
- `c` is where the handle's value goes. Every clause binds it as `k`. A clause
  that does not resume gives its answer to `k`, as `fail(_; r, k) => <0 | k>`
  does.
- `r` is the resumption, a function: codata with one method `apply`, as any
  function is, so that it can go wherever a function can. `resume` names the
  same method, for a clause that calls it directly. Invoking it with a
  value and a continuation continues the body from the `perform`, and the
  resumed body's eventual answer goes to that continuation. A clause that
  resumes and then answers, `ask(; r, k) => <r | apply(10; k)>`, passes its
  own `k`. Handlers are deep.
- `perform E.op(p, ...; c)` performs the operation with its arguments. `c`
  receives what the clause resumes with.

A clause may resume an operation at most once unless the operation is `@many`.
A front end checks this, and the back end may rely on it.

### Effect variables

A definition generic over effects names its effect variables, each with a
kind, after its representation variables:

```text
def meadow:Std/List.map <'a = da, 'b = db; e: Effect> (...) = ...
def idyll:Search/all <e: ManyEffect> (...) = ...
```

The default is `Effect`. A row containing a `@many` operation can be passed
only where the variable is a `ManyEffect`.

### How the lowering chooses

How an effect is lowered depends on its kind. The lowering decides from the
kinds of a definition's effect variables, so a definition generic in its
effects gets the strategy its caller's effects need:

- **`Effect`**: evidence passing (see [Evidence](#evidence)). A tail-resumptive
  clause, `op(x; r, k) => <r | apply(e; k)>` with `r` not otherwise used,
  is a plain call. Any other clause captures the continuation as a stack
  segment, which is resumed in place, at most once. This is Meadow's lowering
  today, on both Glade (heap frame chunks) and Silo (native stack segments). A
  resumption the front end proved is used at most once needs no run-time
  check.
- **`ManyEffect`**: the code between the handler and the `perform` keeps its
  continuations on the heap, as the codata they are in Cut, so resuming twice
  is invoking an object twice. No stack is ever copied.

A whole-program analysis may move a `@many` operation that every handler
resumes at most once onto the `Effect` path. Nothing may depend on that, since
a program loaded later, or patched while it runs, may change it.

## AxCut: the lower IR

AxCut's statements are listed in `meadow-axcut` (`Statement`, `Extern`), and
its abstract machine is `meadow_axcut::machine`. The text form is what
`Program::pretty` prints, with representations on every binder and a version
line:

```text
axcut 0
roles { none = meadow:Std/Maybe.None, some = meadow:Std/Maybe.Just, ... }
tags  { meadow:Std/Maybe.None = 0, meadow:Std/Maybe.Just = 1, ... }

def #0 (v1: ptr)  (entry)
  extern lit 41() -> (v1: ptr, v2: i64);
  extern Add .. 1(v2) -> (v1: ptr, v2: i64, v3: i64);
  substitute [v3, v1] in (v3: i64, v1: ptr)
    invoke v1#0
```

| statement                                                | what it does                                    |
| -------------------------------------------------------- | ----------------------------------------------- |
| `substitute [x, ...] in (params) s`                      | rebuild the environment as `[x, ...]`, then `s` |
| `jump #n`                                                | transfer to a definition, environment unchanged |
| `let x = K#t(y, ...); s`                                 | build data                                      |
| `switch x { #t (params) => s; ...; else (params) => s }` | branch on the tag                               |
| `new f [captures] { #m (params) => s; ... }; s`          | build codata                                    |
| `invoke f#m`                                             | call a method, consuming `f`                    |
| `extern op(args) -> (params); s`                         | a primitive with one continuation               |
| `extern op(args) { #i (params) => s; ... }`              | a primitive with several, such as a branch      |
| `error "message"`                                        | stop, saying why                                |

A block's parameters are the whole environment at that point, not only what
is new. Reading a dump is also reading the register assignment.

A thread is started by `prim threadSpawn(desc(rep), f; k)`: the function to
run, after the descriptor of what it answers. The task `k` is given is a
pointer, so nothing else at the spawn says how the thread's answer is
represented, and the machine that runs the thread has to be told. A spawn
written without the descriptor is of a thread whose answer nobody reads.

## Evidence

Effects lowered by evidence passing pass every function one hidden argument:
the handlers in scope, newest first. Its encoding is fixed by the back end
(`meadow_rt::roles::evidence`), because the runtimes build it too: every
thread starts with the empty evidence.

```text
#ev(key: sym, clause: ptr, target: ptr, rest: ptr)    an ordinary clause
#evt(key: sym, clause: ptr, target: ptr, rest: ptr)   a tail-resumptive clause
#evnone                                               no handlers
```

`key` is the operation's symbol, as a `sym`. `clause` is the clause as codata.
`target` is a `Ref` holding where the handle's value goes. A `perform` walks
the evidence to the first entry with its key. If none has it, the runtime
performs the operation the `native` table binds it to.

A front end that produces Cut never builds evidence; the lowering does. This
section is for the back end's implementers, and for a front end emitting AxCut
directly in the meantime, which must build exactly these constructors.

## Versions

The first line of either IR's text is its name and version: `cut 0`,
`axcut 0`. A reader refuses a version it does not know rather than guessing,
and says which it found. Adding a primitive or a runtime operation does not
change the version, since an old reader refuses the unknown name. Changing a
statement, a representation, or what the table means does.

## Open questions

1. **Vectors.** The vector roles describe Meadow's tree vector, and a runtime
   answers a sequence as a plain array to a program that does not declare them.
   A language whose own vectors are flat, as Idyll's planned `Array n a` is,
   may later want a flat-vector role, so that it gets its own type rather than
   a bare array. Not needed for v0.
2. **`any`.** A boxed self-describing value is new to both runtimes, which
   today rely on descriptors for every value whose representation is not known
   statically. It needs a box with a descriptor in it, and primitives that
   accept one where they accept a `'a`. In v0's grammar, implemented later.
3. **Cut's concrete syntax.** The grammar above is for reading. The parser will
   settle exact rules for binders, literals and separators, and this document
   will quote them before v0 is frozen, since front ends emit the text.
4. **The primitive and operation tables.** Both are to be generated from
   `meadow_rt` and checked against the runtimes before v0 is frozen, so the
   document cannot drift from what runs.
5. **Multi-shot on Glade.** Glade could copy its heap frame chunks to resume a
   continuation twice. That is acceptable as an optimisation, as long as Cut's
   meaning does not change and both runtimes agree on the shared corpus.
