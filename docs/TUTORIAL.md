# The Meadow Tutorial

Meadow is a small, strictly-evaluated, statically-typed functional language. Types
are inferred — you never have to write one — and its two more unusual features are
**algebraic effects** (a `handle` block can intercept I/O, state, randomness or the
clock) and **row-polymorphic records**.

This tutorial walks through the language from the ground up. Every example in it
is a complete program that compiles and runs against the version of Meadow in this
repository.

**Contents**

1. [Getting started](#1-getting-started)
2. [Values, types and operators](#2-values-types-and-operators)
3. [Functions](#3-functions)
4. [Bindings and scope](#4-bindings-and-scope)
5. [Pattern matching](#5-pattern-matching)
6. [Your own types](#6-your-own-types)
7. [Sequences: arrays, vectors and lists](#7-sequences-arrays-vectors-and-lists)
8. [Modules and packages](#8-modules-and-packages)
9. [Effects](#9-effects)
10. [Testing](#10-testing)
11. [Tooling](#11-tooling)
12. [Reference](#12-reference)

---

## 1. Getting started

### Installing

On macOS or Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/meadow-lang/meadow/master/scripts/meadowup-init.sh | sh
```

On Windows, download `meadowup-x86_64.exe` from the
[releases page](https://github.com/meadow-lang/meadow/releases/latest) and run it.

Either way, `meadowup` installs itself and `meadow` into `~/.meadow/bin`, and
`meadowup update` keeps them current. To build from a checkout instead, run
`scripts/install.sh` in it.

### Your first program

Meadow source files end in `.mw`. A file is a list of **declarations** — you cannot
write a bare expression at the top level. The entry point is a function named
`main`:

```meadow
fun main () = println "Hello, Meadow!"
```

Save that as `hello.mw` and run it:

```sh
$ meadow run hello.mw
```

`meadow run` says what it compiles, then calls `main`:

```
   Compiling Hello (/home/you/hello.mw)
    Finished `debug` profile [O1, glade jit] in 0.10s
     Running `main` on the JIT
Hello, Meadow!
```

The lines about compiling go to stderr, so a program's own output is all there
is on stdout: here, the one line `main` printed. To see the type of every
top-level binding as well, pass `--types`:

```sh
$ meadow run --types hello.mw
```

```
=== package Hello ===
  main : () -> () ! Eff
  (7 annotated nodes)
entry: main
Hello, Meadow!
```

`main` takes `()` and returns `()`: it produces nothing useful. Printing is
something it _does_ rather than something it returns, and a function's type
records that too — the `! Eff` says that calling `main` may do what a program
does to the world outside it, writing to the terminal among them. `main` is
always given that type, whatever less its body does.
[Chapter 9](#9-effects) is about how.

Every program's `main` has to fit one type, `() -> () ! Eff`. It must return
`()`, and it may perform at most the effects the runtime itself knows how to
answer — the terminal, the filesystem, the clock and a few more, which the
standard library names `Eff`. `Console` is one of them, so this `main` fits. A
program's output is exactly what it prints, and the examples in this tutorial
print what they want to show. Writing `main` as a value, `def main = ...`, is
an error that says what it should be.

`println` prints a number, a string, a tuple or a vector as a person would
read it: a string without its quotes. To see a value the way it is written in
source, quotes and all, put it in a string as `"${x:?}"`
([below](#strings-and-interpolation)). To print several lines, one after
another, write `let _ = println a in println b` (`let` is in
[chapter 4](#4-bindings-and-scope)).

### The REPL

Running `meadow` with no arguments starts a REPL:

```
> 1 + 2
_ : Int
= 3

> fun square x = x * x
square : Int -> Int

> square 7
_ : Int
= 49
```

An expression is labelled `_`; a declaration is labelled with its own name.

Each entry is compiled as a unit of its own, which is why **defining a name
twice shadows rather than replaces**: what was written before the second
definition goes on using the first, and only new mentions reach the new one.
`:module` lists both and marks the one that has been shadowed.

Useful commands:

|                |                                                            |
| -------------- | ---------------------------------------------------------- |
| `:t <expr>`    | show the type without evaluating                           |
| `:module`      | list what is defined, and what a later definition shadowed |
| `:ir NAME`     | a definition in each IR: core, Cut and AxCut               |
| `:ir cut NAME` | in one of them: `core`, `cut` or `axcut`                   |
| `:reset`       | forget everything defined so far                           |
| `:time`        | time every entry from now on (`:time` again stops)         |
| `:time <expr>` | time just this entry                                       |
| `:tour`        | a guided tour of the language, a step at a time            |
| `:help`        | every command, and the keys                                |
| `:q`           | quit                                                       |

**The tour** is this document's shorter cousin, taken at the prompt rather than
read: `:tour` shows what it covers, `:next` and `:back` move through it, and
`:try` puts a step's example _on the prompt_ so it can be edited before it is
run. It is not a mode — between steps the prompt is the ordinary one, so
anything can be tried, and whatever you define along the way is still there at
the end.

It comes in two parts. **The basics** are the language: expressions,
definitions, data, matching, collections, records, `Maybe`, effects, modules.
**Going further** is optional and waits to be asked for — threads,
parallelism, channels, transactions, mutation that cannot escape, failure that
travels, and writing an effect of your own. `:tour advanced` starts it,
`:tour 14` goes to a step, and the end of the basics stops rather than sliding
into it.

Tab completes names, and knows whether the cursor wants a value, a type or a `use`
path. For multi-line entries, an unfinished line (open bracket, dangling operator,
`| ...` arms) keeps reading; `Alt+Enter` or `Ctrl+J` forces a newline. **End a
multi-line entry with a blank line.**

On macOS, `Ctrl+J` always forces a newline. `Alt+Enter` is Option+Return, and
works once the terminal sends Option as Meta: in Terminal, Settings > Profiles >
Keyboard > "Use Option as Meta key"; in iTerm2, Profiles > Keys > Left Option:
Esc+; in VS Code's terminal, the setting `terminal.integrated.macOptionIsMeta`.
Without that setting Option+Return is an ordinary Return and submits the entry.
A line break forced this way is indented for what comes next: after a whole
`fun f x = e`, a unit in, for the next clause.

`Ctrl+F` finds a declaration. Type part of a name -- `len` finds `length`, and
`Vector.len` only the one in `Std.Collections.Vector` -- or a type, and it
searches by type instead, the way Hoogle does: `[a] -> Int` finds `length`,
`Maybe a -> a -> a` finds `unwrapOr` though it takes its arguments the other way
round, and a leading `:` asks outright, so `: String` finds what makes one. The
preview shows the signature, the module, the file and the doc comment. `Enter`
puts the name on the prompt; if it is not in scope, the `use` that brings it in
is run first, and printed so that you can see it was.

An editor with the language server has the same search as its own picker for
workspace symbols -- `Ctrl+T` in VS Code, `Space S` in Helix -- over the
library and everything in the packages you have open.

### Comments

Only line comments, introduced by `--`. There is no block comment syntax.

```meadow
-- This is a comment.
fun main () = ()  -- so is this
```

---

## 2. Values, types and operators

### The primitive types

| Type                               | Literals                                           | Notes                                                                                                     |
| ---------------------------------- | -------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `Int`                              | `42`, `-7`, `0xff`, `0o17`, `0b1011`               | 64-bit, wrapping; what an integer literal is by default; also spelled `Int64`                             |
| `BigInt`                           | _(same literals)_                                  | arbitrary precision, never overflows; ask for it with `toBigInt` or an annotation                         |
| `Int8` `Int16` `Int32`             | _(same literals)_                                  | signed, wrapping at their width                                                                           |
| `UInt8` `UInt16` `UInt32` `UInt64` | _(same literals)_                                  | unsigned, wrapping at their width                                                                         |
| `Float`                            | `3.14`, `42.0`                                     | 64-bit; also spelled `Float64`                                                                            |
| `Float32`                          | _(same literals)_                                  | 32-bit                                                                                                    |
| `Bool`                             | `True`, `False`                                    | constructors, capitalised                                                                                 |
| `String`                           | `"hi"`, `"tab\there"`, `"hi ${name}"`, `r"C:\dir"` | a sequence of **bytes**; `${…}` interpolates; escapes and raw strings [below](#strings-and-interpolation) |
| `Char`                             | `'a'`, `'é'`, `'\n'`                               | one Unicode **scalar**, not one byte                                                                      |
| unit                               | `()`                                               | one value, written the same way as its type                                                               |

`Bool` values are written, and print, as the constructors `True` and `False`.

### One set of operators for every integer type

`+ - * / % ^` and `< > <= >=` work on **every** integer type. Both sides must
have the same type, and a literal takes whichever type its context needs. When
nothing settles it, an integer is an `Int`: a 64-bit machine word, which wraps
past 2^63. Where a result has to be exact beyond that, say `BigInt`:

```meadow
def big   = toBigInt 2 ^ 100
def small = 2 ^ 62
def byte  = toUInt8 250 + 6

fun main () = println (big, small, byte)
```

```
(1267650600228229401496703205376, 4611686018427387904, 0)
```

Every fixed-width type wraps at its width, as `byte` shows and as `2 ^ 64`
would. Mixing two of them is a type error rather than a silent conversion:
`toInt 1 + toInt32 1` does not compile. Say which one you mean with a
conversion (below).

The floats, `Float` and `Float32`, have the same operators: `+ - * /` and
`< > <= >=`. A float literal nothing settles is a `Float`, and an integer
literal is never a float -- write `2.0`, not `2`, beside one.

A function written with the operators works on any integer type. Its type says
so with a variable named `n`, which stands for "some integer type":

```meadow
fun square x = x * x

fun main () = println (square 12, square (toUInt8 20))
```

```
(144, 144)
```

`square : forall n. Mul n => n -> n`, and `20 * 20` wraps to `144` as a
`UInt8`. `Mul` is the trait `*` is the method of (see
[operators](#operators-are-methods)): every integer type implements it, and so
may a type of yours. Only `fun` is generic in this way: a `def` or a `let` has
one number type, settled by how it is used, or `Int` if nothing uses it at a
particular one.

A `BigInt` costs more than a machine word — it lives on the heap — which is why
it is not the default. Where a computation must not wrap, pin it with an
annotation — `fun fact (n : BigInt) = ...` — and every literal it meets
follows.

Equality and ordering are four traits, as in Rust: `PartialEq` (`==`, `!=`),
`Eq`, `PartialOrd` (`<`, `>`, `<=`, `>=`, `partialCmp`) and `Ord` (`compare`).
Every primitive type is all four except the floats, which are only
`PartialEq` and `PartialOrd`: a `NaN` equals nothing, itself included, and is
ordered against nothing. Tuples, vectors, lists and the rest of the standard
library's types compare field by field, and a type of your own derives them:

```meadow
use Suit.*
@derive(Debug, PartialEq, Eq, PartialOrd, Ord)
data Suit = Clubs | Diamonds | Hearts | Spades

def nan = 0.0 / 0.0

fun main () =
  let _ = println ([1, 2] == [1, 2], Just 1 != None, "b" > "a") in
  println (Clubs < Spades, compare Hearts Clubs, nan == nan)
```

```
(True, True, True)
(True, Greater, False)
```

A derived ordering goes by constructor, in the order they are declared, then by
field, left to right. A type that derives none of them has no `==` at all --
the checker says it does not implement `PartialEq` -- and deriving `Eq` or
`Ord` over a field that is a float is refused, as in Rust.

Integer division truncates, and `%` follows the sign of the _dividend_ — so
`(-7) % 3` is `-1`, not `2`. That trips people up when writing `even`/`odd`-style
tests by hand.

### Converting between them

`toInt` (or `toInt64`), `toInt8`, `toInt16`, `toInt32`, `toUInt8` … `toUInt64` and
`toBigInt` take any integer. Narrowing keeps the low bits, so `toInt8 200` is
`-56`. `toFloat` turns an integer into a `Float`; `toFloat32` and `toFloat64`
convert between the two float types; `floor` goes from a float to an `Int`.

```meadow
fun main () =
  let _ = println (toFloat 3, floor 3.9, toBigInt 5) in
  println (toInt8 200, toFloat32 (toFloat 3))
```

```
(3.0, 3, 5)
(-56, 3.0)
```

### Strings and interpolation

A `${…}` inside a string literal is an expression, and the string holds what it
renders to, as in Rust's `format!`: `${x}` by its `Display` -- for people, a
string without its quotes -- and `${x:?}` by its `Debug` -- for programmers,
the way it would be written in source. A hole can hold anything an expression
can, including strings with holes of their own. `\$` is a dollar sign that does
not start a hole, and a `$` not followed by `{` needs no escape.

```meadow
fun describe (name : String) (items : [Int]) =
  "${name} has ${len items} items, the first is ${getOr 0 items 0}"

fun main () =
  let _ = println (describe "cart" [3, 1, 4]) in
  let _ = println "maybe: ${Just 2}, quoted: ${"hi":?}, a char: ${'c'} or ${'c':?}" in
  println "a price: \$${9}, and \${literal} braces"
```

```
cart has 3 items, the first is 3
maybe: Just(2), quoted: "hi", a char: c or 'c'
a price: $9, and ${literal} braces
```

Every primitive type is `Display` and `Debug`, and so are tuples and the
standard library's types. A type of your own is either with `@derive(Display)`
and `@derive(Debug)`, which render a constructor and its fields as
`Just(2)` -- or with an `impl` that says how:

```meadow
@derive(Debug, Display)
data Shape = Circle Float | Rect { w : Int, h : Int }

use Suit.*
data Suit = Hearts | Spades

impl Display Suit {
  fun display s = match s with | Hearts -> "♥" | Spades -> "♠"
}

fun main () = println "${Shape.Rect { w = 2, h = 3 }} ${Shape.Circle 1.5:?} ${Suit.Spades}"
```

```
Rect(2, 3) Circle(1.5) ♠
```

A type that is neither cannot go in a hole or to `println`: the checker says
it does not implement `Display`. `show` still renders anything, the way the
REPL does, for when that is all you want. Formatting a value some other way is
a function call away: with `use Std.Time as T`, `"took ${T.formatNanos ns}"`.

The escapes are the usual ones, in strings and character literals alike:

| escape                   | means                                                                             |
| ------------------------ | --------------------------------------------------------------------------------- |
| `\n` `\r` `\t` `\0`      | newline, carriage return, tab, NUL                                                |
| `\\` `\"` `\'` `\$`      | the character itself                                                              |
| `\a` `\b` `\f` `\v` `\e` | bell, backspace, form feed, vertical tab, escape (as terminal colour codes start) |
| `\x41`                   | an ASCII character by its two hex digits, up to `\x7F`                            |
| `\u{1F600}`              | any Unicode character, by one to six hex digits                                   |
| `\` at the end of a line | joins the next line on, without the line break or its leading spaces              |

Anything else after a backslash is an error, not a backslash. For text full of
backslashes or quotes, a **raw string** takes everything between its quotes as it
is: no escapes and no `${…}`. `r"…"` cannot contain a `"`; `r#"…"#` can, and ends
at `"#`; add `#`s until the text does not contain the closing sequence.

```meadow
fun main () =
  let _ = println r"C:\Users\${name}" in
  let _ = println r#"she said "hi""# in
  println "${"caf\u{e9} \x41\tB":?}"
```

```
C:\Users\${name}
she said "hi"
"café A\tB"
```

The last line is printed with `:?`, which writes the tab back as `\t` so that
it can be seen.

### Booleans and bit twiddling

`and` and `or` short-circuit, and `not` is an ordinary function. The bit
operators `<<`, `>>` and `>>>`, and the primitives `bitAnd`, `bitOr`, `bitXor`,
`bitNot`, `popCount` and `bitWidth`, work on every integer type at that type's
width: `>>` is arithmetic on a signed type and logical on an unsigned one, and
`>>>` is always logical. The amount shifted by is an `Int`. `Std.Num.Bits` adds
helpers on top.

```meadow
fun main () = println (1 < 2 and 3 < 4, not True or True, 1 << 4, bitAnd 12 10)
```

```
(True, True, 16, 8)
```

### Tuples

```meadow
def point = (1, "north", True)

fun main () = println "${(fst (1, 2), snd (1, 2), point):?}"
```

```
(1, 2, (1, "north", True))
```

`fst` and `snd` only work on pairs. For anything wider, use pattern matching.

---

## 3. Functions

### Defining them

`fun` defines a function; `def` binds a value. Neither needs a type written on
it — the types you saw above were inferred — though you can write one; see
[Signatures](#signatures).

The two are not interchangeable: **`def` takes no parameters.** It binds a
_pattern_ to the value of an expression, so `def square x = x * x` is a parse
error. Write `fun square x = x * x`, or `def square = \x -> x * x`.

```meadow
fun double n = n * 2

fun add a b = a + b

def four = double 2

fun main () = println (double 21, add 1 2, four)
```

```
(42, 3, 4)
```

Application is by juxtaposition — `add 1 2`, not `add(1, 2)` — so parentheses are
only for grouping. `double 2 + 1` means `(double 2) + 1`.

A top-level `def` is evaluated **once**, the first time something uses it, and
the value is kept. `def table = buildTable 1000000` builds its table once however
many functions read it, and a `def` nothing uses is never evaluated.

That is only safe if evaluating a `def` does nothing a program could see, so a
top-level `def` may not perform effects. `def greeting = let _ = println "hi" in
"hi"` is an error: "a top-level `def` cannot perform effects". A `def` can still
_be_ a function with effects, such as `def shout = \s -> println s`: building
the closure does nothing, and the printing happens at each call. Effects belong
inside functions, which run every time they are called — `main` among them,
which the runtime calls once as the program.

### Currying and partial application

Every function of several arguments is really a chain of one-argument functions, so
you can apply one at a time:

```meadow
fun add a b = a + b

def increment = add 1

fun main () = println (increment 41, map (add 10) [1, 2, 3])
```

```
(42, [11, 12, 13])
```

### Lambdas

`\x -> body`, with multiple parameters allowed:

```meadow
fun main () = println (
  (\x -> x * 2) 21,
  (\x y -> x + y) 1 2,
  filter (\n -> n % 2 == 0) [1, 2, 3, 4]
)
```

```
(42, 3, [2, 4])
```

### Pipelines

`|>` sends a value into a function and `<|` does the reverse. Both make long
chains read left-to-right instead of inside-out.

```meadow
fun main () =
  [1, 2, 3, 4, 5]
  |> filter (\n -> n % 2 == 1)
  |> map (\n -> n * n)
  |> sum
  |> println
```

```
35
```

`|>` binds looser than every arithmetic operator, so `x |> f |> g` is `g (f x)` and
`1 + 2 |> double` is `double (1 + 2)`.

### Composing

`compose f g` is "`g` then `f`", and `flip` swaps the first two arguments:

```meadow
def addThenDouble = compose (\n -> n * 2) (\n -> n + 1)

fun main () = println (addThenDouble 5, flip (\a b -> a - b) 3 10, id 7, const 1 "ignored")
```

```
(12, 7, 7, 1)
```

### Order does not matter

Top-level declarations may appear in any order. The compiler sorts them by
dependency before type-checking and evaluating, and handles mutual recursion:

```meadow
fun main () = println (isEven 10)

fun isEven n = if n == 0 then True else isOdd (n - 1)

fun isOdd n = if n == 0 then False else isEven (n - 1)
```

```
True
```

### Signatures

Types are inferred, but you can write them down, in two places. Inline, a
parameter pattern takes `(p : T)`, and the result type goes before the `=`:

```meadow
fun area (w : Int) (h : Int) : Int = w * h

fun main () = println (area 3 4)
```

```
12
```

Or on a line of its own, as a _signature_: `fun name : T`, or `def name : T` for
a value. A signature and the definition under it are **one declaration**: the
clauses that define the name follow the signature, each opening with a `|` and
naming the binding again. Writing `fun name` a second time to define it is an
error -- Meadow has one shape for a definition, and this is it.

```meadow
fun swap : (a, b) -> (b, a)
  | swap (x, y) = (y, x)

def small : Int8
  | small = 5

fun main () = println "${(swap (1, "one"), small):?}"
```

```
(("one", 1), 5)
```

A signature is held to in both directions. The definition has to have the type
it gives -- a body that returns a `String` where the signature says `Int` is a
type mismatch -- and it has to be _as general_: a lowercase name in a signature
is a type variable, a promise that the function works whatever type it stands
for.

```
fun same : a -> a
  | same x = x + 1
    -- this definition needs a number where its signature `a -> a` has a type
    -- variable: it is `n -> n`
```

A function's effects are part of its type, written after `!` the way types print
(see [Reading the types](#reading-the-types)). An arrow without a `!` is pure,
so a signature also says what a function is allowed to do:

```meadow
fun greet : String -> () ! Console
  | greet name = println ("hello, " ++ name)

fun apply : (a -> b ! e) -> a -> b ! e
  | apply f x = f x

fun main () = apply greet "Ann"
```

```
hello, Ann
```

Inline, the effect goes after the result, since that is where the last arrow
would be:

```meadow
fun greet (name : String) : () ! Console = println ("hello, " ++ name)

fun apply (f : a -> b ! e) (x : a) : b ! e = f x

fun main () = apply greet "Ann"
```

```
hello, Ann
```

A result is read the way an arrow is: written without a `!`, it says the body
performs nothing, so `fun inc (x : Int) : Int = …` is pure and a `println` in
it is an error. Only a result left unwritten -- `fun inc (x : Int) = …` -- has
its effects inferred, as its constraints always are. What a function performs
is a bound, not everything a place using it allows: a pure `inc` can still be
handed to `V.map` inside a function that prints. A result that is
itself a function takes its own arrow's `!` first, so the body's effect then
goes outside the parentheses: `: (a -> b ! e) ! Console`. A `def` has no body
that runs when it is used, so it takes no `!` at all.

A definition may be written as several clauses, matched in the order they are
written -- so a signature stands over as many of them as it needs:

```meadow
fun gcd : Int -> Int -> Int
  | gcd a 0 = a
  | gcd a b = gcd b (a % b)

fun main () = println (gcd 48 18)
```

```
6
```

A signature tells the checker a parameter's type before it reads the body, which
is what lets `p.x` select from a nominal record (see [`record`](#record--named-fields)).
`@pub` and `@test` go in front of the whole declaration, which is the signature
line when there is one.

---

## 4. Bindings and scope

### `let ... in`

`let` binds a name for the rest of an expression. It is an expression itself, so it
has a value and can be chained:

```meadow
fun hypotenuseSquared a b =
  let aa = a * a in
  let bb = b * b in
  aa + bb

fun main () = println (hypotenuseSquared 3 4)
```

```
25
```

### `let rec`

A plain `let` cannot refer to itself. For a local recursive helper, use `let rec`:

```meadow
fun countdown n =
  let rec go i acc =
    if i == 0 then acc else go (i - 1) (acc + i)
  in go n 0

fun main () = println (countdown 10)
```

```
55
```

### `if ... then ... else`

`if` is an expression, both branches must have the same type, and **the `else` is
mandatory** — there is no value for a missing branch to produce.

```meadow
fun classify n =
  if n < 0 then "negative"
  else if n == 0 then "zero"
  else "positive"

fun main () = println "${(classify (0 - 3), classify 0, classify 3):?}"
```

```
("negative", "zero", "positive")
```

### Shadowing

A `let` may reuse a name that is already in scope, and its right-hand side sees
the _old_ binding:

```meadow
fun normalise x =
  let x = abs x in
  let x = min x 100 in
  x

fun main () = println (normalise (0 - 250), let a = 1 in let a = a + 1 in a)
```

```
(100, 2)
```

A binding **with parameters** is the exception, and deliberately so: it is
recursive, so its own name refers to itself rather than to anything outer. That is
what makes a local helper work.

```meadow
fun sumTo n =
  let go i acc = if i == 0 then acc else go (i - 1) (acc + i) in
  go n 0

fun main () = println (sumTo 10)
```

```
55
```

`let rec` is accepted as a spelling of the same thing, for emphasis.

---

## 5. Pattern matching

`match` scrutinises a value against a series of patterns, first match wins.

```meadow
fun describe n =
  match n with
  | 0 -> "zero"
  | 1 -> "one"
  | _ -> "many"

fun main () = println "${(describe 0, describe 1, describe 9):?}"
```

```
("zero", "one", "many")
```

### What you can match on

Literals, `_`, variables, tuples, constructors, records, and the sequence forms:

```meadow
data Shape = Circle Int | Rect Int Int

use Shape.*

fun area s =
  match s with
  | Circle r -> 3 * r * r
  | Rect w h -> w * h

fun originDistance p =
  match p with
  | (0, 0) -> "at the origin"
  | (0, _) -> "on the y axis"
  | (_, 0) -> "on the x axis"
  | (_, _) -> "somewhere else"

fun main () = println "${(area (Rect 3 4), area (Circle 2), originDistance (0, 5)):?}"
```

```
(12, 12, "on the y axis")
```

### Patterns in parameters

A function parameter is itself a pattern, so you can destructure on the way in.
Parameter patterns must be **irrefutable** — they must always match — so tuples and
records are fine, but constructors of a multi-case type are not.

```meadow
fun addPair (a, b) = a + b

fun main () = println (addPair (1, 2))
```

```
3
```

### Exhaustiveness

By default (`--debug`) a `match` may be incomplete, so half-written code still
runs. `--release` requires every case to be covered and tells you which is missing:

```sh
$ meadow run --release incomplete.mw
incomplete: non-exhaustive patterns: `G` is not matched
```

### Guards

An arm can add a condition after its pattern: `| p if condition -> body`. The
arm is taken only when the pattern matches **and** the condition is `True`;
otherwise matching goes on with the next arm, as if the pattern had not matched.
The condition sees everything the pattern binds.

```meadow
data Shape = Circle Int | Rect Int Int

use Shape.*

fun classify n =
  match n with
  | x if x < 0 -> "negative"
  | 0 -> "zero"
  | x if x > 100 -> "big"
  | _ -> "positive"

fun area s =
  match s with
  | Circle r if r > 10 -> 999
  | Circle r -> 3 * r * r
  | Rect w h if w == h -> w * w
  | Rect w h -> w * h

fun main () =
  let _ = println "${(classify (0 - 5), classify 500):?}" in
  println (area (Circle 20), area (Rect 3 3), area (Rect 2 5))
```

```
("negative", "big")
(999, 9, 10)
```

A guard is an ordinary expression: it can call functions and perform effects,
and it runs only for an arm whose pattern matched. Because whether an arm is
taken is no longer a question about its pattern alone, **a guarded arm covers
nothing** for the exhaustiveness check: something after it has to handle the
values its guard turns away.

### `as`-patterns

`p as x` matches `p` and also names the whole of what it matched `x`. It binds
loosest of all, so `x :: rest as whole` names the whole list; parenthesize to
name a part.

```meadow
fun dup xs =
  match xs with
  | x :: rest as whole -> (x, whole, rest)
  | [;] as whole -> (0, whole, whole)

fun pairUp p =
  match p with
  | ((a, b) as inner, c) if a + b == c -> Just inner
  | _ -> None

fun main () = println (dup [1; 2; 3], pairUp ((1, 2), 3))
```

```
((1, [1; 2; 3], [2; 3]), Just((1, 2)))
```

It saves rebuilding a value only to return it, which also saves the allocation:
`| Just _ as found -> found` hands back the very value that was matched.

### Constructor arguments

A constructor's arguments are written after it, and each is an atom: a variable,
`_`, a literal, a bracketed or parenthesized pattern, or a constructor **with no
arguments of its own**. So `Node Leaf x r` is `Node` applied to three patterns,
and a constructor that takes arguments needs parentheses: `Just (Cons x rest)`.

### View patterns

`(f -> p)` applies `f` to what is being matched there and matches its answer
against `p`. The function is any expression, and it sees the names the pattern
bound to its left. In a tuple, a component can be a view without parentheses of
its own: `(k, lookup k -> Just v)`.

```meadow
fun half n = if n % 2 == 0 then Just (n / 2) else None

fun lookup k xs =
  match xs with
  | [;] -> None
  | (j, v) :: rest -> if j == k then Just v else lookup k rest

fun describe n =
  match n with
  | 0 -> "zero"
  | (half -> Just h) if h > 10 -> "big and even"
  | (half -> Just _) -> "even"
  | _ -> "odd"

fun named p =
  match p with
  | (k, lookup k -> Just name) -> name
  | _ -> "?"

fun main () = println "${(describe 30, describe 4, describe 5, named (2, [(1, "one"); (2, "two")])):?}"
```

```
("big and even", "even", "odd", "two")
```

A view that does not match goes on to the next arm, as a failed guard does,
and for the same reason **covers nothing** for the exhaustiveness check -- unless
its own pattern matches everything, `(length -> n)`, when it is as good as `_`.
It is applied once, however much of its answer the pattern takes apart, and it
can be a parameter, when its pattern cannot fail:

```meadow
fun swap (a, b) = (b, a)

fun second (swap -> (b, _)) = b

fun main () = println (second (1, 2))
```

```
2
```

What a view in a parameter does -- an effect it performs -- is done when the
function is called, so it is on the function's type.

### Pattern synonyms

`pattern` names a pattern, to match with -- and, where it can, to build with --
as if it were a constructor:

```meadow
pattern Pair a b = (a, b)

pattern Head x <- x :: _

fun swap p =
  match p with
  | Pair a b -> Pair b a

fun first xs =
  match xs with
  | Head x -> x
  | _ -> 0

fun main () = println (swap (Pair 1 2), first [3; 4], first [;])
```

```
((2, 1), 3, 0)
```

`pattern P x y = p` both matches and builds: `p` is read as the expression that
makes what it matches. `pattern P x y <- p` only matches, and a `where` after it
says how `P` builds, which is how a synonym presents a type as another it is not
-- a view in its pattern does the seeing:

```meadow
data Nat = Nat Int

use Nat.*

fun pred (Nat n) = if n > 0 then Just (Nat (n - 1)) else None

pattern Zero <- (pred -> None)
  where Zero = Nat 0

pattern Succ m <- (pred -> Just m)
  where Succ (Nat n) = Nat (n + 1)

fun toInt n =
  match n with
  | Succ m -> 1 + toInt m
  | _ -> 0

fun main () = println (toInt (Succ (Succ (Succ Zero))))
```

```
3
```

The pattern binds each of the synonym's parameters exactly once, and nothing
else. A synonym is exported with `@pub`, and `use M (P)` brings it in.

A synonym whose pattern is made of constructors, tuples and the like counts for
exhaustiveness as that pattern does: the `match` in `swap` above needs nothing
more. One whose pattern has a view covers nothing, as the view would -- unless
a declaration says which synonyms cover their type together, as GHC's
`COMPLETE` does. Then a `match` with an arm for each of them needs nothing more:

```meadow
data Nat = Nat Int

use Nat.*

fun pred (Nat n) = if n > 0 then Just (Nat (n - 1)) else None

pattern Zero <- (pred -> None)
  where Zero = Nat 0

pattern Succ m <- (pred -> Just m)
  where Succ (Nat n) = Nat (n + 1)

pattern Zero | Succ

fun toInt n =
  match n with
  | Zero -> 0
  | Succ m -> 1 + toInt m

fun main () = println (toInt (Succ (Succ Zero)))
```

```
2
```

The compiler takes the declaration's word for it: that every `Nat` is a `Zero`
or a `Succ` is what `pred` has to make true.

Nothing is left of a synonym once it is compiled with optimization: a `match`
on `Pair a b` is a `match` on `(a, b)`. It is a function the compiler writes
-- one that answers what the pattern bound, or that it did not match -- and
inlines where it is used, where what it answered is taken apart as it is made.

---

## 6. Your own types

### `data` — sum types

A `data` declaration lists alternatives, each with zero or more fields. Type
parameters are lowercase names after the type's own name.

```meadow
@derive(PartialEq)
data Colour = Red | Green | Blue

data Shrub a = Tip | Fork (Shrub a) a (Shrub a)

use Colour.*
use Shrub.*

fun size t =
  match t with
  | Tip -> 0
  | Fork l x r -> size l + 1 + size r

def sample = Fork (Fork Tip 1 Tip) 2 Tip

fun main () = println (size sample, Red == Red, Red == Blue)
```

```
(2, True, False)
```

**A constructor lives under its type**, as a variant does in Rust, even in the
module that declares it. `Colour.Red` works anywhere `Colour` does. To write
`Red` bare, bring it in: `use Colour.*` for every constructor, or
`use Colour (Red, Blue)` for some. Without that, `Red` alone is an error:
"unknown constructor `Red`". Keeping constructors under their type is what lets
two types in one program both have a `Leaf`.

One kind of constructor comes with its type: one named exactly like the type, as
a `record`'s is (see below) and a one-case `data Parser = Parser ...` is. That is
a Rust struct, and `Parser f` is written wherever `Parser` is in scope.

### The two you get for free

`Maybe` and `Result` come from the standard library and are always in scope:

```meadow
use Std.String as S

fun safeDiv a b = if b == 0 then None else Just (a / b)

fun parseAge s =
  match S.toInt s with
  | None -> Err "not a number"
  | Just n -> if n < 0 then Err "negative" else Ok n

fun main () = println "${(safeDiv 10 2, safeDiv 10 0, parseAge "30", parseAge "x"):?}"
```

```
(Just(5), None, Ok(30), Err("not a number"))
```

There is also `Either a b` in `Std.Either`, for when neither side means
"failure". Its constructors are written `Either.Left` / `Either.Right`, or bare
after `use Std.Either.Either.*`.

Note the `use Std.String as S` there: the prelude's bare `toInt` is the
integer-conversion primitive, not string parsing. When a name feels like it should
exist, check whether the prelude already means something else by it.

### `record` — named fields

There are two kinds of record, and the difference matters more than you would
expect.

An **anonymous** record needs no declaration. Field selection on it is
_row-polymorphic_: `getX` below reads "any record with at least an `x`", so it
accepts records of different shapes.

```meadow
def anon = { x = 1, y = 2, label = "extra" }

fun getX r = r.x

fun main () = println (getX anon, getX { x = 99 }, getX { x = 5, other = True })
```

```
(1, 99, 5)
```

An anonymous record has no `Display` or `Debug`, so `println anon` does not
compile: print its fields.

A **nominal** record is declared with `record`, constructed by name, and selected
from with the same `.`:

```meadow
@derive(Debug)
record Point = { x : Int, y : Int }

def origin = Point { x = 0, y = 0 }

fun main () = println "${(origin, origin.x, origin.y):?}"
```

```
(Point(0, 0), 0, 0)
```

> **The catch.** Selecting a field needs the record's type known by the time the
> `.` is reached. When nothing has said what it is, selection infers the
> _structural_ row type `{ x : Int | a }`, and that does not unify with a nominal
> type:
>
> ```
> fun magnitudeSquared p = p.x * p.x + p.y * p.y
> magnitudeSquared (Point { x = 3, y = 4 })
>   -- type mismatch: `{ x : Int | a }` vs `Point`
> ```
>
> Say what `p` is -- with a [signature](#signatures), or `(p : Point)` -- or
> pattern-match on it:

```meadow
record Point = { x : Int, y : Int }

fun magnitudeSquared : Point -> Int
  | magnitudeSquared p = p.x * p.x + p.y * p.y

fun manhattan p =
  match p with
  | Point { x = a, y = b } -> abs a + abs b

fun main () = println (magnitudeSquared (Point { x = 3, y = 4 }), manhattan (Point { x = 3, y = -4 }))
```

```
(25, 7)
```

In short: use anonymous records when you want lightweight structural data and
generic accessors; use `record` when you want a named type, and give it its
type where you reach into it.

### Updating a record

`{ r | x = v, y = w }` is `r` with the fields named replaced: a new record, with
`r` itself unchanged. Each field named has to be one `r` has, and keeps its type.

```meadow
@derive(Debug)
record Person = { name : String, age : Int }

fun birthday : Person -> Person
  | birthday p = { p | age = p.age + 1 }

def ann = Person { name = "Ann", age = 41 }

fun main () =
  let moved = { { x = 1, y = 2 } | y = 5 } in
  println "${(birthday ann, ann.age, moved.x, moved.y):?}"
```

```
(Person("Ann", 42), 41, 1, 5)
```

A field whose value is a variable of the same name can be written as the name
alone, as in Rust: `{ x, y }` is `{ x = x, y = y }`, a pattern `Point { x, y }`
binds `x` and `y`, and `{ r | x, y = 2 }` is `{ r | x = x, y = 2 }`. The one
place it does not reach is an update of a single field: `{ r | x }` already
means the extension below, so that one is written `{ r | x = x }`.

The record is evaluated first, then the new values in the order they are
written. As with selection, a nominal record's type has to be known where it is
updated, and a `data` type with several constructors has no one record to
update -- match on it. The field-first form, `{ x = v | r }`, is something else:
it _extends_ an anonymous record with a field.

### `type` — another name for a type

```meadow
type Point = (Int, Int)
type Pair a = (a, a)

fun add : Point -> Point -> Point
  | add (a, b) (c, d) = (a + c, b + d)

fun swapPair : Pair a -> Pair a
  | swapPair (x, y) = (y, x)

fun main () = println "${(add (1, 2) (10, 20), swapPair ("l", "r")):?}"
```

```
((11, 22), ("r", "l"))
```

An alias _is_ what it stands for: a `Point` is an `(Int, Int)` everywhere, with
nothing to convert, and types print with it expanded. When you want a type that
is distinct from what it is made of, declare it with `data` or `record`. An
alias is given all of its arguments wherever it is used, and cannot refer to
itself -- a recursive type is a `data`. It is as visible as any other type:
`@pub type`, `@pub(pkg) type`, or private to its module.

### `trait` and `impl` — one name, a meaning per type

A **trait** says what a type has to provide; an **`impl`** provides it for one
type. A method is then an ordinary function, and which `impl` it means is
decided by the type it meets:

```meadow
trait Describe a {
  fun describe : a -> String

  fun shout : a -> String
    | shout x = describe x ++ "!"
}

impl Describe Int {
  fun describe n = "the number " ++ show n
}

impl Describe Bool {
  fun describe b = if b then "yes" else "no"
  fun shout b = "BOOL"
}

impl Describe [a;] where Describe a {
  fun describe xs =
    match xs with
    | [;] -> "nothing"
    | x :: rest -> describe x ++ ", " ++ describe rest
}

fun main () = println "${(describe 7, shout 7, shout True, describe [True; False]):?}"
```

```
("the number 7", "the number 7!", "BOOL", "yes, no, nothing")
```

In a trait, `fun name : T` declares a method. Clauses under it, opening with a
`|` exactly as a [signature](#signatures) takes them anywhere else, give the
method a **default**, which an `impl` that leaves the method out gets. Most
methods have no default and so are a signature and nothing else; where there is
one it belongs to the signature, so naming the method a second time to define
it is an error. An `impl` says only what its methods do, and never repeats a
signature -- the trait has already declared those. An `impl` is for
one type constructor -- `Int`, `[a;]`, `Maybe a`, `(a, b)` -- and its `where`
says what the type's own parameters must implement: a list can be described
when its elements can. There is one `impl` per trait and type in a whole
program, wherever it is written; a second is an error.

A function that uses a method on a type it does not know **asks its caller**
for the `impl`. Nobody has to say so -- it is inferred, and printed in front of
the type, `Describe a => …` -- but a [signature](#signatures) can, and then the
body may use only what the signature asked for:

```meadow
trait Describe a {
  fun describe : a -> String
}

impl Describe Int {
  fun describe n = show n
}

fun pair x y = describe x ++ " and " ++ describe y

fun bracket : Describe a => a -> String
  | bracket x = "[" ++ describe x ++ "]"

fun main () = println "${(pair 1 2, bracket 3):?}"
```

```
("1 and 2", "[3]")
```

`pair`'s type is `(Describe a, Describe b) => a -> b -> String`. A trait can
**require** another -- `trait Ord a <: Eq a, PartialOrd a { … }` -- and then an
`impl Ord T` needs an `impl Eq T` and an `impl PartialOrd T`, and a function
given `Ord a` may use their methods too.

A trait can also leave a **type** for each `impl` to choose. `type Elem f`
declares one, an `impl` says what it is, and `Elem f` can be written wherever a
type can, of any `f` that implements the trait:

```meadow
trait Container f {
  type Elem f
  fun empty : () -> f
  fun insert : Elem f -> f -> f
  fun toList : f -> [Elem f;]
}

record Stack a = { items : [a;] }

impl Container (Stack a) {
  type Elem (Stack a) = a
  fun empty u = Stack { items = [;] }
  fun insert x s = Stack { items = x :: s.items }
  fun toList s = s.items
}

fun fromList : Container f => [Elem f;] -> f
  | fromList xs =
    match xs with
    | [;] -> empty ()
    | x :: rest -> insert x (fromList rest)

fun stack : [a;] -> Stack a
  | stack xs = fromList xs

fun main () = println (toList (insert 0 (stack [1; 2; 3])))
```

```
[0; 1; 2; 3]
```

`fromList` works for every container, and `stack` picks one by its result
type. Something has to: `toList (fromList xs)` names no container at all, and
is refused -- "cannot tell which `impl Container` is meant".

A trait can be **of several types** at once, and an `impl` is then for one
combination of them. The `impl` a call means is chosen by all of them together,
so the result type can do the choosing:

```meadow
trait Convert a b {
  fun convert : a -> b
}

impl Convert Int String {
  fun convert n = "#" ++ show n
}

impl Convert Int Bool {
  fun convert n = n != 0
}

impl Convert [a;] [b;] where Convert a b {
  fun convert xs =
    match xs with
    | [;] -> [;]
    | x :: rest -> convert x :: convert rest
}

fun labels : [Int;] -> [String;]
  | labels xs = convert xs

fun flags : [Int;] -> [Bool;]
  | flags xs = convert xs

fun main () = println "${(labels [1; 2], flags [0; 3]):?}"
```

```
(["#1"; "#2"], [False; True])
```

A constraint names all of them -- `Convert a b =>` -- an associated type is of
all of them (`type Out v s`), and a trait may require others of any of its
parameters: `trait RoundTrip a b <: Convert a b, Convert b a`. No type
decides another: when one of a trait's types should follow from the rest, make
it an associated type instead of a parameter.

A trait and its methods are visible as any declaration is (`@pub trait`, and a
`use M (Describe, describe)` elsewhere); an `impl` has no name and no
visibility, and is found wherever its trait and its types are.

**What it costs.** Nothing, where the types are known. A function with a
constraint is compiled once, taking its `impl`s as hidden arguments, which is what
lets it live in a package that has never heard of your types. But wherever it
is called at types that are known -- which is everywhere, in the end, since
`main` has no type parameters -- the compiler makes a copy of it for exactly
those `impl`s, and in the copy `describe x` is a direct call to the one
`describe` it can be, which a release build may inline. A loop over a trait's
methods runs two to five times faster for it, on every backend; the CEK
machine alone runs the uncopied program, as the specification of what the
copies must do.

### Operators are methods

Every infix operator is a function with a name made of symbols. `a + b` is
`(+) a b`, and in parentheses an operator is written anywhere a name can be:
`(+) 1 2`, `foldl (+) 0 xs`, `fun (<+>) a b = ...`, `use Std.Ops ((+))`.

`Std.Ops` defines the language's operators as the methods of traits:

| Trait   | Methods         | Implemented for                         |
| ------- | --------------- | --------------------------------------- |
| `Add`   | `+`             | every integer type, `Float`, `Float32`  |
| `Sub`   | `-`             | every integer type, `Float`, `Float32`  |
| `Mul`   | `*`             | every integer type, `Float`, `Float32`  |
| `Div`   | `/`             | every integer type, `Float`, `Float32`  |
| `Rem`   | `%`             | every integer type                      |
| `Pow`   | `^`             | every integer type                      |
| `Shift` | `<<` `>>` `>>>` | every integer type                      |

`==`, `!=`, `<`, `>`, `<=` and `>=` are `Std.Cmp`'s: the methods of
`PartialEq` and `PartialOrd` (see [equality and ordering](#one-set-of-operators-for-every-integer-type)).

Each `impl` there is a primitive and nothing else -- `fun (+) x y = _primAdd x
y` -- and the compiler turns a call of one at a known type back into that
primitive, so `x + y` on two `Int`s is one machine instruction, as it would be
if `+` were built in. A type of your own gets an operator with an `impl`:

```meadow
@derive(Debug)
record V2 = { x : Int, y : Int }

impl Add V2 {
  fun (+) a b = V2 { x = a.x + b.x, y = a.y + b.y }
}

fun sumAll xs = foldl (+) (V2 { x = 0, y = 0 }) xs

fun main () = println "${sumAll [V2 { x = 1, y = 2 }, V2 { x = 3, y = 4 }]:?}"
```

```
V2(4, 6)
```

**A new operator** is any run of the symbols `! % & * + . / < = > ? @ | ^ ~ :
-` that is not already punctuation. Define it as a function, and say how it
binds with a **fixity declaration**, as in Haskell:

```meadow
infixl 6 <+>

fun (<+>) a b = a * 10 + b

fun main () = println (1 <+> 2 <+> 3, 1 <+> 2 * 3)
```

```
(123, 16)
```

`infixl` groups to the left (`a - b - c` is `(a - b) - c`), `infixr` to the
right, and `infix` neither way, so that `a == b == c` is an error asking for
parentheses. The number is how tightly it binds, from 0 to 9; application binds
tighter than any of them, and a prefix `-` tighter than any operator. An
operator nothing declares is `infixl 9`. A fixity belongs to the operator's
spelling, not to one definition of it: it holds in every module of the package
that declares it and in every package that depends on that one. A declaration
may repeat the fixity of one of the language's operators (`Std.Ops` does) but
not change it, and two declarations of one operator must agree.

Two operators of one level that do not group the same way cannot be mixed
without parentheses: `a < b == c` is an error, since `<` and `==` are both
`infix 4`. See the [table](#operators-loosest-to-tightest) for every level.

Without `Std` -- a package that does not depend on it -- the operators still
work on numbers: where no definition of `+` is in scope, `+` is the primitive
itself.

### Traits that build on traits

A trait that requires another **inherits its associated types**. Here
`Visual`'s method mentions `Token s`, which is `Stream`'s, and a function given
`Visual s` may use `Stream`'s methods as well, at the same `Token s`:

```meadow
trait Stream s {
  type Token s
  fun take1 : s -> Int -> Maybe (Token s, Int)
}

impl Stream String {
  type Token String = Char
  fun take1 s i =
    if i < stringByteLength s then Just (charFromCode (toInt (stringByteAt s i)), i + 1) else None
}

trait Visual s <: Stream s {
  fun showToken : s -> Token s -> String
    | showToken _ t = "<" ++ show t ++ ">"
}

impl Visual String {}

fun first : Visual s => s -> String
  | first s = match take1 s 0 with | Just (t, _) -> showToken s t | None -> "empty"

fun main () = println (first "xyz")
```

```
<'x'>
```

`impl Visual String {}` says nothing about `Token String`: that is `impl Stream
String`'s to say, and the `impl` asks for it as if its own `where` had.

**Deriving.** `@derive(Tr)` above a declaration runs the derive macro `Tr`,
which writes an `impl Tr` for the type beside it -- as in Rust, it takes both a
trait and a macro. The compiler has the macros for `Debug`, `Display` and
`PartialEq`, `Eq`, `PartialOrd`, `Ord` and `Std.String.Parse`'s `VisualStream`
built in; any other is a procedural macro
a package defines (see [MACROS](MACROS.md#derive)). A derived `impl` of a type
with parameters asks the same trait of each: `Maybe a` is `Debug` when `a`
is.

**`Display` and `Debug`.** Both are traits, with an `impl` for every primitive
type: `display` is what `print`, `println` and `"${x}"` render with, and
`debug` is what `"${x:?}"` does.

**Point-free functions.** `fun f = e` at the top level is a function of the
dictionaries its type needs -- `fun eof : Stream s => Parser s ()` is used at
every stream type -- and is evaluated wherever it is used. Like a `def`, its
body may not perform effects. A `def` is made once, and so is used at one type
per trait.

**Associated effects.** What a method performs can be each implementation's
to say, as an associated type is. The trait names the effect, of its
parameters; an `impl` says which effects it is there; and a method's type
performs it:

```meadow
data Done = Done #[Int]
data Writing s = Writing (StArray s Int)

trait Rows r {
  effect Reading r
  fun slot : r -> Int -> Int ! Reading r
}

impl Rows Done {
  effect Reading Done = {}
  fun slot r i = match r with | Done a -> arrayGet a i
}

impl Rows (Writing s) {
  effect Reading (Writing s) = { St s }
  fun slot r i = match r with | Writing a -> stGetArray a i
}

fun second rows = slot rows 1
```

`second` is `Rows r => r -> Int`, performing whatever reading its rows does:
nothing for a `Done`, so `second d` is a pure expression, and `St s` for a
`Writing s`, so it runs inside that array's `runSt`. An associated effect is
the _rest_ of the row it is written in, so effects may be named before it --
`! { Console, Reading r }` -- and a variable may not follow it. A function
generic over the trait that performs something of its own performs that and
then the associated effect; one function cannot be generic over two
associated effects at once, since a row has one rest -- give a parameter its
type, and its effect is known. Where the
implementation is known the dictionary is compiled away, as every trait's is,
and a call of `slot` is a call of that `impl`'s function.

**A parameter that is an effect.** The same can be said with a parameter of
the trait: one a method's type performs is an effect, and an `impl` is given a
row for it, or a variable where anything will do.

```meadow
trait Rows r e {
  fun slot : r -> Int -> Int ! e
}

impl Rows Done e {
  fun slot r i = match r with | Done a -> arrayGet a i
}

impl Rows (Writing s) { St s | e } {
  fun slot r i = match r with | Writing a -> stGetArray a i
}
```

An `impl` is found by the trait's other parameters -- here by `Done` or
`Writing s` -- and then says what the effect is, so the effect never has to be
known to find one. A row in an `impl`'s head is in braces as its body is; the
body is the last of them. Which to write is a matter of what reads better: a
parameter shows in every `Rows r e =>`, and an associated effect only in the
trait.

What is left out, for now:

- A method's type mentions the trait's parameter, its associated types,
  concrete types and effects -- not type variables of its own. Write
  `fun foldWith : Container f => (b -> Elem f -> b) -> b -> f -> b` as a
  function over a method that is not generic, as `fromList` is above.
- A trait is of types, not type constructors: there is no `Functor f`.
- An `impl` is for a constructor applied to distinct variables in each
  position -- `Convert Int [a;]`, not `Convert a a` or `Convert (Maybe Int) b`.
- A `data` or `record` declaration cannot mention an associated type:
  `data Item s = Tok (Token s)` is not `Stream`'s `Token s`. Carry the stream
  type itself instead, as `Std.String.Parse`'s errors do.
- Only a top-level `fun` asks its caller for an `impl`. A `def`, and a function
  made with `let`, is used at one type per trait.
- A default method's body may use what the trait's supertraits give it, and
  not more: a default that needs `Display` of the trait's type has the trait
  require it (`trait Pretty s <: Display s`).

---

## 7. Sequences: arrays, vectors and lists

Meadow has three sequence types, and one rule for telling them apart in source:
**a `;` means the linked `List`; brackets without one mean the default `Vector`.**

|             | `Array`                | `Vector`        | `List`                                    |
| ----------- | ---------------------- | --------------- | ----------------------------------------- |
| what it is  | flat contiguous buffer | RRB tree        | cons list                                 |
| type        | `#[a]`                 | `[a]`           | `[a;]`                                    |
| empty       | `#[]`                  | `[]`            | `[;]`                                     |
| one element | `#[x]`                 | `[x]`           | `[x;]`                                    |
| several     | `#[x, y]`              | `[x, y]`        | `[x; y]`                                  |
| indexing    | O(1)                   | O(log n)        | O(n)                                      |
| use it for  | primitives, interop    | **most things** | when O(1) access to the head is the point |

`Vector` is the default: the bare `map`, `filter`, `foldl`, `len`, `range` … in the
prelude are `Vector`'s. `List`'s equivalents need an explicit `use` — the prelude
does not activate a `List.` qualifier for you.

Reach for `List` when constant-time access to the head matters more than
anything else: building by consing onto the front, sharing a tail between
versions (an environment of bindings, say), or recursion that takes one element
off at a time. Such recursion is cheap even when it is not a tail call: a
function that answers `f x :: go rest` -- or any constructor with its own
recursive call in the last field -- is compiled to build the list front to back
in a loop, so it uses no stack whatever the list's length. For everything else a `Vector` does more operations well and
keeps its elements close together in memory. The standard library follows the
same rule — its functions take and return vectors, and a `List` appears only in
the explicit `toList` / `fromList` conversions and where an algorithm is a list
algorithm underneath, like `Std.Sort`'s merge sort.

```meadow
use Std.Collections.List as List

def vec  = [1, 2, 3]
def list = [1; 2; 3]
def arr  = #[1, 2, 3]

fun main () = println (len vec, List.length list, arrayLen arr)
```

```
(3, 3, 3)
```

### Ranges

`[a..b]` builds a `Vector` of the integers from `a` up to, not including,
`b`, and `[a..=b]` includes `b` -- as Rust's ranges do. The prelude's `range`
function is the half-open one:

```meadow
fun main () = println ([1..5], [1..=5], range 1 5)
```

```
([1, 2, 3, 4], [1, 2, 3, 4, 5], [1, 2, 3, 4])
```

### Working with vectors

```meadow
fun main () =
  [1..=10]
  |> filter (\n -> n % 2 == 0)
  |> map (\n -> n * n)
  |> foldl (\acc n -> acc + n) 0
  |> println
```

```
220
```

Other prelude staples: `head`, `last`, `get`, `getOr`, `take`, `drop`, `slice`,
`append`, `reverse`, `zip`, `zipWith`, `unzip`, `partition`, `all`, `any`, `find`,
`elem`, `sum`, `product`, `maximum`, `minimum`, `concat`, `concatMap`, `replicate`,
`takeWhile`, `dropWhile`.

`get` is bounds-checked and returns a `Maybe`:

```meadow
fun main () = println (get [1, 2, 3] 1, get [1, 2, 3] 99, getOr 0 [1, 2, 3] 99)
```

```
(Just(2), None, 0)
```

### Working with lists

Lists are what you pattern-match on. `::` is `Cons`, right-associative:

```meadow
use Std.Collections.List as List

fun total xs =
  match xs with
  | [;] -> 0
  | x :: rest -> x + total rest

fun main () = println (total [1; 2; 3], 1 :: 2 :: [;], List.reverse [1; 2; 3])
```

```
(6, [1; 2], [3; 2; 1])
```

`Vector.toList` and `Vector.fromList` convert between the two.

### Matching an empty vector

`[]` is a pattern too — it matches the empty `Vector`. A _non-empty_ vector has no
structural pattern (it is a balanced tree, not a cons cell), so match on `[]` and
fall through, or use `len`:

```meadow
fun describe v =
  match v with
  | [] -> 0
  | _ -> len v

fun main () = println (describe [], describe [1, 2, 3])
```

```
(0, 3)
```

### Strings are bytes; characters are scalars

`Std.String` works in **bytes**, so `byteLength "é"` is 2. A `Char` is one
Unicode **scalar**, so they are different things and the bridge is explicit:

```meadow
use Std.String as S

fun main () =
  let _ = println (S.byteLength "héllo", S.charLength "héllo") in
  println "${(S.chars "hi", S.fromChars ['h', 'i'], S.charAt "héllo" 1):?}"
```

```
(6, 5)
(['h', 'i'], "hi", Just('é'))
```

The rest of `Std.String` is byte-oriented and ASCII-minded:

```meadow
use Std.String as S

fun main () = println "${(S.concat "foo" "bar", S.split "," "a,b,c", S.toUpper "hi", S.toInt "42"):?}"
```

```
("foobar", ["a", "b", "c"], "HI", Just(42))
```

`concat` is common enough to have an operator. `a ++ b` is `S.concat a b`, and
it is in the prelude, so it needs no `use`:

```meadow
fun main () = println ("n = " ++ show 42 ++ "!")
```

```
n = 42!
```

Strings are ordered byte by byte, which for UTF-8 is the order of their code
points, and `<`, `compare` and the rest work on them as on anything `Ord`;
`S.compare`, `S.lessThan`, `S.lessOrEqual`, `S.greaterThan`,
`S.greaterOrEqual`, `S.minOf` and `S.maxOf` are the same, by name:

```meadow
use Std.String as S
use Std.Sort (sortBy)

fun main () = println "${(S.compare "apple" "banana", S.lessThan "app" "apple", sortBy S.compare ["pear", "Fig", "apple"]):?}"
```

```
(Less, True, ["Fig", "apple", "pear"])
```

`++` binds looser than application and tighter than `==`, and groups to the
right. Like every operator it is an ordinary name, bound with
`fun (++) a b = ...` or `def (++) = ...`. It is imported, exported and shadowed
like any other name, and written `(++)` wherever a name goes: `(++) "a" "b"`,
`use Std.String ((++))`.

Underneath, the bytes of a string are a `#[UInt8]`: `stringToBytes` and
`bytesToString` convert, and `Std.Bytes` works on the array. An array of
`UInt8` takes one byte of memory per element, as a string does, so a file read
with `readBytes` costs its size and no more. A byte is a
`UInt8`, so arithmetic on one stays a `UInt8` and wraps. Convert before you
accumulate, or a digit fold quietly keeps only the low eight bits:

```meadow
fun digits acc bytes i =
  if i >= arrayLen bytes then acc
  else digits (acc * 10 + toInt (arrayGet bytes i - 48)) bytes (i + 1)

fun main () = println (digits 0 (stringToBytes "1234") 0)
```

```
1234
```

Without the `toInt`, `acc` would be a `UInt8` too, and the answer `210`.

A string is not copied to be read. Its length, a byte of it, a slice of it and
a search in it cost what they touch, however long the string is, so a lexer can
walk a whole file byte by byte:

```meadow
use Std.String as S

def text = S.repeat "let x = 1\n" 100000

fun countLines s i n =
  if i >= S.byteLength s then n
  else countLines s (i + 1) (if stringByteAt s i == 10 then n + 1 else n)

fun main () = println "${(S.byteLength text, countLines text 0 0, S.slice text 4 5, S.indexOfFrom "x" text 5):?}"
```

```
(1000000, 100000, "x", Just(14))
```

`stringByteAt s i` is the byte, an error past either end; `S.byteAt` is the same
answering `Maybe`. `S.indexOfFrom needle s from` searches from an offset, which
is what a scanner that keeps its place wants. Strings a program builds are
collected like anything else, so making millions of them costs memory only
while they are in use.

`Std.Char` classifies and converts single characters. Its predicates are
**ASCII-only** by design — doing it properly means shipping the Unicode
character database — so a non-ASCII character answers `False` rather than being
guessed at:

```meadow
use Std.Char as C

fun main () =
  let _ = println "${(C.isDigit '7', C.toUpper 'a', C.code 'A'):?}" in
  println "${(C.fromCode 97, C.digitToInt '7', C.isAlpha 'é'):?}"
```

```
(True, 'A', 65)
('a', Just(7), False)
```

Characters match like any other literal:

```meadow
use Std.Char as C

fun kind c =
  match c with
  | ' ' -> "space"
  | '\n' -> "newline"
  | _ -> if C.isDigit c then "digit" else "other"

fun main () = println "${(kind ' ', kind '\n', kind '4', kind 'x'):?}"
```

```
("space", "newline", "digit", "other")
```

### Parsing text

`Std.String.Parse` is a parser combinator library after Haskell's megaparsec,
with two submodules: `Std.String.Parse.Char` for streams of characters, and
`Std.String.Parse.Lexer` for the tokens of a programming language -- a space
consumer that skips white space and comments, numbers, character literals,
and indentation.

```meadow
use Std.String.Parse as P
use Std.String.Parse.Char as C
use Std.String.Parse.Lexer as L

fun sc = L.space C.space1 (L.skipLineComment "--") P.empty

fun lexeme p = L.lexeme sc p

fun symbol s = L.symbol sc s

fun pairOf = P.between (symbol "(") (symbol ")") (P.sepBy (lexeme L.decimal) (symbol ","))

fun main () = println (P.parse (P.skipThen sc (P.thenSkip pairOf P.eof)) "( 1, 2 -- the second\n, 3 )")
```

```
Ok([1, 2, 3])
```

A parser reads a **stream**: a `String` (a stream of `Char`s), a `Vector` of
tokens, or any type with an `impl Stream`. Three traits divide what a parser
needs of one, as in megaparsec:

- `Stream` is enough to parse: take a token, take a run of them, as `Token s`
  -- a `Char` of a `String`, a `t` of a `[t]`. A run of tokens -- a **chunk** --
  is a stream of the same type.
- `VisualStream` is enough to say what went wrong: how a token reads in a
  message. `@derive(VisualStream)` writes it for a stream whose tokens are
  `Display`, each reading as it displays -- so a lexer's token type with a
  readable `impl Display` makes readable errors. A `Vector` of tokens is one
  such stream.
- `TraversableStream` is enough to say where: it turns offsets into the stream
  into a span of the source. For a `String` those are the same offsets; a
  stream of tokens a lexer made maps each token to the span of text it came
  from, which is how an error in a token stream points back at the source.

Positions are offsets, always: an error is a span of the source and a
message, and turning a span into lines and columns is for whatever shows it to
a person.

A parser that stops without having taken all it could -- `many digit` at a
letter, `optional sign` finding none -- leaves **hints**, as in megaparsec:
what it would also have taken there. If the next parser fails at that spot,
the hints join what it wanted, so the message says `expecting a digit or end of
input` rather than only `end of input`.

Every parser reports whether it consumed input, and `alt` only tries its right
branch when the left one failed **without** consuming, so that an error points
at the real problem rather than at the last alternative tried; `try` is how a
parser that must backtrack over what it ate says so. `chunk` (and so
`C.string`) is atomic: a partial match consumes nothing.

```meadow
use Std.String.Parse as P
use Std.String.Parse.Char as C

fun main () =
  match P.parse (P.skipThen (C.string "let x = ") (P.some C.digitChar)) "let x = y" with
  | Ok _ -> println "fine"
  | Err (span, message) -> let _ = println span in print message
```

```
(8, 9)
unexpected 'y'
expecting a digit
```

`Std.String.Parse.Lexer` has megaparsec's indentation too: `indentGuard`,
`nonIndented`, `indentBlock` for a head and the block of lines under it, and
`lineFold` for a construct that continues on further indented lines.

`Std.Json` is built on it and is worth reading as a worked example, and so is
`examples/MiniML`'s `Parser.mw`.

### Maps keyed by anything

`Std.Collections.HashMap` maps any key that is `PartialEq` to a value:
strings, tuples, vectors, and any type that derives it. Like `Vector` it is persistent, so an
update gives a new map and leaves the old one as it was:

```meadow
use Std.Collections.HashMap as HashMap

def ages = HashMap.fromVec [("ada", 36), ("grace", 45)]

def older = HashMap.adjust "ada" (\n -> n + 1) ages

fun main () = println (HashMap.lookup "ada" ages, HashMap.lookup "ada" older, HashMap.size older)
```

```
(Just(36), Just(37), 2)
```

Underneath it is a hash array mapped trie, as in Rust's `im` or Clojure: a
lookup reads a handful of small nodes and an update copies one per level. Keys
are found by the `hash` primitive, which is structural and agrees with `==` —
`hash [1, 2]` is the same however that vector was built. A `Ref` or a function
cannot be a key: `hash` refuses both. (`Std.Collections.Map` is the older,
`Int`-keyed ordered map, for when the keys should come out sorted.)

When the map is a _place_ rather than a value -- a count being built up, a
cache -- `Std.Collections.HashTable` is the same idea written in place: it
lives inside a `runSt`, like a `StArray`, and `insert` writes into it and
answers `()`. One probe and one write per update, against a path copied per
level, which is what makes it the right thing for a loop over a big input:

```meadow
use Std.St as St
use Std.Collections.Vector as V
use Std.Collections.HashTable as HT

fun counts words =
  runSt (\() ->
    let t = HT.new () in
    let _ = V.foldl (\_ w -> HT.insertWith (\a b -> a + b) w 1 t) () words in
    HT.toVec t)

fun main () = println (V.length (counts ["a", "b", "a", "c", "a"]))
```

```
3
```

`insertWith f k v` puts `v` at `k`, or `f v old` if `k` already holds `old`,
in one probe; `lookup`, `member`, `adjust`, `delete` and `foldl` are what they
are for `HashMap`. What comes out of the `runSt` is what `toVec`, `keys` or
`values` read out, never the table.

For keeping track of which things have been found to be the same --
connected components, equivalence classes, the type variables a type checker
unifies -- `Std.Collections.UnionFind` is a disjoint-set forest of the same
kind. Its elements are the numbers `0`, `1`, …, `union` joins two elements'
groups, and `find` names the group an element is in by a representative,
pointing everything it passes straight at it on the way:

```meadow
use Std.St as St
use Std.Collections.Vector as V
use Std.Collections.UnionFind as UF

fun components n edges =
  runSt (\() ->
    let uf = UF.make n () in
    let _ = V.forEach (\e -> let _ = UF.union (fst e) (snd e) uf in ()) edges in
    UF.groups uf)

fun main () = println (components 6 [(0, 1), (1, 2), (4, 5)])
```

```
[[0, 1, 2], [3], [4, 5]]
```

`union` hangs the smaller group's tree under the larger's and `find`
compresses the paths it walks, so a long run of operations costs a few steps
each, for any input there will ever be. Every group also holds a value -- `()`
above -- which `value` reads, `setValue` replaces, and `unionWith f` sets to
`f` of the two groups' values when it joins them; `examples/MiniML` keeps what
each type variable has been solved to there. `same`, `count` and `sizeOf` ask
whether two elements are in one group, how many groups there are, and how big
one is.

### The collector

Each green thread has a heap of its own, and the VM collects it in short
pauses that stop only that thread. New objects go into a small **nursery**,
which is copied when it fills. Garbage costs nothing there, and what survives
is small. What survives twice moves to the **old generation**, where objects
never move. The old generation is marked on a separate OS thread while the
program keeps running, and space nothing marked is reused; between cycles the
emptiest blocks have their few survivors moved out, a block or two per pause,
so the memory goes back. How much a program
keeps alive has almost no effect on how long it is stopped: a server holding a
map of half a million entries while it handles requests is stopped for tens of
microseconds at a time, and its longest pause is well under a millisecond.

`meadow run --gc-stats` reports what the collector did, pauses included:

```sh
$ meadow run --gc-stats benches/Latency
...
200000 requests in 4658 ms; slowest 210 us; 0 over 1 ms; 500000 entries
gc: 26952 collections across 1 thread, 832.1 ms paused (10.3% of 8077.5 ms)
gc: 12.2 GiB allocated, 1.0 GiB copied in nurseries, 791.1 MiB promoted
gc: 30 marking cycles, 409.0 ms marking, 10.7 MiB evacuated from 3077 blocks
gc: old generation 219.8 MiB, 53.9 MiB alive when last marked
gc: heap 220.2 MiB, compact regions 0 B
gc: pauses p50 24.6 us, p99 81.9 us, p99.9 114.7 us, max 186.0 us
```

`--gc copying` (or `MEADOW_GC=copying`) switches to the simpler collector the
VM used before: one space, all of it copied at every collection. Its pauses
grow with the live data, into tens of milliseconds on that same server. It is
there for comparison. `MEADOW_GC_EVACUATE=0` keeps the generational collector
but stops it moving anything: a little more memory held, and a slightly shorter
tail of pauses.

### Compact regions: big data the collector skips

Data that lives a long time still costs the collector something: it was copied
into the old generation once, and every marking cycle walks it again.
`Std.Compact` moves such a value into a **compact region**, memory the collector
neither copies nor looks inside. The collector treats the whole region as one
object, and frees it all at once when nothing refers to it any more. A region
belongs to no thread's heap, so passing a compacted value to another thread
copies nothing either.

```meadow
use Std.Compact as C
use Std.Collections.HashMap as H

fun squares (n : Int) = foldl (\m i -> H.insert i (i * i) m) H.empty (range 0 n)

def table = C.make (squares 1000)

fun main () = println (H.lookup 12 (C.get table), C.get table == squares 1000)
```

```
(Just(144), True)
```

`C.make` copies the value into a new region, and `C.get` hands it back without
copying. The value is unchanged and works anywhere the original did, so both are
pure. `C.add c x` copies `x` into `c`'s region, sharing whatever of `x` is
already there: add one entry to a compacted map and only the path to that entry
is copied. `C.size` is the region's size in bytes. The same four are primitives,
always in scope, as `compact`, `getCompact`, `compactAdd` and `compactSize`.

Because the collector never looks inside a region, nothing in one may change or
point back out. `C.make` fails at run time on a value that reaches a `Ref`, a
mutable array or a function.

Compacting costs one copy of the value, so it pays for data built once and read
for a long time: a parsed input, a lookup table, a cache that rarely changes.
`meadow run --gc-stats` shows whether it is paying. In `benches/Compact`, a
200,000-entry map compacted takes the heap from 70 MiB to under 1 MiB:

```sh
$ meadow run --gc-stats benches/Compact
...
gc: heap 832.0 KiB, compact regions 20.7 MiB
```

On the CEK machine (`--cek`), which reference-counts, compacting checks the
value and copies nothing.

---

## 8. Modules and packages

### A package

A package is a directory with a `Meadow.toml` and a `src/`:

```
MyApp/
  Meadow.toml
  src/
    Main.mw
    Math.mw
```

```toml
[package]
name = "MyApp"
version = "0.1.0"
```

`meadow run MyApp` builds it and runs `main`. A single `.mw` file also counts
as a package, which is why `meadow run hello.mw` works.

Module files, and any directories under `src/`, are PascalCase: each is a name
in a `use` path, so `src/Math.mw` is the module `Math`. `Main.mw` (or `Lib.mw`) is
the package's root module. A lower-case module file is an error that says what
to rename it to. A lone file run directly is exempt — its name is the package's.

### Modules inside one package are separate namespaces

The package is the **compilation unit** — every module of it is resolved,
inferred and lowered together, so two modules may refer to each other and even
be mutually recursive. But each module is its own **namespace**, as in Rust: a
sibling's names arrive through a `use`, and never for free.

```meadow
-- src/Math.mw
@pub(pkg) fun double n = n * 2
```

```meadow
-- src/Main.mw  (a second file in the same package)
use MyApp.Math (double)       -- the package name, then the module

fun main () = println (double 21)
```

The path starts with the package's own name, which is what `Meadow.toml` says —
`use MyApp.Math`. The name on its own is the root module (`Main.mw` or `Lib.mw`),
as `crate` is in Rust: a child module writes `use MyApp (helper)` to reach
something the root declares. The plain `use Math (double)` works too and means the same
thing; the longer form is the one to write when it is not obvious that `Math` is
next door rather than a dependency.

Every `use` form works on a sibling:

```meadow
use MyApp.Math               -- everything it exports, unqualified
use MyApp.Math as M          -- M.double, and nothing unqualified
use MyApp.Math (double)      -- just `double`
```

Because the namespaces are separate, two modules of one package may both define
`map`, and a module that wants both can take one of them under an alias. Types
are the same: a name is known by its fully qualified path, `MyApp.Math.map`,
`MyApp.Syntax.Expr`, and a constructor by its type's, `MyApp.Syntax.Expr.Int`.
Two modules may each declare an `Expr`, and each means its own.

A constructor lives under its type, as a variant does in Rust. Naming a
**type** in a `use` brings the type and nothing else, so outside the module
that declares it a constructor is written `Expr.Int`:

```meadow
-- src/Syntax.mw
@pub(pkg) data Expr = Int Int | Add Expr Expr
```

```meadow
-- src/Eval.mw
use MyApp.Syntax (Expr)      -- the type; its constructors stay `Expr.Int`

@pub(pkg) fun eval e = match e with
  | Expr.Int n -> n
  | Expr.Add a b -> eval a + eval b
```

To write them bare, put the type on the end of the path — Rust's
`use Expr::{Int, Add}`:

```meadow
use MyApp.Syntax.Expr            -- just the type, same as `use MyApp.Syntax (Expr)`
use MyApp.Syntax.Expr (Int, Add) -- those two constructors, unqualified
use MyApp.Syntax.Expr.*          -- every constructor of `Expr`, unqualified
```

Nothing else flattens them: not naming the type, and not a bare `use MyApp.Syntax`,
which brings the module's values, types and effects but leaves constructors under
their types. `use MyApp.Syntax (Int)` is an error that says where `Int` lives.
That holds inside `Syntax.mw` too: it writes `Expr.Int`, or says `use Expr.*`
once -- Rust's `use self::Expr::*` -- and then `Int`. The prelude re-exports
`Just`, `None`, `Ok`, `Err` and `Ordering`'s three with `@pub use ... .*`, which is
the only reason those need no `use` anywhere.

A package's root module can do the same for its users, as a Rust crate root does
with `pub use Ty::*`. After `@pub use MyLib.Shapes.Shape.*` in `MyLib`'s
`Lib.mw`, a dependent writes `use MyLib (Square)` to bring in one constructor,
or a bare `use MyLib` to bring in all of them. A type re-exports the same way:
after `@pub use MyLib.Shapes (Shape)`, a dependent writes `use MyLib (Shape)`.

### A module inside a file

A file is a module, and that is the default. A module can also be written
where it is declared, with its declarations in braces, as Rust's
`mod name { … }`:

```meadow
-- src/Main.mw
@pub mod Core {
  fun secret n = n + 1            -- this module only

  @pub fun double n = secret n * 2 - 2

  @pub mod Expr {
    use Node.*

    @pub data Node = Int Int | Add Node Node

    @pub pattern Lit n = Int n

    @pub fun eval e =
      match e with
      | Int n -> n
      | Add a b -> eval a + eval b
  }
}

fun main () = println (Core.Expr.eval (Core.Expr.Lit (Core.double 4)))
```

`mod Core` alone still names the module in `Core.mw`; `mod Core { … }` is the
same module with its source here. Everything else about it is the same: it is
its own namespace, `@pub` and `@cfg` on it and inside it mean what they do
anywhere, and another module reaches it by its path, `use MyApp.Core.Expr`.
`@cfg(test) mod Tests { … }` keeps a file's tests beside what they test. A
macro may produce one.

A module under the one you are writing — with a file or without — qualifies by
its path from there, with no `use`: `Core.double`, `Core.Expr.eval`. The path
may end in a type and its constructor, `Core.Expr.Node.Add`, or in a pattern
synonym, `Core.Expr.Lit n`, which matches and builds as it does written bare.
A type is not yet named this way where a type is written — `e : Core.Expr.Node`
— so bring it in with `use MyApp.Core.Expr (Node)`.

A module inside another does not see what is around it for free, any more than
a file does. `use super (helper)` names the module it is written in, as Rust's
`super::`; `use super.super (…)` the one above that; and `use super.Other (…)`
a module beside it.

### Visibility: `@pub`, `@pub(pkg)`, `@pub(super)`

Nothing is visible outside the module it is written in until it says so. Three
attributes say so, each one layer wider:

| written       | seen by                                   |
| ------------- | ----------------------------------------- |
| nothing       | its own module, and the modules inside it |
| `@pub(super)` | ...and its parent module's subtree        |
| `@pub(pkg)`   | ...and every module of this package       |
| `@pub`        | ...and anyone who depends on this package |

This is Rust's arrangement, spellings included, with the package where the crate
goes: a plain `@pub` is Rust's `pub`, and `@pub(pkg)` is `pub(crate)`. As in Rust,
something in the parentheses only ever _narrows_ `@pub`. A library's surface is
its `@pub` declarations; `@pub(pkg)` is for the helper that two of your own
modules share and nobody else should.

```meadow
-- src/Math.mw
fun fudge n = n + 1             -- this module only
@pub(pkg) fun double n = n * 2  -- the rest of the package
@pub fun triple n = n * 3       -- and anyone who depends on us
```

A type's constructors are exactly as visible as the type, and an effect's
operations follow its effect.

One escape hatch, for small programs: **a package that never mentions
visibility has none** — every module sees every other, and everything is
exported. The moment any declaration is marked, the rules above apply to all of
them.

`main` is exempt: it is an entry point rather than an export, so it is found
whether or not it is marked.

### `use`

`use` brings a dependency's module into scope. Three forms, and the thing to
remember is that **a qualifier comes only from `as`**:

| Form                | Effect                                                                |
| ------------------- | --------------------------------------------------------------------- |
| `use M`             | every exported name, unqualified — constructors stay under their type |
| `use M as C`        | `C.name` only — nothing unqualified                                   |
| `use M (a, b)`      | just `a` and `b`, unqualified                                         |
| `use M as C (a, b)` | both: `C.name`, plus `a` and `b` unqualified                          |
| `use M.T`           | the type `T`, its constructors written `T.C`                          |
| `use M.T (C, D)`    | constructors `C` and `D` of `T`, unqualified                          |
| `use M.T.*`         | every constructor of `T`, unqualified                                 |

```meadow
use Std.Collections.List              -- everything, unqualified
use Std.Collections.Vector as V       -- V.len, and nothing else
use Std.String (concat)               -- just `concat`

fun main () =
  let answers = (length [1; 2; 3], V.len [1, 2], concat "a" "b") in
  println "${answers:?}"
```

```
(3, 2, "ab")
```

A bare `use` shadows anything of the same name already in scope, including the
prelude — which is exactly how you switch a file from `Vector` to `List`:
`length` above is `List`'s, not the prelude's.

Naming a trait in the list brings its methods too: `use Std.Macro (Reflect)`
puts `toDatum` and `fromDatum` in scope, since a trait is named to be used. An
effect brings its operations the same way: `use Std.Process (Process)` puts
`spawn` in scope.

A `use` is for the module it is written in. What one module brings, the
next module of the package does not see: each says what it wants for itself.
That goes for types as much as for values. Outside the module that declares
it, a type is in scope only where a `use` names it (`use M (T)`, `use M.T`, or a
bare `use M`), or the prelude re-exports it.

A plain `use` stays private: it brings names into this module and gives
nothing to anyone else. Only `@pub use` re-exports. A module's interface, and
so a package's, is exactly what it exports with `@pub`, its declarations plus
its `@pub use`s, and nothing it merely uses.

### Depending on another package

```toml
[package]
name = "App"
version = "0.1.0"

[dependencies]
Util = { path = "../Util" }
```

Then `use Util` for all of it, `use Util (double)` for one name, or
`use Util as U` to keep it behind a qualifier. Only `@pub` names cross the
boundary, and nothing crosses without a `use`: a dependency puts no name in
scope by being listed in `Meadow.toml`, and every module that wants one of its
names says so with its own `use`. Nothing is inherited either: a name that
`Util` itself `use`s from another package is not `Util`'s to give, unless
`Util` re-exports it with `@pub use`.

Each package's types are its own. If `Shapes` and `Figures` both declare a
`Shape`, they are two types: a value of one is not a value of the other, and a
program may use both. A type mismatch between them says which package each one
comes from:

```
type mismatch: `Shape` (from `Shapes@0.1.0`) vs `Shape` (from `Figures@0.1.0`)
```

Which `Shape` a module means is the one it `use`s: `use Shapes (Shape)`. A type
your own module declares needs no `use`: it shadows any other type of the same
name, including one from the prelude, so a program may declare its own `Parser`.

### Depending on a released package

A package published as a git repository is depended on by URL, and `meadow add`
writes the entry for you:

```sh
meadow add meadow-lang/UnicodeWidth
```

```toml
[dependencies]
UnicodeWidth = { git = "https://github.com/meadow-lang/UnicodeWidth", version = "0.1.0" }
```

A package's **releases** are the tags that read as versions: `v1.2.0`, or
`1.2.0` without the `v`. `meadow add` takes the newest release there is and
writes it down, and that entry means _that release, or any later one that does
not break it_:

| written             | means             |
| ------------------- | ----------------- |
| `version = "1.2.0"` | `>=1.2.0, <2.0.0` |
| `version = "0.3.1"` | `>=0.3.1, <0.4.0` |

Below `1.0` the minor is the breaking digit, as Cargo reads it: a package still
finding its shape changes it there. A pre-release (`1.0.0-rc1`) is only ever
chosen by a requirement that asks for one.

Which release a build actually used is in `Meadow.lock`, with the commit:

```toml
[[package]]
name = "UnicodeWidth"
source = "git+https://github.com/meadow-lang/UnicodeWidth?version=0.1.0"
version = "0.1.4"
rev = "fa95092300d547891f5e1ecbd100b7e2438e1058"
```

The manifest is what you will take; the lockfile is what you took. Building
again takes the same thing, however many releases have happened since —
`meadow update` is what looks for a newer one, and it says so in versions:

```
Updating Widget 0.1.3 -> 0.1.4
```

It never crosses a break: a `version = "0.1.0"` dependency does not move to
`0.2.0`, however new that is. Moving across one is editing the manifest, or
`meadow add` again — a decision, not an update.

A repository with no releases at all is followed by its default branch, which
is what `{ git = "…" }` with no version means. `--branch`, `--tag` and `--rev`
still say exactly what to take, and a dependency pinned with `rev` cannot move.

**Two versions at once.** Two packages in one build may want releases that
cannot be met together — `0.1` and `0.2` of the same package. Both are built,
and each gets the one it asked for. They are then _different packages_: their
types are different types, and a value of one does not pass for the other.
That is what the version in `Shape (from Shapes@0.1.0)` is saying. Anything
that _can_ share a release does: two packages wanting `1.0` and `1.2` both get
`1.2`, and the build holds one copy.

### Workspaces

Several packages developed together can form a **workspace**, as in Cargo: one
`Meadow.toml` at the top lists them, and they share a `target` directory, their
build profiles, and whatever they declare there once.

```
shop/
  Meadow.toml          the workspace
  App/
    Meadow.toml
    src/Main.mw
  libs/
    Text/  Meadow.toml  src/Lib.mw
    Util/  Meadow.toml  src/Lib.mw
```

```toml
# shop/Meadow.toml
[workspace]
members = ["App", "libs/*"]

[workspace.package]
version = "0.3.0"

[workspace.dependencies]
Util = { path = "libs/Util" }
Text = { path = "libs/Text" }

[profile.release]
opt-level = 2
```

`members` lists directories, and `*` or `?` matches within one segment of the
path, so `libs/*` is every package under `libs`. A member takes what the
workspace declares by saying `workspace = true`:

```toml
# shop/App/Meadow.toml
[package]
name = "App"
version.workspace = true

[dependencies]
Util = { workspace = true }
Text.workspace = true         # the same thing, spelled the other way
```

```meadow
-- libs/Util/src/Lib.mw
use Std.Test

@pub fun double x = x * 2

@test
fun doubles () = assertEq (double 4) 8 "double 4"
```

```meadow
-- libs/Text/src/Lib.mw
use Util (double)

@pub fun label s = "${s} x${double 1}"
```

```meadow
-- App/src/Main.mw
use Util (double)
use Text (label)

fun main () = println "${(double 21, label "b"):?}"
```

Commands work from anywhere inside the workspace, and three flags pick the
packages:

```sh
$ meadow run -p App              # a member, by name
(42, "b x2")
$ cd App && meadow run           # or the member you are in
(42, "b x2")
$ meadow test --workspace        # every member
running 1 test
test Util.doubles ... ok

test result: ok. 1 passed; 0 failed
$ meadow build --workspace --exclude App
```

- `-p NAME` (or `--package`) names a member, and can be repeated.
- `--workspace` (or `--all`) is every member; `--exclude NAME` leaves one out.
- With neither, a command at the root means the members listed in
  `default-members = ["App"]` if there is one, and otherwise every member.
  `meadow run` needs exactly one, and says which to choose from if it is given
  more.

Testing several packages names each test after its package, `Util.doubles`,
which is its `use` path. Only the chosen packages' tests run, never those of
what they depend on: `meadow test` in `App` runs `App`'s.

What being a member changes:

- **One `target`.** Everything builds into `shop/target`, and a library two
  members use is compiled once when they are built together. Later builds
  reuse it too, as described next.
- **One set of profiles.** `[profile.*]` is read from the workspace's
  `Meadow.toml`. A member's own `[profile]` sections are ignored, with a warning
  that says so.
- **Membership is checked.** A package under the workspace directory that is
  not a member is an error, because it would build with the wrong profiles into
  the wrong `target`. List it in `exclude = ["scratch"]` to keep it a package of
  its own. A path dependency of a member that lives inside the workspace is a
  member without being listed.

The top-level `Meadow.toml` can be a package as well, with a `[package]` of its
own. Then it is a member too, and it is what a command at the root means.
Without one it is only the workspace, and building it directly says to use
`-p` or `--workspace`.

`meadow init --workspace shop` writes an empty workspace, and `meadow init`
inside one adds the new package to `members`, unless a pattern like `libs/*`
already covers it:

```sh
$ meadow init --workspace shop && cd shop
$ meadow init App
created package `App` at App
  App/Meadow.toml
  App/src/Main.mw
  Meadow.toml
added `App` to the members of .
```

### Incremental builds

A package is compiled as a whole, so it is also the unit a build reuses. A
package that compiles without errors is saved under
`target/<profile>/incremental`. The next build reads it back instead of compiling
it again, as long as nothing it was compiled from has changed:

- its modules: their names, their files and their text;
- the compiler, and the options: the profile, `-O`, `--strict`, and every `@cfg`
  condition and flag;
- the packages it depends on.

That last point is what makes it work across a workspace. A change reaches
exactly the packages downstream of it. Edit `App` and only `App` is compiled.
Edit `Util` and `Util`, `Text` and `App` are compiled, but not a member that
does not use `Util`. The embedded standard library is saved there too, which
is most of what a small program's build used to spend its time on.

What is read back is what compiling would have made: the types, the code and the
program are identical. A package with errors is never saved, so its errors are
reported on every build. `meadow run` and `meadow test` keep separate copies,
since `@cfg(test)` makes them different builds. Deleting `target` starts again
from nothing, and `MEADOW_INCREMENTAL=0` turns reuse off for a command without
deleting anything.

`target` holds only what the current compiler can use. What an older build of
the compiler saved is never read again, so a build removes it. A native build
removes the LLVM IR it compiled once the executable is linked, and the objects
of an earlier build that this one no longer has.

### Embedding a file: `includeStr`

`includeStr "path"` is the text of that file, read while the program is
compiled and left in it as a string. Nothing is read at run time, so the file
need not be anywhere when the program runs.

```meadow
def licence = includeStr "LICENSE"

def grammar = includeStr "grammar/meadow.ebnf"
```

The path is taken **beside the file that wrote it**, as an editor would read
it: `src/Main.mw` saying `"note.txt"` means `src/note.txt`, whatever directory
the build was started from. An absolute path is taken as it is.

It is read before the program exists, so the path has to be written out in
full: a name, however constant it looks, is a value the program computes, and
`includeStr where` is an error. A file that is not there is an error too, and
says which path it looked for.

The file counts as an input to the build. Editing it compiles the package
again, even though no `.mw` file changed — a package records what it embedded,
and a cached build is only reused while those files still hash the same.

`includeStr` is not a binding, so a definition of your own by that name simply
wins, as it would over anything else in scope.

### Conditional compilation: `@cfg`

`@cfg(condition)` in front of a declaration compiles it only where the condition
holds, as `#[cfg]` does in Rust. Where it does not hold, the declaration is gone
before the compiler looks at anything else: it is not type-checked, and nothing
can name it. So two definitions of one name are fine, as long as no build keeps
both.

```meadow
@cfg(windows)
def newline = "\r\n"

@cfg(not(windows))
def newline = "\n"

@cfg(feature = "fancy")
fun greet name = "** Hello, ${name}! **"

@cfg(not(feature = "fancy"))
fun greet name = "Hello, ${name}."

@cfg(debug)
fun log msg = println "[debug] ${msg}"

@cfg(not(debug))
fun log msg = ()

fun main () =
  let _ = log "starting" in
  println (greet "Ann")
```

`meadow run` prints `[debug] starting` and then `Hello, Ann.`;
`meadow run --cfg feature=fancy` prints `** Hello, Ann! **` instead; and
`meadow run --release` skips the log line.

The conditions a build knows:

| condition                                                     | holds when                                                 |
| ------------------------------------------------------------- | ---------------------------------------------------------- |
| `os = "windows"`, `"linux"`, `"macos"`                        | the program is built for that system                       |
| `arch = "x86_64"`, `"aarch64"`                                | …for that processor (`--target` sets it for an executable) |
| `family = "unix"`, `"windows"`, or bare `unix` / `windows`    | …for that family of systems                                |
| `profile = "debug"`, `"release"`, or bare `debug` / `release` | the build profile                                          |
| `backend = "vm"`, `"jit"`, `"aot"`, `"silo"`, `"cek"`         | what runs the program: Glade's backends, Silo, or the CEK  |
| `opt_level = "0"`, `"1"`, `"2"`                               | the optimization level                                     |
| bare `test`                                                   | `meadow test` is building it                               |
| any other name, or `name = "value"`                           | the build turned that flag on                              |

`all(…)`, `any(…)` and `not(…)` combine conditions, and several `@cfg`s on one
declaration must all hold: `@cfg(all(unix, not(test)))`.

**Flags** are yours to name. Turn one on with `--cfg fast` or `--cfg feature=gpu`
(repeat `--cfg` for more), or for a profile in `Meadow.toml`:

```toml
[profile.debug]
cfg = "fast, feature=gpu"
```

A flag nobody turned on is simply off. A built-in name given a value it can never
have, like `os = "linx"`, is an error, so a typo there cannot silently turn code
off.

**Features** are the flags a package offers the ones that build it, as Cargo's
are. `Meadow.toml` names them under `[features]`; each lists the other features
of the package it turns on with it, and `default` is the ones on unless a build
says not:

```toml
[features]
default = ["round"]
round = []
fancy = ["round"]     # `fancy` turns `round` on too
```

In the package's source a feature is asked after as `@cfg(feature = "fancy")`.
The command line turns on features of the package it builds, and a package says
which of a dependency's it wants:

```sh
meadow run --features fancy,loud     # these, and the defaults
meadow run --no-default-features     # nothing but what is named
meadow run --all-features            # every one the package has
```

```toml
[dependencies]
Shapes = { path = "../shapes", features = ["fancy"], default-features = false }
```

A feature is on for a package when anything in the build asks for it: two
packages that depend on `Shapes` get one `Shapes`, built with what both asked
for. So a feature should add and never take away. A name that the package's
`[features]` does not have is an error that lists the ones it has; `--cfg
feature=gpu` still turns a flag on everywhere without declaring it.

These are a package's own. What Meadow itself has not settled yet is asked for
another way, as in Rust: `@!feature(ffi)` at the top of a package's root module
(`src/Main.mw` or `src/Lib.mw`), which only a nightly `meadow` accepts. `@!` is
an attribute of the module it is written in, where `@` is one of the
declaration after it.

`@cfg` works on any top-level declaration (`fun`, `def`, `data`, `record`,
`effect`, `use`, and `@test` functions), and on the fields of a record, the named
fields of a constructor, and the operations of an effect:

```meadow
record Settings = {
  name : String,
  @cfg(windows)
  registryKey : String,
}
```

On a `mod`, it leaves out the whole module file and everything under it. That
is where a package's test-only modules go, like Rust's `#[cfg(test)] mod tests`:

```meadow
-- src/Lib.mw
@cfg(test)
mod Tests      -- src/Tests.mw is compiled by `meadow test`, and only then
```

The standard library can use `@cfg` too, but it only sees the platform (`os`,
`arch`, `family`): it is compiled once for every profile and flag.

---

## 9. Effects

This is Meadow's most distinctive feature, and the one most worth reading slowly.
It assumes nothing: if you have never met algebraic effects, or `! { ... }` in a
type looks like line noise, start here.

The payoff first, because it is the reason to bother: code that talks to the
world — asks a question, reads a file, looks at the clock, gives up halfway — is
written _once_, and something outside it decides what "the world" is. The real
filesystem, or a list of strings. The real clock, or the number 500. Nothing is
written twice and nothing is passed in.

### The problem effects solve

Suppose a page greets whoever is signed in, and the name is only known at the
very top of the program. The function that needs the name is three calls deep:

```
page  ──calls──▶  banner  ──calls──▶  greeting  (needs the name)
```

Without effects you have two options, and both are bad. Pass the name as a
parameter through `page` and `banner`, which have no use for it themselves — and
do it again for every other thing `greeting` might ever need. Or keep it in a
global, and give up on calling `page` for two different people in one program.

An effect is a third option. `greeting` _asks_ for the name, and does not care
who answers:

```meadow
use Std.String as S

effect Ask { ask : String -> String }

fun greeting () = S.concat "Hello, " (ask "name")

fun banner () = S.concatAll ["*** ", greeting (), " ***"]

fun page () = S.concat (banner ()) " Welcome back."

fun main () =
  let _ = println (handle page () with { ask question k -> k "Ada" }) in
  println (handle page () with { ask question k -> k "Grace" })
```

```
*** Hello, Ada *** Welcome back.
*** Hello, Grace *** Welcome back.
```

`banner` and `page` never mention a name, and the same `page` served two people.
The rest of this chapter takes that program apart piece by piece.

### Declaring and performing an operation

```meadow
effect Ask { ask : String -> String }
```

This declares an **effect** called `Ask` with one **operation**, `ask`. The type
says what the operation takes and what it gives back — here a question in, an
answer out — but, unlike a function, there is no body. Nothing here says _how_
a question is answered.

Calling `ask "name"` is called **performing** the operation. It looks exactly
like a function call, and to the code calling it, it is one: it takes a
`String` and evaluates to a `String`. The difference is where the answer comes
from. The call is a request sent _outwards_, to whichever handler is in charge
when it runs.

An effect may declare several operations, and may take type parameters the way
a `data` type does:

```meadow
effect State s { get : () -> s, put : s -> () }
```

An operation takes exactly one argument. For more than one, take a tuple — a
handler can pattern-match it apart, as below.

### Handling: answering the request

```meadow
handle page () with { ask question k -> k "Ada" }
```

`handle` runs the expression between `handle` and `with` — the **body** — and
the braces list what to do about each operation it performs. One entry is a
**clause**, and it reads left to right:

| part       | meaning                                                           |
| ---------- | ----------------------------------------------------------------- |
| `ask`      | which operation this clause answers                               |
| `question` | a pattern for the operation's argument — here `"name"`            |
| `k`        | the **continuation**: the rest of the body, waiting for an answer |
| `k "Ada"`  | resume the body, with `"Ada"` as the value `ask "name"` returns   |

Everything in that table is ordinary except `k`, which is the subject of the
next section. In short: when `greeting` performs `ask`, it stops, the clause
runs, and `k "Ada"` sends `"Ada"` back to the spot where `greeting` stopped, so
`greeting` carries on as if `ask "name"` had simply returned `"Ada"`.

A handler may also have a **`return` clause**, which transforms the body's
final value on its way out. Leave it off and it is `return x -> x`:

```meadow
effect Ask { ask : () -> Int }

fun main () =
  println
    ( handle ask () + ask () with { ask () k -> k 10 }
    , handle 1 + 2 with { ask () k -> k 10, return x -> x * 100 } )
```

```
(20, 300)
```

The first body asks twice and gets `10` both times. The second never asks at
all; the `return` clause still runs, on `3`.

An operation that nothing handles would have nobody to answer it, so a program
in which that could happen does not compile:

```meadow
effect Log { log : String -> () }

fun main () = log "nobody is listening"
```

```
`main` performs `Log`, which nothing would handle
```

The compiler says so before the program ever runs. `main` is
`() -> () ! Eff`: it may perform only the effects the runtime answers itself —
the terminal, the filesystem, the clock, randomness and a few more
([below](#the-standard-librarys-effects)) — because once it is running there is
nothing above it to answer anything else. An effect of the program's own, like
`Log`, has to be handled inside `main`, or inside something `main` calls.

### Reading the types

Ask the compiler what the first program's functions are:

```
  ask : forall e. String -> String ! { Ask | e }
  greeting : forall e. () -> String ! { Ask | e }
  banner : forall e. () -> String ! { Ask | e }
  page : forall e. () -> String ! { Ask | e }
```

After a function's result comes `!` and an **effect row** in braces: the effects
calling the function may perform. `String -> String ! { Ask | e }` reads "takes
a `String`, returns a `String`, and along the way may perform `Ask`". Nobody
wrote those rows — they are inferred, and they spread from `ask` to every
caller that does not handle it, which is how `page` ends up saying it needs an
answer even though it never asks.

The `| e` is a **row variable**, and it means "and possibly other effects too".
It is what lets a function that performs `Ask` be called from one that also
performs something else — the two rows are merged, not compared. A function
that performs nothing has no `!` at all. A function performing two effects
lists both:

```meadow
effect Log { log : String -> () }
effect Ask { ask : String -> Int }

fun work () =
  let _ = log "starting" in
  let n = ask "how many?" in
  n * 2

fun silenced () = handle work () with { log m k -> k () }

fun answered () = handle silenced () with { ask q k -> k 21 }

fun main () = println (answered ())
```

```
  work : forall r. () -> Int ! { Log, Ask | r }
  silenced : forall r. () -> Int ! { Ask | r }
  answered : () -> Int
42
```

Each handler takes one label off the row. `silenced` still asks, so its type
says so; `answered` handles the rest, and its type is plain `() -> Int` — the
compiler's guarantee that calling it cannot perform anything.

A function that takes a function takes on that function's effects, whatever they
are:

```meadow
effect Log { log : String -> () }

fun twice f = let _ = f () in f ()

fun logTwice () = twice (\() -> log "hi")

fun sumTwice () = twice (\() -> 1 + 1)
```

```
  twice : forall a e. (() -> a ! e) -> a ! e
  logTwice : forall e. () -> () ! { Log | e }
  sumTwice : forall n. Add n => () -> n
```

`twice` performs exactly what `f` performs — the row variable `e` appears on both
sides. So `logTwice` performs `Log` and `sumTwice` performs nothing, and `twice`
was written once. This is why `map`, `foldl` and every other higher-order
function in `Std` works with effectful functions for free.

**Naming a group of effects.** A row that lists the same effects again and
again can name them once. `effect App = { Log, Ask }` declares an **effect
alias**, and a row that names it means every effect in it: `! { App | e }` is
`! { Log, Ask | e }`. Naming an effect twice changes nothing, so
`! { App, Log | e }` is that same row. The types the compiler prints spell an
alias out in full:

```meadow
effect Log { log : String -> () }
effect Ask { ask : String -> Int }
effect App = { Log, Ask }

fun work : () -> Int ! { App | e }
  | work () =
    let _ = log "starting" in
    ask "how many?" * 2

fun main () : () ! Eff =
  println (handle work () with { log m k -> k (), ask q k -> k 21 })
```

```
  work : forall r. () -> Int ! { Log, Ask | r }
  main : () -> () ! Eff
42
```

`Eff` is one of these. The standard library declares it as
`effect Eff = { Console, Fs, Process, Random, Time, Test, Mut, Thread }` —
every effect the runtime answers — and it is always in scope, so `main` can say
in its own signature what it is allowed to do, as this one does. Leave the
signature off and `main`'s effects are inferred like any other function's, and
then checked against `Eff`.

### Continuations: what `k` is

`k` is the heart of the whole mechanism, so here it is slowly.

Take the body `ask () + 1`. At the moment `ask ()` is performed, the program has
done some of its work and has some left. What is left is: _take whatever `ask`
returns, add 1 to it, and finish the `handle`_. Write that leftover work with a
hole where the answer goes:

```
□ + 1
```

That is the **continuation** — the rest of the computation, from the point the
operation was performed to the end of the `handle` body. `k` is that hole, made
into a function: `k 41` fills the hole with `41` and runs the rest, which here
means `k 41` evaluates to `42`. In `page`, the continuation at the `ask` is "put
the answer after `Hello, `, then finish building the banner, then finish the
page" — three functions' worth of unfinished work, packaged as one value.

It helps to picture the call stack. When `greeting` performs `ask`, the stack
looks like this, innermost at the top:

```
│ greeting   waiting for ask's answer    ┐
│ banner     waiting for greeting        │  this part becomes k
│ page       waiting for banner          ┘
│ handle ... with { ask question k -> k "Ada" }
│ main
```

Performing `ask` searches downward for the nearest `handle` that has an `ask`
clause. The frames above it — everything between the `handle` and the `ask` —
are lifted off the stack and wrapped up as `k`. Then the clause runs, _in place
of the whole `handle` expression_. What the clause evaluates to is what the
`handle` evaluates to.

Calling `k "Ada"` puts those frames back, with the handler still underneath
them, and makes `ask "name"` return `"Ada"` inside `greeting`. The body carries
on from there. When the body finally finishes, its value goes through the
`return` clause, and _that_ is what `k "Ada"` returns to the clause.

That last point decides the order things happen in, so watch it happen:

```meadow
effect Ask { ask : () -> Int }

fun main () =
  let answer =
    handle (
      let _ = println "body: before ask" in
      let x = ask () in
      let _ = println "body: after ask" in
      x + 1
    ) with {
      ask () k ->
        let _ = println "handler: before k" in
        let r = k 41 in
        let _ = println "handler: after k" in
        r,
      return v ->
        let _ = println "return clause" in
        v
    }
  in
  println answer
```

```
body: before ask
handler: before k
body: after ask
return clause
handler: after k
42
```

Read it as a conversation. The body runs until it asks, then pauses. The
handler runs until it calls `k`, then _it_ pauses while the body finishes —
including the `return` clause. Only then does `k 41` return `42` to the handler,
which prints its last line and makes `42` the value of the whole `handle`,
for `main` to print.

If you know exceptions, that is the one-sentence summary: **performing an
operation is throwing an exception that the handler can choose to resume**. The
jump to the handler is the same. What exceptions cannot do is jump _back_,
and `k` is exactly that ability, handed to the handler as a value.

### What a clause can do with `k`

Because `k` is a value, a clause is free to decide what to do with it, and each
choice is a different kind of program.

**Resume it right away** — `op x k -> k answer`. The body barely notices it was
interrupted. That is `Ask` above, and most handlers.

**Resume it, and use what it returns.** `k` returns the value of the rest of the
body, so the clause can wrap it:

```meadow
effect Log { log : String -> () }

fun work () =
  let a = log "step one" in
  let b = log "step two" in
  42

def collected =
  handle work () with {
    log m k -> pushFront (k ()) m,
    return x -> []
  }

def counted =
  handle work () with {
    log m k -> 1 + k (),
    return x -> 0
  }

fun main () = println "${(collected, counted):?}"
```

```
(["step one", "step two"], 2)
```

In `collected` the first `log` puts `"step one"` in front of whatever the rest of
the run produces, and the rest of the run is the second `log` doing the same
thing, and then the `return` clause throwing `42` away for an empty vector. One
handler collects the messages, the other counts them, and `work` knows about
neither.

**Never resume it.** If the clause does not call `k`, the rest of the body never
runs: its frames are simply dropped, and the clause's value becomes the
`handle`'s value. That is early exit, and it is the whole mechanism behind
`Std.Exn`, `Stream.take` and any "stop now" you write yourself:

```meadow
use Std.String as S

effect Abort { abort : String -> () }

fun search xs =
  let _ = forEach (\x -> if x < 0 then abort (show x) else ()) xs in
  "all non-negative"

fun main () =
  let _ = println (handle search [1, 2, 3] with { abort m k -> S.concat "found " m, return x -> x }) in
  println (handle search [1, -2, 3] with { abort m k -> S.concat "found " m, return x -> x })
```

```
all non-negative
found -2
```

There is no `break` in the language and none is needed: `forEach` does not know
it can be interrupted, and is interrupted anyway.

**Keep it for later.** `k` can be returned, stored in data, and called long
after the `handle` has finished. Here a job reports progress, and the handler
turns each report into a paused job that someone else decides when to resume:

```meadow
effect Progress { report : Int -> () }

data Job = Finished String | Suspended Int (() -> Job)

fun work () =
  let _ = report 25 in
  let _ = report 50 in
  let _ = report 75 in
  "all done"

fun start () = handle work () with {
  report pct k -> Job.Suspended pct k,
  return result -> Job.Finished result
}

fun drive job seen = match job with
  | Job.Finished result -> (result, seen)
  | Job.Suspended pct resume -> drive (resume ()) (pushBack seen pct)

fun main () = println "${drive (start ()) []:?}"
```

```
("all done", [25, 50, 75])
```

`start` returns as soon as `work` reports 25, with the rest of `work` inside the
`Suspended`. Each `resume ()` runs `work` to its next report — still under the
same handler, which wraps that report up the same way — until the `return`
clause produces `Finished`. That is a generator, a coroutine or an async task,
depending on who is calling `drive`, and `work` is none of them. It just reports.

**Return a function, and thread a value through it.** If every clause and the
`return` clause produce a _function_, the `handle` as a whole is a function too,
and applying it to a starting value passes that value from one operation to the
next:

```meadow
effect Counter { next : () -> Int }

fun job () = let a = next () in let b = next () in let c = next () in [a; b; c]

fun counting act =
  (handle act () with {
    next () k -> \n -> (k n) (n + 1),
    return x -> \n -> x
  }) 0

fun main () = println (counting job)
```

```
[0; 1; 2]
```

`next () k -> \n -> (k n) (n + 1)` says: given the current count `n`, answer
`n`, and hand `n + 1` to whatever comes next. `k n` resumes the body, and the
body's remainder is itself one of these functions, waiting for its count. This
is how `Std.State` threads state with no mutation anywhere, and its type says
exactly what happened to the effect:

```
  counting : forall a e. (() -> a ! { Counter | e }) -> a ! e
```

**Not twice.** A continuation can be resumed at most once:

```meadow
effect Choose { choose : () -> Bool }

fun main () =
  println (handle (if choose () then 1 else 2) with {
    choose () k -> k True + k False
  })
```

```
continuation resumed more than once
```

Handlers in Meadow are **one-shot**. Resuming moves the suspended frames back
onto the stack instead of copying them, which is what keeps performing an
operation cheap — and a second resume would find nothing left to move. Programs
that want to explore several answers (backtracking, probability) have to be
written as a loop that performs again rather than one that resumes twice.

### The rules, collected

**The innermost handler wins.** Performing searches outwards and stops at the
first handler with a clause for the operation:

```meadow
effect Ask { ask : () -> Int }

fun main () =
  println (handle (
    handle ask () with { ask () k -> k 1 }
  ) with { ask () k -> k 100 })
```

```
1
```

**Handlers are deep.** Resuming `k` puts the body back _with its handler around
it_, so every later operation in that body goes to the same handler — which is
why `collected` saw both logs, and why `drive` kept getting `Suspended` jobs
back. Nothing has to reinstall anything.

**A clause runs outside its own handler.** An operation performed _inside_ a
clause goes to the next handler out, not back to the one the clause belongs to,
so a handler can pass things along:

```meadow
effect Log { log : String -> () }

use Std.String as S

fun main () =
  let result =
    handle (
      handle log "inner" with {
        log m k -> let _ = log (S.concat "relayed: " m) in k (),
        return x -> "done"
      }
    ) with {
      log m k -> let _ = println (S.concat "outer handler saw: " m) in k (),
      return x -> x
    }
  in
  println result
```

```
outer handler saw: relayed: inner
done
```

**One handler can answer several effects**, and a clause's argument can be any
pattern — which is how an operation with a tuple argument is taken apart:

```meadow
effect Files { write : (String, String) -> () }
effect Log { log : String -> () }

fun save () =
  let _ = log "saving" in
  let _ = write ("a.txt", "hello") in
  write ("b.txt", "world")

fun main () =
  let written =
    handle save () with {
      log m k -> k (),
      write (path, text) k -> pushFront (k ()) path,
      return x -> []
    }
  in
  println "${written:?}"
```

```
["a.txt", "b.txt"]
```

**An operation without a clause passes through** to the next handler out, and
its effect stays in the type — a handler only takes an effect off the row when
it answers every one of its operations:

```meadow
effect Counter { next : () -> Int, reset : () -> () }

fun job () = let a = next () in let _ = reset () in let b = next () in (a, b)

fun partial () = handle job () with { next () k -> k 7 }

fun main () = println (handle partial () with { reset () k -> k (), next () k -> k 0 })
```

```
  partial : forall e. () -> (Int, Int) ! { Counter | e }
(7, 7)
```

The row names effects, not operations, so the type cannot say "`Counter`, but
only `reset`". It errs toward saying too much — which is why the handler in
`main` has a clause for `next` as well, though nothing will ever reach it.
Without one, `main` would still perform `Counter` as far as its type can
tell, and would not compile.

### Which things are effects, and which are not

Printing is an effect like any other:

```meadow
use Std.Console (withOutput)

fun greet name = println name

fun main () =
  let captured = withOutput (\() -> let _ = greet "Ada" in greet 42) in
  println "${captured:?}"
```

```
  greet : forall a e. Display a => a -> () ! { Console | e }
((), "Ada\n42\n")
```

`print` and `println` are ordinary functions in `Std.Console`, re-exported by the
prelude so they need no `use`. Underneath they perform one operation,
`writeOutput : String -> ()`. Unhandled, that writes to the real terminal; under
`withOutput`, as here, nothing is printed and the text comes back as a value.
A value that is not a `String` is written by its `Display`, which is why `42`
came out as `42`. `withOutput` answers only `writeOutput` and not
`readLine`, so by the rule above `Console` stays in the type of anything that
uses it.

Reading works the same way — `Console.readLine`, answered by `withInput` — and
so do the filesystem, the clock, randomness and subprocesses. Anything that
touches the world is an operation a handler can stand in for, which is what
lets a test run a program that prompts, prints and rolls dice without a
terminal or an unpredictable number anywhere near it.

The one exception is `Mut`, for `Ref` cells. `newRef`, `getRef` and `setRef`
carry it so that a function that mutates says so and a pure one still reads as
pure, but it has no operations and nothing handles it: a cell is a cell. It is
covered under the standard library below.

### The standard library's effects

`Std` ships nine, plus `Mut` for mutable cells and `St` for mutation kept local. Each pairs a real implementation
with a handler that fakes it — which is the point, since these are exactly the
things that are otherwise hard to test.

| Module            | Operations                         | Unhandled             | Handled with                                  |
| ----------------- | ---------------------------------- | --------------------- | --------------------------------------------- |
| `Std.Ref` (`Mut`) | via `newRef` / `getRef` / `setRef` | real cells            | —                                             |
| `Std.St` (`St s`) | via `stNewRef`, `stNewArray`, …    | —                     | `runSt`                                       |
| `Std.State`       | `get`, `put`                       | —                     | `runState`, `evalState`, `execState`          |
| `Std.Console`     | `writeOutput`, `readLine`          | real stdout and stdin | `withOutput`, `withInput`                     |
| `Std.Exn`         | `throw`                            | aborts                | `toResult`, `catch`, `withDefault`, `toMaybe` |
| `Std.Yield`       | `yield`                            | —                     | everything in `Std.Stream`                    |
| `Std.Random`      | `nextInt`, `intBetween`, …         | real entropy          | `withSeed`, `withSeedFrom`                    |
| `Std.Time`        | `now`, `monotonic`, `sleep`        | real clock            | `withClock`, `withTickingClock`               |
| `Std.Fs`          | `readToString`, `writeString`, …   | real filesystem       | any `handle`                                  |
| `Std.Process`     | `spawn`, `status`, `argv`, …       | real subprocesses     | any `handle`                                  |
| `Std.Test`        | `fail`                             | fails the test        | `didFail`                                     |

Both engines answer an unhandled `Console`, `Fs`, `Process`, `Random` or `Time`
operation for real, so a test that should not touch the filesystem has to handle
`Fs` — which, for the same reason, is all it takes.

#### Mut — the one mutable cell

`newRef`, `getRef` and `setRef` are primitives and always in scope. Every one of
them carries the `Mut` effect, so a function that mutates says so in its type and
a pure one still reads as pure:

```meadow
use Std.Ref (modify, repeatN)

fun countUp n =
  let r = newRef 0 in
  let ignored = repeatN n (\() -> modify r (\x -> x + 1)) in
  getRef r

fun main () = println (countUp 5)
```

```
5
```

`countUp : Int -> Int ! { Mut | e }`. There is no handler for `Mut` — it is an
effect so that it shows up in types, not so that it can be reinterpreted.

That typing also keeps mutation _sound_. Meadow generalizes a binding only when
its right-hand side is pure — an effect-based value restriction rather than ML's
syntactic one — so `def r = newRef []` is never given `forall a. Ref [a]`, and the
classic trick of storing at one type and reading at another does not typecheck.

One surprise worth knowing: a `Ref` has identity. `==` on two of them compares
cells, not contents, so `newRef 1 == newRef 1` is `False`. Only mutable things
behave that way: `Ref`s, and the arrays in the next section.

#### St — mutation that stays inside

`Mut` never goes away: a function that uses a `Ref` for its own private
bookkeeping still shows `! { Mut | e }` to every caller, forever. `runSt` is the
way out. Mutation done inside it — on cells from `stNewRef`, or mutable arrays
from `stNewArray` — cannot be seen from outside it, so the type of the whole
thing is as pure as the result:

```meadow
use Std.St as St

fun sumTo n =
  runSt (\() ->
    let total = St.newRef 0 in
    let _ = St.forRange 1 (n + 1) (\i -> St.modifyRef total (\t -> t + i)) in
    St.getRef total)

fun fibs n =
  runSt (\() ->
    let a = St.newArray n 0 in
    let _ = if n > 1 then St.set a 1 1 else () in
    let _ = St.forRange 2 n (\i -> St.set a i (St.get a (i - 1) + St.get a (i - 2))) in
    St.toVec a)

fun main () = println (sumTo 100, fibs 10)
```

```
  sumTo : forall n. (PartialOrd n, Add n) => n -> n
  fibs : forall n. Add n => Int -> [n]
(5050, [0, 1, 1, 2, 3, 5, 8, 13, 21, 34])
```

No `Mut`, and no `St` either. What makes that safe is a check on the types, not
on what the code happens to do. Each `runSt` gets its own state type `s`, which
the checker invents for that `runSt` and nobody can name; everything made inside
carries it (`StRef s Int`, `StArray s Int`), and so does every operation's effect
(`! { St s | e }`). `runSt` takes the `St s` away, and in exchange nothing that
mentions `s` may leave:

```meadow
def cell = runSt (\() -> stNewRef 0)
```

```
state from inside a `runSt` escapes it
```

The same goes for a closure that reads a cell, a cell written into some `Ref`
from outside, or a cell used after its `runSt` has returned. Everything that
does not mention `s` passes through as usual: an inner `runSt` may use an outer
one's cells, a `Log` performed inside stays in the type, and a callback handed
in from outside keeps its own effects and nothing more. `Std.Sort.sortBy` works
this way — it sorts a mutable array in place and is still
`(a -> a -> Ordering ! e) -> [a] -> [a] ! e`.

A `StArray` is written in place, so `St.set` is O(1) where `arraySet` copies the
whole `Array`. `St.freeze` and `St.thaw` copy between the two, and `St.fromVec` /
`St.toVec` do the same for vectors. `runSt` has to be applied to its body
directly, `runSt (\() -> ...)`: that application is where the special typing
happens.

#### State — a value threaded for you

Where `Mut` is a real cell, `State` is a value passed along invisibly and handed
back at the end. Nothing is allocated and nothing is shared.

```meadow
use Std.State (get, put, modify, runState, evalState, execState)

fun tick () =
  let n = get () in
  let ignored = put (n + 1) in
  n

fun main () = println (runState 0 (\() -> let a = tick () in let b = tick () in get ()))
```

```
(2, 2)
```

`runState` returns `(value, finalState)`; `evalState` keeps the value, `execState`
the state. `modify f` is `put (f (get ()))` and `gets f` is `f (get ())`.

Reach for `State` when the state is part of what a computation _means_ and you
want it out of the signatures; reach for `Mut` when you want a cell with identity,
or speed.

#### Console — the terminal, and why it makes a program testable

Two operations: `writeOutput`, which `print` and `println` are built on, and
`readLine`. `prompt` is `print` then `readLine`. `None` from `readLine` means
end of input — a closed pipe, or Ctrl-D — which is not an error, so a loop ends
by matching it rather than by catching anything.

```meadow
use Std.Console (prompt, withInput, readLine)
use Std.String as S

fun greet () =
  match prompt "name: " with
  | Just name -> S.concat "hello, " name
  | None -> "nobody there"

fun main () = println "${(withInput ["ada"] greet, withInput [] greet):?}"
```

```
name: name: ("hello, ada", "nobody there")
```

The two `name: ` are real: `withInput` answers only `readLine`, so the `print`
inside `prompt` went past it to the terminal. Add `withOutput` and the whole
conversation stays inside the program:

```meadow
use Std.Console (prompt, withInput, withOutput)

fun main () =
  let captured = withOutput (\() -> withInput ["ada"] (\() -> prompt "name: ")) in
  println "${captured:?}"
```

```
(Just("ada"), "name: ")
```

`withInput` feeds a vector of lines and answers `None` once they run out, so the
same function covers both the interactive and the exhausted case.

The example in `examples/RockPaperScissors` is the whole point of this in
practice. It is an interactive terminal game — it prompts, it loops, it keeps a
tally — and its test plays a _complete game_ with no terminal and no entropy
anywhere near it:

```meadow
@test fun playsAFullRound () =
  let played =
    R.withSeed 7 (\() -> withInput ["rock", "nonsense", "paper", "quit"] game) in
    assertEq played () "a full game plays through to the summary"
```

That runs `game` itself, not a copy of it — the same function `main` calls. The
`"nonsense"` line exercises the re-prompt path, and `"quit"` exercises the exit.
Two handlers nest: `withSeed` fixes what the machine plays, `withInput` fixes what
you type, and between them the transcript is fully determined.

Handlers compose by nesting, and the inner one sees the outer one's effects:

```meadow
use Std.Console (prompt, withInput)
use Std.Random as R
use Std.String as S

fun guess () =
  let secret = R.between 1 11 in
  match prompt "pick 1-10: " with
  | None -> "no answer"
  | Just typed -> if S.trim typed == show secret then "right" else "wrong"

fun main () = println (R.withSeed 1 (\() -> withInput ["3"] guess))
```

```
pick 1-10: wrong
```

#### Exn — failure that unwinds

`raise` abandons the computation and travels to the nearest handler, so nothing in
between has to mention failure:

```meadow
use Std.Exn (raise, toResult, withDefault, toMaybe, ensure)

fun half n = if n % 2 == 0 then n / 2 else raise "odd"

fun main () =
  let results =
    ( toResult (\() -> 1 + half 8)
    , toResult (\() -> 1 + half 7)
    , withDefault 0 (\() -> half 7)
    , toMaybe (\() -> half 7) )
  in
  println "${results:?}"
```

```
(Ok(5), Err("odd"), 0, None)
```

Also: `catch recover act` runs `recover` on the error, `threw act` answers a
`Bool`, and `ensure cond e` / `refute cond e` throw when a condition fails.
`ofResult` and `ofMaybe` go the other way, turning a value back into a throw.

Use `Result` when the caller should inspect the failure; use `Exn` when it should
travel a long way untouched. `toResult` converts at the boundary.

#### Yield and Stream — generators

`Std.Yield` is one operation, `yield : a -> ()`. A producer performs it; a
consumer decides what it means. `Std.Stream` is the consumers.

```meadow
use Std.Yield (yield)
use Std.Stream as St

fun countdown n =
  if n <= 0 then ()
  else let _ = yield n in countdown (n - 1)

fun main () = println (St.toVec (\() -> countdown 5), St.take 2 (\() -> countdown 100))
```

```
([5, 4, 3, 2, 1], [100, 99])
```

`take` is the interesting one: it simply stops resuming, which unwinds the
producer where it stands. So a producer is lazy without being written lazily —
`countdown 100` really does stop after two, and this stops after three rather than
building a million-element anything:

```meadow
use Std.Stream as St

fun main () =
  println
    ( St.take 3 (\() -> St.range 0 1000000)
    , St.toVec (\() -> St.map (\x -> x * x) (\() -> St.range 1 5))
    , St.sum (\() -> St.range 1 101) )
```

```
([0, 1, 2], [1, 4, 9, 16], 5050)
```

Consumers: `toVec`, `toList`, `forEach`, `fold`, `count`, `sum`, `take`,
`takeWhile`, `first`, `any`, `all`, `find` — the ones that collect build a
`Vector`, except `toList`. Transformers, which are producers that
consume: `map`, `filter`. Producers: `ofList`, `ofVec`, `range`, `repeat`,
`iterate`. Every collection has `toStream`.

#### Random — deterministic when you want it

```meadow
use Std.Random as R

def rolls = R.withSeed 42 (\() -> [R.between 1 7; R.between 1 7; R.between 1 7])

fun main () = println (rolls, rolls == R.withSeed 42 (\() -> [R.between 1 7; R.between 1 7; R.between 1 7]))
```

```
([1; 4; 4], True)
```

Same seed, same sequence — which is what makes a shuffle or a simulation testable.
Operations: `nextInt`, `intBetween` (and the friendlier `between lo hi`),
`nextFloat`, `nextSeed`, plus `bool`, `choose` and `shuffle` on top. Unhandled,
`between` uses real entropy.

#### Time — a clock you control

```meadow
use Std.Time as T

def frozen = T.withClock 500 (\() -> (T.now (), T.now ()))

def ticking = T.withTickingClock 1000 10 (\() -> (T.now (), T.now (), T.now ()))

fun main () = println (frozen, ticking)
```

```
((500, 500), (1000, 1010, 1020))
```

`withClock` freezes time; `withTickingClock start step` advances it by `step` on
every read, which is how you test a timeout without waiting for one. `sleep`
returns immediately under either. There are helpers for units (`seconds`,
`minutes`, `hours`, `days`, `toSeconds`) and `timed` / `elapsed` for measuring a
computation with the monotonic clock.

To time something and see the answer, wrap it: `T.time "sort" (() -> sort xs)`
prints `sort: 12.3ms` and returns the sorted vector, and `T.timeRuns "parse" 1000
(() -> parse s)` runs it a thousand times and prints the total and the time per
run. (In the REPL, `:time` does this for every entry.) Durations and dates print
the way a person reads them:

```meadow
use Std.Time as T

fun main () =
  let _ = println (T.formatNanos 1234567) in
  let _ = println (T.formatMillis (T.minutes 90)) in
  let _ = println (T.formatTimestamp 1789405445123) in
  let _ = println (T.formatReadable 1789405445123) in
  println (T.formatAgo (T.hours 5) (T.hours 2))
```

```
1.23ms
1h 30m 00s
2026-09-14T17:04:05.123Z
Mon 14 Sep 2026 17:04:05 UTC
3h ago
```

`formatNanos` picks the unit and keeps three significant figures; `utc ms` takes a
moment apart into `{ year, month, day, hour, minute, second, millis, weekday }`,
and `formatDate`, `formatTimeOfDay`, `monthName` and `weekdayName` are there for
building your own. Dates are UTC: there are no time zones yet.

#### Fs — the filesystem, or a pretend one

Every operation answers a `Result`, so failure is a value rather than a throw:

```meadow
use Std.Fs (readToString, writeString, exists, removeFile)

fun main () =
  handle
    match readToString "config.txt" with
    | Ok text -> println text
    | Err e -> println e
  with {
    readToString path k -> k (Ok "colour = blue"),
    return x -> x
  }
```

```
colour = blue
```

No file was touched. `Fs` has no bundled fake — a handler is a few lines and the
one you want depends on the test, so write it inline as above. Operations cover
reading (`readToString`, `readBytes`, `readDir`, `metadata`), writing
(`writeString`, `writeBytes`, `appendString`, `copy`, `rename`), directories (`createDir`,
`createDirAll`, `removeDir`, `removeDirAll`) and predicates (`exists`, `isFile`,
`isDir`). Convenience: `readToStringOr`, `tryReadDir`, `existsAll`. `readDir`
answers a `Vector` of names; `readBytes` answers a `#[UInt8]`, the byte-array
type `Std.Bytes` works on.

#### Process — subprocesses, argv and the environment

A command is built up rather than passed as one lump: `command "git"` then
`withArg`, `withArgs`, `withCwd`, `withEnv`. `run` and `runInherit` are the short
forms.

```meadow
use Std.Process as P

fun versionOf tool =
  match P.run tool ["--version"] with
  | Ok out -> P.outputStdout out
  | Err e -> e

fun main () =
  let version =
    handle versionOf "meadow" with {
      spawn cmd k -> k (Ok (0, "meadow 0.2.0", "")),
      return x -> x
    }
  in
  println version
```

```
meadow 0.2.0
```

`spawn` captures stdout and stderr and gives you `(status, out, err)` — pick them
apart with `outputStatus` / `outputStdout` / `outputStderr`, or ask `succeeded`.
`status` inherits the parent's stdio and yields only the exit code. Also here:
`exit`, `currentPid`, `argv`, `getEnv`, `setEnv`, `removeEnv`.

`argv ()` is the program's own arguments, not including its name. A native
executable gets them from its command line; under `meadow run` they are what
follows `--`, so `meadow run . -- input.txt -v` hands the program
`["input.txt"; "-v"]`, not `meadow`'s arguments.

#### Test — an assertion is an effect too

`Std.Test`'s `fail` is an ordinary operation, which is why a failing assertion
stops the test it is in and nothing else. `didFail` handles it, so you can assert
that something _should_ fail:

```meadow
use Std.Test (assertEq, didFail)

fun main () = println (didFail (\() -> assertEq 1 1 "same"), didFail (\() -> assertEq 1 2 "different"))
```

```
(False, True)
```

#### Thread — green threads and channels

`Std.Thread` runs code at the same time as other code. A thread is cheap, so a
program can have thousands, and the runtime spreads them over every core. You
never deal with operating-system threads.

```meadow
use Std.Thread as Thread

fun fib (n : Int) = if n < 2 then n else fib (n - 1) + fib (n - 2)

fun main () =
  let a = Thread.spawn (\() -> fib 20) in
  let b = Thread.spawn (\() -> fib 21) in
  println (Thread.await a, Thread.await b, Thread.parMap (\n -> n * n) [1, 2, 3])
```

```
(6765, 10946, [1, 4, 9])
```

`spawn` starts a thread and answers a `Task`; `await` waits for it and hands
back its result. Threads also talk over channels: `send` puts a value in and
never waits, `receive` takes the oldest value out and waits if there is none.

```meadow
use Std.Thread as Thread

fun main () =
  let requests = Thread.newChannel () in
  let replies = Thread.newChannel () in
  let server = Thread.spawn (\() ->
    let rec serve (k : Int) =
      if k == 0 then ()
      else
        let n = Thread.receive requests in
        let _ = Thread.send replies (n * n) in
        serve (k - 1)
    in serve 2) in
  let _ = Thread.send requests (toInt 7) in
  let first = Thread.receive replies in
  let _ = Thread.send requests (toInt 8) in
  println (first, Thread.receive replies)
```

```
(49, 64)
```

The rules that make this safe:

- **Every thread has its own heap.** A value reaches another thread only as a
  copy: the function passed to `spawn` (with whatever it captures), a value
  sent on a channel, and a result passed back by `await`. Copying immutable
  data changes nothing a program can see, so you only notice when something
  mutable would cross. A `Ref`, a mutable array or a continuation is refused at
  run time. Threads can't share mutable state. A `Compact` crosses without
  being copied, so compacting a large value is how threads share it cheaply.
- **What a thread may do is in its type.** A thread starts with no handlers,
  so its function may perform only what the runtime answers: `Eff`, as `main`
  may — `Console`, `Fs`, `Process`, `Random`, `Time`, `Test`, `Mut` and
  `Thread`. Any other effect must be handled inside the thread. `spawn (\() -> log "x")` with a `Log`
  handled outside is a type error: "the effect `Log` is not allowed here".
- **`main` ending ends the program**, as in Go. Threads still running are
  stopped.
- **A failure stays in its thread.** `await` on that thread fails with the
  same message.
- **If every thread is waiting and none can wake another, that is a
  deadlock**, reported as an error.
- **A thread can be asked to stop.** `Thread.cancel t` returns at once, and
  `t` fails with "cancelled" at its next operation the runtime answers -- a
  `receive`, an `await`, a `println`, a read of the clock -- or, if it is
  waiting in one, straight away. A thread in a loop that performs nothing is
  ended a moment later, where the runtime would have preempted it. `await` of
  a cancelled thread fails with the same message. A thread that would rather
  finish on its own terms asks `Thread.cancelled ()` and returns what it has.

```meadow
use Std.Thread as Thread

fun main () =
  let jobs = Thread.newChannel () in
  let worker = Thread.spawn (\() ->
    let rec work (n : Int) = let _ = Thread.receive jobs in work (n + 1) in
    work 0) in
  let _ = Thread.send jobs "one" in
  let _ = Thread.cancel worker in
  println "the worker stops at its next receive"
```

```
the worker stops at its next receive
```

Nothing of a cancelled thread runs after it stops, and no handler inside it
can catch the failure, so code never has to guard its cleanup against a
cancel arriving half way through: there is no exception to mask, because a
thread is only ever stopped at an operation it performs, or in pure code that
has nothing to clean up. Threads share nothing mutable, so a thread stopped
part way through leaves nobody else's state half made.

`Thread` is an effect in the types, but unlike the others here it cannot be
handled: the operations are the runtime's. On the VM, threads run in parallel
on every core (set `MEADOW_THREADS` to limit how many OS threads they use).
Under `--cek` they take turns on one core in a fixed order, which makes a run
repeatable.

#### Stm — shared state that changes as one step

Threads share nothing mutable, with one exception: a `TVar` from `Std.Stm`,
which every thread can see and which changes only inside `atomically`. A
transaction reads and writes as if it were alone. If another transaction
commits something it read first, it quietly runs again, so no thread ever sees
it half done.

```meadow
use Std.Stm as Stm
use Std.Thread as Thread

fun transfer from to (amount : Int) =
  Stm.atomically (\() ->
    let balance = Stm.readTVar from in
    let _ = Stm.check (balance >= amount) in
    let _ = Stm.writeTVar from (balance - amount) in
    Stm.modifyTVar to (\b -> b + amount))

fun main () =
  let a = Stm.newTVarIO (toInt 0) in
  let b = Stm.newTVarIO (toInt 0) in
  let waiting = Thread.spawn (\() -> transfer a b 30) in
  let _ = Stm.atomically (\() -> Stm.writeTVar a 100) in
  let _ = Thread.await waiting in
  println (Stm.atomically (\() -> (Stm.readTVar a, Stm.readTVar b)))
```

```
(70, 30)
```

`check` blocks the transfer until the money is there. It is `retry` underneath:
the transaction waits until something it read changes, then runs again. `orElse`
tries one transaction and, if it retries, runs another instead.

A transaction is a function of type `() -> a ! { Stm }`, which rules out
printing, spawning a thread, or touching a `Ref`. That is what makes running it
again safe. A `println` inside one is a compile error ("the effect `Console` is
not allowed here"), not a bug that shows up under load. For the same reason
transactions can't nest: `atomically` itself performs `Thread`.

A `TVar` holds what a `Compact` can: no `Ref`, mutable array or function. On the
VM its value lives in a shared region, so every thread reads it in place, and
updating a large value copies only the part that changed.
`examples/Stm` has more: an auditor that never sees a half-finished transfer,
and bounded queues.

### Writing a handler for your own effect

The shape is always the same. Declare the operations; write the code that
performs them without thinking about who answers; then write one handler per
interpretation — the real one, and the one a test wants. Each clause is one of
the choices from [What a clause can do with `k`](#what-a-clause-can-do-with-k):

- **Resume with an answer** — `op x k -> k answer`. The computation carries on.
- **Resume, then use the result** — `op x k -> f (k ())`. The clause sees
  everything that followed; `collected` and `counted` work this way.
- **Do not resume** — `op x k -> value`. The rest is abandoned; `Exn`, `take` and
  `Abort` work this way.
- **Keep `k`** — store it or return it, and resume it later from somewhere else.
  `Job.Suspended` works this way.
- **Return a function** — `op x k -> \state -> ...`, with the `return` clause
  `\state -> ...` too, then apply the whole `handle` to a starting value. This is
  how `State` threads a value and how `withInput` threads the remaining lines;
  `Std.State.runState` is the smallest complete example.

And whichever you choose, resume at most once.

## 10. Testing

Mark a function `@test` and `meadow test` runs it. A test takes one argument — the
runner calls it with `()`.

```meadow
use Std.Test (assertEq, assertTrue)

fun double n = n * 2

fun main () = println (double 21)

@test fun doubling () = assertEq (double 21) 42 "double 21"

@test fun listsCompare () = assertEq [1; 2] [1; 2] "list equality"

@test fun somethingTrue () = assertTrue (double 2 == 4) "double 2"
```

```sh
$ meadow test .
running 3 tests
test doubling ... ok
test listsCompare ... ok
test somethingTrue ... ok

test result: ok. 3 passed; 0 failed
```

A failure names both values. `assertEq` compares with the structural
primitive and prints with `show`, so it works at any type, whether or not it
derives `PartialEq` or `Debug`:

```
---- doubling ----
double 21: expected 43, got 42
```

The assertions: `assert`, `assertEq`, `assertNeq`, `assertTrue`, `assertFalse`,
`refuteThat`, `failWith`, plus `didFail` / `assertFails` for checking that
something _does_ fail.

A test fails by performing `Std.Test`'s effect rather than returning a value, so an
assertion five calls deep still stops the test and still names itself.

`meadow test <path> <filter>` runs only tests whose name contains `<filter>`, and
`meadow test --std` runs the standard library's own 154 tests. A package's tests
are its own: those of the packages it depends on run when you test them.

In a package of several modules a test is named by its module — `Parser.parses`
rather than `parses` — since two modules may each have a test of that name.
`--exact` runs only the test whose name is the filter, so
`meadow test . Parser.parses --exact` is one test and never `Parser.parsesInts`.
In VS Code, the **▶ Test** link above a `@test` runs exactly that.

Tests run **side by side**, one per core, as `cargo test`'s do. Each is a run of
its own -- its own heaps, threads, `TVar`s and channels -- so tests share nothing
inside the language, and are reported in the order they finish. What they can
still collide on is what two processes can: a file of the same name, a port.
`--test-threads 1` (or `MEADOW_TEST_THREADS=1`) runs them in order for that.
What a test prints is kept rather than written among the others' lines, and
shown under `---- name output ----` if the test fails; `--no-capture` writes it
as it comes.

---

## 11. Tooling

### Commands

|                                                                             |                                                                                                                          |
| --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `meadow`                                                                    | REPL                                                                                                                     |
| `meadow run <path>`                                                         | build and run `main`, on the VM and its JIT                                                                              |
| `meadow run --release <path>`                                               | …optimized, as an executable compiled ahead of time                                                                      |
| `meadow run --backend vm\|jit\|aot <path>`                                  | …on the Glade backend named (`--jit` and `--aot` for short)                                                              |
| `meadow run --runtime silo <path>`                                          | …on Silo: compiled by LLVM, counting references                                                                          |
| `meadow run --gc-stats <path>`                                              | …and report what the garbage collector did                                                                               |
| `meadow run --gc copying <path>`                                            | …with the copying collector instead of the generational one                                                              |
| `meadow run <path> -- <args>`                                               | …passing `<args>` to the program, which `Process.argv` reads                                                             |
| `meadow exec <image.mbc> [--backend vm\|jit] [-- <args>]`                   | run a bytecode image, like the one `meadow build` writes                                                                 |
| `meadow link <image.mbc> [-o <exe>] [--target <arch>]`                      | compile a bytecode image into a native executable                                                                        |
| `meadow link --emit asm <image.mbc>`                                        | …or into the text of its native code, `<image>.s`                                                                        |
| `meadow build <path>`                                                       | type-check, link, and write the bytecode image to `target/`                                                              |
| `meadow build --release [--target x86_64] <path>`                           | …and an executable, under `target/release/native/`                                                                       |
| `meadow build --annotations <path>`                                         | …and dump every node's type                                                                                              |
| `meadow build --emit bytecode,asm <path>`                                   | write text in place of the binaries: `bytecode/<name>.mbc.txt`, `native/<name>.s`; `image` and `exe` are the binaries    |
| `meadow dis [--asm [--target <arch>]] <path>`                               | print the bytecode the VM runs, or the native code it compiles to                                                        |
| `meadow run --cfg fast --cfg feature=gpu <path>`                            | …with flags on for `@cfg` ([conditional compilation](#conditional-compilation-cfg)); `run`, `build` and `test` take them |
| `meadow run --features gpu,simd`, `--no-default-features`, `--all-features` | …with those of the package's `[features]` on; `run`, `build`, `test` and `dis` take them                                 |
| `meadow test [<path>] [<filter>]`                                           | run `@test` functions                                                                                                    |
| `meadow test --test-threads N`, `--no-capture`                              | …`N` at a time instead of one per core; and writing what they print instead of keeping it for the failures               |
| `meadow build -p app`, `meadow test --workspace [--exclude app]`            | in a [workspace](#workspaces): the members named, or all of them; `run`, `build`, `test` and `dis` take these            |
| `meadow init [--workspace] <path>`                                          | create a package, or a workspace; a package made inside a workspace joins it                                             |
| `meadow fmt <path>`                                                         | format in place                                                                                                          |
| `meadow fmt --check <path>`                                                 | report, exit 1 if anything differs                                                                                       |
| `meadow lsp`                                                                | run the language server (editors start this)                                                                             |
| `meadow update`                                                             | replace the binary with the latest release                                                                               |

`--release` and `--debug` select a profile. Release requires every `match` to
be exhaustive, compiles a `match` to a decision tree, and copies generic code
once per representation it is used at (`Int`, `Float`, `String`, a reference,
…), so it runs as fast as code written at those types. Debug compiles faster:
generic code is compiled once and told what its values are as it runs.

Each profile also has a runtime, and Glade, the default, a backend. Debug
runs on Glade's VM, which compiles a block to machine code once it has run
often; release links an executable compiled ahead of time. The other runtime,
Silo, is compiled all the way down by LLVM and counts references instead of
collecting: it is always an executable, and has no backend to choose. A package
can choose differently in its `Meadow.toml`:

```toml
[profile.debug]
backend = "jit"    # Glade's: "jit" | "aot" | "vm"

[profile.release]
runtime = "silo"   # "glade" | "silo"
```

### Editors

`meadow lsp` speaks the Language Server Protocol over stdin and stdout, so any
LSP client can use it. It gives you diagnostics as you type, hover showing the
inferred type and the `--` comment above the definition, go-to-definition,
rename, completion, inlay hints for parameters and `let` bindings, and semantic
tokens.

**Completion after `|>`** is the one worth knowing about. Where an
object-oriented language offers a menu after a full stop, Meadow can offer one
after a pipe, and for the same reason: by then it knows what the value _is_.

```meadow
def total = [1, 2, 3] |>
--                       ^ len, head, reverse, drop ⟨…⟩, …
```

The list is what that value can be piped into, so nothing that wants another
type is in it. Two shapes fit, and both are offered:

- `x |> f` is `f x`, so a function whose **first** parameter takes the value;
- a library written to chain takes its subject **last** —
  `table |> setWidth 40` is `setWidth 40 table` — so a function whose last
  parameter takes it, offered with holes for the arguments that come first.

What comes first in the list: a parameter of that very type, before one that is
a type variable and so fits everything; something that performs no effect,
before something that does, since a pipeline is usually a chain of plain
transformations; then what this file already uses; then what the standard
library's own source uses most, which is counted rather than curated — `map`
and `foldl` are written constantly there, `splitAt` hardly ever. Whether the
value goes in first or last is the _last_ thing considered: both are ordinary,
so ranking by it would bury `map`, `filter` and `foldl` under every function
that happens to take its argument the other way round.

**Completion after a dot** is the other half of the same idea. A dot names a
path, and what follows it is whatever sits under what was named:

```meadow
use Std.
--      ^ Collections, Maybe, String, …

use Std.Maybe (
--             ^ map, unwrapOr, andThen, Maybe, …

use Std.Maybe as M

data Shape = Circle Int | Square Int

def shape = Shape.
--                ^ Circle, Square

def first = M.
--            ^ map, unwrapOr, …, and Just and None
```

A type is a path too, with its constructors under it — so `Shape.` offers
`Circle` and `Square`. `Maybe` is both a module and the type inside it, and
only the type is something a constructor can follow, so `Maybe.` offers `Just`
and `None`. A module is reached qualified only through `use … as`, which is why
`M.` offers what `M` was pointed at and a module nothing has aliased offers
nothing: writing it would not compile.

A dot after a value — `point.` — selects a field. That is a question about the
value's type rather than about a path, and nothing is offered there yet.

Only names you can write _here_ are offered. A function that would need a `use`
first is left out rather than inserted as something that does not compile.

A file inside a package is analysed as part of that package, so a `use` of a
sibling module resolves exactly as it does in a build, and go-to-definition and
rename both cross between modules. **Rename** follows name resolution rather
than text: it changes the binding under the cursor and not something else
spelled the same, and it refuses a name whose definition is outside the package
(the standard library's, say) rather than editing half of it.

The VS Code extension lives in `editors/vscode`; `editors/vscode/build.sh`
produces a `.vsix`, and every release attaches one. It runs `meadow lsp`, and
falls back to plain syntax highlighting if the executable is not on your `PATH`.
The Meadow colour theme, dark and light, is a second extension beside it in
`editors/vscode-theme`, built and attached the same way.

### The formatter

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

---

## 12. Reference

### Operators, loosest to tightest

| Operators                                               | Fixity     |                                  |
| ------------------------------------------------------- | ---------- | -------------------------------- |
| `<\|`                                                   |            | apply, right-associative         |
| `\|>`                                                   |            | pipe                             |
| `or`                                                    |            | short-circuit                    |
| `and`                                                   |            | short-circuit                    |
| `==` `!=` `<` `>` `<=` `>=`                             | `infix 4`  | comparison; do not group         |
| `::` `++`                                               | `infixr 5` | cons, string concatenation       |
| `+` `-`, `<<` `>>` `>>>`                                | `infixl 6` |                                  |
| `*` `/` `%`                                             | `infixl 7` |                                  |
| `^`                                                     | `infixr 8` | power                            |
| an operator nothing declares                            | `infixl 9` |                                  |
| `-` (prefix)                                            |            | negation, tighter than any infix |
| _juxtaposition_                                         |            | function application, tightest   |

### Always in scope

Without any `use`, from the prelude:

- **Bool** `not`
- **Ordering** `isLess` `isEqual` `isGreater`
- **Function** `id` `const` `flip` `compose` `apply` `twice`
- **Tuple** `fst` `snd` `pair` `swap` `mapFst` `mapSnd`
- **Num.Int** `min` `max` `abs` `signum` `clamp` `succ` `pred` `even` `odd` `gcd` `lcm`
- **Num.Bits** `lowMask` `lowBits` `bit` `testBit` `setBit` `clearBit` `flipBit`
  `byteOf` `fromBytes4` `countLeadingZeros` `countTrailingZeros` `rotateLeft`
  `rotateRight`
- **Bytes** `bytesLength` `bytesGet` `bytesSet` `bytesPush` `bytesSlice`
  `bytesConcat`, the `U16`/`U32`/`U64` accessors, …
- **Fs** `readToString` `writeString` `exists` `readDir` …
- **Process** `command` `run` `exit` `argv` `getEnv` …
- **Vector** `empty` `singleton` `len` `length` `isEmpty` `get` `getOr` `head`
  `last` `nth` `set` `pushBack` `pushFront` `popBack` `popFront` `append`
  `splitAt` `slice` `take` `drop` `map` `filter` `foldl` `foldr` `reverse`
  `updateAt` `forEach` `range` `elem` `notElem` `sum` `product` `sumBy` `all`
  `any` `find` `concat` `concatMap` `replicate` `takeWhile` `dropWhile`
  `partition` `zip` `zipWith` `unzip` `maximum` `minimum` `toArray` `toList`
  `fromArray` `fromList`

Also always available: the operators and their traits (`Std.Ops`: `Add`,
`Sub`, `Mul`, `Div`, `Rem`, `Pow`, `Shift`; `Std.Cmp`:
`PartialEq`, `Eq`, `PartialOrd`, `Ord`, with `compare` and `partialCmp`;
`Std.Display` and `Std.Debug`); `++`
(`Std.String.concat`); `print` and `println`; `runSt`; the effect alias `Eff`; the primitives (`show`, `display`, `hash`,
`arrayLen`, `arrayGet`, `stringToBytes`, `stringToChars`, `charCode`, `bitAnd`,
`toInt`, `toUInt8`, `toFloat`, `toBigInt`, …); and the constructors `Just`, `None`, `Ok`, `Err`,
`Less`, `Equal`, `Greater`, `True`, `False`, `Nil` and `Cons`.

### The standard library

`Ops` `Display` `Debug` `Bool` `Ordering` `Function` `Tuple` `Num` (`Int` `Bits`) `Maybe` `Cmp` `Char` `Result`
`Either` `Bytes` `Yield` `Collections` (`Vector` `List` `Tree` `Set` `Map` `HashMap`) `State` `St` `Compact` `Thread` `Stm` `Exn`
`Stream` `Random` `Fs` `Process` `String` (`Parse` (`Char` `Lexer`)) `Path` `Json` `Time` `Test` `Eff`

`Std.String.Parse` is a megaparsec-style parser combinator library -- see
[Parsing text](#parsing-text); `Std.Json` is built on it and is worth reading as
a worked example.

### Gotchas, collected

- `%` follows the sign of the dividend: `(-7) % 3` is `-1`.
- An integer literal nothing pins down is an `Int`, which wraps at 64 bits;
  annotate a result that must be exact past 2^63 as `BigInt`.
- A byte is a `UInt8`, and so is arithmetic on it: `toInt (b - 48)` before
  accumulating digits.
- `+` is for integers and floats alike, but two number types never mix
  without a conversion: `toFloat n + 0.5`, and `2.0`, not `2`, beside a float.
- `[1..5]` is **half-open**, as `range 1 5` is; `[1..=5]` includes the 5.
- `@pub` is public, as in Rust; `@pub(pkg)` stops at the package, like `pub(crate)`.
- A package that marks nothing has no visibility rules at all — mark one thing
  and every module has to say what it shares.
- Each module of a package is its own namespace; a sibling's names come by `use`.
- Exhaustiveness is only checked under `--release`.
- `def` takes no parameters — `def f x = ...` is a parse error; use `fun`.
- Selecting a field (`p.x`) or updating one (`{ p | x = 1 }`) of a _nominal_
  record needs `p`'s type known there: give the function a signature, annotate
  `(p : Point)`, or match.
- A signature has to be as general as it says: `fun f : a -> a` cannot add one.
- A signature and the clauses under it are one declaration: writing `fun f : T`
  and then `fun f x = ...` declares `f` twice, and is an error. Put the clauses
  under the signature, each opening with `| f`. The same goes for a trait
  method and its default.
- `where`, `trait` and `impl` are keywords now, and cannot name a value.
- A local function that uses a trait's method is used at one type: make it a
  top-level `fun` if it has to work at two.
- No block comments.
- `main` is a function, `fun main () = ...`: it returns `()`, performs at most
  `Eff`, and a program shows what it prints and nothing else. `def main = ...`
  is an error.
- A guarded `match` arm counts for nothing in the exhaustiveness check.
- `Std.Char`'s predicates are ASCII-only; `String` counts bytes, `Char` counts
  scalars.
- `Std.Test` is not in the prelude — `use Std.Test (assertEq)`.
- A qualifier comes only from `as`: after `use Std.Collections.List`, it is
  `length`, not `List.length`.
