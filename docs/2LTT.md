# Two-level Meadow

_A roadmap, not a spec. Draft 3. L0 is built; nothing after it is. Syntax in
it is a sketch. The choices taken on earlier drafts are in
[Decisions](#decisions); what is still open is listed after them._

Meadow is an ML: Hindley–Milner inference with let-generalisation, traits,
effect rows, general recursion, and control over how values are represented.
This document plans how Meadow gains proofs without losing any of that. The
framing is two-level type theory (2LTT).

- **The run-time level** is Meadow as it is. Opt-in erased dependent
  functions are added for stating and proving facts about run-time values.
- **The compile-time level** is a total dependent type theory. It is used for
  type-level computation, typed code generation and syntax extensions.

Both levels are checked by one kernel, **Strata**, which Meadow shares with
Idyll.

## Contents

1. [Why](#why)
2. [Words used here](#words-used-here)
3. [The design](#the-design)
4. [For agents](#for-agents)
5. [How it fits Meadow today](#how-it-fits-meadow-today)
6. [Milestones](#milestones)
7. [Risks and open problems](#risks-and-open-problems)
8. [Prior art](#prior-art)
9. [Decisions](#decisions)
10. [Still open](#still-open)

## Why

**Agents are expected to be most of Meadow's authors.** An agent writes code
fast and checks it as fast as the tools let it. What it lacks is a way to know
that what it wrote is right, other than running tests. That makes a checker's
guarantees, and the tools around a proof (goals, search, structured errors),
worth more to Meadow than they would be to a language written mostly by hand.
It is also why this roadmap puts verification and tooling first, and staging
and macros later.

**One language, for scripts and for proofs.** Meadow is quick to write.
Inference means a script needs no annotations, which suits people and agents
alike. What a Meadow program promises, though, is held up only by tests and
differential runs. The aim is that a program is written as Meadow is written
today, and proofs are added where they pay, in the same files, about the same
definitions. Code that asks for nothing pays nothing.

**Meadow and Idyll stay two languages, sharing a kernel and a back end.**
They have different first priorities:

- **Meadow is inference first.** Its strength is opt-in: proofs, indices and
  totality are there when a signature asks for them. Its compile-time level is
  for staging and syntax.
- **Idyll is dependent and graded first.** It has linearity throughout,
  low-level code with Low\*-style `Mem` and `@extract` to C, and the
  verification stack (SMT, heap proofs) that Meadow's runtime is to be written
  in ([MEADOW-IN-IDYLL.md](MEADOW-IN-IDYLL.md)).

They share **Strata** (the kernel: checker, normalisation by evaluation,
termination, staging) and **Cut** (the back end's IR), and nothing else needs
to be shared. A proof checked in Meadow is checked by the same code as one in
Idyll.

**Why two levels, and not dependent types everywhere.** If every run-time
term could appear in a type, every type would wait on termination and effect
checking, and Hindley–Milner inference would be lost. 2LTT keeps the levels
apart. Compile-time terms are total and pure, so the checker may evaluate them
freely. Run-time terms reach types only where a definition opts in, and only
the ones the checker proves total. Inference stays Hindley–Milner everywhere
else.

**Why typed code generation, and not more macros.** A Meadow macro today maps
token trees to token trees ([MACROS.md](MACROS.md) §1, §9). Its output is
type-checked only after it is spliced, so its mistakes are reported in its
caller's code, and it runs under fuel in a separate program. Custom syntax
stays, as a layer before elaboration. A compile-time function that returns
typed code is checked where it is written, its output is well typed by
construction, and it runs by evaluation inside the compiler.

## Words used here

| word | meaning |
| --- | --- |
| **run-time level** (object level, stage 0) | Meadow as it is, plus opt-in erased dependent functions. Its types live in `Type0`. |
| **compile-time level** (meta level, stage 1) | a total, pure, dependent type theory, run by the compiler. Its types live in `Type1` and above. |
| **Strata** | the kernel both levels, and Idyll, are checked by: the core calculus, NbE, unification, termination, coverage and staging. A package in this repository. |
| **`Code a`** | for a run-time type `a`, the compile-time type of run-time terms of type `a` (Kovács's `↑A`). |
| **quote** and **splice** | quote turns a run-time term into a `Code a`; splice runs a compile-time computation of type `Code a` and puts the term it gives in its place. |
| **staging** | running every compile-time computation in a program, leaving a run-time program with none. |
| **erased** | gone before the program runs. By stage: everything at the compile-time level. By relevance: run-time things at grade 0. |
| **grade 0 / grade ω** | erased / unrestricted (ω is Idyll's `[]`, 0..∞, the grade of a binder that writes nothing). The two grades Meadow's surface allows; Idyll has the full algebra. |
| **`Div`** | the one implied effect: might not return normally. |
| **shared fragment** | data types, total pure functions, and traits and impls over them, written once and usable at both levels. |
| **dictionary** | a trait's evidence at run time: a record of the impl's methods ([RUNTIME.md](RUNTIME.md)). |
| **descriptor** | a run-time value naming a representation (`desc` in [CUT.md](CUT.md)), passed to code generic over a type. |

## The design

### Two levels, and two kinds of erasure

**The run-time level is Meadow.** It keeps traits, Hindley–Milner inference
with let-generalisation, effect rows, general recursion, and representation
polymorphism by descriptors ([CUT.md](CUT.md), _Descriptors and
representation variables_). Every program that compiles today compiles
unchanged, infers the same types, and means the same.

**The compile-time level is a total dependent type theory.** It has Π and Σ
types, inductive data types with dependent matching, propositional equality,
an unbounded `Nat`, and universes. Every compile-time term terminates and has
no effect. Strata evaluates compile-time terms during conversion by
normalisation by evaluation.

**The levels meet in four places:**

1. **`Code a`**, with quote and splice. Staging evaluates every splice and
   leaves a run-time term. This is _staging by evaluation_, Kovács's analogue
   of normalisation by evaluation (ICFP 2022).
2. **Indices.** A run-time type former may take index arguments (`Vec n a`,
   `Socket Open`, `Region r`). An index is an erased run-time value. A
   compile-time value lifts into an index, never back.
3. **The shared fragment.** Its definitions exist at both levels.
4. **Erased dependent functions at run time.** These are opt-in (see
   [below](#erased-dependent-functions-at-run-time)).

**Two kinds of erasure.** _Erasure by stage_ removes everything at the
compile-time level before the program runs. _Erasure by relevance_ removes
run-time things at grade 0: proofs, and indices about values that are known
only at run time. 2LTT itself allows a dependent run-time level. The earlier
limit, that a run-time type never depends on a run-time value, came only from
keeping Meadow's run-time level non-dependent. Grade-0 erasure lifts that
limit where a signature asks for it.

```meadow
-- Sketch. Run time, unchanged: no annotation, inferred.
fun sumSquares xs = List.foldl (\acc x -> acc + x * x) 0 xs

-- Run time, opted in: the proof is a grade-0 argument, erased.
fun get (b : Buffer @0 n) (i : Int) (@0 ok : i < n) : Byte = unsafeRead b i

-- Inferred code calling it. `_` is a proof the checker finds: the `if`'s
-- run-time comparison gives it.
fun firstOr d b = if 0 < len b then get b 0 _ else d

-- Compile time: a function from a compile-time Nat and run-time code to code.
@compTime
fun power (n : Nat) (x : Code Int) : Code Int =
  match n with
  | Zero -> quote { 1 }
  | Succ m -> quote { $x * $(power m x) }

fun cube (x : Int) : Int = power 3 x   -- after staging: x * (x * (x * 1))
```

### Strata: the kernel

Strata is a package in this repository (`Strata/`). It holds the core
calculus: the checker, NbE, unification, termination and coverage checking,
and staging. MeadowBoot depends on it. It depends on no front end.

**Strata depends on Std alone, and in particular not on Lingua.** `front/`
pins Lingua at one revision, because Cut's builders take `Lingua.Lang.Meta`
and two Lingua revisions in one build are two different types. A kernel that
depended on Lingua would put every user of Strata in the same lockstep. So
Strata's terms are plain Meadow data, not a `lang!`, and where the kernel
needs a source position it has a span type of its own, or a type parameter.
L0's Strata imports nothing from Lingua.

Strata is to be built from Idyll's kernel, which Idyll's `front/` already has
in Meadow. Measured against `front/`'s imports, the kernel splits as follows.

- **Clean to extract**: `Grade`, `Core`, `Value`, `Tc` (metavariables),
  `Ctx`, `Eval` (NbE), `Unify` (pattern unification), `Termination`
  (size-change, with transparent unfolding), `Positivity`, and `Usage`.
  `Hir`'s `Icit` and `Literal` move down with them.
  - `Positivity` imports `Lingua.Lang (Meta)` for spans, the one Lingua
    import among them. It takes Strata's own span type before it moves.
- **Not clean**: `Matching`, the elaboration to case trees. It imports about
  twenty functions from `Elab` (`check`, `infer`, `insert`, `freshMeta`,
  `gradeFits`, `settleGrade` and others). The proposal is to split it:
  - case-tree compilation and coverage move into Strata;
  - the elaborator is passed in as a record of functions.

  Strata does not take Idyll's elaborator. Meadow's elaboration is
  inference first, and Idyll's is not, so each front end keeps its own.
- **`Core` is more than a dependent core.** It includes Idyll's effect rows,
  handlers (`OpClause`), effect keys, and its set of builtins and
  primitives. That primitive set is one to one with Cut's primitives, which
  are Meadow's.

That kernel is graded throughout: `Value.Pi` and `Core.Pi` each carry grades
as terms, solved as polynomial normal forms. Meadow starts with that kernel
as it is.

- At the compile-time level every grade is ω.
- On run-time binders a grade is 0 or ω.
- ω is Idyll's `[]`, the grade 0..∞. A Meadow binder that writes nothing has
  that grade.

A kernel parameterised over its grade algebra can follow if `Mem` brings more
grades to Meadow.

**How the extraction is done** is a proposal, for the user and Idyll's session
to agree (it is L1's first part):

1. **Refactor inside Idyll's repository** until the kernel modules import
   nothing from `Elab` or from Lingua. That means the span type, the
   `Matching` split, and moving `Icit` and `Literal` down. Idyll's Rust
   compiler and `front/` stay in parity at every step.
2. **Move those modules into `Strata/` verbatim.** `front/` then depends on
   Strata by revision. Nothing changes meaning in the move.

`front/` (Idyll's compiler in Meadow, a Meadow package) depends on Strata as
a git dependency with `subdir = "Strata"`. Idyll's Rust compiler does not use
Strata.

**Idyll's parity suite is an independent check of Strata's kernel.** Idyll
keeps its Rust compiler, checked pass by pass against `front/`
(`the_messages_agree`, `the_error_places_agree` and the rest). Once `front/`
runs on Strata, that suite checks Strata's kernel for the graded fragment
against a second implementation. The rule: a Strata change that touches the
kernel keeps Idyll's parity suite green, or comes with Idyll's matching
change.

### Div: the one implied effect

**`Div` means "might not return normally".** It covers looping, and the
failures no handler can catch: division or remainder by zero, a match with no
case for its value, a record with no such field. There is no separate
`Partial`.

- **At run time, `Div` is implied in every row**, never written and never
  shown. So every existing signature means what it did: `: Bool` promises no
  effect but `Div`, as it always did.
- **The termination and coverage checker removes `Div`** from a definition
  where it can prove the definition total. A definition with no effect left,
  `Div` included, is total. As in Idyll, an empty row means total.
- **`@total` asserts it** on a run-time definition: a definition so marked
  that the checker cannot prove total is an error naming the reason.
- **At compile time nothing performs `Div`.** Every compile-time definition is
  total, generators and syntax extensions included. There is no implicit
  fuel.

When the checker cannot prove a compile-time definition (or an `@total` one)
total, the error names the reason and offers the fixes that apply:

- explicit fuel: a `Nat` argument that shrinks;
- `@assumeSmaller` at the call, like Idris's `assert_smaller`;
- `@assumeTotal` on the definition. Like `@assumeSmaller` it is trusted,
  listed in the inventory of assumptions, and warned about;
- restructuring the definition so that the shrinking is visible;
- for division, the form that takes a proof that the divisor is not zero;
- for a match, the missing case.

### Totality and coverage

Termination is structural and size-change on case trees, the checker Agda and
Idyll use, shared through Strata. Measures (`decreases`) come later, with
Idyll's solver work. Coverage is MeadowBoot's exhaustiveness check (`Exhaust`),
read as "this match cannot fail". Both run on run-time definitions too, but
only to decide what `Div` can be removed from. They never refuse a run-time
definition that did not ask to be total.

### Erased dependent functions at run time

_Recommended. Whether Meadow takes it is still open: see
[Still open](#still-open)._

Meadow's run-time level gets opt-in erased dependent functions through
Strata's kernel, limited to grades 0 and ω. The full grade algebra,
linearity, Low\*-style extraction and the verification stack stay Idyll's.

- **A signature is required exactly where code opts in.** A definition with a
  dependent or graded binder states its type. Everything else is inferred as
  today.
- **Inferred code calling a dependent function is an ordinary call.** Its
  implicit arguments, indices included, are solved by unification. A proof
  argument is found by the checker where it follows from what is in scope
  (the branch of an `if` on a decidable comparison, an earlier match),
  or left as a goal.
- **An index is an erased run-time value by default.** A compile-time value
  lifts into it. A run-time value at grade 0 never flows back into computation.
- **Usage is checked only on definitions that write a grade.** Scripting pays
  nothing.
- **Only total run-time terms reach types.** A run-time function may appear in
  a type only where the checker has removed its `Div` and it has no other
  effect. This is Idyll's rule that types depend only on terms whose row is
  empty.
- **Proofs about run-time functions live at the run-time level, erased.**
  `@0 reverseTwice : (xs : List a) -> reverse (reverse xs) == xs` is a
  run-time definition at grade 0, about the run-time `reverse` itself. No copy
  is involved, so nothing about the compiler has to be trusted to make it
  apply.
- **Grade 0 is written `@0` on a binder** (suggested; still open). Idyll's
  `[r]` cannot be used because Meadow's lists are written `[a; b]`. `@0`
  clashes with nothing in Idyll.
- **A binder that writes nothing is ω**, Idyll's `[]`: unrestricted, as every
  Meadow binder is today.

### The compile-time level

Compile-time definitions are marked `@compTime`, or inferred to be
compile-time from a signature that mentions a compile-time type (`Type`,
`Code a`, `Nat`, a Σ). Today's `@compileTime def`, which is read as a
`Datum` and never evaluated, becomes `@compTime def`, which is evaluated. The
old spelling is kept for one release, with a warning.

Compile-time code is total and pure. Two compiler-provided effects are
proposed for it, `Fail` (an error at a place, which ends the computation) and
`Expand` (compile-time bindings, as macros have today). Both terminate, so
neither is `Div`. Whether they are effects or ordinary values is still open.
Compile-time code never performs `Fs`, `Time` or `Random`, so its answers stay
cacheable.

### Stage inference

Quote and splice are mostly implicit, as in Kovács's design. His demo infers
them during bidirectional elaboration, by coercive subtyping between `a` and
`Lift a`. It has no stage metavariables, and it wants every `let` to say its
stage. Meadow follows the same split.

- **A definition's level is fixed before its body is elaborated.** It is
  fixed by `@compTime`, by a signature that mentions a compile-time type, or
  by default. **The default is run time.** So every existing definition is
  what it was.
- **Inside a body, quotes and splices are inserted by checking.** A run-time
  term where `Code a` is expected is quoted. A compile-time term of type
  `Code a` where a run-time `a` is expected is spliced. A literal takes the
  level of its expected type.
- **The run-time part keeps today's inference.** The bidirectional checker
  runs only on compile-time terms, dependent signatures, and the borders. A
  compile-time call inside run-time code is checked against the type the
  run-time inference has at that point. Where that type is still unsolved,
  the call waits, as a trait predicate waits for its type.
- **Where inference cannot decide, `quote { e }`, `$x` and `$(e)` settle
  it.** It is the spelling `quote!` and macro templates use now. It adds no
  reserved word. The `$` spellings are decidable by an LL automaton, which
  `<e>` is not.

### The shared fragment

**What is in it.** A data type is shared when its fields are of shared types.
A function or `def` is shared when:

- after the checker removes `Div`, its row is empty, apart from the effect
  variables of functions it is given (`map (f : a -> b ! e) xs : List b ! e`
  is shared);
- every primitive it calls has a compile-time model that agrees with the
  runtime;
- everything it names is shared.

A trait is shared when its methods' types are shared. An impl is shared when
its methods are. Primitives that can fail (`/`, `%`, out-of-range indexing)
perform `Div`, so a definition that uses them is not shared until it uses the
forms that take a proof.

**Membership is inferred.** The check records each definition's membership in
the unit's interface. A definition that fails it stays run-time-only,
silently, with nothing about it changed. `@shared` asserts membership, and
failing it is an error naming the reason. A compile-time use of a definition
that has left the fragment is an error at the use, naming why it left.

**What it is for.** A shared definition can run at compile time (type-level
computation, generators, syntax extensions) and at run time, written once.
`List`, `map` and `Ord Int` are not written twice. Its compile-time copy is
the same syntax read in the compile-time theory: effect rows are emptied, and
dictionaries become compile-time records. With erased dependent functions at
run time, proofs about a shared definition are stated about its run-time
copy, so they need no transfer between copies.

**Cross-stage persistence.** A value of a shared, first-order data type can be
_lifted_: a compile-time `List Int` becomes the run-time code that builds it.
This is derived per shared data type. It plays the part that `ToTokens` and
`Datum` play for macros today ([MACROS.md](MACROS.md) §10), but it is typed.

**The meaning must be the same at both levels.** That the two copies agree is
a property of the compiler. It is trusted, listed in the inventory, and tested
differentially ([L10](#l10-stage-polymorphism)). It matters for what a
generator computes; with run-time proofs it no longer matters for what a proof
says.

- **`Int` is 64-bit two's complement and wrapping at both levels.** Its
  compile-time model is arithmetic modulo 2^64 read as signed. So
  `x + 1 > x` is not a theorem about `Int`. The same holds for `Int8` to
  `UInt64` at their widths.
- **`Nat` is unbounded, for proofs and indices.** A shared unbounded integer
  type (`BigInt`) can come later.
- **`Float` is shared only bit for bit.** The compile-time evaluator must use
  the runtime's operations (`meadow_rt::num`), and the fragment leaves out
  anything whose result differs between platforms, such as the C library's
  transcendental functions. Otherwise a cross-compiled program would compute
  one thing when compiled and another when run.
- **`String` is shared with the semantics `meadow_rt::text` gives it**, once
  that semantics is written down as a compile-time model.

### Coherence

There is one impl of a trait per type, across both levels and every package
of a build. A shared impl such as `Ord Int` is one impl, never two. Coherence
is what makes "the `Ord Int`" something to prove laws about, and what lets a
dictionary be chosen at compile time with no run-time search. Today
MeadowBoot keys an impl by its trait and head constructor
(`Traits.registerImpl`). I found no orphan rule, and coherence across
packages needs one. The proposal is Rust's: an impl lives with its trait or
with its type's head constructor. Idyll has the same rule for its unnamed
instances.

### Traits at each level

**At the compile-time level** a trait is a record whose fields may be types,
and an impl is a canonical value of it, found by instance search during
elaboration. This is Idyll's design: traits are auto-implicit records, and an
associated type is a field that is a type. Compile-time dictionaries are
evaluated away by staging.

**At run time** traits stay as they are: a predicate per `where`, a
dictionary per predicate, and an associated type carried as one more type
variable of the predicate, settled by the impl ([RUNTIME.md](RUNTIME.md),
_Traits are dictionaries, and then they are not_). Instance resolution is
always at compile time. Dictionary passing is kept because it gives
polymorphic recursion, trait objects and existentials, and separate
compilation: a constrained function compiled in a package that has never seen
its caller's types.

**Trait members come in three kinds:**

| kind | examples | where it lives | strategy |
| --- | --- | --- | --- |
| **ordinary methods** | `compare`, `take1`, `(+)` | in the dictionary | dictionaries or specialisation, by build setting |
| **erased members** | laws (`compare x x == Equal`), compile-time associated items (a width, a layout) | nowhere at run time | erased; checked or computed at compile time |
| **code-generating members** | a staged `compare` returning `Code Ordering`, a derive | nowhere at run time | always run at compile time |

A `where C a` elaborates to two things: the run-time dictionary (ordinary
methods, associated types' descriptors, the superclasses' run-time parts), and
an erased record of the erased and code-generating members.

- **A law is a grade-0 member, proved in each impl** about the impl's own
  run-time methods. It needs those methods to be total, with `Div` removed.
  In generic code a law is a hypothesis about an abstract `a`. That costs
  nothing per instantiation, so polymorphic recursion is unaffected.
- **An impl whose methods are not provably total cannot prove its laws.**
  Its laws are assumed: they are listed in the inventory as trusted, and each
  is turned into a property test that `meadow test` runs with Clover.
- **A code-generating member runs at compile time**, so it needs its instance
  at compile time. A generic function that uses one is staged at each use, as
  a macro is, and is always specialised. Polymorphic recursion through it is
  refused, because staging it would not end.

```meadow
-- Sketch: a trait with an ordinary member and a law.
@pub trait Ord a <: Eq a, PartialOrd a {
  fun compare : a -> a -> Ordering
  @law fun reflexive : (x : a) -> compare x x == Equal
}
```

### Associated types at run time

Today an associated type is lifted into the scheme of the function that asks
for it (`Traits.liftAssocs`), and nothing below inference knows it exists.
That works while every generic function is called by someone who knows the
types. It does not work for trait objects, existentials, or a dictionary
stored in a data structure, where the code holding the dictionary did not
choose the types.

**A dictionary carries a descriptor for each of its trait's associated
types.** For `Stream s`, the dictionary holds `Token s`'s descriptor beside
`take1` and the rest. An impl fixes it: a constant for `Token String = Char`;
for `impl Stream (List a)` with `Token (List a) = a`, the dictionary
constructor takes `a`'s descriptor and stores it. Code generic over `s` reads
the descriptor out of the dictionary and binds `Token s`'s representation
variable to it.

Swift's witness tables have the same shape: IRGen gives each associated type
a slot for its type metadata (`lib/IRGen/GenProto.cpp`). Idyll does this
already, where "a record's field that is a type holds its descriptor" (Idyll
`ROADMAP.md`, Phase 4, _Associated types_).

**What Cut needs.** Today a Cut definition binds a representation variable
only to a descriptor parameter (`<'a = da>`). Two extensions follow:

1. binding a representation variable to a descriptor read from a field
   (`case { Stream#dict('t = dt: desc, take1: ptr, …) => … }`);
2. the existential package (next section): data with a descriptor field, or
   the boxed `any` representation that [CUT.md](CUT.md) has in its grammar
   but has not implemented.

A function's own `where` may keep today's flattening, with each associated
type passed as one more descriptor parameter beside the dictionary, as Idyll
passes them. Cut is shared with Idyll, so both changes go through
[CUT.md](CUT.md) and Idyll's review, under its versioning rule.

### Existentials

**General existentials**, `exists a. C a => T a`, with `dyn C` as shorthand
for `exists a. C a => a`.

- **Packing happens where a value is checked against the type.** The witness
  is inferred, so `let s : dyn Stream = "text"` packs `String`.
- **Opening happens at the use.** A use of a package gets a fresh rigid type
  for the witness, and an escape check refuses a result whose type mentions
  it.
- **Existential types are never inferred from use.** They come only from
  annotations and signatures, so Hindley–Milner inference keeps principal
  types, and code that writes none behaves exactly as now.
- **At run time a package holds a descriptor, the dictionaries and the
  value**, boxed by default. The associated types' descriptors come with the
  dictionaries. Unlike Rust's `dyn Iterator<Item = T>`, a Meadow trait object
  need not fix its associated types (`dyn Stream`, or `dyn Stream { Token =
  Char }`). Code that opens one gets an abstract `Token s` whose
  representation is known at run time.
- **At compile time an existential is a Σ type.** So an existential over
  shared types is in the shared fragment.
- **Existentials over indices** (`exists (n : Nat). Vec n a`,
  `exists r. Region r`) quantify the same erased index variables that `Mem`
  regions use. They hold no descriptor, since an index is not a type a value
  has.

They are phased: `dyn` in L5, general existentials in L5b, existentials over
indices in L7.

### Dictionaries or specialisation: a build setting

Both strategies give the same answers. Which one a build uses is a setting,
`traits = "dictionaries" | "specialise"`, in `[profile.*]` and by `--set`. It
replaces today's `dictionaries : Bool` option.

- **Debug: dictionaries**, for compile speed, plus what O1 does today (a
  method asked of a known impl is read directly, `Dictionaries.mw`).
- **Release: specialisation.** Every call with known dictionaries gets a copy
  for them, transitively, keyed by representation as the release specialiser
  keys copies. Dictionaries remain only where they must: polymorphic
  recursion, existentials, and generic code called with a dictionary it was
  handed.
- **`@specialise`** always specialises, in every profile. A code-generating
  member implies it. **`@dictionaries`** never copies, for code size.

The copying is done by the back end's specialise-on-known-arguments pass over
Cut. A dictionary and a descriptor are both known arguments there, Idyll's
dictionaries are the same records, and one pass serves both front ends.
MeadowBoot's `Inline` keeps its known-dictionary reads for debug builds.
Unspecialised code still runs by reading its descriptors, as
[CUT.md](CUT.md) requires.

### Erasure

Before the program runs, everything at the compile-time level is gone:
compile-time definitions and dictionaries, staging, compile-time indices. So
is everything at grade 0: proofs, laws, and erased indices. The run-time copy
of a shared definition is kept only where run-time code reaches it, which
linking's pruning already decides. An index needs no descriptor. A run-time
value derived from an index is an ordinary value, made by lifting it. Cut
stays free of source types, as it is now.

### Indices

A run-time type former may take index parameters:

```meadow
-- Sketch: a run-time data type indexed by an erased Nat.
data Vec (@0 n : Nat) a where
  | Nil : Vec 0 a
  | Cons : a -> Vec n a -> Vec (n + 1) a
```

- **Index variables are generalised like type variables.** A function over
  `Vec n a` is generic in `n` as it is in `a`, with nothing written, and `n`
  is erased.
- **Index equations are solved by Strata.** `Vec (n + 1) a` against
  `Vec (m + 1) a` normalises. An equation it cannot decide is a goal. A
  solver can come later; Idyll's planned SMT interface is the one to share.
- **Matching on an indexed constructor refines indices**, as a GADT match
  refines types. That is where annotations become necessary, and only in code
  that uses indexed types.
- **Run-time facts reach indices through dependent functions and
  existentials.** A comparison gives a proof (`if i < len b then get b i _
  else …`). A value whose index is known only at run time is an existential over
  it (`exists n. Buffer n`).

### Proofs: four routes

The compile-time level cannot look inside a run-time term. That keeps
staging sound and leaves the run-time level free to optimise. So a proof
about run-time behaviour takes one of four routes:

1. **At run time, erased.** A grade-0 proof about the run-time function itself
   (`reverseTwice` above). This is the main route for proofs about values,
   and it needs the function to be total.
2. **Indices on run-time types.** Invariants carried in types (lengths,
   bounds, protocol states, regions) are checked as the program is
   elaborated. The program is proved by being well typed.
3. **Correct by construction.** Write the algorithm at compile time, prove it
   there, and stage it down. Examples are a staged parser or serialiser proved
   to round-trip, or memory code generated from a model of `Mem`.
4. **A reflected semantics of the run-time language, later.** A compile-time
   data type of run-time syntax, a compile-time definition of what it means
   (Cut's reference interpreter, written there), and quote able to hand a
   term's syntax to the compile-time level. This deliberately breaks the
   opacity of `Code`, and it is research.

### Macros and syntax extensions

**Custom syntax stays, as a layer before elaboration**, which is Lean 4's
model: syntax becomes syntax, hygienically, and only then is it elaborated.
Meadow's own grammar stays fixed and LL. An extension's syntax lives inside a
macro call's brackets (`name! { … }`), as it does today
([MACROS.md](MACROS.md) §1).

- **Declarative rules**, as today's `macro` declarations, which follow Rust's
  `macro_rules`.
- **Procedural syntax extensions** as `@compTime` functions over tokens and
  syntax trees. They replace `@macro` functions run on Glade.
- **Hygiene, and source positions carried through expansion.** A C-like eDSL,
  such as a Low\*-style `low { … }` block, reports its errors against its own
  source.
- **Extensions are total, like all compile-time code.** A parser that consumes
  tokens shrinks its input. A recursive rule must recurse on part of what it
  matched. Fuel, where it is needed, is an explicit argument.
- **Later, extensions that ask the elaborator for types**, as Lean's `elab`
  does. One example is a derive that reads a declaration's checked type.
- **Typed code generation** (`Code a`, staging) is a separate thing, and it
  comes after.

| today ([MACROS.md](MACROS.md)) | after |
| --- | --- |
| `macro` rules | the same, checked total |
| procedural macro `[TokenTree] -> [TokenTree]`, on Glade, under fuel | `@compTime` function over tokens or syntax trees, total, evaluated in the compiler |
| `quote!` with `$x` | the same spelling; `quote { … }` for typed code |
| `@derive(X)`, built in (`Derive.mw`) or procedural | a `@compTime` function over a declaration's syntax (later, its type) |
| `Expand`, `lookup` and `define` of a `Datum` | compile-time definitions, exported and typed; `Datum` kept for the token interface |
| `@compileTime def`, read as data | `@compTime def`, evaluated |

Lingua's `syntax!`, `lang!` and `pass!` are the largest users of macros in the
repository, and Lingua is its own project. Whether they pass the totality
checker as they are is the first thing L8 measures. They move when Lingua's
session decides to.

### Staging

Staging runs after elaboration and before MeadowBoot's lowering to core
(`Elab.mw`). Its output is an ordinary typed run-time program, so everything
from `Elab` down (dictionaries, `Inline`, `Seq`, Cut, Glade and Silo) is
untouched by it.

- **Open evaluation, for checking.** Strata's NbE over terms with free
  variables, for conversion, index equations and laws.
- **Closed evaluation, for staging and syntax extensions.** By the time a
  splice or an extension runs, its computation is closed. An interpreter is
  enough at first. Compiling heavy compile-time code and running it on Glade,
  as procedural macros run now, is the answer if L8 finds it slower than
  macros are today.

## For agents

These come early, in L4. Most of them serve people as well.

- **Proof tooling in the REPL and the language server.** It is ported from,
  or shared with, Idyll's, which already has:
  - named goals `?name`, each a warning with its type and context;
  - proof search (`:prove`), which proposes terms and keeps only what the
    checker accepts;
  - tactic mode (`:tactic`, `:by`, `:qed`);
  - `Idyll agent`, an MCP server (Lingua's `mcp!`) whose tools are `check`,
    `goals`, `typeAt`, `search`, `prove`, `tactic` and `fill`, each answered
    as JSON over the same session the REPL and the language server use
    (`front/src/Harness.mw`).

  Meadow's would be the same engine reached three ways.
- **Machine-readable diagnostics.** Every error and warning as JSON, with its
  kind, place, the types involved, and the fixes it offers. One case is the
  list of fixes a totality error gives.
- **Fast incremental checking.** One definition re-checked after an edit,
  through Lingua's query engine, not the whole unit.
- **Specifications that read well to a person.** Signatures, laws and
  indices are the specification an agent writes and a person reviews, so
  `meadow doc` and hover show them as written, laws included.
- **An inventory of every trusted assumption.** One command, and one view in
  the editor, lists every `@assumeTotal`, `@assumeSmaller`, assumed law,
  native operation and trusted compiler property. A person can then see in
  one place what a proof rests on.

## How it fits Meadow today

### MeadowBoot only

New language work lands in MeadowBoot (`bootstrap/`), never in the Rust front
end. The next release ships MeadowBoot as `meadow` with the Rust front end
beside it for that one release, and then the Rust front end is deleted.
Nothing here is ported to it.

That removes the Rust compiler as the oracle for the differential tests. For
this work the oracles are:

- **MeadowBoot's own output from before each change**, frozen as golden
  dumps: `infer`'s scheme dumps, and the Cut each package compiles to;
- **the back end's Cut reference interpreter** (`meadow_cut::interp`), which
  defines what a program means and depends on no front end;
- **the compile-time evaluator against the run-time copy**, for the shared
  fragment.

MeadowBoot's rules hold here too:

- Every stage is a Lingua `lang!` and every transformation a `pass!`. Solver
  code is library code, as `Solve` and `Traits` are, and as Strata is.
- The grammar stays LL(k) with no backtracking.
- Names obey Meadow's scoping rule. A `use` of a shared definition brings it
  at both levels, and compile-time definitions are exported only by `@pub`.
- MeadowBoot builds itself, and a release waits for that. Every milestone
  keeps the self-build green, with stage 2 writing the Cut it was made from.

### Where it goes in the pipeline

```text
parse → fixity → @cfg + macros and syntax extensions → rename
      → infer (HM, unchanged) → Strata: dependent signatures, compile-time
        terms, borders; Div removal; shared-fragment membership
      → stage → Elab → Inline / dictionaries → Seq → Cut
```

`Elab.mw` is today's lowering from the typed tree to core, where dictionary
applications are written. It stays the lowering, and sees only run-time code
with grade-0 terms removed. A unit's interface (`Packed`) grows to carry
compile-time definitions as terms, so that a dependent can run them, and
shared definitions' membership and laws. Idyll found that exporting normal
forms made its Std 18 MB, so terms are exported as written, closed over what
precedes them.

### The back end: Cut, AxCut, representation polymorphism

The back end changes little. The compile-time level and grade-0 terms never
reach Cut. A shared definition's run-time copy is ordinary Cut. The
extensions are the two of [Associated types at
run time](#associated-types-at-run-time). The polymorphism plan is already
that front ends turn type abstraction into descriptor parameters, and that
one back-end pass specialises on known arguments. That plan is what lets the
dictionaries-or-specialisation setting be a single back-end decision.

The compile-time level can also compute representations. A run-time type
indexed by a compile-time width (`Word w`) has its representation chosen at
staging, so no representation variable is needed. Representations and
layouts as compile-time values are where this meets `Mem`.

### Idyll

Idyll is a separate language and stays one. It shares two things with
Meadow:

- **Strata**, built from Idyll's kernel by an extraction agreed with Idyll's
  session (see [Strata](#strata-the-kernel));
- **Cut**, whose changes are reviewed by both front ends.

How a Meadow package and an Idyll package use each other is still open. Today
they can meet only through Cut's exported symbols, with no shared data types.
Idyll's repository is Idyll's. Nothing here edits it.

### The `Mem` effect

`Mem` is Idyll's design (Idyll `docs/LOW.md`): C's memory model as an effect,
with buffers whose lengths are in their types, liveness and disjointness as
erased propositions about a ghost heap, a model handler that runs it in pure
code, and extraction to C. On Meadow's side it serves
[MEADOW-IN-IDYLL.md](MEADOW-IN-IDYLL.md): the runtime written in low-level
Idyll.

Meadow does not design its own `Mem`. When Idyll's is built, Meadow adopts it,
and Meadow's indices (L7) are built so that it fits:

- **Identity and lifetime** of a region or buffer come from abstract, erased
  index variables introduced by rank-2 functions, as `runSt` introduces `s`.
  An existential over one (`exists r. Region r`) is how a region is returned.
- **Static sizes and layouts** are compile-time values.
- **Lengths known only at run time** are erased run-time indices with proofs
  from comparisons, as in `get` above.
- **The ghost heap** and pre- and postconditions appear only inside
  proof-carrying modules, as Idyll designs them.

## Milestones

Two invariants hold from L0 on, and every milestone keeps them green:

- **corpus**: every package in the repository infers the schemes it did
  before;
- **self-build**: MeadowBoot compiles itself, and stage 2 writes stage 1's Cut
  byte for byte (`scripts/meadowboot-selfbuild.sh`).

"Byte-identical Cut" below means the Cut each package of the repository
compiles to, compared with the golden output of the MeadowBoot from before
the milestone.

The order is set by who writes Meadow. Agents are expected to be the main
authors, so verification and the tools for it come first. The rest of the
verification work follows. Staging, macros and syntax extensions come last.
L5 and L5b need only L1, and can run in parallel with L2 to L4.

| milestone | what | needs |
| --- | --- | --- |
| [L0](#l0-strata-exists) | Strata exists, wired in behind an option | — |
| [L1](#l1-a-two-level-core-run-time-only) | a two-level core, everything at run time | L0 |
| [L2](#l2-div-totality-and-coverage) | `Div`, totality and coverage | L1 |
| [L3](#l3-erased-dependent-functions-at-run-time) | erased dependent functions at run time | L2 |
| [L4](#l4-tools-for-agents-and-proofs) | tools for agents and proofs | L3 |
| [L5](#l5-run-time-associated-types-dyn-and-the-strategy-setting) | associated-type descriptors, `dyn`, the strategy setting | L1 |
| [L5b](#l5b-general-existentials) | general existentials | L5 |
| [L6](#l6-laws-erased-members-and-coherence) | laws, erased members, coherence | L3, L5 |
| [L7](#l7-indices) | indices, existentials over them, `Mem`-ready regions | L3, L5b |
| [L8](#l8-macros-and-syntax-extensions) | macros and syntax extensions | L2 |
| [L9](#l9-the-compile-time-level-and-staging) | the compile-time level and staging | L3 |
| [L10](#l10-stage-polymorphism) | stage polymorphism, code-generating members | L9, L6 |
| [L11](#l11-later) | reflected semantics, `elab`-style extensions, `Mem` with Idyll | — |

### L0. Strata exists

_Proves_: a kernel package can sit under MeadowBoot without changing what it
does.

**Done.** What L0 built:

- **`Strata/`**, a package with no dependencies. It holds what Strata knows
  so far: the two levels, and the rule that a definition is at run time
  unless it says `@compTime` (`Strata.Level`). It has its own tests.
- **MeadowBoot depends on it** by path. A build option, `strata`, off by
  default, has each unit's declarations asked of Strata for their level.
  Every one is at run time until L1, and a `@compTime` declaration is refused
  with a message that says so.
- **CI** runs Strata's tests and formatting check (`scripts/check.sh`). The
  self-build script copies Strata beside MeadowBoot's sources.

_Tested by_:

- `meadow test Strata`, on Glade and on Silo;
- byte-identical Cut, debug and release, for every example package, with the
  MeadowBoot from before L0, with L0's, and with L0's under `strata=true`;
- the same three compilers compiling MeadowBoot's own sources from before L0
  (47 MB of Cut), byte-identical;
- the self-build, stage 2 reproducing its Cut;
- a package with a `@compTime` declaration, which builds and runs with the
  option off and is refused with it on.

### L1. A two-level core, run time only

_Proves_: the elaborator can be replaced without changing anything.

- **First, the extraction** ([Strata](#strata-the-kernel)), if the user
  and Idyll's session agree to it. The kernel modules are refactored inside
  Idyll's repository until they import nothing from `Elab` or Lingua, with
  Idyll's Rust compiler and `front/` in parity at each step. They are then
  moved into `Strata/` verbatim, and `front/` depends on Strata by revision.
- **Then the levels.** Strata's core (plain Meadow data, no `lang!`) gains
  two universes, `Code`, and quote and splice.
- **Then MeadowBoot's elaboration into it.** Under the `strata` option,
  MeadowBoot's typed tree elaborates into the core, with everything at run
  time and grade ω, and staging is the identity.

_Tested by_:

- Idyll's parity suite, green on `front/` running on Strata, for the move;
- byte-identical Cut over the whole repository, option on against option
  off;
- the corpus and self-build invariants.

_Done when_: `front/` runs on Strata with its parity suite green, and the
option can be on in CI with no difference.

### L2. Div, totality and coverage

_Proves_: Meadow can tell which of its definitions are total, without asking
anything of code that does not care.

- Implied `Div`, and its removal by Strata's termination checker
  (structural and size-change) and coverage (`Exhaust`).
- `@total`, `@assumeSmaller`, `@assumeTotal`, and errors that offer the
  fixes.
- Shared-fragment membership recorded in interfaces, and `@shared`.
- A report of each definition's membership and, for each non-member, its
  reason.

_Tested by_:

- a golden report over Std and the examples;
- termination cases ported from Idyll's corpus, accepted and rejected, each
  rejection pinned to its message;
- the corpus invariant, since `Div` must never change a scheme as printed.

_Done when_: the report for Std is reviewed. It is the first real evidence of
how much of Meadow is total as written.

### L3. Erased dependent functions at run time

_Proves_: a run-time fact can be stated and proved in Meadow, and costs
nothing at run time. This depends on the open decision to take it.

- Dependent signatures with `@0` binders. Implicit arguments, indices included,
  are solved by unification. A usage check runs only on definitions that
  write a grade.
- Grade-0 proofs about total run-time functions, including proofs taken from
  comparisons.
- Checked forms of the partial primitives (`div` with a proof that the
  divisor is not zero).

_Tested by_:

- accepted and rejected programs;
- erasure: a program with proofs compiles to Cut byte-identical to the same
  program with them removed;
- the corpus invariant, since code that writes no grade is unaffected;
- proofs of a few Std facts (`reverse` twice, `length` of `append`) checked
  in Std's build.

_Done when_: those proofs check, and erasure holds.

### L4. Tools for agents and proofs

_Proves_: an agent can do type-driven development and proofs in Meadow
through structured tools.

- Goals (`?name`), proof search and tactic mode in MeadowBoot's REPL and
  language server, ported from or shared with Idyll's `front/` (through
  Strata where the engine is kernel code).
- An MCP server over the same session, with Idyll's tool set.
- Diagnostics as JSON.
- Incremental re-checking per definition.
- The inventory of trusted assumptions.

_Tested by_:

- scripted sessions against the REPL, the language server and the MCP
  server, with answers compared with golden JSON;
- the language server's differential, which must keep agreeing where it did;
- re-check latency after a one-line edit, measured by
  `scripts/bench-meadowboot.py`, which must not regress.

_Done when_: an agent can open a goal, search, fill it and see the file
check, using only the MCP tools.

### L5. Run-time associated types, `dyn`, and the strategy setting

_Proves_: dictionaries carry what generic code needs, and the two strategies
agree. Needs only L1.

- Dictionaries carry associated types' descriptors. Cut gains a representation
  variable bound from a field, and the existential package, reviewed with
  Idyll under [CUT.md](CUT.md)'s versioning.
- `dyn C`, with associated types optional.
- `traits = "dictionaries" | "specialise"`, with profile defaults, and
  `@specialise` and `@dictionaries`.

_Tested by_:

- every program of the back-end differential corpus, and Std's tests, under
  both settings, with answers compared on the reference interpreter, Glade
  and Silo;
- a lint that, under `specialise`, no dictionary is left at a monomorphic
  call;
- polymorphic recursion and `dyn` run under both settings;
- MeadowBoot's `Lint` (representations that meet agree), and Silo under the
  cycle collector and the smallest nursery, where a wrong descriptor shows up
  as a collector fault.

_Done when_: the whole corpus agrees under both settings, and release
defaults to `specialise` with no benchmark slower.

### L5b. General existentials

_Proves_: `exists a. C a => T a` packs by checking, opens with an escape
check, and is never inferred from use.

_Tested by_:

- accepted and rejected programs, escape errors among them;
- the corpus invariant, since no existing program writes one;
- L5's differential extended to programs that pack and open.

_Done when_: `dyn` is sugar over it, and both pass.

### L6. Laws, erased members, and coherence

_Proves_: `Ord Int` is written once, its laws are proved once, and coherence
holds across packages.

- `@law` as grade-0 members, proved in impls whose methods are total.
- Laws of impls that are not provably total are assumed, listed, and
  generated as Clover property tests.
- Compile-time associated items.
- The orphan rule.

_Tested by_:

- golden coherence and orphan errors;
- Std's `Eq` and `Ord` laws on `Int`, `Bool`, `Char`, `List` and tuples,
  proved in Std's build;
- the generated property tests run by `meadow test`;
- byte-identical Cut, since laws leave no trace.

_Done when_: Std states its order laws, proves those it can, and tests the
rest.

### L7. Indices

_Proves_: invariants in types, at no run-time cost, ready for `Mem`.

- Run-time type formers with erased index parameters, generalised like type
  variables, with equations solved by Strata and GADT-style refinement.
- Existentials over indices.
- Regions through rank-2 index variables.

_Tested by_:

- accepted and rejected programs: `Vec`, a bounds-checked buffer, a socket
  typestate;
- erasure: each compiles to Cut byte-identical to the same program written
  without indices;
- the corpus invariant.

_Done when_: those examples check and erase. The typestate example also needs
an answer to its linearity risk.

### L8. Macros and syntax extensions

_Proves_: custom syntax works as total compile-time code, and reports errors
in its own source.

- First, an inventory: each of today's macros, Lingua's DSLs included, put
  through the totality checker as it is, with the result and the reason for
  each failure.
- Then:
  - declarative rules checked total;
  - procedural extensions as `@compTime` functions over tokens and syntax
    trees;
  - hygiene, and positions through expansion;
  - the built-in derives rewritten as extensions.

_Tested by_:

- a small C-like DSL (a `low { … }` block of loops and array reads over
  `Int`), whose errors are reported in its own source and whose output runs
  on the reference interpreter;
- for every derive in the repository, old path against new, byte-identical
  Cut, the self-build included;
- build times by `scripts/bench-meadowboot.py`, which must not regress.

_Done when_: the DSL works, the built-in derives are extensions, and the
inventory is reviewed.

### L9. The compile-time level and staging

_Proves_: typed code generation works, with quotes and splices inferred.

- `@compTime` definitions, `Code`, quote and splice inserted by checking,
  staging by evaluation, and `meadow build --emit staged`.

_Tested by_:

- accepted and rejected programs;
- staging goldens, comparing `--emit staged` output with the expected
  run-time program (Kovács's `power`, map fusion, a staged interpreter);
- every staged program run on the Cut reference interpreter against an
  unstaged evaluation in the compile-time evaluator;
- byte-identical Cut for the corpus.

_Done when_: those examples stage to the expected code and run.

### L10. Stage polymorphism

_Proves_: one definition, two copies, one meaning.

- Compile-time copies of shared definitions, and lifting of first-order
  shared values.
- Code-generating trait members, with forced specialisation.

_Tested by_ a differential of the compile-time copy against the run-time copy.
For each shared function in Std, generated inputs are given to the
compile-time copy in the evaluator, and to the run-time copy on the reference
interpreter, Glade and Silo, at every optimisation level, and the answers are
compared. The inputs come from Clover, weighted to the edges: overflow,
`minInt`, shifts, empty and long lists.

_Done when_: every shared function in Std passes.

### L11. Later

- A reflected semantics: Cut's meaning as a compile-time definition, and one
  staging step proved against it.
- Syntax extensions that ask the elaborator for types.
- `Mem` with Idyll, once Idyll's is built.
- A correct-by-construction library: a staged serialiser proved to
  round-trip.

### Labels in earlier drafts

| draft 1 | draft 3 |
| --- | --- |
| L1 two-level core | L1 |
| L2 shared-fragment classification | L2, with `Div` |
| L3 meta level and staging | L9 |
| L4 stage polymorphism | L10 |
| L5 run-time traits | L5, L5b |
| L6 meta traits, laws, coherence | L6 (laws at run time), L10 (code members) |
| L7 indices | L7 |
| L8 macros | L8, now macros and syntax extensions |
| L9 proofs | L3, L6, L11 |
| — | L0 Strata, L4 tools for agents |

## Risks and open problems

- **The border between inference and checking.** Hindley–Milner and
  bidirectional dependent checking in one elaborator must stay predictable. A
  call whose expected type is unsolved has to wait, and the order things wait
  in must not change a program's meaning. Meadow's trait predicates already
  wait this way.
- **`Div` must stay invisible.** Implying it in every row is safe only if
  nothing that prints, compares or exports a row shows it. Interfaces must
  record which definitions had it removed, and the corpus invariant is the
  check.
- **Membership drift.** An edit that makes a function possibly non-total
  breaks proofs downstream, far from the edit. `@total`, `@shared`, the
  interface record and errors at the use are the mitigation.
- **Totality is weaker than people expect.** Std's `range` and `replicate`
  recurse on an `Int` that is not structurally smaller, so they keep `Div`
  until measures exist, or `@assumeSmaller` is written.
- **No implicit fuel at compile time.** Today's procedural macros, and
  Lingua's grammar generator above all (LL automaton construction, fixpoint
  iterations), run under fuel. Some will fail the checker as written. L8's
  inventory measures how many, and explicit fuel is the fallback.
- **The checker's arithmetic must be the runtime's.** A proof about a run-time
  `Int` function is checked by evaluating it in Strata, so Strata's `Int` must
  wrap as the runtime's does. The same holds for floats, bit for bit,
  including when cross-compiling. A compile-time model of the text primitives
  does not exist yet.
- **The copies of shared definitions agree only by trust**, held up by L10's
  differential. With proofs at run time this matters for generators, not for
  proofs.
- **Compile time.** Strata's evaluation, the size of staged code, and
  interfaces carrying compile-time terms all cost build time, and MeadowBoot
  is already slower than the Rust compiler on cold builds
  (`bootstrap/README.md`, _How fast it is_). L4 and L8 have performance gates.
- **Separate compilation and staging.** A dependent runs its dependencies'
  compile-time functions, so a change to one rebuilds every unit that uses it.
  Lingua's build fingerprints must cover this.
- **Logic.** `Type : Type` is inconsistent, so a predicative hierarchy with
  level inference comes before proofs are relied on (L3). Axiom K is kept, as
  Idyll keeps it. Function extensionality and a `Prop` universe wait until a
  proof needs them.
- **Typestate needs linearity.** `Socket Closed -> Socket Open` is unsound if
  the old handle can still be used. Indexed effects, or linear handles for
  indexed types only, are the options. Neither is designed, and linearity is
  Idyll's.
- **Strata is shared.** A change Meadow needs in the kernel is a change Idyll
  receives, so the two must agree on its pace and its tests. The rule is that
  a kernel change keeps Idyll's parity suite green, or comes with Idyll's
  matching change.
- **The `Matching` split may not be clean.** Case-tree compilation that
  calls back into an elaborator passed in as a record of functions has to
  serve two elaborators with different ideas of when a type is known.
  A proposed mitigation splits by the kind of match instead of stretching
  one interface over both:
  - **Plain matches** are compiled after inference. Meadow's ordinary
    run-time matches refine no types, so they can become case trees once
    types are solved, which is when `Exhaust` runs today. No elaborator is
    passed in.
  - **Dependent matches** take the interleaved path, with the elaborator
    passed in as a record. These are Idyll's matches and Meadow's opt-in
    matches on grade-0 indices, where the match refines types as it goes.
    Meadow's opt-in ones already come with written signatures, so their
    types are known when the match is checked.
- **Reflected semantics** breaks the opacity of `Code` that staging's
  soundness rests on. Doing it without losing that is open research.
- **The oracle disappears.** With the Rust front end deleted, the
  differentials compare MeadowBoot with its own earlier output, so a bug
  present in both versions is invisible to them. Two independent checks are
  left:
  - the Cut reference interpreter, for what programs mean;
  - Idyll's parity suite, for Strata's kernel. Idyll keeps its Rust
    compiler, checked pass by pass against `front/`, so once `front/` runs on
    Strata, that suite tests the graded fragment against a second
    implementation.

## Prior art

- **2LTT.** Annenkov, Capriotti, Kraus and Sattler, _Two-Level Type Theory and
  Applications_ (Mathematical Structures in Computer Science 33(8), 2023;
  arXiv 1705.03307), after Voevodsky's Homotopy Type System.
- **2LTT for staging.** Kovács, _Staged Compilation with Two-Level Type
  Theory_ (ICFP 2022): staging by evaluation, a proof of staging
  correctness, and a prototype (`AndrasKovacs/staged`, `demo/`) whose README
  describes inference of staging operations by coercive subtyping and
  bidirectional elaboration, with no stage metavariables and with explicit
  stages on `let`s. Kovács, _Closure-Free Functional Programming in a
  Two-Level Type Theory_ (ICFP 2024): a first-order, closure-free object
  language under a dependent meta level, implemented in Agda and in typed
  Template Haskell. His `elaboration-zoo` is the reference for bidirectional
  elaboration and pattern unification.
- **Stage polymorphism.** Ofenbeck, Rompf and Püschel, _Staging for Generic
  Programming in Space and Time_ (GPCE 2017), which abstracts over the stage
  of expressions in Scala's LMS. The shared fragment is a narrower form: a
  definition is at both stages, not abstract over which.
- **Lean 4.** Macros and syntax extensions expand syntax to syntax,
  hygienically, before elaboration, and `elab` rules reach the elaborator
  (Ullrich and de Moura, _Beyond Notations: Hygienic Macro Expansion for
  Theorem Proving Languages_, IJCAR 2020). Lean also relates checked and
  compiled code: `partial def` is opaque to the kernel, `@[implemented_by]`
  swaps in other code unchecked, and `@[csimp]` swaps in code proved equal.
- **Idris 2.** Erasure by quantities (Brady, _Idris 2: Quantitative Type
  Theory in Practice_, ECOOP 2021), and `assert_smaller` and `assert_total`,
  which `@assumeSmaller` and `@assumeTotal` follow.
- **Idyll.** Traits as auto-implicit records with associated types carrying
  descriptors; `Div` as an effect; a graded kernel in Meadow; goals, proof
  search, tactics and an MCP harness (Idyll `ROADMAP.md`; `docs/LOW.md` for
  `Mem`).
- **Dictionaries and specialisation.** GHC passes dictionaries and
  specialises with `SPECIALISE` and `INLINABLE`. Swift passes witness tables
  and type metadata, with associated types' metadata in the witness table.
  Rust monomorphises, has associated types, and requires a trait object to
  fix them (`dyn Iterator<Item = T>`).
- **Representation polymorphism.** Eisenberg and Peyton Jones, _Levity
  Polymorphism_ (PLDI 2017). Meadow's descriptors are the run-time
  alternative to GHC's restriction.
- **Abstract indices from run-time checks.** Noonan, _Ghosts of Departed
  Proofs_ (Haskell Symposium 2018). `runSt`'s rank-2 state variable is
  Meadow's own instance.
- **Low-level code as an effect.** F\*'s Low\* and KaRaMeL (Protzenko et al.,
  ICFP 2017), the model for Idyll's `Mem`.

## Decisions

Taken on drafts 1 and 2.

| | decision |
| --- | --- |
| **Q1** where the prototype lives | A separate package in this repository, **`Strata`**: the core calculus (checker, NbE, staging, termination). Built from Idyll's kernel and shared with Idyll; the extraction is agreed with Idyll's session, which owns `front/`. |
| **Idyll** | Stays a separate language. Meadow and Idyll share Strata and Cut. Meadow: inference first, staging, opt-in strength. Idyll: dependent and graded first, linearity, Low\*/`@extract`, the verification stack. |
| **Q2** syntax | `@compTime`, also inferred from signatures that mention compile-time types; `quote { e }`, `$x`, `$(e)` where inference cannot decide; `@law`. |
| **Q3** effects | One implied effect, `Div`: might not return normally, covering looping and uncatchable failures (division or remainder by zero, a missing case, a missing field). Implied in every run-time row, never written or shown; removed where the checker proves totality; an empty row is total. All compile-time code is total, generators and syntax extensions included, with no implicit fuel. A definition the checker cannot prove total is an error offering the fixes that apply. `@total` asserts totality at run time. |
| **Q4** totality | Structural and size-change termination shared with Idyll, plus `@assumeSmaller` and `@assumeTotal`. |
| **Q5** logic | `Type : Type` while prototyping, then a predicative hierarchy with level inference before proofs are relied on; Axiom K kept. |
| **Q6** `Mem` regions | Abstract erased index variables (rank-2, as `runSt`) for identity and lifetime; compile-time values for static sizes and layouts; the ghost heap only in proof-carrying modules, as Idyll designs it; Meadow adopts Idyll's `Mem` rather than designing its own. |
| **Q7** strategy | Debug uses dictionaries (with O1's known-dictionary reads), release specialises; `@specialise` and `@dictionaries`; `traits = "dictionaries" \| "specialise"`. |
| **Q8** sharing with Idyll | (b), the shared kernel, starting with (b1): Idyll's graded kernel with grades fixed at ω. Run-time erased dependent functions, if taken, use 0 as well, a subset of the same algebra, so they need no parameterised kernel (b2). |
| **Q9** partial primitives | Not shared at first (they perform `Div`); checked forms taking a proof come later. |
| **Q10** shared membership | Inferred, with `@shared` to assert it and an error at a compile-time use of a definition that left the fragment. |
| **Q11** laws outside the fragment | Assumed and listed as trusted, and generated as property tests. |
| **Q12** trait objects | `dyn Trait` in L5; general existentials (`exists a. C a => T a`, packed by checking, opened with an escape check, never inferred from use, boxed at run time, a Σ at compile time) in L5b; existentials over indices in L7. |
| **Macros** | Custom syntax stays, as a layer before elaboration (Lean 4's model): declarative rules and procedural `@compTime` extensions over tokens and syntax trees, hygienic and position-preserving, total; elaborator-aware extensions later. L8 is macros and syntax extensions, tested with a small C-like DSL, starting with an inventory of what passes the checker as it is. |
| **Priority** | Agents are expected to be the main authors: run-time dependent functions and proof tooling come first, then the rest of verification, then staging, macros and syntax extensions. Machine-readable diagnostics, fast incremental checking, readable specifications and an inventory of trusted assumptions are priorities. The invariants (same types, self-build reproduces its Cut) hold throughout; L5 may run in parallel. |

## Still open

1. **Erased dependent functions at run time.**
   - **(a)** Meadow's run-time level gets opt-in erased dependent functions
     through Strata's kernel, limited to grades 0 and ω.
   - **(b)** Proofs about run-time values only through indices and
     correct-by-construction code.

   **Recommendation: (a)**, with these rules:
   - a signature is required exactly where code opts in;
   - inferred code calling a dependent function is an ordinary call, with
     implicits solved by unification;
   - an index is an erased run-time value by default, and a compile-time value
     lifts into it, never back;
   - usage is checked only on definitions that write a grade;
   - proofs about run-time functions live at run time, erased.

   The full grade algebra, linearity, extraction and the verification stack
   stay Idyll's. L3 and much after it assume (a).
2. **How grade 0 is written.**
   - **(a)** `@0` on a binder: `(@0 ok : i < n)`, `Buffer @0 n`.
   - **(b)** Idris's `(0 ok : i < n)`.
   - **(c)** Idyll's `[0]`, which clashes with Meadow's list literals.

   **Recommendation: (a)**, since it reads as Meadow's other attributes.
3. **How Meadow and Idyll packages use each other over Cut.**
   - **(a)** Only exported symbols with no shared data types, as today.
   - **(b)** Shared declarations for data types both define the same way
     through Strata (`List`, `Maybe`).
   - **(c)** Interfaces carrying Strata types, so that either language calls
     the other with checked types. Grades and dependencies are checked at the
     border, or erased there with a run-time check.

   **Recommendation: (a) now, (c) as the goal.** Idyll's session agrees. The
   border format has to be designed for size: Idyll's interfaces carry normal
   forms, which once made its Std's export 18 MB until it was trimmed.
4. **`Fail` and `Expand` at compile time.**
   - **(a)** As effects, with total handlers in the compiler.
   - **(b)** As ordinary values, with a generator returning a result.

   **Recommendation: (a)**, since it keeps generators direct-style, as Meadow
   code is.
