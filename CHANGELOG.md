# Changelog

What changed in each release of Meadow: the language, the standard library,
`Meadow.toml`, and the tools. Versions follow [semantic versioning](https://semver.org).
While the major version is 0, a minor version may break source and a patch may
not. The promise covers the language, `Std`'s public API and `Meadow.toml`; the
bytecode image, the Cut IR and the runtimes' layouts may change in any release
until 1.0.

Each release has three lists: what **breaks** code that built before, what is
**added**, and what is **fixed**. A nightly build is `master`, and what it has
beyond the last release is under _Unreleased_.

## Unreleased

### Breaking

- The float operators `+.`, `-.`, `*.`, `/.`, `<.`, `>.`, `<=.` and `>=.`
  are gone, and so is their trait, `Std.Ops.Floating`. `Float` and `Float32`
  implement `Add`, `Sub`, `Mul` and `Div`, and were already `PartialOrd`, so
  `x + y` and `x < y` are what float code writes now, at the same cost. Write
  `+` for `+.`, `<` for `<.`, and so on. A program built without `Std` names
  the float primitives (`_primAddF x y`), since there `+` is the integers'
  primitive.

### Added

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
  `meadow_rt::thread`. An operation made in place, such as a file read or
  `Time.sleep`, runs to its end before the thread stops.
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

### Fixed

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
