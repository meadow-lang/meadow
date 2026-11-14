# Changelog

What changed in each release of Meadow: the language, the standard library,
`Meadow.toml`, and the tools. Versions follow [semantic versioning](https://semver.org).
While the major version is 0, a minor version may break source and a patch may
not. The promise covers the language, `Std`'s public API and `Meadow.toml`; the
bytecode image, the Cut IR and the runtimes' layouts may change in any release
until 1.0.

Each release has up to four lists: what **breaks** code that built before,
what is **changed** in what code that still builds does, what is **added**,
and what is **fixed**. A nightly build is `master`, and what it has beyond the
last release is under _Unreleased_.

## Unreleased

### Breaking

- The float operators `+.`, `-.`, `*.`, `/.`, `<.`, `>.`, `<=.` and `>=.`
  are gone, and so is their trait, `Std.Ops.Floating`. `Float` and `Float32`
  implement `Add`, `Sub`, `Mul` and `Div`, and were already `PartialOrd`, so
  `x + y` and `x < y` are what float code writes now, at the same cost. Write
  `+` for `+.`, `<` for `<.`, and so on. A program built without `Std` names
  the float primitives (`_primAddF x y`), since there `+` is the integers'
  primitive.

### Changed

- MeadowBoot's `build` and `run` act on the flags they used to accept and
  ignore. `--types` and `--annotations` write what MeadowBoot's inference
  found, as `meadow` writes it; `run --cek` runs the program on MeadowBoot's
  CEK machine; `--no-prune` links every definition; `build --emit image`,
  `bytecode`, `asm` and `exe` write what the back end makes of MeadowBoot's
  program where `meadow` writes each; and `--backend vm`, `--gc`,
  `--gc-stats`, `--profile-to`, `--sample-every`, `--sample-hz`, `--train`,
  `--nursery` and `--leaks` reach the back end, as does `-O` for the JIT.
  `--backend`, `--jit`, `--aot` and `--runtime` decide `@cfg(backend = …)`
  as in `meadow`. What MeadowBoot does not act on yet -- `--emit expanded`,
  a package's features, a workspace's members -- it says it does not.
  `meadow cut`, which it hands a program to, takes `--backend`, `--gc`,
  `--gc-stats`, `--profile-to`, `--sample-every`, `--sample-hz`, `--train`
  and `--calls` for it, `--emit bytecode`, `asm` and `image`, and `-o` to
  write what it emits to a file; and it runs the JIT at the `-O` it is
  given, where it ran it at `-O2` whatever it was given.
  `buildtools/meadow/tests/flags_differential.rs` sets each flag on both
  compilers against each other.
- MeadowBoot's prompt runs each entry where `meadow repl` runs one: on
  Glade, with the JIT, in its own process through `meadow-host`, where it
  used to run them on MeadowBoot's CEK machine. `fib 30` takes under a
  millisecond, where it took minutes, and a program that starts threads runs.
  As the Rust prompt does, it keeps what each entry was compiled to -- here
  its object of Cut -- and runs an entry as a program of its own, of what its
  value reaches of the standard library and the entries before it. `:vm`
  runs it without the JIT, and `:cek` on MeadowBoot's CEK machine, which is
  also what runs it where the library cannot be loaded. `meadow-host` has
  two functions for it, `meadow_repl_run` and `meadow_repl_pages`. `:ir`
  shows an entry or a definition in each IR, as `meadow repl`'s does: its
  core, written as the Rust compiler writes core -- by `Core.prettyDefs`, as
  the language server's `meadow/ir` writes it, where the prompt had a printer
  of its own that wrote sized integers and undecided literals otherwise --
  then its Cut, AxCut,
  Glade's bytecode and machine code, and LLVM IR. A package in a symbol of
  Cut may have a `:` in its name, as a prompt's entries do: `meadow:repl:3/d0`.
  The two prompts agree on 257 of the differential test's 265 entries, where
  they agreed on 240.
- `meadow repl` no longer runs an `effect` entry's operation, which said
  `program has no entry point`, and no longer shows a derived `impl`'s
  dictionary and methods, `(#impl.Debug) : Debug Pair`, as an entry's
  bindings or in `:module`: `@derive(Debug) data Pair = …` says `defined type`
  as any other `data` does.
- A program of Cut that handles no effect and performs an operation that is
  not the runtime's fails where it performs it, `unhandled effect …`, as one
  that handles others does. It used to be refused before it ran, though
  the function that performs it may be one it never calls.
- `x |> f` evaluates `x` first, and then `f`, as it is written, on every
  engine. It used to be exactly `f x`, which evaluates `f` first. Only a
  program in which working out what is piped into performs something can
  tell: in `read () |> check (log ())`, `read` now runs before `log`, and a
  stage whose `f a` itself performs something runs after the value before it.
  A name, a constructor or a function on either side of the `|>` costs and
  means what it did, and a pipe types as it did. The `_`s of a section are
  numbered as they are written across a pipe, as they are everywhere else:
  `(_ |> f _)` takes the piped value first, where it used to take `f`'s
  argument first. `<|` is unchanged: `f <| x` is `f`, then `x`.
- A Cut clause whose every way through ends by resuming, and that uses its
  resumption for nothing else, is now called where its operation is
  performed, as Meadow's own `op x k -> k e` already was. Its operation no
  longer cuts the stack off or makes a resumption. Ten million such
  operations take a quarter of a second on Silo, where they took two.
- A Cut `perform` looks its handler up with a search of its own, as each of
  Meadow's own does: the evidence is walked with the arguments and the
  continuation passed along, each entry's key compared with a constant, and
  the clause found entered from there. It used to make two objects at every
  `perform`, for what to do when the handler was found and when it was not,
  and enter the clause through the first. Ten million operations answered by
  a clause that only resumes take 66 ms on Silo, where they took 278: what
  Meadow's own take.
- `meadow update` says which version each dependency went from and to:
  the release, or, for a dependency following a branch, the version its
  manifest gives and the commit (`Scythe 0.2.0 (f7e5d965b) -> 0.3.0
  (3d2afe126)`). A dependency held back by a newer release that breaks its
  `version` is named, with that release. It used to say every dependency was
  current, as if there were nothing newer.
- `end` is no longer reserved and may be a name: a function, a parameter, a
  local, a field or a pattern variable may be called `end`. It was a keyword
  that no form used, so writing it anywhere was a syntax error. The language
  server, the REPL and the VS Code grammar no longer colour it as a keyword.
- A module may be empty: a file with no declarations, or only comments or
  `@!` attributes, is a module that defines nothing. It used to be a syntax
  error, so a file just made showed an error in the editor until something
  was written in it. MeadowBoot already took one.
- MeadowBoot's language server and prompt are built on Lingua 92955a5, and
  Cut moves to it too. The server refuses a rename it cannot do as
  `meadow lsp` does, with the error `-32803` and the reason
  (`` `2x` is not a name ``, `a module is named by its file: rename the
  file`), where it answered nothing. It hears `workspace/didChangeWatchedFiles`:
  when a file changed on the disk is not an open document as the editor
  holds it, the packages that documents are open in are built again and
  every open document is looked at again. A method it does not know, a
  request before `initialized` and one after `shutdown` get the protocol's
  errors, and closing a document publishes an empty list of diagnostics for
  it. The prompt, like `meadow repl`, keeps every line in its history,
  including `:` lines. Ctrl-C ends it, and a `:` line that is no command goes
  to the parser. It has no `:theme`, and an entry that answers nothing
  writes nothing, where it used to write an empty line.

### Added

- Cut carries positions and names, for a debugger: `source S = "path"`
  declares a file, `@S:a-b s` is a statement written at bytes `a` to `b` of
  it, and `x: i64 @"count" "Int"`, `def S @"fact"` say what the source calls
  a binder, with its type, and a definition. Every pass keeps them, the
  lowering makes them AxCut's marks, and Glade's code generator writes the
  same debug information from them as from the Rust core's. `meadow cut`
  takes them off before it builds or runs a program, which then compiles to
  the same bytecode and LLVM IR as without them. A program of Cut built for
  debugging can be debugged on Glade: `Session::launch_cut`, and
  `meadow-host`'s `meadow_debug`, a session another program holds open --
  launch, breakpoints, run and step, the stack, scopes, locals and values,
  JSON in and out. See `docs/CUT.md`, "Positions and names".
- `MEADOWBOOT_TIMINGS=1` has MeadowBoot say how long each pass of each unit
  took -- parse, fixity, lower, rename, infer, elaborate, optimise, cut --
  for a build, a prompt entry and the language server's study of a package;
  and `scripts/bench-meadowboot.py` runs the same builds, prompt entries and
  editor edits through MeadowBoot and the Rust `meadow` and prints both.
  MeadowBoot is to replace the Rust front end, so how much slower it is has
  to be measurable, and a regression visible. The numbers are in
  `bootstrap/README.md`. Checking an entry at MeadowBoot's prompt went from
  about 180 ms to about 45 ms: a unit's database no longer copies every
  interface it imports, renaming no longer reads every constructor of a
  dependency for each of its types, and compiling the standard library lost
  a check that compared every export with every other.
- MeadowBoot has a prompt of its own: `meadowboot` with no command, as
  `meadow` with none, is now MeadowBoot's REPL (`bootstrap/src/Repl.mw`),
  declared with Lingua's `repl!` rather than handed to the Rust `meadow`. It
  is a step in retiring the Rust front end: each tool is written again over
  MeadowBoot's passes, in Lingua, and checked against the Rust one. Each entry
  is compiled by MeadowBoot as a unit of its own after the ones before it, as
  the Rust prompt compiles them, and run on MeadowBoot's CEK machine; the
  banner, an entry's type and value, its errors, when an entry of several
  lines is finished, and the `:` commands are the Rust prompt's.
  `buildtools/meadow/tests/repl_differential.rs` types the same sessions into
  both and reports how far they agree, by kind of entry; `:ir` is not
  MeadowBoot's yet, and neither are threads on its CEK machine. Along the
  way MeadowBoot came to agree with the Rust compiler in more places: its CEK
  machine prints a `Vector` as its elements, `[1, 2, 3]`, and writes and
  reads for a `Console` operation nothing handles; a name nothing binds has
  the error's type, so what is wrong with it is said once; and a name
  written through a qualifier nothing names is refused, as `module `Nope`
  is not in scope here`.
- MeadowBoot has a language server of its own: `lsp` in MeadowBoot
  (`bootstrap/src/Lsp.mw`), declared with Lingua's `lsp!`, answers hover, go
  to definition, completion, inlay hints, code lenses, semantic tokens,
  prepare rename and rename, formatting and diagnostics in the shapes
  `meadow lsp` answers them, reading each document as part of its package as
  MeadowBoot compiles it. A differential test,
  `buildtools/meadow/tests/lsp_differential.rs`, sends both servers the same
  session and says by kind of request how far they agree; it is ignored until
  they do. Workspace symbols and `meadow/ir` are not answered yet, and
  `lsp --check` is still `meadow`'s.
- MeadowBoot's language server answers workspace symbols, `meadow/ir` and
  `lsp --check`. Workspace symbols are `meadow_find`'s search, ported
  (`bootstrap/src/Find.mw`): by name, scored as the Rust server's `nucleo`
  matcher scores it, or by type, priced as `meadow_find` prices a match, over
  what each open package declares and what it depends on exports.
  `meadow/ir` answers a definition as core, written as `meadow_core` writes
  it (`Core.prettyDefs`, from `Compiler.studyCore`); Cut and AxCut are not
  answered yet. `lsp --check PATH…` opens each `.mw` file under the paths in
  the server and prints every error it would mark, with the Rust command's
  output and exit status: it finds no errors in the sources CI and a release
  check.
- MeadowBoot has a debug adapter of its own: `dap` in MeadowBoot
  (`bootstrap/src/Dap.mw`), declared with Lingua's `dap!`, launches a
  program, stops at breakpoints and on entry, continues and steps in, over
  and out, and answers the stack, scopes, variables and evaluation in the
  shapes `meadow dap` answers them, with a failure stopping the program where
  it failed. The program is built by MeadowBoot and run by its CEK machine,
  which a debugger can now watch: it stops before each place the Rust core
  marks with a position -- a call, a branch, a body, a `let` -- and keeps a
  frame for each function body under way, so the stack reads as a call
  stack. Names, places and types come from MeadowBoot's renaming and
  inference. `buildtools/meadow/tests/dap_differential.rs` drives both
  adapters through the same sessions and says by kind of request how far
  they agree; it is ignored until they do. A launch's `entry`, the raw
  machine's scopes and stepping into the standard library are not
  MeadowBoot's yet.
- A Silo program that runs out of stack on Windows says so: how deep it got,
  in a stack of what size -- the main thread's gigabyte, or a green thread's
  or a handler's 256 MiB -- and where in the executable. It used to end with
  status `0xC00000FD` and not a word, since Rust's report of an overflow
  comes with a Rust `main` and a Silo program's is C. With
  `MEADOW_SILO_OVERFLOW_TRACE` set it goes on with the words on the stack
  that point into the executable, and reports an access violation the same
  way; `scripts/silo-map-names.py` names them from the linker's map
  (`docs/RUNTIME.md`, "A native program that dies on Windows").
- The MeadowBoot workflow builds MeadowBoot with itself on each of its five
  targets, after the archive is kept: MeadowBoot compiled by itself must
  compile `bootstrap/` to the very Cut it was made from, and then run a
  program and build a library (`scripts/meadowboot-selfbuild.sh`).
- `meadow new MyApp` makes a package in a new directory, as `cargo new` does:
  named after the directory (`my-app` makes `MyApp`), and a git repository
  of its own unless it is inside one already or joins a workspace.
  `--lib` makes a library, `--workspace` a workspace, and `--no-git` no
  repository. With no path, at a terminal, it asks what to make, what to call
  it, where, and whether it gets a repository, and shows what it will make
  before it makes it. `meadow init --lib` makes a library too: a
  `src/Lib.mw` with a function and a test of it.
- MeadowBoot runs the Rust back end in its own process: `meadow-host`, the
  `meadow` command line built as a library, is loaded through `Std.Ffi` and
  called for what MeadowBoot does not do itself -- building and running its
  Cut, and the commands it has no code for yet. It used to start the Rust
  `meadow` as a second program for each, so a MeadowBoot could not be
  `meadow` without one beside it. MeadowBoot's archives carry the library,
  and `MEADOW_HOST` names another. `MEADOW_BOOTSTRAP=1` lets a stable
  `meadow` take unstable features, as `RUSTC_BOOTSTRAP` does rustc, for
  building MeadowBoot, which uses `Std.Ffi` to do this. MeadowBoot reads a
  module's own attributes, `@!feature(ffi)`, as the Rust compiler does; it
  could not parse a file that began with one.
- `meadow update --breaking` moves a dependency past a release that breaks
  the `version` it asks for: the `version` in `Meadow.toml` is raised to the
  newest release, and the lockfile follows. With `--dry-run` it says what it
  would raise and changes nothing.
- A Cut definition, or a method of an object, may say what it can perform:
  its row, `! {E, .. | e, ..}`, after its parameters (`docs/CUT.md`,
  "Rows"). A row may name only effects that are declared, and only the
  definition's own effect variables. Anything else is refused when the
  program is read. With rows, a program with an operation that may be
  resumed twice (`@many`) keeps its continuations as frames wherever no such
  operation can reach them (`docs/CUT.md`, "How the lowering chooses"). One
  `@many` operation anywhere used to put every continuation of the program
  on the heap, and still does where a row is missing.
- `Std.Process.currentDir ()` is the directory a process runs in, as an absolute
  path, for a tool that must walk up from where it was started. It is empty when
  the platform cannot say.
- Threads can be cancelled: `Std.Thread.cancel t` asks `t` to stop, and
  `await t` then fails with "cancelled". A cancelled thread stops softly at
  its next operation the runtime answers -- a thread operation, the wait of a
  `retry`, or an operation of `Console`, `Fs`, `Net`, `Time`, `Random`,
  `Process`, `Log` or `Test` -- and is woken to stop if it is waiting on a
  channel, another thread, a `retry` or a socket. One that performs nothing
  is ended at a preemption point a little after the cancel, so no thread can
  ignore one. No code is interrupted between two of its operations in a way
  it could notice, and no handler can catch the failure, so there is no
  asynchronous exception to guard cleanup against or mask; threads share
  nothing mutable, so ending one leaves nobody else's state half made.
  `Std.Thread.cancelled ()` asks, for a computation that would rather stop
  on its own terms. Cancelling a finished thread does nothing. On the CEK
  machine, Glade (the VM, the JIT and executables) and Silo; the design is in
  `meadow_rt::thread`. An operation made in place, such as a file read, runs
  to its end before the thread stops.
- The REPL's `:ir` opens a full-screen viewer of the last entry -- or, with
  a name, of a definition -- with a page for each form the compiler puts it
  in, in the order a program passes through them: core, Cut, AxCut, Glade's
  bytecode, the machine code Glade's JIT makes of it, and the LLVM IR a Silo
  build compiles. Each page is the entry's own part of the program, its
  local functions and continuations included, coloured for its IR. `Tab` and
  `Shift-Tab` turn the page; the arrows, `j`/`k`, `PgUp`/`PgDn` and
  `Home`/`End` scroll, a long line sideways rather than wrapped; `q` or `Esc`
  gives the prompt back as it was. With no terminal to draw on, the pages
  are printed one after another under their titles, and `:ir <which>
  <name>` -- `core`, `cut`, `axcut`, `bytecode`, `native` or `llvm` -- still
  prints one as it is, for a script to read.
- Glade builds a library of what a package exports to C, as Silo does:
  `MeadowBoot build --runtime glade` for a package of `@export` functions, or
  `meadow cut --runtime glade` for a program of Cut with exports, makes
  `Name.dll`, `libName.so` or `libName.dylib` in `target/<profile>/native/`.
  Its C functions take and answer the C types Silo's do, so a Silo program
  calls Glade's code through `Std.Ffi` -- and a Glade program calls it too.
  A call that fails answers zero and leaves its message for
  `Std.Ffi.failure`, as Silo's does. Each call runs with a heap of its own:
  the top-level values a call makes are made again by the next. For this
  system only; a library for another is Silo's.
- Tail recursion modulo context for every program of Cut, on both runtimes:
  a call whose answer only goes into a constructor -- `Cons (f x) (map f
  rest)`, two cells at once, a tuple on the way, a tree's last subtree, or a
  call to another definition that calls back -- is made a tail call that
  writes its answer into the cell, built first. So a program MeadowBoot or
  Idyll compiles builds a list front to back in constant stack, as `meadow
  run`'s already did: a map over ten million elements runs in less than half
  the time on either runtime, with a third (Glade) to a half (Silo) less
  memory, and one over four million records on a thread, which overflowed
  its stack on Silo, runs. Where the list being mapped is held by nothing
  else, Silo builds each new cell in the old one's block. A tree, whose recursion is only as deep as the tree,
  gains nothing, and on Glade is about 12% slower for the write each node
  now takes. Nothing a program can see changes: the order of evaluation is
  Cut's, and a program that may resume an operation twice is rewritten only
  where no handler can capture the recursion. `MEADOW_CUT_TRMC=0` leaves it
  out, and `docs/CUT.md`, "Tail recursion modulo context", is the design.
- `Std.Collections.UnionFind`: disjoint sets inside a `runSt`, with union
  by size and path compression. `make`, `add`, `find`, `union`, `same`,
  `count` and `groups`, and a value per group that `unionWith` merges, which
  is what unification wants; the MiniML example's type checker is built on
  it.
- Silo finds a program's garbage cycles by concurrent tracing, unless the
  build asks for trial deletion with `cycles = "trial"` in a
  `[profile.<name>]` of `Meadow.toml` or `MEADOW_CYCLES=trial`. A trace of a
  thread's heap takes its roots in one short pause, marks on a thread of its
  own while the program runs, and sweeps a slice at a time, freeing every
  cycle counting left -- including the ones trial deletion missed, whose
  `Ref` was let go of while the cycle was still held, and without the long
  pause trial deletion took over a large one. Counting still frees everything
  else, and a program that cannot tie a knot carries neither collector.
  `MEADOW_SILO_CYCLES` says what the traces did. `docs/SILO.md`, "Cycles", is
  the design.
- The language reference says in what order a program is evaluated -- as it
  is written, from left to right, each part of an expression before the
  expression -- and why that is part of the language: an effect happens in
  the order the program performs it, so `raise "a" + raise "b"` is
  `Err("a")` under `toResult` wherever it runs. `docs/TUTORIAL.md`,
  "Evaluation order". One program checks it on every engine at every
  optimization level -- Glade's VM, JIT and native code, Silo, the CEK and
  AxCut machines, and MeadowBoot's CEK machine -- and another through
  `meadow run` with each backend, in debug and release.
- `meadow` prunes its cache in `~/.meadow/lib`. Each build of `meadow` writes
  its runtime libraries there, and a build without a precompiled `Std` its
  compile of that, in directories named for the sources that made them -- so
  every nightly and every development build added tens of megabytes that
  nothing removed, until the disk was full and no link worked. Now, at most
  once a day, `meadow` removes the directories other builds wrote that have
  not been used for a week, never one used within the last hour, and nothing
  in `~/.meadow/lib` whose name it does not know; a prune that fails is left
  for the next and never fails a build. `meadow clean --cache` removes all but
  the running `meadow`'s own at once and says how much that freed.
  `docs/RUNTIME.md`, "AOT executables".
- `Time.sleep` waits on the scheduler's timer instead of on the core running
  the thread. Other threads go on running in the meantime, so a program can
  have ten thousand threads asleep at once on a few workers. A sleeping
  thread that is cancelled is woken at once and fails "cancelled", as a
  thread waiting on a channel is. A thread that is asleep will wake, so
  threads waiting on it are not a deadlock. This works on the CEK machine,
  on Glade (the VM, the JIT and executables) and on Silo. A machine that no
  scheduler runs still sleeps in place: a procedural macro's, or a Silo
  program that never spawns. `docs/CANCELLATION.md` is the design, for this
  and for what follows from it: cleanup that runs as a thread ends, sockets
  closed with the thread that held them, and timeouts.
- A thread can say what to undo on its way out: `Std.Thread.bracket acquire
  release use` releases what `acquire` gave when `use` returns, or, if the
  thread fails or is cancelled first, before `await` sees it end.
  `Std.Thread.finally` is a bracket with nothing to acquire, and
  `Std.Thread.defer` registers a release for the end of the bracket around it
  or of the thread. Releases run newest first. One that fails ends only
  itself, and a thread that had succeeded then fails with what it failed
  with. A cancel waits while a release runs: its operations answer and its
  waits are not broken into. In a cancelled thread a release that runs on and
  on is still ended, after longer than other code is given, and the next one
  runs. A release performs only what the runtime answers, as a spawned
  function does, which its type checks: on the way out of a failure no
  handler is left to answer anything else. What a handler drops, such as a
  bracket inside a `toResult` that raised, is released when the bracket
  around it ends, or the thread does. `Std.Thread.race f g` runs both on
  threads of their own and answers whichever ends first, cancelling the other
  and waiting for its releases; `Std.Thread.timeout ms act` is `Just` the
  answer, or `None` once `ms` milliseconds have passed and `act` has been
  cancelled. Works on the CEK machine, Glade (the VM, the JIT and
  executables) and Silo. `docs/CANCELLATION.md` is the design, and says why
  this is a library over a stack the runtime keeps per thread rather than a
  clause on `handle`.
- A thread that ends closes the sockets it holds. A listener, stream or TLS
  stream belongs to the thread that opened it, and is closed when that
  thread ends, whichever way it ends and after its releases have run. A
  cancelled server's port is free again at once, and its peers see the end
  of their streams. A socket handed to another thread, in a function it
  spawns, a message or its answer, belongs to that thread from then on. So
  an accepting thread can give each connection to a worker, and a worker
  that is cancelled takes its connection with it. A socket kept in a `TVar`
  or a `Compact` belongs to nobody and stays open until it is closed. A
  connection accepted for a thread that was cancelled while it waited is
  closed as it arrives. This works on the CEK machine, on Glade (the VM, the
  JIT and executables) and on Silo. The runtimes find sockets in what crosses
  between threads by two new roles, `listener` and `stream`, which a program
  declares for `Std.Net`'s constructors. A program that declares neither,
  such as Cut from another front end, keeps its sockets open as before.

### Fixed

- MeadowBoot's language server forgets a document the editor closes. It
  went on compiling the document's package with the closed document's last
  text, saved or not, so the files still open kept the errors of an edit that
  was never saved. Now the package goes back to what is on the disk, as with
  `meadow lsp`, and the documents still open are looked at again: a sibling
  broken only in the editor and then closed leaves no error behind, and one
  saved broken and mended in the editor before closing shows the disk's
  error again. MeadowBoot's prompt keeps a `:t`, `:time` or unknown `:` line
  in its history once, where it kept it twice. It still writes what it says
  of an entry before the entry runs (what is wrong, `name : scheme`) ahead of
  what the run prints, now through `repl!`'s `speak`. MeadowBoot and Cut are
  built on Lingua d40e7af, which gives `lsp!` its `closed` and `repl!`
  its `speak`.
- `meadow fmt` no longer folds a `macro` into the declaration above it or
  breaks its rules a word to a line. A `macro` that did not start at the
  margin was read as more of what came before -- `Std.Cmp`'s two ran on
  from `isEqualOrdering`'s last arm -- and every `macro`'s templates, which
  hold declarations as often as an expression, were laid out as one
  expression. A `macro` is now always kept as it is written, as a
  declaration under `@fmt(skip)` is, and `Std.Cmp`'s and `Std.Ops`'s are
  back as they were written.
- `meadow fmt` lays out a function defined by clauses as one line where it
  fits, and otherwise with each clause on a line of its own, a step in,
  under its signature or its first clause. It used to read a clause's `|`
  as an operator after the signature, so a long one broke inside a clause
  and left its body to the left of its `|`.
- `meadow fmt` leaves a comment on its own line before a bracketed argument
  where it is. It put the bracket at the end of the line of what it is
  applied to, and the comment with it, so that it read as about that.
- MeadowBoot could not build Lingua: a value a package exports again from
  one of its own dependencies, as Doodle exports Bloom's styles, reached the
  packages after it with no type, so every use of it there shared one, and a
  function generic in what it performs was not generalized when it called
  one. Lingua's finder came out performing `Console`, which its pure tests
  were then refused for. Such an export now has the type its own package
  gave it. MeadowBoot is built with Lingua and Cut at their current `master`.
- Glade compiles MeadowBoot's own Cut. It used to refuse it with "no run of
  64 free registers": an array literal of tuples, each made where the array
  is from values computed before it, left a value in every third register, so
  the array had no run of free registers to be read from although most of the
  file was free. Where an instruction wants a run of registers -- an array, a
  record, a constructor's fields in a match -- the values in the way are now
  moved aside first, and a call through an object that finds no run for its
  arguments puts them at the bottom of the file, as a jump does.
- A Cut program that both handles an operation and exports a definition to C
  was lowered with its `perform`s jumping to the wrong definition: the
  search they share was numbered as if there were no exports.
- A handler in a loop overflowed Silo's stack: a Cut program after about a
  million operations, and a Meadow one whose clause does more than `k e` after
  about ten million. Cut's `perform` kept its continuation in the objects that
  search for the handler, so it was built on the heap, and what came after the
  `perform` ran inside the resumption, a frame deeper for every operation. It
  is handed to the search instead, and stays a frame. And a clause that
  resumes as its last act left a frame of the runtime's on the handler's stack
  for every operation: its handler now resumes the continuation itself.
  MeadowBoot compiled by itself was such a program, twice over. On Windows
  it overflowed a handler's 256 MiB segment compiling its own sources, with
  one of those frames for each operation resumed. And compiling the
  standard library and a small program, about one run in fifteen it crashed
  or said every thread was waiting (on Linux it hung instead): the same
  stage 2, built with the lowering of Cut's `perform` alone fixed, or with
  trial deletion in place of concurrent tracing, failed no run in a hundred.
- `Std.Json` read an integer too long for an `Int` as whatever it wrapped round
  to. It now reads as the nearest `Float`, the one number that is close.
- MeadowBoot compiled a float literal of more than about 18 digits to the
  wrong number, wrapping them through an `Int`, so that `Std.Num.Float.sin` and
  `cos` differed from the compiler's. It now gives the nearest float to every
  literal.
- A Silo thread cancelled as its `Net.connect` or `Net.accept` came back could
  leave the socket open for as long as the program ran, so the other end of
  it waited forever. The call was counted twice, once in a frame the cancelled
  thread never returned to, so it never seemed to end, and what it opened was
  not closed.
- On every Glade backend, a listener stayed open after the thread that opened
  it ended, for as long as the program ran: `Net.listen` was the one socket
  call that did not say whose the socket was. A `connect` to it went on
  succeeding, and nothing ever accepted it.
- `meadow cut` and `meadow run` said nothing when the program they ran
  crashed: a signal left them as a plain failure with status 1, and an
  exception on Windows as its own status with no word of what it was. They now
  say what ended it, and leave with the shell's 128 and the signal on Unix.
- A function that performs two effects whose rows met in inference read
  with its effect's tail as a record's -- `forall r. () -> () ! { Ask, Tell |
  r }` -- in an editor's hover and wherever a type is shown, and was
  quantified as one. It is `e`, as for any other effect.
- `MEADOW_SILO_POISON` and `MEADOW_SILO_LEAKS` together counted every
  poisoned block as left behind at exit.
- Under `MEADOW_SILO_POISON`, a Silo thread that failed gave its heap's
  memory back, and the blocks it had freed were said to be written after
  they were freed by whichever thread was given the memory next, or the check
  at exit read memory that was gone and crashed.
- A Silo thread that failed inside a handler kept the stacks of the handlers'
  segments for the rest of the run, and the leak check counted them as live.
  They are given back with the thread.
- The language server answers for one file of a large package without
  compiling the whole package on every change. It infers the module being
  edited and reuses what earlier compiles found for the rest, while what
  that was worked out from is unchanged; the first compile of a package
  answers for all of its open files at once; and an autosave of an open file
  re-analyses nothing. An edit in MeadowBoot took about 23 seconds to answer
  before, and the same again for each other file open.
- In an editor, a declaration a procedural derive is written for is not a
  link to itself from its keyword: what `@derive(Lexer)` wrote spanned the
  whole declaration, so `data` went to the type, and a rename of the type
  would have written over all of it. Only names are links now.
- A constructor given its fields by name out of their declared order,
  `Point { y = …, x = … }`, evaluated them in the declared order, and a
  record extended, `{ x = v | r }`, evaluated `r` before `v`. Both are
  evaluated as they are written now, as an anonymous record and an update
  already were -- in MeadowBoot too.
- A release build that inlined a generic function answering one of its
  arguments, `(id f) x`, lost the type of a function it answered: the
  AxCut machine refused the program, holding a closure in a name of no
  known representation.
- Glade's native code on x86-64 reads promoted objects itself: a tag test or
  a field read of an object in the old generation went to the interpreter,
  as it has not on arm64 since the block tables. It also keeps `r0`–`r4` in
  their registers from one function to the next and enters a method directly
  on a call or a return, does arithmetic and comparisons in those registers,
  and finds an object once for a tag test and the field reads after it. And a
  nursery collection promotes at once what keeps surviving to be promoted,
  rather than copying it within the nursery first. On an x86-64 desktop,
  `benchmarks/binarytrees` went from 1.96s to 0.75s, `rbtree` from 8.2s to
  3.6s, `wordfreq` from 373ms to 302ms and `fib` from 35ms to 21ms, with
  nursery pauses no longer than they were.
- `benchmarks/run.py` measures Meadow's startup with a `main` the compiler
  accepts.
- A Silo program built with trial deletion (`MEADOW_CYCLES=trial`) could die
  of a segmentation fault, with nothing said, when two candidates of one run
  of the collector both reached the same garbage: the second walk read the
  block the first had freed, whose colour is now where its size class links
  to the next free block, and could take it for garbage again and free it
  twice. A run frees what it found once it has walked everything. Under the
  leak check (`MEADOW_SILO_LEAKS`), a block freed twice is now said where it
  happens.
- Nothing is left in the system's temporary directory when a step fails part
  way. `meadow test` on Silo removes the executable it built of a package's
  tests when linking it fails, as it did when the tests ran, and `meadowup`
  removes a downloaded release when the download or the install fails rather
  than only once it succeeds. Meadow's own test suites, which had left
  thousands of directories there -- tens of gigabytes on a developer's system
  drive -- now remove each test's directory when it ends, passed or failed,
  and keep it in the build's `target/tmp`; `MEADOW_KEEP_TEST_DIRS=1` keeps
  them to look at.
- `it` in the REPL is the entry before, so `1 + 2` then `it * 10` is `30`. An
  entry is compiled as `def it = ...`, a top-level name is in scope in its own
  body, and so `it` was the entry itself: it recursed until the runtime aborted
  with "the old generation has used up its address space", taking the session
  with it, or, as `it` alone, spun for ever. That was every top-level value
  that mentioned itself, in a file as well. Outside of a function a `def` no
  longer sees its own name: the name means what it meant before, an import or
  an earlier entry, as in a `let`, and is undefined if that is nothing. Inside
  a function it is the definition still, so `def f = \n -> f (n - 1)` recurses.
- A type with five or more type variables printed `e`, `f`, `n` and `r` for
  plain variables too, the letters of an effect row, a fraction, a number and
  a record row, so a scheme could name two different variables `e`. Plain
  type variables now skip those four letters everywhere a type is printed --
  hovers, errors, `meadow build --types`, MeadowBoot -- going `a`, `b`, `c`,
  `d`, `g`, `h`, so a letter always says what kind of variable it is. A type
  with four or fewer prints as before.
- `Std.Json` lost a float's bits both writing and reading it. `render` wrote
  six decimals, cut off rather than rounded, so that every float in a `make!`
  cache was damaged and came out a little lower each time it was written
  again. Below zero it wrote another number (`-1.5` came out as `-2.500000`),
  past an `Int`'s range a nonsense one, and it stopped the program on what is
  not finite. It now writes what `show` writes, the fewest digits that read
  back as that float, with a point even on a whole number, so a float stays a
  float; and `null` for NaN and the infinities, which JSON has no text for,
  so that the document stays JSON and no number is claimed. Reading goes
  through `Std.String.Parse.Lexer.float`, which added its digits up in floats:
  it lost bits at the ends of the range, read `5e-324` as zero and `0e999` as
  not a number, and wrapped an exponent too long for an `Int`. It now gives
  the nearest float to every text, by the new `Std.Num.Float.fromDecimal`, on
  every engine, and a minus keeps the sign of zero. A cache written before
  this still holds the damaged numbers, and rebuilding the compiler does not
  clear all of it: Lingua's `make!` keeps a part of a unit under a key with no
  compiler in it. Remove the `target` directory of anything built by a
  Lingua-built compiler once, after rebuilding that compiler.
- A Silo program that cannot spawn a thread could have its tracing collector
  free what its outermost frames held. A trace scans the stack up to where the
  program began, and that place was taken a little too low, so those frames
  were read only if a copy of what they held happened to be left lower down.
  Compiled from Cut on Linux, whose calling convention keeps those frames
  small, a program lost its `main`'s continuation in about one run in seven:
  it died with a segmentation fault as `main` answered, or ran on through
  memory given to something else until it ran out. A trace now scans every
  frame the program has.

## 0.4.0 - 2026-10-24

Meadow on phones: Silo builds a program, or a library an app loads, for
Android from any desktop and for iOS from a Mac, and the MeadowBoot archives
carry the runtime each needs. And the standard library speaks to the network:
`Std.Net`, `Std.Tls` and `Std.Http`, with `Std.Log` for what a program says it
is doing.

### Breaking

- One `use … as` alias for two modules is an error, labelled at both. The
  second took the first's place before, silently, and what the first module
  exported was then "not exported" by the alias. The same `use` repeated is
  still let be.

### Added

- Android: `--target aarch64-linux-android` or `x86_64-linux-android`, from
  any desktop, builds a Silo program or an `@export` library (`libName.so`)
  with the NDK's clang. The NDK is found from `ANDROID_NDK_HOME`,
  `ANDROID_NDK_ROOT`, `ANDROID_NDK_LATEST_HOME` or `ANDROID_NDK`, and the API
  level is `MEADOW_ANDROID_API`, 24 by default. CI runs a program, and a C
  host that loads a Meadow library, on an Android emulator.
- iOS: `--target aarch64-apple-ios`, `aarch64-apple-ios-sim` or
  `x86_64-apple-ios`, on a Mac, builds with Xcode's clang for iOS
  `MEADOW_IOS_MIN` (15.0 by default) and later. An `@export` library for iOS
  is static, `libName.a`, with Silo's runtime in it, since an app loads no
  code from outside itself. CI runs a program and a C host of a Meadow
  library in the simulator, and links both for a device.
- `--target` takes a target triple as well as an architecture, in
  `meadow build`, `meadow cut` and MeadowBoot's `build` and `cut`. A build
  for another system compiles with that system's `@cfg`, where
  `os = "android"` and `os = "ios"` are now known, and is not run.
  `scripts/silo-runtime.sh <triple>` builds Silo's runtime for one; the
  MeadowBoot archives carry Android's, and on macOS iOS's, in
  `lib/<triple>/`, where `meadow` looks for a target's.
- A library of `@export` functions carries a table of them by name, which
  `Std.Ffi.own ()` looks in, so that it finds them in an app whose release
  build stripped its symbols.
- `Reflect`, and so `@derive(Reflect)`, for `Char`, `Float32`, every
  fixed-width integer, and pairs and triples. A value read back that does not
  fit its type -- 256 as a `UInt8`, a surrogate as a `Char`, a list of two as
  a triple -- is an error, not wrapped.
- The language server goes to the definition of a module, at a `use` path, an
  alias or a qualifier, and of a record field; and a module's type in an
  inlay hint is a link to it.
- Two diagnostics for Silo's heap: `MEADOW_SILO_POISON` fills what is freed
  with junk and never reuses it, and at exit reports any block written since
  or freed twice; `MEADOW_KEEP_IR` keeps a Silo build's LLVM IR after it
  links.
- A constructor is written through the module a `use … as` names, as its
  functions and types are: `use Mira as M` and then `M.VoxelHit b f d k`, in
  a pattern or an expression, of the package's own modules and a
  dependency's alike. Where two of the module's types have a constructor of
  that name, `M.Type.Ctor` says which, and `M.Ctor` is an error saying so.
- `Std.Log`, what a program says it is doing, as an effect: `Log.info`,
  `Log.warn`, `Log.error`, `Log.debug`, or `log level message`. Where nothing
  handles it the runtime writes each line to standard error with its time and
  level, if the level is at or above the one `MEADOW_LOG` names (`debug`,
  `info`, `warn`, `error` or `off`; `info` by default). `captured` collects a
  block's lines, `silenced` drops them, and `atLeast` keeps those at a level
  or above. `main` and a spawned thread may perform it, as `Eff` now says; a
  program's own effect called `Log` is still its own.
- `Std.Net`, TCP sockets: `listen`, `accept`, `connect`, `read`, `write` and
  their text forms, `close`, `setTimeout`, `port` and `lookup`, on a
  `Listener` and a `Stream`. Every operation answers `Err` with what went
  wrong rather than stopping the program. `Net` is in `Eff`, so `main` and a
  spawned thread may use it.
- `Std.Tls` makes a `Std.Net` stream a TLS session: `connect` as a client,
  checked against Mozilla's roots (and one more authority with
  `connectTrusting`), or `accept` as a server, from a certificate chain and
  key in PEM files. What it answers is a `Stream` like any other. The
  runtime's TLS is rustls, so nothing has to be installed where a program
  runs.
- `Std.Http`, HTTP/1.1 in the standard library. A server: `serve host port
handler`, a `handler : Request -> Response ! Eff` answering each request on
  a thread of the connection's own, with keep-alive, chunked bodies and
  `serveTls` for HTTPS; `serveOn` and `serveOne` for a listener of the
  program's own. A client: `get`, `post` and `request`, over `http://` or
  `https://`. `text`, `html`, `json` and `withHeader` build a response;
  `header` reads one in any case, and `bodyText` and `requestText` read a
  body.
- A thread waiting on a socket holds up only itself. Glade and Silo make a
  call that may wait on an I/O thread while the worker runs the program's
  other threads, and the CEK machine makes it on an OS thread of its own, so
  a server's thousand reading connections need no more workers than cores. A
  program ends when `main` does even with a thread still accepting, and
  closing a socket wakes a call another thread waits in on it.

### Fixed

- `Std.Ffi` calls a C function of seven or eight integer and pointer
  arguments on x86-64 Linux and macOS, as it did on every other target; it
  refused more than six there.
- Silo's cycle collector frees a garbage cycle whole when it reaches past the
  candidates of one run. It left the rest for their own turn, pointing at
  what it had freed, and a program that made such cycles could corrupt its
  heap.
- A Silo program that ends with `Process.exit` writes what it was asked to
  report, as one that returns from `main` does: `MEADOW_SILO_PROFILE`'s
  samples and the counts of `MEADOW_SILO_PRIMS` and `MEADOW_SILO_CYCLES`.
- A Cut program's `Process.argv`, and the other sequences a runtime makes, are
  plain arrays when the program declares no vector, as `docs/CUT.md` says.
  Glade made one with an unknown tag, and Silo gave it a wrong length.
- A Cut handler that resumes runs on Glade and Silo: `resume` is `apply`, as
  `docs/CUT.md` says, and an object's methods it does not define take its
  captures. Lowering one panicked before.
- `stringByteAt` given straight to a continuation is a `u8`, as every runtime
  answers it, and not a pointer.
- Silo, at `-O2`: a literal in a tuple, matched through an inlined copy of a
  generic, is tested as its own type, where it reached the runtime boxed.
- A package whose directory is spelt differently from its name links on
  Windows: two of its units were one file there, and one overwrote the other.
- `meadowup` and `meadowup-init.sh` send `GITHUB_TOKEN` with their requests
  to GitHub where it is set, so that a CI job installing Meadow is held to
  its token's rate limit. Unnamed, a runner's requests share its address's,
  and GitHub answered 403 when that was spent.

## 0.3.0 - 2026-10-16

The release of MeadowBoot, the Meadow compiler written in Meadow with the
Lingua toolkit: it compiles itself, agrees with the Rust compiler pass by
pass, and is published beside `meadow` as an archive for each platform, with
the `meadow` it hands the back end to and the standard library's sources.
Meadow libraries can now be called from C, which is how mira runs Meadow
scripts.

### Breaking

- The lockfile is `Meadow.lock`, spelt as `Meadow.toml` is. One named
  `meadow.lock` is still read, and is renamed the first time the lock is
  written; commit the rename.

- `[a..b]` stops before `b`, as Rust's `a..b` does, and `[a..=b]` includes it.
  Both included `b` before: the parser read the two alike. Write `..=` where
  a range is to reach its end -- `[1..=10]` for one to ten.
- An unstable feature is asked for in the source, not in `Meadow.toml`:
  `@!feature(ffi)` at the top of the package's root module, as Rust's
  `#![feature(…)]` is at the top of a crate's. `features = ["ffi"]` under
  `[package]` is an error that says so. A lone file that uses `Std.Ffi` asks
  the same way; a nightly let it through unasked before.
- `meadow fmt` prints a file from its tokens, as rustfmt does, and no longer
  keeps the line breaks it was written with: the same code comes out the same
  however it was typed. What fits in the width is one line and what does not
  is cut at its outermost joint first. Every file it formatted before is
  formatted differently now; run it once over a project. It kept the lines a
  file had, and cut or joined them by rules that depended on where they were,
  so two layouts of one function could both count as formatted.
- `meadow fmt` keeps lines within 100 columns by default, as rustfmt does. It
  was 80. `--width 80` asks for the old width.

### Added

- `Std.Ffi` calls C on Windows (x86-64), where before it was Unix only: a
  library is opened with `LoadLibrary`, and a call is made in one of sixteen
  shapes, by which of its first four arguments are floats.
- `Std.Ffi` passes and answers a C `float` (`Arg.Float32`, `callFloat32`),
  calls a function whose address is in memory (`functionAt`, as a C library
  hands its plugins a table of what they may call), lays out C structs
  (`CType`, `sizeOf`, `alignOf`, `offsetOf`, `Aligned` for an over-aligned
  one), reads and writes one value of a C type where it is (`readInt`,
  `writeFloat`, `readPtr`, …), and lends text to C as a pointer and a length
  (`withText`).
- `Std.Ffi.own ()` opens the module the calling code is in -- the program
  itself, or the library a plugin is -- so a function of it can be called by
  address, as `open ""` does.
- C calls Meadow: MeadowBoot builds a package whose functions say `@export`
  -- or `@export(cName)`, under another name -- as a C library named for the
  package (`Name.dll`, `libName.so`, `libName.dylib`), in
  `target/<profile>/native/silo` as a native program is, each such function a C function of its numbers
  (`int64_t`, `double`, `float`, the sized integers, `bool`) answering one
  of them or `void`. Cut writes them as `export "name" = SYMBOL -> rep`, and
  the Silo back end gives each a C entry that starts the runtime the first
  time it is called on a thread. A function whose answer is not a number or
  `()` is refused, saying so. A library exports its dependencies' `@export`
  functions too, as a Rust `cdylib` does every crate's `#[no_mangle]` ones,
  and two of one name are an error; a program exports only its own.
- A call C makes into a Meadow library that fails -- an `error`, a division
  by zero -- ends that call, which answers zero, and not the program C is:
  each runs on a stack of its own, let go of when it fails. The message is on
  stderr, and `Std.Ffi.failure ()` answers it once, so a plugin that calls
  its own function through C, by address, can tell its host what went wrong.
- `Std.Num.Float`: `sqrt`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`,
  `exp`, `ln`, `pow`, `hypot`,
  `ceil` and `round` (to an `Int`, as the built-in `floor` is), `abs`,
  `isNaN`, `isFinite`, `pi` and `tau`, written in Meadow so every runtime
  answers alike: good to a unit or two in the last place for arguments of
  moderate size.
- `intercalate` in the prelude joins strings with one between each two:
  `intercalate ", " names` (`Std.String.join` by the name a list's has).
  `Std.Collections.Vector` has `intersperse` and `intercalate` too, for any
  vector: `V.intersperse 0 [1, 2]` is `[1, 0, 2]`.
- A type is written through the module a `use … as` names, as its functions
  are: `use Mira as M` and then `(s : M.System)`, without importing the type
  by name. A type the module lacks is an unknown type, `M.Nope`.
- A git dependency may be a directory of its repository rather than all of
  it: `{ git = "…", rev = "…", subdir = "lib/Cut" }`. Its directory is part of
  its source in `Meadow.lock`, so two packages of one repository are two.
- `meadow cut --no-run` builds what would be run -- the bytecode image, or the
  executable with `--aot` or `--runtime silo` -- and stops: what a front end's
  `build` asks the back end for.
- Property tests and fuzzing in the standard library. `Std.Test.Property`
  checks a property on a hundred values a generator makes -- `forAll gen
prop`, or `check` where the property draws for itself -- and fails with the
  smallest case it found. A generator is a function that draws numbers
  (`between`, `bool`, `vector`, `element`, `oneOf`, `maybe`, `char`, `text`),
  so one of your own is any function that calls them, and it shrinks as they
  do: a failing case is run again on fewer and smaller draws. The same cases
  are checked every run, and `replay draws prop` checks one again.
  `Std.Test.Fuzz` runs a function on text close to examples it is given:
  `fuzz corpus words prop` tries a thousand, each some example with pieces
  taken out, doubled, moved from another, or words dropped in, and shrinks
  what fails. `fuzzGuided` is the same steered by coverage: it asks the
  machine after each input how much of the program has been reached, and an
  input that got somewhere new is kept as an example for later ones -- so it
  finds a text that starts `fuzz` in a few hundred tries, a character at a
  time, where `fuzz` would need all four at once. It steers on the bytecode
  machine, where tests run, and a thread that has asked is interpreted from
  then on; a native program is told nothing is new. A program that stops the
  process is not caught by either.
- `Process.covered afresh` answers how many places in the program the thread
  has reached since it first asked, or since it last asked with `True`: what
  `fuzzGuided` steers by.
- `Std.Test.failure act` runs `act` and says whether it failed an assertion
  and with what message, where `didFail` says only whether.
- A natively compiled program can profile itself: run with
  `MEADOW_SILO_PROFILE=<file>` it samples where its time goes, and
  `scripts/silo-profile.py` reads the file against the executable. For a
  machine that allows no profiler from outside, as a container does not.
  `MEADOW_SILO_PRIMS` also says which blocks a program erased, which
  functions dropped them, and which made its arrays.
- A program of Cut keeps the continuation a call answers through on the
  stack, as a program the Rust compiler lowers does, and no longer on the
  heap: a handled body runs on a stack segment of its own, which an
  operation that captures its continuation cuts off and its resumption puts
  back, once. A program with an operation marked `@many` is lowered as
  before. `MEADOW_CUT_FRAMES=none` asks for the old lowering.
- `main` is `() -> () ! Eff` wherever its type is shown, not the row its body
  happened to perform; and a row of every effect the runtime answers is
  written `Eff`.
- `Std.Collections.Vector` folds and searches a vector where it stands; each
  of `foldl`, `find`, `any`, `elem` and their kin copied it whole first.
- `Std.Json` writes a value in time and room in its size, escapes control
  characters as `\u00XX`, and reads `\uXXXX` back.
- The Meadow colour theme for VS Code, **Meadow Dark** and **Meadow Light**, is
  an extension of its own in `editors/vscode-theme`: a colour for each kind of
  name the language server tells apart. Each release attaches it as
  `meadow-theme-<version>.vsix`, and `scripts/install.sh --with-extension`
  installs it beside the language extension.
- `meadow cut FILE -- ARGS` gives the program what follows `--` as its
  arguments.
- `meadow cut` on Glade compiles what runs often to machine code as it goes,
  as `meadow run` does, where it only interpreted; `--aot` compiles the
  whole program ahead of time into an executable.
- `meadow cut FILE` on Glade keeps the program's bytecode image beside it,
  under `target/cut`, named for the program's text and the `meadow` that
  lowered it. A second run of the same text reads the image and lowers
  nothing: a program a front end runs again and again -- the one that serves
  a package's macros, once for every round of a build -- starts in a tenth
  of the time.
- A program of Cut may spawn threads: `prim threadSpawn(desc(rep), f; k)`
  says how what the thread answers is represented. And a top-level `val` is
  made the first time a thread reads it, not when the program starts, so a
  thread other than the first has it too.

- The language server answers `meadow/ir`: the definition at a position as
  core, Cut or AxCut, with what each part of the text is and where the name
  at the position is in it. The VS Code extension's **Meadow: Show IR** shows
  it beside the source and follows the cursor.
- The REPL shows a definition in the compiler's IRs: `:ir NAME` prints it as
  core, as Cut and as the AxCut the runtimes are handed, and `:ir core NAME`,
  `:ir cut NAME` or `:ir axcut NAME` one of them. The Cut is core lowered for
  showing: a program is not compiled through it yet.
- `meadow cut FILE` runs a program written in Cut, the IR a front end hands
  the back end (`docs/CUT.md`): on Glade, or with `--runtime silo` as a native
  executable. It takes programs that handle no effects of their own so far.
- A trait can have associated effects: `effect Reading r` in the trait, a row
  in each `impl` (`effect Reading Done = {}`, `effect Reading (Writing s) = {
St s }`), and `! Reading r` where a method's type says what it performs. A
  function generic over the trait performs what its type's `impl` says.
- A trait's parameter can be an effect: one a method performs, `trait Rows r e
{ fun slot : r -> Int ! e }`. An `impl` gives a row for it, `impl Rows
(Writing s) { St s | e }`, or a variable, and is found by the other
  parameters.
- `@fmt(skip)` on a declaration, or on a line of its own before one, has
  `meadow fmt` leave it exactly as it is written, as `#[rustfmt::skip]` does.
- A record update takes a field by its name alone, as construction does:
  `{ r | x, y = 2 }` is `{ r | x = x, y = 2 }`. An update of one field still
  writes its `=`, since `{ r | x }` is the extension of `x` with a field `r`.
- `@!name(…)` before a module's first declaration is an attribute of the
  module. `@!feature(…)` is the one there is.
- A package has features, as a Cargo crate does: `[features]` in `Meadow.toml`
  names them, each with the features it turns on with it, and `default` is the
  ones on unless a build says not. `--features a,b`, `--no-default-features`
  and `--all-features` choose among the features of the package being built,
  a dependency is written `{ …, features = ["a"], default-features = false }`
  to choose among its own, and the source asks with `@cfg(feature = "a")`. A
  feature the package does not declare is an error that lists the ones it has.
- `meadow fmt` sets a record out as rustfmt sets out a struct: its brace on the
  line of what it belongs to, a field to a line, and the closing brace back
  under the start of that line. A function given last to a call starts on the
  call's line, as a closure does there.
- `meadow fmt` gives a declaration's head that does not fit a line to each
  thing it takes and one to its type, as rustfmt does a signature.
- `meadow fmt` puts what is written over several lines back on one when it fits
  the width: a definition and its body, a call and its arguments, an arm and
  what it answers, a record and its fields, an `if` and its branches. The
  steps of a `let … in` have a line each, and the arms of a `match`, unless
  there are two short ones.
- Formatting on save in an editor does what `meadow fmt` does.
- `meadow fmt` puts a space between a name and a `{` written against it,
  `Ctor{` to `Ctor {`.
- The REPL starts with a short banner, so its name is still on the screen at
  the first prompt, and `:help` lists the commands and the keys.
- The REPL's banner and the tutorial say which keys force a new line on macOS:
  `Ctrl+J` always, and Option+Return once the terminal sends Option as Meta.
- The REPL colours what is typed as the language server would: the entry is
  compiled against the session as it is typed, so types, constructors,
  modules, functions and variables each have a colour, by what each name
  resolved to and its type. The `name : type` line an entry prints is
  coloured too.
- The language server's semantic tokens mark a name whose type is a function
  as `function`; every lower-case name was `variable`.

### Fixed

- A natively compiled program that makes `Ref`s by the million and no cycles
  -- an interpreter, each of whose scopes shares cells with the one it was
  made from -- no longer spends most of its time looking for cycles. The
  collector waits for twice as many candidates each time a round of its runs
  frees nothing, up to 262,144, and goes back to 1,024 when one does; most
  candidates have died by then and are freed without a walk. One macro call
  on MeadowBoot's CEK machine took 312 seconds, 72% of it in the collector,
  and takes 54.
- A natively compiled program lets go of what a finished thread answered,
  and of a `TVar`'s value, once nothing can reach them. Both were kept for
  the whole run -- "nothing says when the last handle is gone" -- and with
  them every compact region they held. Handles are counted now, in heaps, in
  values on their way between threads, and inside regions.
- A natively compiled program frees what a thread kept in a `Ref` when the
  thread ends. A `Ref` that had been written and then let go of was left for
  the cycle collector, which a thread that ends does not run again: the
  `Ref` and everything it held -- compact regions among it -- stayed for the
  rest of the program.
- A natively compiled program frees a compact's region once a thread that
  held the compact has ended. What the thread had let go of but not yet
  erased went with its heap, and the region was kept for good.
- Threads that read values out of one compact region no longer slow each
  other down on Silo. Each share and erase of such a value wrote the region's
  one count; a thread now counts for itself and tells the region when it
  moves on. Parsing the standard library's files on ten threads took as long
  as on one, and takes a fifth of that now.
- A natively compiled program no longer aborts when a thread waits, or is
  made to give way, under two handlers that keep their continuations and then
  performs the outer one's operation. The runtime lost count of which
  suspended stack segments belonged to which handler.
- A build keeps the release its lock names when a newer one has been tagged
  since. On a machine that had not fetched the dependency yet, a dependency
  taken by `version` was resolved to the newest release before the lock was
  looked at, and the lock's commit was then refused as "a tag that moves" --
  so tagging a release broke every lock written before it, until
  `meadow update`.

- Two builds fetching the same dependency at once no longer fail with
  "could not put … in place": each unpacks into a directory of its own, and
  the one that finishes second finds the checkout there. On Windows the two
  wrote into, and removed, one staging directory.
- `meadow fmt` keeps what an associated effect is on its line in an `impl`,
  `effect Reading (Writing s) = { St s }`. It laid the row out as the fields
  of an `effect` declaration, a label a line.
- A trait used at a type with the caller's own type variables in it -- an
  `impl` for `Builder s`, inside a function generic in `s`, as everything run
  under a `runSt` is -- is compiled away in a release build as it is at a
  known type. Its dictionary was looked up and its method called at run time.
- A small function that takes a value apart and is inlined where its answer
  is needed -- an accessor, a trait's reader at a known type -- no longer
  costs a continuation frame on every call: what waits for a one-armed
  `match` on a constructor goes into its arm, as it already did for a `match`
  with several.
- A trait used inside a `runSt` at a type that names its state -- `label cell`
  for a `Cell s` made in there, through a function generic over the trait --
  was refused as the state escaping. What such a use asks for is answered
  before the `runSt` closes.
- `meadow fmt` keeps the lines of a macro call's body when one of them has a
  comma in it: `lang! { pub Core extends Base with In, set` and a rule to a
  line after it came out with each line ending in the first word of the next.
- `meadow fmt` puts a declaration back at the margin that an earlier version
  had pushed in from it -- after a string that ran over lines, everything
  that followed was set out as more of the expression before.
- Formatting on save does not wait for the language server to finish
  analysing: it is answered from the text at once, on a thread of its own, and
  of several changes to a document waiting to be analysed only the last is.
  In a package with macros to expand a save took seconds.
- `Std.Ffi` answers an error for the null address where it crashed: reading or
  writing through a pointer that is 0, which is what `alloc` gives where there
  is no C library to ask, as on Windows.
- `meadow fmt` puts the line after a `let … in` level with the `let`. A second
  `let` written further in stayed there, and its `in` with it, so the body
  under them was out of line with both.
- `meadow fmt` places the `in` of a `let` written in brackets. It sent that
  `in` to the left margin and lost its place in the bracket for the lines
  after. A bracketed `let` is now laid out with its value four in, its `in`
  two in, and its body on the `in`'s line when they fit; and a line that
  carries another on moves with it when that line is moved.
- The finder reads a signature still being typed as the start of one, so
  `(a -> b) -> [a;]` finds `map` first. It took the last type typed for the
  result, and put `drop` first: a list comes back from it, and its first
  argument is a type variable, which a function fits.
- The finder reads `[a;]` in a type as a list. It dropped the `;` and looked
  for a vector, so `(a -> b) -> [a;] -> [b;]` did not find a `map` over lists.
- Two builds that fetch the same git dependency at once take turns in its
  cache. One could find the other's unfinished clone and report that the
  repository "has no releases".

## 0.2.0 - 2026-10-04

The first release with a version that does not move. Before it there was one
release, `v0.1.0-alpha`, republished from `master`; from here on a version is
published once, and `master` is the nightly channel.

### Breaking

- `[a..b]` stops before `b`, as Rust's `a..b` does, and `[a..=b]` includes it.
  Both included `b` before: the parser read the two alike. Write `..=` where
  a range is to reach its end -- `[1..=10]` for one to ten.
- `Std.Ffi` is unstable. A package that uses it says so in its `Meadow.toml`,
  `features = ["ffi"]`, and a stable `meadow` refuses it; a nightly accepts it.
- `Std.Ffi.Arg`'s constructors are `Arg.Int`, `Arg.Float`, `Arg.String` and
  `Arg.Ptr`. They were `IntArg`, `FloatArg`, `StringArg` and `PtrArg`.
- `Std.String.Parse.Lexer.IndentOpt`'s constructors are `IndentOpt.None`,
  `IndentOpt.Many` and `IndentOpt.Some`. They were `IndentNone`, `IndentMany`
  and `IndentSome`.
- `meadow fmt` cuts lines longer than 80 columns by default. `--width N` sets
  another width and `--width 0` leaves long lines alone.
- A module under the one being written is a qualifier by its name: after
  `mod Core`, `Core.name` means that module's `name`. A `use … as Core`
  written in the same module still wins.

### Added

- Channels. `meadowup default stable` or `nightly` says which releases
  `meadowup update` follows; `meadowup install 0.2.0` installs a version.
  `meadow --version` says which a build is.
- `features = […]` under `[package]` in `Meadow.toml`: the unstable features a
  package uses.
- A `meadow-toolchain` file beside a `Meadow.toml`, holding `stable`, `nightly`
  or a version: `meadow` refuses to build the project with another toolchain
  and says how to get the one it asks for.
- Modules written inside a file, `mod Name { … }`, which nest. A module under
  the current one is reached by its path, `Core.Expr.name`, and so are its
  types' constructors and its pattern synonyms. `use super (…)` names the
  module a module is written in.
- `Std.Ffi`: opening a C library, calling its functions, and reading and
  writing C's memory.
- `Std.Terminal`: raw mode, reading keys, and the terminal's size.
- `Std.String.Parse.keep` and `skip`, for parsers written as pipelines.
- `meadow build --emit expanded` writes each module with its macros expanded.
- `--nursery SIZE`, and `nursery = "8M"` in a profile: how large Glade's
  nursery may grow.
- Syntax highlighting in the REPL.
- `Std.Json` reads through a byte scanner, about twenty times faster.

### Fixed

- Silo on aarch64 passes nothing on the stack, which works around an LLVM bug
  that overwrote a caller's value and crashed programs built with `-O 0`.
- A build is the same from one run to the next, and Silo reuses the object
  files of modules that did not change.
- A `match` on a run of constructors lowers to one switch at every
  optimisation level.
- `@pub` and `@cfg(…)` written in a macro rule's template mean what they say.
  A declaration a rule marked `@pub` was private.
- A pattern synonym reached through a qualifier, `E.Lit n` after
  `use … as E`, is resolved through it.
