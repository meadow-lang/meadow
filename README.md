# Meadow

A small ML-family language: Hindley–Milner inference with row-polymorphic
records, algebraic effects with deep one-shot handlers, and a register bytecode
VM with a copying collector.

```
fun classify n =
  match compare n 0 with
  | Less    -> "negative"
  | Equal   -> "zero"
  | Greater -> "positive"

fun main () = println (classify 42)
```

## AI disclosure

Meadow is written with AI coding agents: Anthropic's Claude, through Claude
Code. Most of the code, the tests, the documentation and the commit messages in
this repository were written by an agent, under the direction of the project's
author, who decides the design and what goes in. Read it, and rely on it, with
that in mind.

## Install

**Windows** — download `meadowup-x86_64.exe` from the
[latest release](https://github.com/meadow-lang/meadow/releases/latest) and run
it. (Take `meadowup-aarch64.exe` on an ARM machine.)

**macOS / Linux / Android**

```sh
curl -fsSL https://raw.githubusercontent.com/meadow-lang/meadow/master/scripts/meadowup-init.sh | sh
```

On Android, run that in [Termux](https://termux.dev): it notices it is on
Android and takes the build linked against Android's own C library. The REPL and
`meadow run` need nothing else; a `--release` build links a native executable,
which wants a C compiler — `pkg install clang`.

Either way you get `meadow` and `meadowup` in `~/.meadow/bin`, added to your
`PATH` — open a new terminal and run `meadow`.

The two split the way Rust's do:

|            |                                                                            |
| ---------- | -------------------------------------------------------------------------- |
| `meadow`   | the build system — `build`, `run`, `test`, `add`, `update`, `fmt`, `clean` |
| `meadowup` | the toolchain — which version of Meadow you have, and where                |

```sh
meadowup update      # bring the toolchain up to date
meadowup show        # what is installed, and where
meadowup uninstall   # remove it and undo the PATH entry
```

<details>
<summary>Options</summary>

On every platform, the installer is `meadowup`. `meadowup-init.sh` downloads
it for your machine and runs `meadowup install`, and the Windows `.exe` is
`meadowup` itself. `meadowup` then copies itself into `~/.meadow/bin` and
fetches the rest of the toolchain. As with `rustup-init`, there is no separate
installer program.

Options after `sh -s --` are passed to `meadowup install`:

```sh
curl -fsSL https://raw.githubusercontent.com/meadow-lang/meadow/master/scripts/meadowup-init.sh \
  | sh -s -- --version v0.2.0 --no-modify-path
```

```sh
meadowup install 0.2.0                    # a particular release
meadowup install nightly                  # master's latest build, and follow it
meadowup default stable                   # follow the releases again
meadowup update                           # bring the toolchain forward
meadowup update --force                   # install again even if current
meadowup install --no-modify-path         # leave profiles and PATH alone
meadowup show                             # what is installed, and where
meadowup uninstall
```

**Building from source.** `scripts/install.sh` builds `meadow` (the compiler,
build system and language server) and `meadowup` from the checkout it is in.
The `meadowup` it builds then installs both binaries in the same layout a
release uses, so afterwards `meadowup update` moves the source build on to the
next release like any other install. You need a Rust toolchain, and the build
takes a few minutes.

```sh
git clone https://github.com/meadow-lang/meadow && cd meadow
scripts/install.sh                     # build and install this checkout
scripts/install.sh --with-extension    # ...and the VS Code extension and theme
scripts/install.sh --version <tag>     # fetch that tag into ~/.meadow/src and build it
scripts/install.sh --local <path>      # build the checkout at <path> instead
scripts/install.sh --no-modify-path    # leave shell profiles alone
scripts/install.sh --uninstall
```

`meadowup show` tells you whether you have a release or a local build. A local
build has the same version number as the release it followed, and
`meadowup update --force` replaces it with that release.

`meadowup install --from <dir>` installs binaries from a directory instead of
downloading them. `install.sh` uses it, and it also installs an unpacked
archive with no network access at all.

`MEADOW_HOME` overrides the install directory for both.
</details>

A full walkthrough of the language lives in [docs/TUTORIAL.md](docs/TUTORIAL.md).
How programs run -- the backends, the memory model, effects and threads -- is
in [docs/RUNTIME.md](docs/RUNTIME.md). Macros -- declarative, procedural, and
the compile-time bindings they leave for one another -- are in
[docs/MACROS.md](docs/MACROS.md).

## Examples

Each is a package: `meadow run examples/<name>` runs it, `meadow test
examples/<name>` runs its tests.

|                                                     |                                                                                                                                  |
| --------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| [`tour`](examples/Tour)                             | the language in one program: data types, records, matching, numbers, modules, local mutation                                     |
| [`effects`](examples/Effects)                       | one program under different handlers: logging, configuration, failure, state, a seeded random game, a tested interactive greeter |
| [`streams`](examples/Streams)                       | generators with `yield`: infinite streams, `take`, `filter`, `map`                                                               |
| [`concurrency`](examples/Concurrency)               | green threads and channels: a worker pool, a pipeline, fan-out and fan-in                                                        |
| [`parallel`](examples/Parallel)                     | splitting work across every core, and sharing a compacted table between threads                                                  |
| [`stm`](examples/Stm)                               | transactional memory: a bank whose total never wavers, transfers that wait for funds, bounded queues                             |
| [`mini-ml`](examples/MiniML)                        | a small ML with Hindley-Milner inference and an interpreter, over four modules                                                   |
| [`euler`](examples/Euler)                           | Project Euler problems                                                                                                           |
| [`rock-paper-scissors`](examples/RockPaperScissors) | an interactive game, tested by feeding it input                                                                                  |

## Use

```sh
meadow                          # REPL
meadow init                     # start a package here, named after the directory
meadow init pkg --name myPkg    # ...or elsewhere, under a name you choose
meadow run examples/Euler       # build a package and run `main`, on the VM and its JIT
meadow run --types pkg          # ...printing every top-level binding's type first
meadow run --release pkg        # ...optimized, as an executable compiled ahead of time
meadow run --backend vm pkg     # ...on the bytecode VM alone (or `jit`, `aot`)
meadow run --cek pkg            # ...on the CEK machine
meadow build path/to/pkg        # type-check, link, and write the bytecode image
meadow build --release pkg      # ...optimized, `match` exhaustive, and an executable
meadow build --aot --target x86_64 pkg  # an executable for another architecture
meadow run -O2 pkg              # ...or just the optimization level
meadow dis pkg                  # disassemble: the bytecode the VM would run
meadow dis --asm pkg            # ...or the native code it compiles to
meadow build --emit bytecode,asm pkg  # write those as text, in place of the binaries
meadow run pkg -- in.txt -v     # pass the program arguments, which `Process.argv` reads
meadow exec target/debug/bytecode/pkg.mbc   # run a bytecode image
meadow link pkg.mbc -o pkg      # ...or compile one into an executable
meadow link --emit asm pkg.mbc  # ...or into the text of its native code
meadow fmt src                  # format .mw sources in place
meadow fmt --check src          # ...or just report, and exit 1 if any differ
meadow test                     # run the package's `@test` functions
meadow test . parse             # ...only those whose name contains "parse"
meadow test . Parser.parse --exact  # ...or exactly one
meadow init --workspace shop    # a workspace; `meadow init` inside it adds a member
meadow run -p app               # in a workspace: a member, by name
meadow test --workspace         # ...or every member (`--exclude NAME` leaves one out)
meadow add owner/repo           # add a dependency from GitHub
meadow add owner/repo --tag v1  # ...at a tag (or `--branch`, `--rev`)
meadow add ../util              # ...or a directory
meadow update                   # bring dependencies forward, rewriting Meadow.lock
meadow update json --dry-run    # ...one of them, and only say what would change
meadow build --locked           # fail rather than change Meadow.lock (what CI wants)
meadow build --offline          # never fetch; use what is already cached
```

### Dependencies

There is no central registry yet, so a dependency is the repository it lives in:

```toml
[dependencies]
json = { git = "https://github.com/someone/meadow-json", tag = "v1.2.0" }
util = { path = "../util" }
```

`git` takes an optional `branch`, `tag` or `rev`. `meadow add` writes these for
you, and finds the package's name by reading its own manifest rather than
guessing it from the URL.

A package that is one directory of a repository, rather than all of it, is
named by `subdir`:

```toml
Cut = { git = "https://github.com/someone/monorepo", rev = "…", subdir = "lib/Cut" }
```

What was actually used goes in **`Meadow.lock`** — the commit, and a hash of its
source tree. Commit that file: it is what makes a build on another machine the
build you tested. A branch moves, and a tag _can_ be moved, so a build follows
the lockfile and never the reference; `meadow update` is how the lockfile
changes. If a tag comes to name a different commit than the one locked, the
build stops and says so rather than quietly compiling something else.

Repositories are fetched into `~/.meadow/git`, one checkout per commit, shared
by every package on the machine.

A package is a directory with a `Meadow.toml` and a `src/`, which `meadow init`
writes for you; `meadow run` also takes a single `.mw` file. The `Std` library
is embedded in the binary, so there is nothing else to install.

What a build makes goes in the package's own `target/` directory, one directory
per profile: the bytecode image at `target/debug/bytecode/<name>.mbc`, and
native object code and executables under `target/<profile>/native/`. `--emit`
chooses what is written: `image` and `exe` are the binaries, `bytecode` the image
as text (`<name>.mbc.txt`) and `asm` the native code as text (`<name>.s`). `meadow
init` writes a `.gitignore` that ignores it.

Builds are **incremental**. Every package that compiles cleanly, and the
embedded `Std`, is kept under `target/<profile>/incremental/` with a
fingerprint of its sources, the compiler and its options, and the packages it
depends on. The next build reads back each one whose fingerprint still matches,
so an edit recompiles only the package changed and those downstream of it.
`MEADOW_INCREMENTAL=0` turns this off.

Packages developed together can be a **workspace**, as in Cargo: a root
`Meadow.toml` with `[workspace] members = ["app", "libs/*"]`, whose members
share one `target/`, the root's `[profile.*]`, and a version and dependencies
declared once under `[workspace.package]` and `[workspace.dependencies]`
(`util = { workspace = true }` in a member). The
[tutorial](docs/TUTORIAL.md#workspaces) has the details.

### Two runtimes: Glade and Silo

A program runs on one of two runtime systems, and they are two backends of
their own:

| runtime                 | what it is                                                                                                                                                                             |
| ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Glade** (the default) | bytecode, on an interpreter and a JIT, or compiled ahead of time into an executable; green threads, and a garbage collector tending the heap                                           |
| **Silo**                | compiled all the way down by LLVM, counting references instead of collecting, on the native stack; always an executable, with no interpreter in it -- see [docs/SILO.md](docs/SILO.md) |

`--runtime <glade|silo>` picks one for a command, and `runtime = "…"` in a
`[profile.<name>]` of the manifest for a package.

Glade compiles its bytecode to machine code itself, for **aarch64** and
**x86-64**, with no LLVM or Cranelift: typed arithmetic, comparisons, branches,
moves, constants and the common allocations run as machine instructions, and
everything else calls back into the runtime. When it compiles is Glade's to
choose -- its backend:

| Glade backend |                                                                                                                                       | default for |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------- | ----------- |
| `jit`         | the VM, compiling each block to machine code once it has run 16 times (`MEADOW_JIT_THRESHOLD`)                                        | `--debug`   |
| `aot`         | an executable: a Mach-O or ELF object holding the code and the program's image, linked by the system's C compiler against the runtime | `--release` |
| `vm`          | the bytecode interpreter alone                                                                                                        |             |

`--backend <vm|jit|aot>` (or `--jit`, `--aot`) picks one for a command, and a
package can pick one per profile in its manifest. Silo has no backend to pick:
it is compiled ahead of time by what it is, and a backend named beside it is
an error on the command line and a warning in the manifest.

```toml
# Meadow.toml
[profile.debug]
backend = "vm"

[profile.release]
runtime = "silo"
```

An executable links against its runtime built as a static library: `cargo
build --release` in `glade` or `silo`, or with `--target x86_64-apple-darwin`
for the other Mac architecture; `MEADOW_RUNTIME` can name one. A release
`meadow` carries Glade's inside it. A runtime has to be built from the same
sources as `meadow`, and a program will not link against one that is not.
When Glade's `aot` is only the release default and no executable can be made
-- a lone `.mw` file, which has no `target/` to put one in, or no runtime
library -- the JIT runs the program instead, with a warning; an `aot` that was
asked for is an error, and so is Silo that cannot be had. `meadow test` runs
in-process, so it uses the JIT for both of Glade's.

A package's name is the first segment of a `use` path, so it has to lex as one
identifier — `meadow init` says so rather than letting a directory called
`my-pkg` produce a package nothing can refer to.

### Two machines

Programs run on a **register bytecode VM** with a generational garbage
collector: each green thread's heap is collected in pauses of tens of
microseconds, however much it keeps alive, because the old generation is marked
on another OS thread while the program runs.
Behind it, the compiler lowers to a sequent-calculus IR (AxCut) and then to
straight-line instructions over a flat register file — with no `call` or `ret`,
since in that IR returning from a function is entering the continuation it was
given. A call's continuation is a frame on a chunked, per-thread frame stack
kept in the heap, as GHC's is, rather than on the machine's.

The older **CEK abstract machine** is still there, and is still the definition of
what a Meadow program means. `--cek` on `run` and `test`, or `:cek` in the REPL,
switches to it; if the two disagree the CEK is right and the VM has a bug, which
makes the flag the first thing to reach for when a program does something
inexplicable. Both run the whole standard library test suite on every CI build,
so a disagreement should not survive long enough for you to find one.

### Formatting

`meadow fmt` is a pretty-printer, as rustfmt is. It reads a file's tokens and
prints them again, deciding every line break itself, so the same code comes out
the same however it was typed, and formatting what is formatted changes nothing.
No token is added, dropped or reordered, and `meadow fmt` checks that against the
compiler's own lexer before it writes a file, so formatting cannot change what a
program means.

The rule is rustfmt's: what fits in the width (100 columns; `--width` sets
another) is one line, and what does not is cut at its outermost joint first.

```meadow
fun total (orders : [Order]) : Int =
  V.foldl (\acc o -> acc + o.price * o.count) 0 (V.filter (\o -> o.count > 0) orders)

fun describe n =
  Decl.Record {
    name = someFunction n,
    fields = V.map (\f -> lowerField context f) (fieldsOf n),
    span = spanOf n
  }
```

- A definition is cut after its `=`, a call has what it is given a line each, a
  function given last starts on the line of the call, and a record is set out as
  rustfmt sets out a struct.
- A head too long for its line has a line to each thing it takes, and its type
  the line after, as rustfmt has a signature.
- The steps of a `let … in` have a line each, and so do the arms of a `match`; a
  `match` with two short arms, or one `let` with its body, stays on one line.
- An `if` keeps its `then` on its line, and each `else` starts one.
- A `record`, an `effect`, a `trait`, an `impl` and a `mod` have a line to each
  thing in them, always.

Three things are kept from how the file was written, because its tokens do not
say them: comments, each with the token it is before or after; one empty line
where there were any, between declarations, steps and arms; and, inside a macro
call such as `lang! { … }`, which lines start a new entry, since what a macro
reads is a language of its own with a rule to a line. Two tokens that touch are
left touching, so `a+b` and `f -1` stay as they are.

`@fmt(skip)` on a declaration, or on a line of its own before one, leaves that
declaration exactly as it is written, as `#[rustfmt::skip]` does in Rust: for a
table set out by hand, a row to a line.

```meadow
@fmt(skip)
def weights =
  [ ("small",   1),
    ("medium", 10),
    ("large", 100) ]
```

A file whose brackets do not match is only indented.

### Testing

Mark a function `@test` and `meadow test` runs it, cargo-style:

```
use Std.Test (assertEq)

@test fun doubling () = assertEq (double 21) 42 "double 21"
```

```sh
$ meadow test
running 2 tests
test doubling ... ok
test thisOneFails ... FAILED

failures:

---- thisOneFails ----
double 21: expected 43, got 42

test result: FAILED. 1 passed; 1 failed
```

`meadow test --std` also runs the standard library's own tests, which live at
the bottom of each `Std` module.

A test takes one argument (the runner calls it with `()`) and fails by
performing `Std.Test`'s `Test` effect, which `assert` / `assertEq` do for you —
so an assertion five calls deep still stops the test and still names itself.
Both `==` and `show` are structural, so `assertEq` works at any type and prints
what it actually got.

A test is named by its module — `Parser.handlesEmpty`, or just `handlesEmpty`
in the package's root module — because two modules may each declare a test of
the same name. `meadow test <path> <filter>` runs the tests whose name contains
`<filter>`; add `--exact` to run only the one whose name _is_ it, which is what
the editor's **▶ Test** link above each `@test` does.

A property says what holds of every value, and `Std.Test.Property` looks for
one it does not hold of:

```
use Std.Test.Property (forAll, vector, between)

@test fun reversingTwiceIsTheSame () =
  forAll
    (\() -> vector 20 (\() -> between 0 100))
    (\xs -> assertEq (V.reverse (V.reverse xs)) xs "reversed twice")
```

A generator is a function that draws numbers, so one of your own is any
function that calls the ones there. A case that fails is made smaller before
the test fails with it, by running it again on fewer and smaller draws, and
the same cases are checked every run. `Std.Test.Fuzz` is the same thing for
text: `fuzz examples words prop` runs `prop` on a thousand texts made by
changing the examples -- pieces taken out, doubled, moved, words dropped in.
`fuzzGuided` is `fuzz` steered by coverage: an input that reaches somewhere in
the program no input before it did is kept as an example to make more from,
so it gets through a check one change at a time. Either sees a failure only
as a test does: a program that stops the process is not caught.

### Effects

`Std` leans on algebraic effects for the things that are usually hardest to
test. Each has a real implementation _and_ a handler that fakes it:

| module                  | with no handler | handled                                                    |
| ----------------------- | --------------- | ---------------------------------------------------------- |
| `Std.Random`            | real entropy    | `withSeed 42` — a pure SplitMix64, same numbers every run  |
| `Std.Time`              | the real clock  | `withClock t`, `withTickingClock t tick`                   |
| `Std.State`             | —               | `runState`, `evalState`, `execState`                       |
| `Std.Exn`               | —               | `toResult`, `catch`, `withDefault`, `toMaybe`              |
| `Std.Stream`            | —               | `toList`, `toVec`, `fold`, `take`, `find`, `map`, `filter` |
| `Std.Fs`, `Std.Process` | real I/O        | any `handle`                                               |

`Std.Stream` is generators: a producer performs `yield` and a consumer decides
what that means. `take` simply doesn't resume, which unwinds the producer — the
part a lazy list gives you for free and an eager language otherwise cannot do:

```
def firstThree = Stream.take 3 (\() -> Stream.range 0 1000000)
```

Every collection has `toStream`, and `Std.Stream` has `ofList` / `ofVec` for the
other direction.

### Sequences

There are two sequence types and one rule for telling them apart: **a `;` means
the linked `List`; brackets without one mean the default RRB `Vector`.** It holds
for types, literals and patterns alike.

|             | `Vector`    | `List`      |
| ----------- | ----------- | ----------- |
| type        | `[a]`       | `[a;]`      |
| empty       | `[]`        | `[;]`       |
| one element | `[x]`       | `[x;]`      |
| several     | `[x, y, z]` | `[x; y; z]` |

`#[x, y]` is the third, lower-level one: the builtin `Array`, which both are
built on.

Patterns follow the same rule, with `::` as the usual `Cons` sugar:

```
fun total xs = match xs with
  | [;] -> 0
  | x :: rest -> x + total rest
```

`[]` matches an empty `Vector` — the library keeps `Vector.Empty` the only
representation of one, so it matches a vector emptied at run time too. A
_non-empty_ `Vector` has no structural pattern (it is a balanced tree, not a
cons list); match on `Vector.len` or convert with `Vector.toList`.

### Views and pattern synonyms

A pattern can look at a value through a function, `(f -> p)`, and a pattern
can have a name, as GHC's `ViewPatterns` and `PatternSynonyms` allow:

```
pattern Succ m <- (pred -> Just m) where Succ (Nat n) = Nat (n + 1)
pattern Pair a b = (a, b)

fun toInt n = match n with
  | Succ m -> 1 + toInt m
  | _ -> 0
```

A synonym matches and builds as if it were a constructor, and costs nothing
once optimized: its matcher is inlined, and what it answers is taken apart
where it is made. See the tutorial's §5.

### Editor support

`meadow lsp` is a language server — diagnostics as you type, hover with the
inferred type and the doc comment above the definition, go-to-definition,
rename across a whole package, inlay hints, and semantic highlighting from the
compiler's own lexer. A file inside a package is analysed as part of it, so a
`use` of a sibling module resolves the way it does in a build. Any LSP client
can drive it; it speaks the protocol over stdin and stdout.

For VS Code, build and install the extension:

```sh
editors/vscode/build.sh
code --install-extension editors/vscode/meadow-*.vsix
```

The Meadow colour theme is a second extension, in `editors/vscode-theme`, so
that either can be used without the other. It has a colour for each kind of
name the server tells apart, in a dark and a light variant:

```sh
editors/vscode-theme/build.sh
code --install-extension editors/vscode-theme/meadow-theme-*.vsix
```

Each release also attaches both `.vsix` files, and
`scripts/install.sh --with-extension` builds and installs the two together.

The extension runs `meadow lsp`, and finds it on your `PATH`, then in
`$MEADOW_HOME/bin`, `~/.cargo/bin` and `~/.meadow/bin` — the last three because
an editor launched from the Dock or the Start menu does not inherit your shell's
`PATH`. It checks that whatever it finds actually understands `lsp`, so an old
`meadow` is skipped rather than started and left to fail. `meadow.server.path`
overrides all of it. Without a server you still get syntax highlighting from the
bundled TextMate grammar.

### Debugging

`meadow dap` is a debug adapter, and the VS Code extension registers it: open a
`.mw` file and press F5, or add a `meadow` launch configuration naming a
`program` (a package directory or a single file). You get breakpoints, step
in / over / out, the call stack, and three views of a stopped program:

- **Locals** — the names in scope with their inferred types, and values you can
  open up: a `Just [1; 2]` expands into its list, a record into its fields, a
  closure into what it captured.
- **Registers** — the VM's raw register file, with the names the compiler put
  in each, and the program counter and instruction count under **Heap**.
- **Handlers** — the effect handlers installed, and what each one covers.

A program need not start at `main`. Each top-level definition has a **▶ Debug**
link above it (and **Meadow: Debug Function…** on the right-click menu) that asks
for its arguments as Meadow expressions — showing its type while you type them,
and remembering what you gave last time — then debugs a call to it, stopped as
the function is entered. The call is compiled inside the function's own module,
so a private helper can be debugged as easily as an exported one, and an
argument of the wrong type is a compile error before anything runs. In
`launch.json` the same thing is an `entry`:

```json
{
  "type": "meadow",
  "request": "launch",
  "name": "Debug eval",
  "program": "${workspaceFolder}/examples/MiniML",
  "entry": {
    "module": "${workspaceFolder}/examples/MiniML/src/Eval.mw",
    "expression": "eval [;] (Expr.Int 1)",
    "function": "eval"
  }
}
```

The machine has no `call` or `ret` — returning is invoking a continuation,
a frame on the frame stack or now and then a heap closure — so the adapter
reconstructs a call stack from the continuation chain, using what
the compiler records about which name is a function's return continuation. A
call in tail position replaces its caller's frame, as it does at run time. The
program's `print` output goes to the Debug Console; `Console.readLine` sees the
end of input, since the adapter's own stdin is the protocol.

A debug session compiles at `-O1` with source positions kept all the way to the
bytecode. That changes where the compiler remembers things, not what the
program does: the standard library's test suite runs identically either way,
and recording the positions changes no instruction.

### Profiles

`--debug` (the default) and `--release` are names for a bundle of compiler
options, not options themselves. There are two:

|                        | `--debug` | `--release` |
| ---------------------- | --------- | ----------- |
| optimization level     | `-O1`     | `-O2`       |
| non-exhaustive `match` | allowed   | an error    |
| runtime                | Glade     | Glade       |
| backend                | `jit`     | `aot`       |

```sh
$ meadow run --release missing.mw
missing: non-exhaustive patterns: `None` is not matched
```

Irrefutability of _binding_ positions — function and lambda parameters, `def`
and `let` destructuring — is checked at both, since those have no fallback arm.

`-O2` compiles a `match` to a decision tree rather than trying its arms in turn.
Everything below it is unconditional: a known call becoming a jump, a literal
folding into the instruction that uses it, a comparison fusing into the branch
that tests it. Those cost nothing to read and nothing to compile, so a debug
build gets them too — `-O0` exists for the first pass that changes that, and is
`-O1` today.

Either axis can be set on its own, and a package can change what the profiles
mean for it:

```sh
meadow run -O2 pkg          # optimize, but still allow a half-written `match`
meadow build --strict pkg   # check exhaustiveness without optimizing
```

```toml
# Meadow.toml
[profile.debug]
opt-level = 2               # this package is slow to run, not slow to build

[profile.release]
strictness = "lenient"      # "lenient" | "strict"
runtime = "glade"           # "glade" | "silo"
backend = "jit"             # Glade's: "jit" | "aot" | "vm" -- or `aot = true`
threads = 4                 # OS threads for the green ones, as `-j`
leaks = true                # what the program left behind, as `--leaks`
target = "aarch64"          # the processor an executable is for, as `--target`
```

A flag beats the manifest, and the manifest beats the profile's built-in
meaning. A key that is absent is simply not overridden, so a `[profile.debug]`
that names only `opt-level` leaves everything else as debug.

## Building from a checkout

The tree is six independent Cargo workspaces:

|               |                                                                                                                                                                                                                                                                                                                      |
| ------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `compiler/`   | the front end and back end, one crate per pass — through `meadow-seq` (the AxCut IR and its reference machine) and `meadow-codegen` to `meadow-bytecode`                                                                                                                                                             |
| `eval/`       | the CEK machine — the specification of what a program means                                                                                                                                                                                                                                                          |
| `glade/`      | Glade, the default runtime: a register bytecode VM, its JIT and native code generator, green threads, and a low-pause generational collector (a copying nursery, and an Immix old generation marked concurrently and evacuated a block at a time). It loads an image and knows nothing about the IR that produced it |
| `silo/`       | Silo, the runtime a program compiled by LLVM links against: its heap, counted by reference, its primitives, and its scheduler -- see `docs/SILO.md`                                                                                                                                                                  |
| `buildtools/` | the tools you point at Meadow source: `meadow` (build system, CLI and REPL — the binary) and `meadow-fmt` (the formatter)                                                                                                                                                                                            |
| `installer/`  | `meadowup`, which installs and updates the toolchain                                                                                                                                                                                                                                                                 |

```
core ──▶ AxCut ──▶ bytecode ──▶ VM
          │
          └─▶ the AxCut machine, and the CEK machine beside it: two
              independent checks that the pipeline preserved meaning
```

```sh
scripts/check.sh                         # everything CI runs
scripts/check.sh --strict                # ...plus rustfmt and clippy
scripts/bench.sh                         # the benchmarks, on both runtimes
scripts/bench-compact.sh                 # what compacting a large live value saves the collector
scripts/install.sh                       # install this checkout the way a release installs
scripts/install.sh --with-extension      # ...and the VS Code extension and theme with it
cargo install --path buildtools/meadow   # install the CLI
```

`scripts/check.sh` is the single entry point: `ci.yml` runs it on every push and
`release.yml` runs it before publishing, so a green run locally is a green run
there. It covers all five workspaces — there is no one `cargo test --workspace`
that does, which is the reason it exists.

`scripts/check-parallel.sh` runs the same checks side by side, each with a log
of its own, and the release workflow's smoke test with them: about ten minutes
on a machine with cores to spare, where `check.sh` is most of an hour. It is
what to run on a commit before pushing it, with `--version v0.2.0` for a
release's tag.

A test that needs files makes its directory with `meadow_scratch::scratch!`
(`compiler/meadow-scratch`), which is removed when the test ends, failed or
not, and is under the workspace's `target/tmp` rather than the system's
temporary directory, so `cargo clean` takes whatever a killed run left. Set
`MEADOW_KEEP_TEST_DIRS=1` to keep them, to look at what a failing test wrote.

A commit is checked that way on a rented Linux machine before it is pushed, and
whoever ran it says so on the commit, as a status called `suite/linux-x86_64`:

```sh
gh api repos/meadow-lang/meadow/statuses/<sha> -f state=success \
  -f context=suite/linux-x86_64 -f description="check-parallel.sh passed"
```

`ci.yml` and `release.yml` look for it (`scripts/checked-elsewhere.sh`) and skip
their own run of the checks on Linux when it is there. The Windows leg of CI
runs whatever anybody says: nothing else checks Windows.

## Releasing

Meadow has two channels, as Rust does.

**Stable** is a release with a [semantic version](https://semver.org) and a
tag that is pushed once and never moves. While the major version is 0, a minor
version may break source and a patch may not; the promise covers the language,
`Std`'s public API and `Meadow.toml`. `Std`'s version is the toolchain's.
[`CHANGELOG.md`](CHANGELOG.md) says what each one changed.

**Nightly** is `master`, built once a day when it has moved and published
under the tag `nightly` by `.github/workflows/nightly.yml`. It says so —
`meadow 0.3.0-nightly (2026-10-05 892808f)` — and it is the only channel that
accepts unstable features, each named at the top of the root module of the
package that uses it, as `#![feature(…)]` is in Rust:

```meadow
@!feature(ffi)      -- calling C through Std.Ffi

use Std.Ffi as Ffi
```

These are not the features a package offers the ones that build it, which are
`[features]` in its `Meadow.toml` and work on every channel.

A build from a checkout (`scripts/install.sh`) is `-dev`, and takes what a
nightly takes. A project can say which toolchain it is built with in a
`meadow-toolchain` file beside its `Meadow.toml` — `stable`, `nightly`, or a
version such as `0.2.0` — and `meadow` then refuses to build it with another,
and says how to get the one it asks for.

To cut a release:

1. Move what is under _Unreleased_ in `CHANGELOG.md` into a section for the
   version, and set that version in every manifest: the six `Cargo.toml`s
   (`compiler`, `eval`, `glade`, `silo`, `buildtools`, `installer`),
   `lib/Std/Meadow.toml`, `editors/vscode/package.json` and
   `editors/vscode-theme/package.json` -- and the lockfiles that pin them --
   and the `version` MeadowBoot says, in `bootstrap/src/Main.mw`, which
   `a_command_takes_what_meadow_s_takes` holds to `meadow`'s.
2. Run `scripts/check.sh --version v0.2.0`, which fails if any of them, or the
   changelog, disagrees with the tag.
3. Tag that commit and push the tag:

   ```sh
   git tag v0.2.0 && git push origin v0.2.0
   ```

   `.github/workflows/release.yml` runs the checks again, cross-builds for
   Linux, macOS and Windows (x86-64 and arm64) and attaches the archives the
   installers look for. It refuses a tag that already has a release.

4. Set the next minor version in the manifests on `master`, so that the
   nightlies after a release are builds of the version they are working
   towards.
