# Macros

The design for Meadow's macros: what they look like, how they are expanded, and
what is deliberately left out. Written before the implementation, so much of it
describes code that does not exist yet; [section 12](#12-the-order-this-is-built-in)
is the order it is being built in, and says how far that has got.

- [1. What a macro is](#1-what-a-macro-is)
- [2. Calling one](#2-calling-one)
- [3. Defining one](#3-defining-one)
- [4. Fragments and repetition](#4-fragments-and-repetition)
- [5. How expansion runs](#5-how-expansion-runs)
- [6. Hygiene](#6-hygiene)
- [7. Spans and diagnostics](#7-spans-and-diagnostics)
- [8. Exporting, and incremental builds](#8-exporting-and-incremental-builds)
- [9. Procedural macros](#9-procedural-macros)
- [10. Macros that talk to each other](#10-macros-that-talk-to-each-other)
- [11. What is left out](#11-what-is-left-out)
- [12. The order this is built in](#12-the-order-this-is-built-in)

## 1. What a macro is

A macro takes **token trees** and produces token trees. A token tree is a token,
or a bracketed run of token trees — `( … )`, `[ … ]`, `{ … }` — so the only
thing a macro's argument has to satisfy is that its brackets balance. It need
not be an expression, or parse at all.

This is Rust's arrangement, and the reason for it is that the parser stays
fixed. A macro call is parsed as an opaque argument, expanded later, and the
result is parsed with the ordinary grammar. Nothing a macro does can change how
the rest of the file parses, and `meadow fmt` — an indenter that never moves a
token to another line — keeps working inside a macro call it has never heard of.

## 2. Calling one

A call is a name, `!`, and one bracketed argument:

```meadow
assertEq!(got, want, "they differ")
vec![1; 2; 3]
config! { name = "demo", debug = True }
```

The three brackets mean the same thing. Which one to use is a question of how
the call reads, exactly as in Rust.

A call is an **expression**, a **pattern** or a **declaration**, decided by where
it is written. In declaration position it takes attributes like any other
declaration:

```meadow
@pub
derive! { Show for Colour }
```

`name!` cannot be confused with anything else: `!` is otherwise only read after
`->`, as the effect arrow of a function type. One lexical trap is worth knowing:
`foo!=x` is `foo != x`, because `!=` is one token. A call is always followed by
an opening bracket, so this cannot arise in a real call, and `foo! =` should be
reported as the mistake it is.

## 3. Defining one

A macro matches token trees against rules and takes the first that fits — which
is what `match` does, so a macro is written the way a `match` is:

```meadow
macro swap
  | ($a, $b) -> { ($b, $a) }
```

Each rule is `| ⟨matcher⟩ -> { ⟨template⟩ }`. The matcher is a token tree, and
so is the template; the braces around the template are its brackets, not a
record. Rules are tried in order:

```meadow
@pub macro vec
  | ()                 -> { empty }
  | ($x)               -> { push empty $x }
  | ($x, $( $rest ),+) -> { push (vec!($( $rest ),+)) $x }
```

`macro` is a declaration, so everything that applies to declarations applies to
it: `@pub` exports it and `@pub(pkg)` / `@pub(super)` narrow that, `@cfg`
compiles it conditionally, and a doc comment above it is its documentation.

Macros have a **namespace of their own**, so a function `vec` and a macro `vec!`
can both exist, and the `!` is what an import names the macro by:

```meadow
use Std.Collections.Vector (vec!)
```

### `$` is the language's splice

`$name` in a template is the fragment the matcher bound. This is not borrowed
notation: `${…}` is already a hole in an interpolated string, and it is the same
idea. The two compose without a rule saying they must, because what is inside
`${…}` is tokens:

```meadow
macro describe
  | ($x : expr) -> { "value: ${$x}" }
```

Two more forms:

| written | means                                |
| ------- | ------------------------------------ |
| `$$`    | a literal `$` in the output          |
| `$pkg`  | the package the macro was defined in |

`$pkg` is Rust's `$crate`: it writes the name of the package the macro was
defined in, so a path a template writes reaches what the macro can see rather
than whatever happens to be in scope where it lands.

Where Rust writes `$crate::text::escape` in an expression, Meadow cannot — a
qualified expression has one segment before the dot — so what `$pkg` is for is
the `use` a template writes, which reaches a helper in the macro's own package
from a package that has never heard of it:

```meadow
@pub macro withShout
  | ($( $d : item );*) -> {
      use $pkg.Text (shout)
      $( $d );*
    }
```

A metavariable may not be called `pkg`, since the name is taken.

## 4. Fragments and repetition

A matcher binds a metavariable with `$name : kind`, which reads the way a type
annotation does:

```meadow
macro unlessEmpty
  | ($xs : expr, $body : expr) -> {
      match $xs with
      | [;] -> ()
      | _   -> $body
    }
```

| kind    | matches        |
| ------- | -------------- |
| `tt`    | one token tree |
| `ident` | one identifier |
| `lit`   | one literal    |
| `expr`  | an expression  |
| `pat`   | a pattern      |
| `item`  | a declaration  |

`tt`, `ident` and `lit` need no parser at all. The rest are matched by calling
the parser on the rest of the tokens, which raises the question of where the
fragment stops.

**Meadow needs stricter follow rules than Rust.** Application is juxtaposition,
so an expression does not end where a Rust one would: in `($f : expr $x : expr)`
the first fragment swallows the second, and no amount of care in the matcher
changes that. So a fragment may only be followed by a token that cannot continue
it: `,` `;` `->` `|` a closing bracket, or one of `then`, `else`, `in`, `with`. A
matcher that puts anything else after an `expr` is rejected where it is written,
not where it is called.

That rule is what says where a fragment _ends_, and matching uses it directly:
the fragment runs to the first such token at the top level of the argument —
brackets are already grouped, so a `,` inside one is not a candidate — and that
run is handed to the parser. A run the parser cannot read is not a fragment of
that kind, so the rule does not match and the next is tried, exactly as a
mismatched token does.

A fragment written into a template comes out **parenthesised**, so that it stays
one thing: `$x` bound to `1 + 2` under `show $x` is `show (1 + 2)`, which is what
was passed, and not `(show 1) + 2`. Rust uses an invisible bracket for the same
purpose; a real one costs nothing and is honest in `stringify!`. Declarations are
not parenthesised, since a declaration is never part of something larger.

The same rule applies inside a repetition, where the separator is what ends each
pass: `$( $e : expr ),*` is fine, and `$( $e : expr )*` is refused, because
nothing says where one expression stops and the next begins.

Repetition is `$( … )` with a separator and a count, as in Rust:

```meadow
macro logAll
  | ($( $x : expr ),*) -> { let _ = [ $( println $x );* ] in () }
```

`*` is zero or more, `+` one or more, `?` zero or one. The separator in the
template need not be the one in the matcher — above, the matcher separates with
`,` and the template with `;`, because the template builds a list.

## 5. How expansion runs

Expansion happens on the AST, before name resolution, in `compile_unit` where
[`cfg::strip`](../compiler/meadow-compiler/src/cfg.rs) already runs.

1. The lexer produces tokens, and a **token-tree builder** groups them by
   bracket. Unbalanced brackets are a lexical error, reported there.
2. The parser produces an AST in which a macro call is an **unexpanded node** —
   `Expr::MacCall`, `Pat::MacCall`, `Decl::MacCall` — holding the macro's path
   and its argument token tree. The parser never looks inside one.
3. The expander loops: strip `@cfg`, find a call, match its argument against the
   macro's rules, substitute into the template, **parse the result with the
   entry point for that position**, and splice it in. Whatever comes out may
   contain calls of its own, so this repeats to a depth limit.

The parser therefore needs an entry point per position — an expression, a
pattern, a declaration list — rather than only a whole module. That is the first
thing the implementation adds, and it is useful on its own: it is what the REPL
and the language server would rather call.

`@cfg` is stripped before expansion and again after it, so a `@cfg` on a call
removes the call, and one in a template is read in the expansion.

## 6. Hygiene

A macro that writes `let tmp = …` in its template must not capture a `tmp` the
caller passed in, and must not be capturable by one the caller defines. Meadow
takes Rust's **mixed-site** position: local bindings are hygienic, and items,
types and constructors are not.

The mechanism is a **mark**. Every identifier a template writes comes out as
`tmp#3`, the number being that expansion's; a `#` cannot be lexed into an
identifier, so no source can spell a marked name and no two expansions share
one. Everything else follows from that:

- a template's `let tmp#3 = …` binds a name the caller cannot write, so the
  caller's `tmp` is a different variable;
- what came from the _call_ is spliced in unmarked, so `$x` is still the
  caller's `x`;
- a marked name that nothing bound — `push#3`, where the template meant the
  ordinary `push` — is simply not found, and the resolver looks again without
  the mark. That single fallback is what makes items unhygienic.

The one thing marking must not touch is a lowercase name that is _not_ a
variable: a record label, a module member, a type variable, the name of a
declaration. Those are matched against something outside the expansion, so a
mark would break them. A label and a variable are the same token, though, and
only a tree tells them apart — so marks go on at substitution and come off
those slots once the expansion has been parsed.

Two consequences worth stating. A macro name is an item, so it is _stripped_
before the macro is looked up: that is what lets a template call itself, which
is how a recursive macro is written. And a name a template only mentions
resolves where the call is, not where the macro was defined — harmless while a
macro can only be used in its own module, and the thing [`$pkg`](#3-defining-one)
is for once [export](#8-exporting-and-incremental-builds) arrives.

## 7. Spans and diagnostics

A token that came from the call site keeps its own span, so an error in an
argument is reported where the argument was written. A token the template
produced is given the call's span, and a side table says which macro wrote it.
An error in expanded code is therefore reported at the call — the only place in
the file there is to point at — with a label naming the macro:

```
error: type mismatch: `String` vs `Int`
  ╭─[ main.mw:4:12 ]
4 │ def four = addOne!(1)
  │            ─────┬────
  │                 ╰──── `addOne!` wrote this
```

An expansion inside an expansion names both, innermost first. And because an
argument keeps its own, narrower span, an error in one is still reported as the
caller's own and says nothing about the macro: it is the caller's mistake.

This is also what the editor needs: hover and go-to-definition keep working on
the arguments of a call, because those tokens never lost their spans.

## 8. Exporting, and incremental builds

A macro's rules are stored in its `CompiledPackage` as token trees, so a
dependent expands it without re-parsing the dependency's source.

A macro is a declaration, so it is seen as far as its `@pub` says: `@pub`
anywhere, `@pub(pkg)` within its package, `@pub(super)` in the module above it
and below, and nothing at all without one — with the same fallback the rest of
the surface has, that a unit which never says `@pub` exports all of it.

A `use` names one the way it names anything else, with the `!` saying which
namespace is meant:

```meadow
use Std.Collections.Vector (vec!)   -- that macro, and nothing else
use demo.Helpers                    -- every name it has, macros included
use demo.Helpers as H               -- H.twice!(…)
```

A `use` that names only macros brings in only those: it is not a bare `use`,
which would bring in every value as well.

Incremental builds need nothing new for declarative macros. A package's
fingerprint already covers its module sources and its dependencies'
fingerprints ([`incremental.rs`](../buildtools/meadow/src/incremental.rs)), so
changing a macro invalidates everything that used it.

## 9. Procedural macros

A procedural macro is an ordinary Meadow function, `TokenStream -> TokenStream`,
lowered to an image and **run on the VM during compilation**. There is no
dynamic library, no ABI between compiler and macro, and no bridge crate: the
compiler already contains a machine that runs Meadow.

Three things follow from that, and they are the reason to do it this way:

- **A proc macro is sandboxed by its type.** Its signature forbids `Fs`,
  `Process`, `Random` and `Time`, and the effect system checks it. A macro
  cannot read a file or the clock.
- **So it is deterministic, and its output is cacheable** on the tokens it was
  given. Impure procedural macros are a standing source of stale-cache bugs
  elsewhere; here the type system rules them out.
- **There is no host/target split.** An image runs wherever the VM does, so cross-compiling
  a program does not mean building its macros twice.

A proc macro lives in a package of its own, as it does in Rust, which with
workspaces is just another member. It is given a fuel budget, so a macro that
loops forever fails the build instead of hanging it.

`Std.Macro` provides the `TokenTree` type: a `Word`, a `Punct` by the text it is
written with, a literal, a bracketed `Group` — each with the `Loc` it was written
at — and `Code`, which holds Meadow as text for the compiler to lex where the
call was. A macro writes code with `quote!` (below), or as text with `Code`.

```meadow
-- in its own package: an ordinary exported function that says what it is
@macro
@pub fun shout ts = [Code "\"${spaced ts}!\""]
```

```meadow
use shouty (shout!)

fun main () = println shout!(hello there)   -- "hello there!"
```

**`@macro` is what makes one.** Nothing about a function's type does: a package
that means a function to be called as a macro says so, which is what lets you
find a package's macros by looking, and what lets the type be checked where the
macro is _written_ rather than in whoever imports it. That type is
`[TokenTree] -> [TokenTree]`; the argument may be looser, since a macro that
ignores it never constrains it, but the answer is exactly tokens. It may
perform `Expand`, which is how it reads and leaves compile-time bindings
([section 10](#10-macros-that-talk-to-each-other)).

A macro has to be compiled before it can run, so it belongs to a package the
one using it depends on — as in Rust, and for the same reason. A `use` of one
in its own package is refused, and says why.

The sandbox is that same signature. A macro may not perform `Fs`, `Process`,
`Random`, `Time`, `Thread` or `Stm`, which the effect system checks where the
macro is imported, so nothing it answers can depend on when it ran. That is what
makes the answers cacheable, and they are cached, on exactly the tokens it was
given. It runs with a step budget (`MEADOW_MACRO_FUEL` to change it), so a macro
that does not stop fails the build rather than hanging it.

It runs on the VM. Every macro a build can call is compiled
into one image the first time one is called -- each behind
`Std.Macro.Serve.serve`, which reads the call on its input and writes the answer
on its output, as text -- and each call runs its macro's entry in it. A package
the back end cannot lower has its macros run by the CEK machine, as
they all once were: the same answers, a good deal more slowly.
`MEADOW_MACRO_TIMES=1` says, for every call, which ran it and how long it took.

### Where a token was written

Every token a macro is given carries a **`Loc`**: `At start end`, the bytes of
the calling file it was written at, or `Nowhere` for one a macro made up. What
comes back is placed by it. A token the macro passes through — the caller's
expression, a clause of a pass — stays where the caller wrote it, so an error in
it is reported on the caller's own line; a token written `Nowhere`, and all of
`Code`, stands where the call is.

A macro that cannot use what it was given says where: `failAt "expected a name"
t` answers `[Fail … (spanOf t)]`, and the compiler reports the message at `t`.
`fail` reports at the call.

A binding's `Code` loses its places when it is stored: it was written in some
other file, and is spliced wherever a later macro puts it.

### `quote!`

Writing tokens out constructor by constructor buries what they say. A quote is
the tokens themselves, with `$name` — or `$(an expression)` — where something
goes in, and `$$` for a `$`:

```meadow
use Std.Macro (TokenTree, Delim, Loc, ToTokens, quoted)

@macro
@pub fun twice (ts : [TokenTree]) : [TokenTree] = quote! { ($ts, $ts) }
```

It is expanded where it is written, in the macro's package, into an expression
building the tokens: what it writes stands `Nowhere`, and a splice is whatever
`toTokens` makes of it — tokens as they are, still where the caller wrote them;
an `Int` or `String` as a literal; a `Datum` as the source that would read it
back. It names `quoted`, `toTokens`, `TokenTree`, `Delim` and `Loc`, which is
the `use` above.

A macro's argument is handed over as tokens and nothing else, so its type has
to say so when inference would leave it open: `toTokens ts` alone makes `ts`
anything with `ToTokens`, and a macro whose argument needs a trait is refused
with a message saying to write `(ts : [TokenTree])`.

### Reading an argument: `Std.Macro.Parse`

A macro with a grammar to read uses `Std.String.Parse`'s combinators on it.
`Std.Macro.Parse` makes the argument a stream of its own, `Tokens` — laid flat,
every bracket a `Punct` in its own place — and `parseTokens` runs a parser over
all of it, answering a failure as a `Fail` at the token the parser stopped on,
however deep in brackets:

```meadow
use Std.Macro.Parse (expandWith, ident, punct, group)
use Std.String.Parse as P

-- `pair!(a (b, c))`: fails at `c` if the comma is missing
@macro
@pub fun pair (ts : [TokenTree]) : [TokenTree] =
  expandWith (P.bind ident (\a -> P.map (\b -> …) (group Paren (P.thenSkip ident (punct ","))))) ts
```

`ident`, `keyword`, `punct`, `stringLit`, `number` and `group` read tokens;
`treesUntil` takes a run as the trees it was, groups rebuilt and every token in
its place — what a macro hands back untouched.

### `@derive`

`@derive(Show)` runs a macro over a declaration and puts what it wrote beside
it. The declaration is passed **as it was written**, from the source rather than
from the tree, so a derive sees the attributes on the variants:

```meadow
@derive(Lexer)
data Token
  = @token("+") Plus
  | @regex("[0-9]+") Number
```

Nothing in the language reads `@token` or `@regex`; they are there for whatever
derives over the declaration. A macro is a function and functions are
lower-case, so `@derive(Lexer)` finds the macro `lexer!`.

**Deriving a trait** is as in Rust: it takes a trait and a macro that writes
the `impl` of it, and the macro is named for the trait. `@derive(Debug)` needs
both `Debug` and a derive macro called `Debug` (or `debug`), and a derive with
no macro of its name is an error however many defaults the trait has. The
compiler has the derive macros for `Debug`, `Display`, `PartialEq`, `Eq`,
`PartialOrd`, `Ord`, `Std.String.Parse`'s `VisualStream` and `Std.Macro`'s
`Reflect` built in -- the standard library derives
them for its own types, which are compiled before any package that could
define a macro -- and a procedural macro of the same name is found first.

## 10. Macros that talk to each other

A token tree in and a token tree out is enough for `vec!` and `assertEq!`. It is
not enough for an **embedded language**, which is the interesting case:

```meadow
defineLanguage! {
  L0 = e : Var x | Lam x e | App e e
}

definePass! {
  removeLam : L0 -> L1
  | Lam x e -> ...
}
```

`definePass!` cannot be written against token trees alone, because it has to
_know what `L0` is_: which productions exist, so it can check the pass covers
them, generate the traversal for the cases the author did not write, and reject
output that is not an `L1`. `defineLanguage!` knows that, and by the time
`definePass!` runs, it is gone.

This is the thing Lisp has that a token-tree macro system does not, and it is
not really quotation — Meadow's `$` already quotes. It is that a macro can leave
something behind for a later macro to find. Racket calls it a compile-time
binding; nanopass is built on exactly this. Meadow has it.

### What is left behind: a `Datum`

A compile-time binding is a name that stands for a **`Datum`**, `Std.Macro`'s
one shared format:

```meadow
data Datum
  = Sym String | Str String | Int Int | Float Float | Bool Bool
  | List [Datum]
  | Rec [(String, Datum)]        -- named fields, in order
  | Tag String [Datum]           -- a constructor and what it is applied to
  | Code [TokenTree]             -- source, carried as tokens
```

It is data with a fixed shape rather than a value of whatever type the defining
macro had in mind, and that is the point. A binding is stored in the
`CompiledPackage` of the package that defined it and read by macros of other
packages, compiled against other versions of anything the definer declared;
it is cached on; and it holds no functions, which could be neither stored nor
compared. A `Datum` means the same thing to all of them.

A macro works with its own types on top of that through **`Reflect`**:

```meadow
trait Reflect a {
  fun toDatum : a -> Datum
  fun fromDatum : Datum -> Result String a
}
```

`Std.Macro` has it for `Int`, `Float`, `Bool`, `String`, `Datum`, `[a]` and
`Maybe a`, and **`@derive(Reflect)`** writes it for a `data` or `record`: a
constructor is a `Tag` of its name and its fields — named fields as one `Rec` —
and a record is a `Rec`. Reading one back says what was wrong (`no field `y``)
rather than only that something was. `use Std.Macro (Reflect)` has to be in
scope where it is derived, as any trait does, and naming the trait brings its
methods with it. `Datum` need not be: what a built-in derive writes means the
standard library's `Datum`, and its lower-case helpers (`get`, `map` and the
like) mean the prelude's, whatever the module has in scope under those names. A
package that declares or `use`s its own `get` still derives.

### How a macro reads and writes one: the `Expand` effect

```meadow
effect Expand {
  lookup : String -> Maybe Datum,
  define : (String, Datum) -> (),
}
```

A procedural macro performs it like any other effect; its type becomes
`[TokenTree] -> [TokenTree] ! { Expand | e }`, which the sandbox allows.
`lookupAs : Reflect a => String -> Result String a` and
`defineAs : Reflect a => String -> a -> ()` are the typed versions.

```meadow
@macro
@pub fun remember ts =
  match (V.get ts 0, V.get ts 1) with
  | (Just (Word n), Just (Num v)) -> let u = define (n, Datum.Int v) in []
  | _ -> [Fail "remember!(name number)"]

@macro
@pub fun recall ts =
  match V.get ts 0 with
  | Just (Word n) ->
      (match lookup n with
       | Just (Datum.Int v) -> [Num v]
       | _ -> [Fail "nothing is called `${n}`"])
  | _ -> [Fail "recall!(name)"]
```

```meadow
use Maker (remember!, recall!)

remember!(answer 42)
fun main () = println recall!(answer)     -- 42
```

The compiler runs every procedural macro under the handler of `Expand`
(`Std.Macro.expanding`). Nothing about it is a side channel: what a macro can
read is a value, with a type, handed to it by a handler.

### Where a name can be seen

A binding is a declaration like any other, in the **macro namespace**:

- `lookup` resolves a name the way the **call's** module would: its own
  bindings, and what it `use`s — `use M` brings all of `M`'s it may see,
  `use M (name!)` one of them, `use M as A` puts them behind `A.`.
- A binding a call defines belongs to the module of the call, and is seen as far
  as the `@pub` written **on the call** says: `@pub defineLanguage! { … }`
  exports what it defines, `@pub(pkg)` keeps it in the package, and nothing
  keeps it in the module — with the same fallback the rest of the surface has,
  that a unit which never says `@pub` exports all of it.
- One module may not define a name twice.

So a language can be defined in one package, and the passes over it written in
the packages that depend on that one.

A binding can also be written by hand, as **`@compileTime def`**:

```meadow
@compileTime def origin = Point { x = 40, y = 2 }
```

Its right-hand side is **read as data, never evaluated**: a name is a `Sym`, a
literal itself, `True`/`False` a `Bool`, a constructor applied to things a
`Tag`, a list, vector or tuple a `List`, a record a `Rec`. A call, an operator
or a lambda would need running, and is refused. The compiler runs no code of the
package it is compiling — that is why a macro lives in a dependency — and
`@compileTime` does not change that.

### Expansion is demand-driven

Once a macro can read what another produced, the order calls are written in
cannot be the order they run in. `recall!(answer)` above could just as well be
written first.

So expansion runs in **rounds**. A call that `lookup`s a name nothing has
defined **yet** is set aside — its run abandoned, and the call left where it is
— and every other call runs. The next round tries it again. Macros are pure, so
running one twice is safe, and the answers are cached, so it is cheap: a set-aside
call costs one run per round it waits.

Rounds go on while they define something. When one defines nothing and calls
are still waiting, what they wait for is not coming, and the next round is
**settled**: there, a lookup of an unknown name is answered `None`, so every
call finishes. `None` therefore always means _there is no such thing_, never
_not written yet_. Two macros waiting on each other both see `None` and report,
in their own words, what they were missing.

What a `use` gets wrong is said once, after the last round: `use M (name!)`
naming a binding a later round defines is not an error.

### Caching

A macro's answer depends on its argument **and on the bindings it read**. The
handler records every name a run looks up, so an answer is cached on the tokens
it was given and reused only while each of those names still stands for what it
did. A package's fingerprint already covers its dependencies', so a binding
changing upstream rebuilds whoever read it.

The answers outlive the build. Each package keeps what its calls answered in
`target/<profile>/incremental/<name>-<options>.macros`, under a fingerprint of
the standard library and of every package it depends on -- which is to say, of
every macro it could have called. A later build after an edit that touched no
macro reads them back, and a call it asks again is answered without anything
running; so is a call set aside to wait, which is kept as a wait for the name it
wanted. An edit to a macro, or to anything under it, is a different fingerprint,
and every call runs again.

An embedded language also wants to read _structured_ syntax rather than count
brackets. `Std.Macro.Parse` (see [section 9](#reading-an-argument-stdmacroparse))
reads a macro's own grammar; what is not built yet is the parser's own entry
points — the ones [section 5](#5-how-expansion-runs) added — as library
functions, so that a proc macro could ask for a run of tokens as a Meadow
expression or pattern and work on the tree.

## 11. What is left out

**Macros in type position.** `a -> Foo!{ … }` cannot be told from the effect row
`a -> Foo ! { Console | e }`. Allowing only `Foo!( … )` and `Foo![ … ]` would
work, but a rule of "macros in types, except with braces" is worse than not
having them yet.

**Macros that define macros**, beyond what falls out of a template containing a
`macro` declaration.

**`macro_rules` hygiene for items.** Matching Rust here is a deliberate limit,
not an oversight: full referential transparency is
[sets of scopes](https://users.cs.utah.edu/plt/publications/popl16-f.pdf), and it
is a much larger commitment than the rest of this document.

## 12. The order this is built in

Each step is useful on its own and none commits to the next.

1. **Done.** **Token trees**, a `$` token, and parser entry points for an
   expression, a pattern and a declaration list. No user-visible change: the
   test is that every `Std` module still parses to the same AST.
   ([`tt.rs`](../compiler/meadow-lexer/src/tt.rs))
2. **Done.** **Unexpanded call nodes and the expansion loop**, with built-in
   macros only — `line!`, `file!`, `stringify!`, `concat!`. This exercises
   expansion, re-parsing and spans without a matcher.
   ([`expand.rs`](../compiler/meadow-compiler/src/expand.rs))
3. **Done.** **`macro` declarations**: the matcher, `tt` / `ident` / `lit`,
   repetition, local hygiene, use within one module.
   ([`rules.rs`](../compiler/meadow-compiler/src/expand/rules.rs),
   [`hygiene.rs`](../compiler/meadow-compiler/src/expand/hygiene.rs))
4. **Done.** **`expr` / `pat` / `item` fragments**, with the follow rules
   above, checked where the macro is written.
   ([`rules.rs`](../compiler/meadow-compiler/src/expand/rules.rs))
5. **Done.** **Export**: the macro namespace, visibility, storage in
   `CompiledPackage`, `$pkg`.
   ([`expand/mod.rs`](../compiler/meadow-compiler/src/expand/mod.rs))
6. **Done.** **Diagnostics**: an error in what a macro wrote is reported at the
   call, labelled with the macro that wrote it.
   ([`expand/mod.rs`](../compiler/meadow-compiler/src/expand/mod.rs))
7. **Done.** **Procedural macros**: `@macro`, `Std.Macro`, running on the VM
   under an effect bound and a fuel budget, cached on input, and `@derive`.
   ([`expand/proc.rs`](../compiler/meadow-compiler/src/expand/proc.rs),
   [`proc.rs`](../buildtools/meadow/src/proc.rs))
8. **Done.** **Compile-time bindings**
   ([section 10](#10-macros-that-talk-to-each-other)): `Datum`, `Reflect` and
   `@derive(Reflect)`, the `Expand` effect with `lookup` and `define`,
   `@compileTime def`, bindings exported with their package, and expansion in
   rounds. This is what an embedded language needs, and it came last because it
   rests on every step before it.
   ([`datum.rs`](../compiler/meadow-compiler/src/expand/datum.rs),
   [`Macro.mw`](../lib/Std/src/Macro.mw))
9. **Done.** **Tokens with places**: a `Loc` on every token, kept through what a
   macro passes back and reported at by `failAt`; `quote!`, with `$` splices
   through `ToTokens`; and `Std.Macro.Parse`, a token stream for
   `Std.String.Parse` whose failures land on the token they stopped at.
   ([`quote.rs`](../compiler/meadow-compiler/src/expand/quote.rs),
   [`Macro/Parse.mw`](../lib/Std/src/Macro/Parse.mw))
