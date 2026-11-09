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

`buildtools/meadow/tests/lsp_differential.rs` starts both servers and sends
them the same session: every file of a package opened, then hover, go to
definition and prepare rename at every name, a rename of each name, semantic
tokens, inlay hints, code lenses, formatting, completion after each `|>` and
`.` and inside names, `meadow/ir`, workspace symbols, an edit and its undoing,
and a close. It reports agreement by kind of request, and is ignored until
the two agree. Where they cannot yet, the reason is one of three: MeadowBoot
has not ported it (workspace symbols, `meadow/ir`); Lingua's `lsp!` cannot say
it (a refusal as an error, diagnostics without `relatedInformation`,
`initialize` without `positionEncoding` and `serverInfo`, notifications other
than opening, changing and closing, a close that forgets the document); or
the two compilers' answers differ, which is what the test is for.

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

## The back end, in the same process

What MeadowBoot does not do itself -- building its Cut into a program, and
the commands it has no code for yet -- is the Rust `meadow`'s. It loads that
as a library, `meadow-host` (`buildtools/meadow-host`: `meadow_host.dll`,
`libmeadow_host.so`, `libmeadow_host.dylib`), and calls its command line
through `Std.Ffi`, so that MeadowBoot can be the program `meadow` runs with no
second compiler beside it. It looks for the library where `MEADOW_HOST`
says, then beside itself, as a release's archive has it. `MEADOW` naming a
`meadow` program asks for that program instead, run as a process of its own,
and so does a missing library.

`Std.Ffi` is unstable, so a stable `meadow` builds MeadowBoot only with
`MEADOW_BOOTSTRAP=1`, which lets it take what a nightly takes, as
`RUSTC_BOOTSTRAP` does rustc. A build from a checkout, or a nightly, needs
nothing.

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
