# MeadowBoot

Meadow's compiler, written in Meadow — the bootstrap. It is built with
[Lingua](https://github.com/meadow-lang/Lingua), Meadow's language workbench,
and its lexer is written by [Scythe](https://github.com/meadow-lang/Scythe).

Each pass is checked against the Rust compiler's, input by input. A pass
writes what it made as text, a line an item, and the test writes the Rust
pass's output the same way. A file whose two texts differ is a difference
between the compilers, shown at its first line. Nothing moves on to the next
pass until the one before agrees everywhere it is asked.

## Passes

| pass     | module                                                           | checked against                                      | inputs                                                                   |
| -------- | ---------------------------------------------------------------- | ---------------------------------------------------- | ------------------------------------------------------------------------ |
| lex      | `src/Lex.mw`                                                     | `meadow-lexer`'s `tokenize`                          | every `.mw` in the repository, `tests/lex/`                              |
| evaluate | `src/Cek.mw`, `Core.mw`                                          | `meadow-eval`, the Rust CEK machine                  | the programs `glade/tests/differential.rs` checks every back end with    |
| parse    | `src/Syntax.mw`                                                  | `meadow-parser`                                      | every `.mw` in the repository: each parses (the trees are compared next) |
| group    | `src/Fixity.mw`                                                  | `meadow-rename`'s `reassociate`                      | every `.mw` in the repository, each with its unit's fixities             |
| expand   | `src/Rules.mw`, `Hygiene.mw`                                     | `meadow-compiler`'s `macro` rules, by what they name | every call of a `macro` in the repository, `tests/rename/Macros`         |
| rename   | `src/Lower.mw`, `Rename.mw`, `Scopes.mw`                         | `meadow-rename`, as an editor reads it               | every `.mw` in the repository, each with its unit and what it depends on |
| infer    | `src/Infer.mw`, `Solve.mw`, `Traits.mw`, `Types.mw`, `Groups.mw` | `meadow-infer`, each top-level binding's scheme      | every `.mw` in the repository, each with its unit and what it depends on |
| lower    | —                                                                | `meadow-core`'s lowering                             |                                                                          |

### Lexing

Two Scythe lexers taking turns, as logos's `morph` has them: `Code` for the
program, `Piece` for the text of a string literal, each asked for one token at
a time from wherever the other stopped. That is how a `${…}` hole — code again,
strings and braces and all — is read, which no one pattern can match. What logos
decides by priority, Scythe decides by the order the variants are written in,
so they are written in that order; where logos matches a character, the
patterns match one of UTF-8's. An error spans what the machine read before it
gave up, as logos's does.

### Evaluating

The CEK machine is `meadow_eval`'s, frame for frame: the same continuation,
the same order of evaluation, deep one-shot handlers. It runs `Core`, a Lingua
language (`lang!`): a program is tables, a term a row of them, read with the
patterns `lang!` makes. For now Core is read from the core the Rust compiler
makes — generic definitions copied per number type, types erased — written in
`CoreText`, a grammar Lingua makes the parser of (`syntax!`); the language that
grammar implies, `Printed`, becomes Core by a pass (`pass! read`), which makes
numbers of numerals and a float of its bits. Numbers are Meadow's own: a
`UInt8` in the program is a `UInt8` here, so what it does is what the machine
running MeadowBoot does, which the Rust machine's differential tests already
hold to the same answers. Values print as the Rust machine prints them.

### Parsing

Lingua writes the parser from the grammar in `src/Syntax.mw` (`syntax!`), an
automaton decided when MeadowBoot is compiled: each choice a test of at most
five tokens ahead, and nothing tried and taken back. The Rust parser is a PEG
and does take things back; where it does, the grammar says the same thing a
way looking ahead can decide — patterns in parentheses read as expressions
first, as GHC reads them; a signature and a definition, or a context and a
type, read as one form and told apart after. Words that mean something only
where they are (`pattern`, `rec`, `.*`, `<:`, `:?`) and the names in a macro
call's path are kinds of their own, given by the lexer.

The abstract syntax is not a second tree. `syntax!` makes each rule a pattern
over the lossless tree, `SurfaceIfE cond yes no`, which matches an `if` node and
binds its parts, themselves trees to match further. `src/Parse.mw` reads the
tree with them and writes what `meadow-parser` would have built, desugarings
and spans included: several equations as one `match`, sections as lambdas,
`[a .. b]` as `range`, synonyms as their matcher and builder. What the grammar
reads as one form and the Rust parser does not accept is refused there too: an
expression in a pattern's parentheses that is no pattern, a context on a
binding, a signature with parameters. The parse test compares the two dumps
over every source in the repository and the cases in `tests/parse/`, those in
`tests/parse/rejected/` each one the Rust parser refuses.

### Grouping operators

The parser reads `a + b * c` as a chain; how it groups waits for every fixity
the unit declares. The fixity pass (`pass! regroup`) rewrites the parser's tree
as one of `Grouped`, a tree language (`lang!`) whose chains are `Infix` nodes,
grouped as `meadow-rename` groups them, each byte still where it was.

### Renaming

A unit's modules are lowered from their grouped trees (`src/Lower.mw`) to
`Ast`, a table language shaped as `meadow-parser`'s abstract syntax, read as
the parse test reads a tree. Renaming is a pass from `Ast` to `Hir`
(`pass! rename`), whose context is the scope and whose side table (`Lingua.Table`)
is every binder: what it is called, where it was declared, and every place it
is used. `src/Scopes.mw` works out each module's scope as the Rust resolver
does: locals; then the module's own names and what its `use`s bring, first come
first served; then the base -- the primitives, then the prelude. Nothing else is
in scope: a sibling's names and a dependency's arrive only through a `use`.

What is compared is every name a module writes and where what it names was
declared, `from to kind file@offset`, which the Rust side reads off the resolved
HIR the way the language server does (`meadow_lsp::analysis::mentions`). A unit
is renamed with what the units before it export, so each unit's interface
carries its exports, and Lingua's build (`make!`) compiles them in order.

A call of one of the module's own `macro`s is expanded here. `src/Rules.mw`
matches it against the rules and writes the template out as tokens, as
`meadow_compiler::expand::rules` does: each token the template wrote stands
where the call's argument is, and each the call passed in stands where it was
written. Every lowercase name the template wrote is marked with the
expansion's number, `tmp#3`, counted as the Rust expander counts, a call and
then what it wrote. The parser reads the tokens where they stand
(`parseSurfaceTokens`, which Lingua writes beside `parseSurface`): a tree whose
text is the tokens' and whose every node spans what its tokens stand for, so
each node of what a macro wrote spans what the Rust parser's does. It is
grouped by the unit's fixities and lowered like any other. Then
`src/Hygiene.mw` takes the marks off every name that is not a local, slot for
slot as `meadow_compiler::expand::hygiene` does. The rename test's
`tests/rename/Macros` package calls a macro of each kind: recursive, hygienic,
in a pattern, writing declarations, labels, local functions, token trees.

Any other macro call is what the Rust compiler expanded it to, for now, as
the CEK machine's core is what it lowered. The expander records every
expansion, and each `@derive`'s, as JSON of its syntax with its spans
(`meadow_compiler::expand::record`). The rename test writes a package's to
`Pkg.json` where `MEADOWBOOT_EXPANSIONS` says, a unit reads that as one of its
files, and `Lower` takes an expansion where its call is. A chain of operators
in one is grouped there by the unit's fixities. A call of the module's own
`macro` never falls back to this, so the test sees what `Rules` makes of it.

### Inferring

Inference is `meadow_infer`'s, step for step: Algorithm J over an arena of
meta variables linked by union-find (`src/Types.mw`), written inside a
`runSt`; generalization by levels, as OCaml's; records and effects as rows.
A unit's top-level bindings are inferred group by group, each group a strongly
connected component of what mentions what (`src/Groups.mw`, `meadow_scc`),
found three times as the Rust compiler finds them: the modules in order, each
module's bindings in order, and the unit's bindings as one graph walked in
those orders, which is what breaks ties. A trait's `where` is what an
instantiated scheme wants, answered by an `impl` once its type is known that
far, or joined to the `where` of the function whose variable it is
(`src/Traits.mw`). A name with several meanings is chosen by what fits. The
primitives' types are the Rust compiler's table, written out as Meadow
(`src/Prims.mw`, by the test `write_prims`), whose test holds every one to
how the Rust compiler writes it.

Inference is a pass, `infer : Hir -> Inferring` (`src/Infer.mw`): every
expression and pattern is given its type by a case of its own, and `settle :
Inferring -> Typed` then reads each type out of the arena. A case infers a
child in a context of its own by taking it `later` -- a lambda's body in the
lambda's region, a `let`'s value a level in -- and one it has to look at
before it can say how by taking it `raw`: `runSt`'s argument, a handler's
clauses, a constructor given a record for its named fields. Children are
inferred in `meadow_infer`'s order, since what unification knows when a field
is selected or a name chosen depends on it. What no one node sees -- the order
a unit's top-level bindings are inferred in, group by group across its
modules, each group generalized when it is done -- is a driver's, which hands
each binding to the pass in turn and writes it into its module's tables. The
solver the cases call -- the arena, unification, rows, levels, traits -- is
library code (`Types`, `Solve`, `Traits`), as the scopes are rename's.

A name is known by its binder, as a `VarId` is in the Rust compiler; a type,
an effect, a trait and a constructor by its canonical name, the fully
qualified path renaming gives each (`Scopes.qualified`). Renaming leaves
inference what the rename dump does not show: a name's other meanings, the
operation a handler clause answers, the effect a written row names. A unit's
interface carries its exports' schemes and everything it knows about types,
as a dependency's declarations are to a Rust compiler's unit.

What is compared is each top-level binding's scheme, `from to name : scheme`,
as `meadow build --types` writes it: quantifiers lettered by kind, `where` in
front, an effect variable said once left out.

## The language server

The tools built on the Rust front end go with it, so `meadow lsp` is
MeadowBoot's too: `src/Lsp.mw`, a server declared with Lingua's `lsp!`. What
an editor shows should not change when the compiler behind it does, so the
server answers what the Rust one (`buildtools/meadow-lsp`) answers, in the
same shapes, and is held to it by a differential like every pass.

Lingua speaks the protocol; every request is one of the language's own
(`requests`), written in the Rust server's shape, and what the server says it
can do is the Rust server's word for word (`capabilities`). A document is
read as part of its package, as the Rust server reads it: when one is opened,
the package is built -- by another run of this program, `units`, so that
nothing the build prints reaches the editor -- and what each unit was
compiled from is read back. The package is then studied (`Compiler.studyUnit`)
with the open documents' texts standing for their files: renamed, its types
inferred, and each document's names, types and macro calls read off that.
A document in no package is studied as a package of its own, as the Rust
server analyses one alone. Formatting is `meadow fmt`'s until MeadowBoot's
formatter is ready.

Workspace symbols -- the editor's own picker over every declaration, by name
or by type -- are `meadow_find`'s search, ported as `src/Find.mw`. What is
searched is read off each open package's study and what each unit it imports
exports: every type, trait and effect and its constructors, every value a
dependency exports and every top-level value of the package itself, each a
value listed as its signature. A name is matched as the Rust server's
`nucleo` matcher scores it, step for step, so that the same query finds the
same declarations in the same order; a type, as Hoogle does, by unifying it
with each signature and pricing what that took.

`meadow/ir` answers a definition as core: the package's core as `Elab` makes
it (`Compiler.studyCore`), written as `meadow_core` writes its own
(`Core.prettyDefs`), every binder and type variable renumbered in the order
it is first written. The two cores are the same program but for effects:
MeadowBoot's core carries no effect types, since nothing after `Elab` needs
them, where the Rust core quantifies each definition over its effect
variables and applies it to them. Cut and AxCut are not answered yet.

`meadow lsp --check PATH…` is the server too, run over files rather than
an editor: each `.mw` file under the paths opened in turn, as an editor opens
it, and every error the server would mark printed where it is, then `no
errors in N files` or how many there were, with the exit status saying which.
It is what CI and a release run over every source in the repository, so a
file the editor would mark fails the build whichever compiler is `meadow`.

`buildtools/meadow/tests/lsp_differential.rs` starts both servers and sends
them the same session: every file of a package opened, then hover, go to
definition and prepare rename at every name, a rename of each name, semantic
tokens, inlay hints, code lenses, formatting, completion after each `|>` and
`.` and inside names, `meadow/ir`, workspace symbols, an edit and its undoing,
and a close. It reports agreement by kind of request, and is ignored until
the two agree; its doc comment says how far they do, and why the rest
differs. A refusal, a file changed on the disk and a document closed, which
Lingua's `lsp!` once could not say, it now does: a closed document's text
stops standing for its file (`closed`), and the package goes back to what is
on the disk.

## Running

```sh
meadow run --release bootstrap -- lex FILE…     # tokens, one a line
meadow run --release bootstrap -- eval FILE…    # a core program's value
meadow run --release bootstrap -- parse FILE…   # each file's abstract syntax
meadow run --release bootstrap -- fixity FILE…  # each chain of operators, grouped
meadow run --release bootstrap -- rename FILE…  # each name, and what it names
meadow run --release bootstrap -- infer FILE…   # each top-level name's scheme
```

and the differential tests, from `buildtools/`:

```sh
MEADOWBOOT_MEADOW=target/release/meadow cargo test -p meadow --test bootstrap
```

`MEADOWBOOT_MEADOW` names the `meadow` that builds and runs MeadowBoot; the
debug one the test is built with works too, slowly.

## How fast it is

MeadowBoot is to replace the Rust front end, so it has to be about as quick
to use: a build, an entry at the prompt and an edit in the editor should not
be noticeably slower. Two tools say how far off it is.

`MEADOWBOOT_TIMINGS=1` has MeadowBoot say how long each pass of each unit
took, as the unit finishes -- a build's, a prompt entry's, and each time the
language server studies a package:

```text
timings Std:   2561.8 ms
  parse            77.2 ms
  fixity          101.1 ms
  lower           188.4 ms
  rename          124.4 ms
  infer           706.7 ms
  elaborate       241.4 ms
  ...
```

`scripts/bench-meadowboot.py` runs the same work through MeadowBoot and the
Rust `meadow`, both release builds, and prints the two side by side: builds
of `examples/` cold and after an edit, the standard library from its
sources, MeadowBoot itself, sixty prompt entries, and edits in the language
server. `--passes` adds the timings above, summed over each case's units.

What it said on one Windows desktop (28 cores), in milliseconds:

| case                          | `meadow` | MeadowBoot, Nov 13 | MeadowBoot now |
| ----------------------------- | -------: | -----------------: | -------------: |
| `examples/Euler`, cold build  |      150 |               3469 |           3378 |
| `examples/Euler`, after edit  |      153 |               1744 |            758 |
| `examples/MiniML`, cold build |      217 |               3564 |           3396 |
| `examples/MiniML`, after edit |      218 |               1966 |            874 |
| a prompt entry, checked       |        4 |                179 |             42 |
| the prompt, started and quit  |      131 |               1336 |            554 |
| a file opened, to diagnostics |      225 |                911 |            500 |
| an edit, to its diagnostics   |       16 |                300 |            265 |

A cold `meadow` build has the standard library compiled into the binary; a
cold MeadowBoot build compiles it, about 2.5 s of the time. Most of a build
after an edit is reading the standard library's interface, which Lingua's
build does twice -- once for the unit and once for the link -- at about
300 ms each. A unit's own passes take 70-90 ms of a small package's build.

What was slow, and is not now:

- A unit's database held the interfaces of every unit it imports, as an
  input, though only their fixities are asked of it there; a database keeps
  an input where any thread can read it, which copied the standard library's
  interface for every unit and every prompt entry -- 90-115 ms each. It is
  given their fixities alone.
- Renaming found each dependency's type's constructors by reading every
  constructor the dependency has, for each of its types; with the standard
  library that was a few hundred thousand comparisons per unit. They are
  grouped by their type once.
- The check for a value a module defines twice compared every export with
  every other: 150 ms of compiling the standard library. It keeps the first
  of each name.
- The walk that copies definitions for number types (`Inline.numbered`) also
  worked out which definitions could be inlined and what each dictionary
  holds, which only the walks that inline or read dictionaries ask.
- An interface was kept as the JSON Lingua writes a `Datum` as: an object
  for each record and constructor, an array element for each number. The
  standard library's came to 8.4 MB, and reading it -- the JSON parsed,
  made a `Datum`, the `Datum` made an interface -- took about 900 ms, and
  writing it as long. An interface is now one string (`Packed`): each datum
  a letter and what it holds, each string written once and then named by
  its number. The standard library's is 4.5 MB, read in about 200 ms and
  written in about 500.

## The back end, in the same process

What MeadowBoot does not do itself -- building its Cut into a program, and
the commands it has no code for yet -- is the Rust `meadow`'s. It loads that
as a library, `meadow-host` (`buildtools/meadow-host`: `meadow_host.dll`,
`libmeadow_host.so`, `libmeadow_host.dylib`), and calls its command line
through `Std.Ffi`, so that MeadowBoot can be the program `meadow` runs with no
second compiler beside it. It looks for the library where `MEADOW_HOST`
says, then beside itself, as a release's archive has it. `MEADOW` naming a
`meadow` program asks for that program instead, run as a process of its own,
and so does a missing library. The prompt calls two more of the library's
functions, which a program could not stand in for: `meadow_repl_run`, which
runs an entry's program of Cut on Glade and hands back what it answered, and
`meadow_repl_pages`, which says what the back end makes of it, for `:ir`
(see the next section).

`Std.Ffi` is unstable, so a stable `meadow` builds MeadowBoot only with
`MEADOW_BOOTSTRAP=1`, which lets it take what a nightly takes, as
`RUSTC_BOOTSTRAP` does rustc. A build from a checkout, or a nightly, needs
nothing.

## What `build` and `run` are told

MeadowBoot's `build` and `run` take `meadow`'s flags, and each one does
what it does there. A flag accepted and quietly ignored would be worse than
one refused: whoever moved from `meadow` to MeadowBoot would take the run
for the one they asked for. So what MeadowBoot does not act on yet it says
it does not, on standard error -- ``meadowboot: `--features` is not acted on
yet`` -- and builds as though the flag were not there.

What a flag asks about the program is the front end's, and MeadowBoot's own
passes answer it (`src/Listing.mw`, and `drivenWith` in `src/Main.mw`):

- `--types` writes the type of every value each package of the build
  exports, but the standard library's, then the program's entry, as
  `meadow`'s `LinkedProgram::dump` writes them: from MeadowBoot's inference,
  each unit studied again as the language server studies it, its `@cfg`s
  decided as the build decided them. `--annotations` adds the type of every
  node, each package's numbered from nought, the standard library's first.
  A node is numbered as MeadowBoot's typed tree has it -- each expression and
  pattern -- where the Rust compiler numbers every node of its own tree, so
  the numbers, and the count `--types` gives of them, are each compiler's
  own.
- `run --cek` builds with `@cfg(backend = "cek")` holding and runs `main` on
  MeadowBoot's CEK machine, every unit's core linked into one program, as
  the debugger runs one. With `--gc-stats` it says, as `meadow` does, that
  the machine has no collector to report on.
- `--no-prune` links every definition of every unit rather than what the
  entry reaches (the build's `prune` option).
- `--runtime`, `--backend`, `--jit` and `--aot` decide what
  `@cfg(backend = …)` holds of, as they do in `meadow`.

What a flag asks about running the program is the back end's, and it is
handed to `meadow cut` with the program: `--backend vm`, the interpreter
without the JIT; `--gc` and `--gc-stats`; `--profile-to`, `--sample-every`
and `--sample-hz`; and `--train`, which writes what the JIT saw to
`target/<profile>/calls.pgo`, which an `aot` build of that profile is then
guided by (`meadow cut --calls`). `-j`, `--nursery` and `--leaks` the
runtimes read from the environment, which MeadowBoot sets as `meadow` does.
`build --emit image,bytecode,asm,exe` asks `meadow cut --emit` for each, and
writes it where `meadow` writes it under `target/<profile>`. The image, the
code and the executable are of MeadowBoot's program, so they are not the
Rust compiler's byte for byte, and a profile names its frames by the
symbols of the program's Cut.

What MeadowBoot does not act on yet: `--emit expanded`, which needs each
macro call written down as MeadowBoot expands it; a package's features
(`--features`, `--no-default-features`, `--all-features`); and a
workspace's members (`-p`, `--workspace`, `--exclude`).
`buildtools/meadow/tests/flags_differential.rs` runs each flag on both
compilers and compares what they say -- what the front end says as it is,
and what the back end makes by its shape -- and checks that MeadowBoot says
which it does not act on.

## The prompt

`meadow` with no command is the prompt, and MeadowBoot's is its own
(`src/Repl.mw`), declared with Lingua's `repl!`. Retiring the Rust front end
means every tool built on it has to be written again over MeadowBoot's
passes, and a tool declared in Lingua is one whose line editing, history,
colour, completion and `:` commands come from the same place as every other
Lingua language's: what is left to write is what is Meadow's own.

It answers what `meadow repl` answers, line for line. Each entry is a unit
of its own, named `repl:N` as the Rust prompt names them, compiled against
the standard library and every entry before it (`Compiler.enteredUnit`).
What those define is in scope bare, the newest of a name over an older one
(`Scopes.isEntry`), so a name defined again is a new definition and what was
compiled against the old one goes on using it. An expression is the entry
`def it = …`. The `use`s typed so far are written ahead of each entry, so a
qualifier lasts for the session. When an entry is finished is the Rust
prompt's rule, ported (`needsContinuation`): `repl!` reads another line while
every error its `errors` reports is at the end, and the prompt reports one
there exactly when the Rust prompt would read on.

An entry is run where `meadow repl` runs one: on Glade, with its JIT
(`:jit`, the default) or without it (`:vm`), in this process, through
`meadow-host` (see the section before). The Rust prompt keeps each entry as
the compiler left it and makes each one a program of its own, of what it
reaches of the standard library and of the entries before it; this prompt
does the same with what MeadowBoot leaves of a unit, its object of Cut --
each definition's declarations, with the symbols they mention. It reads the
standard library's object once, from where the standard library was built,
keeps each entry's as it is run, and links an entry as a build links a
program (`Repl.linkedEntry`, after `Compiler.linkAt`): what its last value
reaches of every object, and a definition that answers that value as the
text the prompt shows (`Seq.aroundShown`). `meadow_repl_run` lowers that
and runs it, and nothing is kept on the back end's side from one entry to
the next. So an entry costs what `meadow repl`'s does -- `fib 30` runs in
under a millisecond -- and a program with threads runs as it does there.
`:cek` runs an entry on MeadowBoot's own CEK machine instead (the standard
library's core, each entry's and its own linked into one program,
`Core.linkedUnits`), and so does every entry where the library cannot be
loaded. The standard library is built once, in `meadowboot-repl` in the
scratch directory, by another run of MeadowBoot, so that nothing a build
prints reaches the prompt. `:tour` takes the Rust prompt's tour, which
`src/Tour.mw` is written out from (see its header).

`:ir` shows an entry, or the definition a name means, in each IR, a page
each, as `meadow repl`'s does: its core as MeadowBoot made it, written as
the Rust compiler writes core (`Core.prettyDefs`, which the language
server's `meadow/ir` writes with too); and then what `meadow_repl_pages`
says the back end makes of the Cut it is linked into -- the Cut itself, AxCut, Glade's bytecode and the machine code its JIT
compiles, and the LLVM IR a Silo build would. They are MeadowBoot's IRs, not
the Rust compiler's: a definition's symbol is its binder (`meadow:repl:3/d7`),
where the Rust compiler's is its name (`meadow:repl-3/x`).

What it does not do yet: Tab completes from the session's names, not from
the compiler's scope at the cursor.

`buildtools/meadow/tests/repl_differential.rs` types the same sessions into
both prompts and compares what they print, entry by entry, and reports how
far they agree by kind of entry. Its doc comment has the numbers.

```sh
MEADOWBOOT_EXE=bootstrap/target/release/native/silo/MeadowBoot.exe \
  cargo test --release -p meadow --test repl_differential -- --ignored
```

## Tests

`meadow test` is MeadowBoot's (`src/Testing.mw`), the standard library's
tests (`--std`) too, so that no test needs the Rust front end to be run. The
package is built with `@cfg(test)` holding in it, and each module is read
with its tests listed at its end (`src/Tests.mw`); `--std` builds the
standard library by itself the same way. The back end makes the program
ready once -- its bytecode kept, or an executable linked -- and each test is
then a run of it of its own, as each of the Rust runner's tests is a run of
its own: a test stopped by a runtime error fails alone, with that error, and
what a test prints is kept to be shown beside it if it fails. Runs go side
by side, `--test-threads` of them, and `--no-capture` lets what they print
through. `--cek` runs the tests on MeadowBoot's CEK machine, in its own
process and one after another, since the machine's cells cannot cross to
another thread.

What is said is what `meadow test` says, and
`meadowboot_test_says_what_meadow_test_says` and
`meadowboot_runs_the_standard_library_s_tests` in `tests/bootstrap.rs` hold
the two to it, flag for flag and on the standard library's 385 tests. Where
they may differ is order: tests run side by side are said as they finish,
and failures in the order the tests are declared, which each compiler
counts in its own order of modules.

`dis` is MeadowBoot's build too, listed by the back end: `meadow cut --emit
bytecode`, or `--emit asm` for `dis --asm`.

### The standard library, compiled ahead of time

A release of the Rust `meadow` carries the standard library compiled, so
that no build compiles it (`__precompile-std`, `buildtools/meadow/src/stdlib.rs`).
What it carries is its own front end's output, a serialised
`CompiledPackage`, which nothing of MeadowBoot's reads. MeadowBoot's
`__precompile-std OUT [--target TARGET]` writes its own: for the `debug` and
the `release` profile, the library's unit as a build keeps it -- its
interface, and its objects of Cut, which the back end links into a program
as it links any unit's. A build finds it where `MEADOWBOOT_PRECOMPILED_STD`
says, or as `Std.mwstd` beside the library's sources, as a release would lay
it out, and takes the compile its options ask for in place of compiling the
library. Each compile in the file is named by the options and the library's
sources, the file by the size of the MeadowBoot that wrote it, so another
compiler's, or one for other options, is not taken: the library is compiled
as before. The file is one of the library's sources to the build, read
each time it plans; its 34 MB cost a warm build about a third of a second,
and save a cold one the library's compile, two to three seconds.

## The debugger

`meadow dap` is MeadowBoot's as well: `src/Dap.mw`, a debug adapter declared
with Lingua's `dap!`. Lingua speaks the Debug Adapter Protocol -- the
request queue, running in slices so that a pause lands, the program's output
sent to the editor's console -- and what is Meadow's own is the machine: a
program that runs a little at a time and can say where it is and what it
holds.

That machine is the back end's: the program runs on Glade, as `meadow dap`
runs one. A launch builds the package as `meadow build` would, by a run of
this program (`units`), so that nothing the build prints reaches the editor,
and hands the program the build linked to `meadow-host`, which holds the
session open in this process (`meadow_debug`, JSON in and out) and runs it
to its first boundary. The session is the Rust adapter's own
(`buildtools/meadow/src/dap/session.rs`), so stepping, the stack, scopes and
values are what `meadow dap` makes of Glade's bytecode, at Glade's speed.

What makes that possible is that a debug build says where it was written.
The debug profile has `positions` set, and a unit built with it writes into
its Cut (`src/Shown.mw`, `Seq.lowerAt`; `docs/CUT.md`, "Positions and
names"):

- a `source` for each file it was written in, named by a symbol of the
  package's -- `meadow:App/"src/Main.mw"` -- so that the units' objects,
  linked text by text, name their files apart;
- a position on each term a step may stop before, at the terms below, by the
  term's span in the file its definition was written in;
- what the source calls each binder and its type, `v12: i64 @"count"
  "Int"`, and what it calls each definition, `def S @"double"`, which the
  stack and the locals are shown by. A definition copied for a number type,
  or for the dictionaries it is given, is shown as the one it is a copy of:
  `Shown.originsOf` finds that by where the copy's terms were written. A
  local function is shown by the name the Rust core gives what it lifts out
  of a definition, `mainLocal1` (`Seq.lambdasIn`).

The standard library is built with them too in a debug build, as the Rust
compiler's debug information covers it, so a step goes into it as
`meadow dap`'s does. They cost little: a debug build of the library is a fifth
larger as Cut and takes as long. `meadow cut` takes them off before it builds
or runs a program, so a debug build runs as it did, and compiles to the same
bytecode and LLVM IR.

Where `meadow-host` cannot be loaded, or `MEADOWBOOT_DAP=cek` asks, the
machine is MeadowBoot's CEK machine, as the prompt's can be. A launch then
makes each unit's core (`compilerWhole`), links them into one program
(`Core.linkedUnits`), and runs `main` to its first boundary. The rest of this
section is of how that machine does what Glade's debug information does.

- **Where a step may stop.** The terms that stand where the Rust core has a
  position (`meadow_core::Term::Loc`) are the machine's boundaries: the head
  of an application, a function's body, a branch of an `if`, an arm of a
  `match` and its guard, a `let` as it was written, a definition's body that
  is not a function. The machine stops before each and hands itself back
  (`Cek.Watch`), and the adapter decides whether that is a stop: a
  breakpoint, or the end of a step. Breakpoints are by line, each moved to the
  first line at or just after it that has a boundary, as the Rust adapter
  moves them. Only the package's own code has boundaries; what it depends
  on runs through without one.
- **The stack.** The CEK machine has a continuation, not a call stack, as
  Glade has none. While a debugger watches, entering a function's body puts
  a frame on the continuation saying where its caller waits and with what
  in scope; a tail call puts none, so the stack is the one a person expects.
  `next` and `stepOut` compare how many such frames there are.
- **Names, places and types.** A term's place is its span in the module its
  definition was written in, found by the binder's site; a frame's name is
  its definition's; a local's name is its binder's, and its type what
  inference gave it, its variables named by kind (`n` for a number's) as the
  Rust compiler names them. A value is shown as the Rust adapter shows
  Glade's (`Cek.preview`). Evaluating looks a name up in a frame.

What it does not do yet: a launch's `entry`, which starts at an expression
rather than at `main`. On the CEK machine, the raw machine's scopes
(registers, handlers, the heap) are not beside a frame's locals, a frame
below the top shows everything its scope holds, where the Rust adapter shows
what its continuation captured, the standard library is not stepped into,
and there are no threads.

`buildtools/meadow/tests/dap_differential.rs` drives both adapters through
the same sessions -- each program stepped in, over and out from its entry,
and run from breakpoint to breakpoint with one on every line -- and at each
stop asks for the stack, the scopes and locals of the first frames, and
every local evaluated. It reports how far they agree by kind of request;
its doc comment has the numbers.

```sh
MEADOWBOOT_EXE=bootstrap/target/release/native/silo/MeadowBoot.exe \
  cargo test --release -p meadow --test dap_differential -- --ignored
```

## What the Rust compiler learned

Writing a second compiler finds what the first one does that nothing else
checks:

- The CEK machine printed a record's fields in the order their labels were
  interned — the process's order, not the program's. It prints them by label
  now, as every other machine does.
- A recursive group's functions were seeded as bare type variables, so a
  sibling applied to fewer arguments than it takes (`map (f k) xs`) was taken to
  perhaps perform something, and that guess was joined to the caller's effect.
  When the function turned out to be pure until its last argument, the guess
  closed the caller's effect too: `the effect Tick is not allowed here`. A
  function is seeded as the function it is now.
- A native build of a large program named each of its hundreds of objects on
  clang's command line, past the 32K characters Windows allows. They are in a
  response file now.
- Type names were unique across a package, not by their path: two modules'
  private `type Syn` clashed, and one module could resolve the other's. A type
  is known by its fully qualified path now, the standard library's too.
