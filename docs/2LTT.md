# Two-level Meadow

_A roadmap, not a spec, for review. Nothing in it is built. Syntax in it is a
sketch, and every choice that is not yet settled is listed under
[Questions for review](#questions-for-review)._

Meadow is an ML: Hindley–Milner inference with let-generalisation, traits,
effect rows, general recursion, and control over how values are represented.
This document plans how Meadow grows a second level above that one, a total
dependent type theory that runs at compile time. The framing is two-level
type theory (2LTT). The run-time language stays what it is. The compile-time
language is used for proofs, for computing types, and for generating code,
and in time it replaces the token macros of [MACROS.md](MACROS.md).

## Contents

1. [Why](#why)
2. [Words used here](#words-used-here)
3. [The design](#the-design)
4. [How it fits Meadow today](#how-it-fits-meadow-today)
5. [Milestones](#milestones)
6. [Risks and open problems](#risks-and-open-problems)
7. [Prior art](#prior-art)
8. [Questions for review](#questions-for-review)

## Why

**One language, for scripts and for proofs.** Meadow is quick to write.
Inference means a script needs no annotations, which suits people and agents
alike. What a Meadow program promises, though, is held up only by tests and
differential runs. Idyll, the sibling language, has dependent types and can
state and prove those promises. But it is another language, with another
syntax, other libraries, and grades on every binder. Writing a library in
Meadow and its proofs in Idyll means writing everything twice. The aim here is
one language. A program is written as Meadow is written today, and proofs are
added where they pay, in the same files, about the same definitions.

**Why two levels, and not dependent types everywhere.** Making Meadow
dependently typed throughout would put run-time terms into types. Run-time
terms in Meadow perform effects and may not terminate, so types would need a
divergence effect, a termination checker, and conversion that evaluates
run-time code. Every program would meet them, and Hindley–Milner inference,
which is what makes Meadow quick to write, would be lost. 2LTT keeps the two
apart:

- A run-time type may depend on compile-time values (a length, a protocol
  state, a region), never on run-time ones.
- Compile-time terms are total and pure, so a type checker may evaluate them
  freely.
- Inference at run time stays Hindley–Milner. The compile-time level is
  checked bidirectionally, where signatures are expected anyway.

**Why typed code generation, and not more macros.** A Meadow macro today maps
token trees to token trees ([MACROS.md](MACROS.md) §1, §9). Its output is
parsed and type-checked only after it is spliced, so a mistake in a macro is
reported in its caller's code. It runs in a separate program on Glade under a
fuel budget, and what macros leave for each other is an untyped `Datum`. A
compile-time function that returns typed code is checked where it is written.
Its output is well typed by construction, and it runs by evaluation inside the
compiler, with no tokens re-parsed.

**Why one shared fragment.** The data types, total functions and trait
instances that both levels need, such as `List`, `map` and `Ord Int`, should
be written once. A law proved about the compile-time copy then speaks about
the run-time copy too, because the two are one definition.

## Words used here

| word                        | meaning                                                                                                                        |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| **object level**, stage 0   | the run-time language: Meadow as it is. Its types live in `Type0`.                                                             |
| **meta level**, stage 1     | the compile-time language: a total, pure, dependent type theory. Its types live in `Type1` (and above, if there is a hierarchy). |
| **`Code a`**                | for an object type `a`, the meta type of object terms of type `a` (Kovács's `↑A`).                                             |
| **quote** and **splice**    | quote turns an object term into a `Code a`; splice runs a meta computation of type `Code a` and puts the term it gives in its place. |
| **staging**                 | running every meta computation in a program, leaving an object program with no meta level in it.                               |
| **shared fragment**         | data types, total pure functions, and traits and impls over them, each written once and elaborated at both levels.              |
| **stage polymorphism**      | a definition being usable at both levels; here, membership of the shared fragment.                                             |
| **dictionary**              | a trait's evidence at run time: a record of the impl's methods (today's lowering, [RUNTIME.md](RUNTIME.md)).                   |
| **descriptor**              | a run-time value naming a representation (`desc` in [CUT.md](CUT.md)), passed to code generic over a type.                    |

## The design

### The two levels

**The object level is Meadow.** It keeps traits, Hindley–Milner inference
with let-generalisation, effect rows, general recursion, representation
polymorphism by descriptors ([CUT.md](CUT.md), _Descriptors and
representation variables_), and later the `Mem` effect. Every program that
compiles today compiles unchanged and means the same.

**The meta level is a total dependent type theory.** It has Π and Σ types,
inductive data types with dependent matching, propositional equality, an
unbounded `Nat`, and universes. Every meta term terminates and performs
nothing a type may not depend on (see [question 3](#q3-does-the-meta-level-get-effects)
for code generators). The checker evaluates meta terms during conversion by
normalisation by evaluation, as Idyll's checker does.

**The levels meet in three places.**

1. **`Code a`.** `Type0` is a meta type, so a meta function may compute an
   object type, and `Code a` is the meta type of object terms of type `a`.
   Quote and splice convert between them. Staging evaluates every splice and
   leaves an object term. This is _staging by evaluation_, Kovács's analogue
   of normalisation by evaluation (ICFP 2022).
2. **Indices.** An object type former may take meta arguments: `Vec n a`
   where `n : Nat` is meta, `Socket Open`, `Buf r` for a region `r`. Indices
   are erased, as types are.
3. **The shared fragment**, whose definitions exist at both levels (below).

What 2LTT rules out is a run-time value in a type. An object term never
appears in a meta type, and the meta level cannot look inside an object term
(see [Proofs](#proofs-three-routes)).

```meadow
-- Sketch. Object level, unchanged.
fun sumSquares (xs : List Int) : Int = List.foldl (\acc x -> acc + x * x) 0 xs

-- Meta level: a function from a meta Nat and object code to object code.
@compileTime
fun power (n : Nat) (x : Code Int) : Code Int =
  match n with
  | Zero -> quote { 1 }
  | Succ m -> quote { $x * $(power m x) }

-- Back at the object level. `3` is read as a meta Nat because `power` wants
-- one, `x` is quoted, and the result is spliced, all by inference.
fun cube (x : Int) : Int = power 3 x
-- after staging: fun cube (x : Int) : Int = x * (x * (x * 1))
```

### Stage inference

Quote and splice are mostly implicit, as in Kovács's design. His demo infers
them during bidirectional elaboration, by coercive subtyping between `a` and
`Lift a`. It has no stage metavariables, and it wants every `let` to say its
stage. Meadow follows the same split.

- **A definition's stage is fixed before its body is elaborated.** It is fixed
  by a marker (`@compileTime`, see [question 2](#q2-surface-syntax-for-the-meta-level-and-for-stages)),
  by a signature that mentions a meta type, or by default. **The default is
  the object level.** So every existing definition is what it was. Whether an
  object definition is also in the shared fragment is decided after
  inference, by [a check](#the-shared-fragment), and never changes its object
  meaning.
- **Inside a body, quotes and splices are inserted by checking.** An object
  term where `Code a` is expected is quoted. A meta term of type `Code a`
  where an object `a` is expected is spliced. A literal takes the level its
  expected type has.
- **The object part keeps today's inference.** Inside object code the
  elaborator is Hindley–Milner, step for step as MeadowBoot's `Infer` is now.
  The bidirectional checker runs only on meta terms and at the borders. A
  meta call inside object code is checked against the type the object
  inference has at that point. Where that type is still an unsolved variable,
  the call waits, as a trait predicate waits for its type.
- **Ambiguity is an error, and it says what to write.** When a border cannot
  be decided, an explicit `quote { … }` or `$( … )` settles it.

Existing code meets none of this. It has no meta definitions, so it has no
borders.

### The shared fragment

**What is in it.** A data type is shared when its fields are of shared types.
A function or `def` is shared when:

- its body performs no effect of its own. It may be effect-polymorphic in a
  function it is given: `map (f : a -> b ! e) xs : List b ! e` is shared;
- it terminates, by the meta level's termination checker;
- its matches are exhaustive;
- every primitive it calls has a meta model that agrees with the runtime
  (below);
- everything it names is shared.

A trait is shared when its methods' types are shared. An impl is shared when
its methods are.

**Membership is inferred.** After inference, a check decides each
definition's membership and records it in the unit's interface. A definition
that fails the check stays object-only, silently. Nothing about it changes,
and existing code needs no annotation. A definition marked `@shared` that
fails is an error naming the reason. A library marks what its users' proofs
will rely on, so that an edit cannot quietly take it out of the fragment (see
[question 10](#q10-shared-fragment-membership-inferred-or-declared)).

**Each shared definition is elaborated twice.** The object copy is what is
compiled today. The meta copy is the same syntax read in the meta theory:
`List a` becomes a meta data type over `a : Type1`, effect rows are emptied
(the meta copy of `map` is `map : (a -> b) -> List a -> List b`), and
dictionaries become meta records. A meta function may call the meta copy, and
a law may be stated about it.

**Cross-stage persistence.** A value of a shared, first-order data type can be
_lifted_: a meta `List Int` becomes the object code that builds it. This is
derived per shared data type. It plays the part `ToTokens` and `Datum` play
for macros today ([MACROS.md](MACROS.md), §10), but it is typed.

**Why a law about the meta copy holds at run time.** The two copies are one
definition read under two semantics. The claim that the object semantics of
the shared fragment agrees with its meta semantics is a property of the
compiler. Proofs inside the language do not establish it. It is trusted, and
it is tested differentially ([L4](#l4-stage-polymorphism)). The optimiser
must keep it: dictionary specialisation, inlining, tail recursion modulo
context and representation specialisation already have to preserve meaning,
and the same differential holds them to it. When the transfer is about an
effect-polymorphic definition, it holds at the pure instance (`e = {}`).
Laws about effectful uses need a semantics of effects at the meta level,
which is later work.

**The meaning must be the same at both levels.**

- **`Int` is 64-bit two's complement and wrapping at both levels.** Its meta
  model is arithmetic modulo 2^64 read as signed, not the integers. So
  `x + 1 > x` is not a theorem about `Int`, and a proof cannot claim it is.
  The same holds for `Int8` to `UInt64` at their widths.
- **`Nat` is the meta level's unbounded natural number, for proofs and
  indices.** A shared unbounded integer type (Meadow's `BigInt`) can come
  later, with `Nat` and `Int` models related by explicit conversions.
- **Primitives that fail at run time are not shared at first.** Division by
  zero, an index out of bounds and a non-exhaustive match each stop a
  Meadow program at run time. A total meta copy has no such outcome. The
  first version keeps definitions that use them object-only. A later version
  can add checked forms that take a proof of their precondition (see
  [question 9](#q9-partial-primitives-in-the-shared-fragment)).
- **`Float` is shared only bit for bit.** The compile-time evaluator must use
  the runtime's own operations (`meadow_rt::num`), and the shared fragment
  must leave out anything whose result differs between platforms, such as
  the transcendental functions of the C library. Otherwise a cross-compiled
  program would compute one thing at compile time and another at run time.
- **`String` is shared with the semantics `meadow_rt::text` gives it,** as a
  sequence of Unicode scalar values over UTF-8 bytes, once that semantics is
  written down as a meta model.

### Coherence

There is one impl of a trait per type, across both levels and across every
package of a build. A shared impl such as `Ord Int` is one impl with two
copies, never two impls. Coherence is what makes "the `Ord Int`" a
meaningful thing to prove laws about, and what lets a dictionary be chosen at
compile time with no run-time search. Today MeadowBoot keys an impl by its
trait and head constructor (`Traits.registerImpl`). I found no orphan rule.
Coherence across packages needs one. The proposal is Rust's: an impl lives
with its trait or with its type's head constructor. Idyll has the same rule
for its unnamed instances.

### Traits at each level

**At the meta level** a trait is a record whose fields may be types, and an
impl is a canonical value of it, found by instance search during elaboration.
This is Idyll's design ("traits are auto-implicit records"). An associated
type is a field that is a type. Meta dictionaries exist only at compile time
and are evaluated away by staging.

**At the object level** traits stay as they are: a predicate per `where`, a
dictionary per predicate, and an associated type carried as one more type
variable of the predicate, settled by the impl ([RUNTIME.md](RUNTIME.md),
_Traits are dictionaries, and then they are not_). Instance resolution is
always at compile time. A dictionary is chosen statically wherever the type is
known, and passed in from the caller where it is not. Dictionary passing is
kept because it is what gives polymorphic recursion, trait objects and
existentials, and separate compilation: a constrained function compiled in a
package that has never seen its caller's types.

**Trait members come in three kinds:**

| kind | examples | where it lives | strategy |
| --- | --- | --- | --- |
| **ordinary methods** | `compare`, `take1`, `(+)` | in the dictionary | dictionaries or specialisation, chosen by the build |
| **erased members** | laws (`compare x x == Equal`), meta associated items (a width as a `Nat`, a layout) | nowhere at run time | erased; checked or computed at compile time |
| **code-generating members** | a staged `compare` that returns `Code Ordering`, a derive | nowhere at run time | always run at compile time |

A `where C a` at the object level elaborates to two things: the run-time
dictionary (ordinary methods, associated types' descriptors, superclasses'
run-time parts), and a meta record of the erased and code-generating members.

- **Erased members** never enter a dictionary. In code generic over `a`, a
  law is a hypothesis about an abstract `a`. That needs nothing per
  instantiation, so polymorphic recursion is unaffected.
- **A law is a statement about the meta copy.** So a law can be proved only
  for a shared impl. For an object-only impl, a law cannot be stated about
  its run-time behaviour, because the meta level cannot see inside it. Such an
  impl either assumes the law, which is then listed as trusted, or has it
  checked as a generated property test (see
  [question 11](#q11-laws-on-impls-outside-the-shared-fragment)).
- **A code-generating member has to run at compile time**, so it needs its
  instance at compile time. A generic function that uses one is therefore
  staged at each use, as a macro is, and is always specialised. Polymorphic
  recursion through such a function is refused, because staging it would not
  end.

```meadow
-- Sketch: a trait with all three kinds.
@pub trait Ord a <: Eq a, PartialOrd a {
  fun compare : a -> a -> Ordering                       -- ordinary
  @law fun reflexive : (x : a) -> compare x x == Equal   -- erased
}
```

### Associated types at run time

Today an associated type is a type variable lifted into the scheme of the
function that asks for it (`Traits.liftAssocs`). Below inference nothing knows
it exists. That suffices while every generic function is called by someone
who knows the types. It does not suffice for trait objects, existentials, or
a dictionary stored in a data structure, where the code holding the
dictionary did not choose the types.

The rule proposed here: **a dictionary carries a descriptor for each of its
trait's associated types.** For `Stream s`, the dictionary holds
`Token s`'s descriptor beside `take1` and the rest. An impl fixes it:

- a constant for `Token String = Char`;
- for a parameterised impl, `impl Stream (List a)` with `Token (List a) = a`,
  the dictionary constructor takes `a`'s descriptor and stores it.

Code generic over `s` reads the descriptor out of the dictionary, and binds
the representation variable of `Token s` to it. Swift's witness tables have
the same shape: IRGen gives each associated type a slot for its type metadata
(`lib/IRGen/GenProto.cpp`). Idyll already does this for its record-typed
dictionaries, where "a record's field that is a type holds its descriptor"
(Idyll `ROADMAP.md`, Phase 4, _Associated types_).

**An existential** (a trait object) is a package of the hidden type's
descriptor, its dictionary, and the value. Its associated types' descriptors
come with the dictionary. Unlike Rust's `dyn Iterator<Item = T>`, a Meadow
trait object need not fix its associated types. Code that opens one gets an
abstract `Token s` with a representation known at run time.

**What Cut needs.** Today a Cut definition binds a representation variable
only to a descriptor parameter (`<'a = da>`). Two extensions follow:

1. binding a representation variable to a descriptor read from a field
   (`case { Stream#dict('t = dt: desc, take1: ptr, …) => … }`);
2. the existential package as data with a descriptor field. Alternatively,
   the boxed `any` representation, which [CUT.md](CUT.md) has in its grammar
   but not yet implemented.

A function's own `where` may keep today's flattening, with each associated
type passed as one more descriptor parameter beside the dictionary, as Idyll
passes them. Cut is shared with Idyll, so both changes go through
[CUT.md](CUT.md) and Idyll's review, under its versioning rule.

**With specialisation** a known dictionary is a constant, and so are its
descriptor fields. The back end's specialise-on-known-arguments pass then
copies for the representation, and the descriptor disappears. Specialisation
is never required for meaning. Code left unspecialised still runs by reading
the descriptor, as [CUT.md](CUT.md) requires of every descriptor.

### Dictionaries or specialisation: a build setting

Both strategies give the same answers. Which one a build uses is a setting.
It is not a property of the program.

- **Debug: dictionaries**, for compile speed, plus what O1 does today (a
  method asked of a known impl is read directly, `Dictionaries.mw`).
- **Release: specialisation.** Every call with known dictionaries gets a copy
  for them, transitively, keyed by representation as the release
  specialiser already keys copies. Dictionaries remain only where they must:
  polymorphic recursion, existentials, and generic code called with a
  dictionary it was handed.
- **Markers per definition**: one to always specialise, even in debug (hot
  code, and every function forced by a code-generating member), and
  possibly one never to (code size). See [question 7](#q7-the-default-strategy-and-markers).

MeadowBoot already declares `dictionaries : Bool` and `opt : O0 | O1 | O2 | O3`
in its `make!` options. The setting becomes one option with two values, set
by profile (`[profile.debug]`, `[profile.release]`) and by `--set`. Where the
copying happens: the recommendation is the back end's specialise pass over
Cut. A dictionary and a descriptor are both known arguments there, Idyll's
dictionaries are the same records, and one pass serves both front ends.
MeadowBoot's `Inline` keeps its known-dictionary reads for debug builds.

### Erasure

After staging, nothing of the meta level is left: meta definitions, meta
dictionaries, laws, proofs and indices are gone. The object copy of a shared
definition is kept only where run-time code reaches it, which linking's
pruning already decides. An index needs no descriptor, because it is not a
type a value has. A run-time value derived from an index is an ordinary
object value, made by lifting it. Cut stays free of source types, as it is
now.

### Indices

An object type former may take meta parameters:

```meadow
-- Sketch: an object data type indexed by a meta Nat.
data Vec (n : Nat) a where
  | Nil : Vec 0 a
  | Cons : a -> Vec n a -> Vec (n + 1) a
```

- **Index variables are generalised like type variables.** A function over
  `Vec n a` is generic in `n` as it is in `a`, and `n` is erased.
  Hindley–Milner generalises it with nothing written.
- **Index equations are solved by the meta checker.** `Vec (n + 1) a` against
  `Vec (m + 1) a` normalises, and an equation it cannot decide is reported. A
  solver can come later. Idyll's planned SMT interface is the candidate for
  sharing.
- **Matching on an indexed constructor refines indices**, as a GADT match
  refines types. That is the one place annotations become necessary, and only
  in code that uses indexed types.
- **Dynamic facts are reached by rank-2 abstraction**, as `runSt` makes its
  `s` today: a run-time check hands its continuation a value typed by an
  abstract index (Noonan's _Ghosts of Departed Proofs_). For example,
  `withLength : Array a -> (forall n. Buf n a -> r) -> r`, and
  `check : Int -> Buf n a -> Maybe (Ix n)`. Plain 2LTT has no object-level
  quantifier over meta values. Erased, parametric quantifiers of this kind
  are the extension Meadow needs, and they behave as type variables do.

### Proofs: three routes

The meta level cannot look inside an object term. That is what keeps staging
sound and the object level free to optimise, and it means a proof about
run-time behaviour has to take one of three routes:

1. **Correct by construction.** Write the algorithm at the meta level, prove
   it there, and stage it down. The shared fragment is the case where staging
   is the identity, so a law proved about `List.reverse`'s meta copy holds of
   the one at run time. A staged parser or serialiser proved to round-trip is
   the case where staging generates new code.
2. **Indices on object types.** Invariants carried in types (lengths, bounds,
   protocol states, regions) are checked by the meta checker as the object
   program is elaborated. The program is proved by being well typed.
3. **A reflected semantics of the object language, later.** A meta data type
   of object syntax and a meta definition of what it means (Cut's reference
   interpreter, written at the meta level), with quote able to hand a term's
   syntax to the meta level. Proofs about object code become proofs about its
   syntax under that semantics. This deliberately breaks the opacity of
   `Code`, and it is research (see [Risks](#risks-and-open-problems)).

### Staging

Staging runs after elaboration and before MeadowBoot's lowering to core
(`Elab.mw`). Its output is an ordinary typed object program, so everything
from `Elab` down (dictionaries, `Inline`, `Seq`, Cut, Glade and Silo) is
untouched by it.

Two evaluators are involved:

- **Open evaluation, for checking.** NbE over meta terms with free variables,
  for conversion and index equations. It runs inside the type checker.
- **Closed evaluation, for staging.** By the time a splice runs, its meta
  computation is closed. Like a procedural macro today, it can be compiled
  and run on Glade, rather than interpreted, when it is heavy. An
  interpreter is enough at first. Compiling it is the answer if
  [L8](#l8-macros-on-the-meta-level) finds staging slower than macros are now.

## How it fits Meadow today

### MeadowBoot only

New language work lands in MeadowBoot (`bootstrap/`), never in the Rust front
end. The next release ships MeadowBoot as `meadow` with the Rust front end
beside it for that one release, and then the Rust front end is deleted.
Nothing here is ported to it.

That removes the Rust compiler as the oracle the differential tests compare
with. For this work the oracles are:

- **MeadowBoot's own output from before each change**, frozen as golden
  dumps: `infer`'s scheme dumps and the Cut each package compiles to;
- **the back end's Cut reference interpreter** (`meadow_cut::interp`), which
  defines what a program means and does not depend on any front end;
- **the meta level's evaluator** against the object copy, for the shared
  fragment.

The project's rules for MeadowBoot hold here too:

- every stage is a Lingua `lang!`, every transformation a `pass!`, and solver
  code is library code as `Solve` and `Traits` are;
- the grammar stays LL(k) with no backtracking, which constrains the syntax
  for quote and splice;
- names obey Meadow's scoping rule: a `use` of a shared definition brings
  both its copies, and meta definitions are exported only by `@pub`;
- MeadowBoot builds itself, and a release waits for that build. Every
  milestone keeps self-build green, and the stage-2 compiler writes the same
  Cut as stage 1.

### Where it goes in the pipeline

```text
parse → fixity → @cfg + macros → rename → infer (HM, unchanged)
      → check (meta level, bidirectional; borders) → classify (shared or not)
      → stage → Elab → Inline / dictionaries → Seq → Cut
```

`Elab.mw` is today's lowering from the typed tree to core, and it is where
dictionary applications are written. It stays the lowering and sees only
object code after staging. The two-level elaborator sits between `infer` and
`Elab`. A unit's interface (`Packed`) grows: meta definitions are exported as
terms, so that a dependent can stage them, and so are shared definitions'
meta copies and laws. Idyll found that exporting normal forms made its Std
18 MB, so terms are exported as written, closed over what precedes them.

### The back end: Cut, AxCut, representation polymorphism

The back end changes little. The meta level never reaches Cut. A shared
definition's object copy is ordinary Cut. Indices are erased. The two Cut
extensions are the ones [above](#associated-types-at-run-time): a
representation variable bound from a field, and the existential package. The
plan for polymorphism is already that front ends turn type abstraction into
descriptor parameters, and that one back-end pass specialises on known
arguments. That plan is what lets the dictionaries-or-specialisation setting
be a single back-end decision.

The meta level can also compute representations. An object type indexed by a
meta width (`Word w`) has its representation chosen at staging, so no
representation variable is needed. Exposing representations and layouts as
meta values is the natural meeting point with `Mem`.

### Macros

| today ([MACROS.md](MACROS.md)) | on the meta level |
| --- | --- |
| procedural macro `[TokenTree] -> [TokenTree]`, run on Glade | meta function `[TokenTree] -> Code a`: its output is checked against the type wanted where it is spliced |
| `quote!` with `$x` splices | `quote { … }` with `$x`: the same spelling, typed |
| `@derive(X)`, built in (`Derive.mw`) or procedural | a meta function over a reflected declaration that returns impl code |
| `Expand` effect, `lookup` and `define` of a `Datum` | meta definitions, exported in the interface and typed; `Datum` stays for macros that keep the token interface |
| `@compileTime def` read as data | a meta definition, evaluated |
| fuel budget, effect sandbox | totality for what types depend on; for code generators, see [question 3](#q3-does-the-meta-level-get-effects) |

Token-tree macros remain for syntax that is not Meadow's, such as the
grammars in Lingua's `syntax!` and `lang!`. What changes is what they return:
code typed where it is written, with the token-returning interface kept for
compatibility. Lingua's macros are the largest users of code generation in the
repository, and Lingua is its own project, so they move when Lingua's session
decides to.

### Idyll as a second front end

Idyll already has much of what the meta level needs, written in Meadow in
its `front/`: NbE (`Eval`, `Value`), pattern unification (`Unify`),
elaboration to case trees (`Matching`), size-change termination with
transparent unfolding (`Termination`), strict positivity (`Positivity`),
traits as records with associated types carrying descriptors, divergence as
an effect (`Div`), and the rule that types depend only on terms whose row is
empty. It also has decisions the meta level should agree with: Axiom K, and
`Int` as 64-bit wrapping.

So the question is how much the two share ([question 8](#q8-how-much-idyll-shares)):
only Cut, as today; a kernel library both depend on; or Idyll's theory as
Meadow's meta level outright. Either way, Meadow's meta level should not
diverge from Idyll's without a reason. Idyll's repository is Idyll's. Any
sharing is agreed with its session and made by extraction into a package
both depend on, never by editing it from here.

### The `Mem` effect

`Mem` is Idyll's design (Idyll `docs/LOW.md`): C's memory model as an effect,
with buffers whose lengths are in their types, liveness and disjointness as
erased propositions about a ghost heap, a model handler for running it in
pure code, and extraction to C. Its purpose on Meadow's side is
[MEADOW-IN-IDYLL.md](MEADOW-IN-IDYLL.md): the runtime written in low-level
Idyll.

What 2LTT offers it:

- **Correct by construction.** Memory code is written as a meta program over
  a meta model of `Mem`, its safety proved against the model, and staged to
  object code that performs `Mem`'s operations. The proof is about the
  generator, and the object code needs no checks.
- **Indices.** A buffer's length or a region is an index (`Buf r n a`). Its
  identity and lifetime come from rank-2 abstraction, as `runSt`'s `s` does,
  and its static sizes and layouts are meta values.
- **What it does not give.** Plain 2LTT cannot index an object type by a
  run-time length, and a ghost heap threaded through pre- and postconditions
  is Low\*'s machinery, not 2LTT's. Proof-carrying low-level code with
  dynamic sizes needs either the erased quantifiers of [Indices](#indices)
  with run-time checks, or the reflected semantics. See
  [question 6](#q6-how-mem-regions-are-indexed).

## Milestones

Each milestone lands on its own, behind an option where it changes the
pipeline, and keeps two regressions green from L1 on:

- **corpus**: every package in the repository still infers to the schemes it
  did before;
- **self-build**: MeadowBoot's self-compile, with stage 2 writing stage 1's
  Cut byte for byte.

"Byte-identical Cut" below means the Cut each package of the repository
compiles to, compared with the golden output of the MeadowBoot before the
milestone.

The order follows the suggested shape, with two changes:

- The shared fragment's classification comes before the meta level (L2),
  because it is cheap and its numbers (how much of Std qualifies) shape every
  milestone after it.
- Run-time traits are split. Associated-type descriptors, existentials and
  the build setting (L5) need nothing from the meta level and can proceed in
  parallel from L1 on. Erased and code-generating members (L6) need the meta
  level, so they wait for it.

### L1. A two-level core, object-only

_Proves_: the elaborator can be replaced without changing anything.

- A core language (a `lang!`) with two universes, `Code`, quote and splice,
  into which MeadowBoot's typed tree elaborates. Everything is placed at the
  object level, and staging is the identity.
- Behind an option (`--set levels=true`), off by default.

_Tested by_: byte-identical Cut over the whole repository, with the option
on, against the option off; the corpus and self-build regressions.

_Done when_: the option can be on in CI with no difference.

### L2. Classifying the shared fragment

_Proves_: the fragment is large enough to be worth it, and its rules can be
checked.

- The check of [The shared fragment](#the-shared-fragment): purity, a
  termination checker on case trees (structural and size-change, as Idyll's),
  exhaustiveness (MeadowBoot's `Exhaust`), and a table of primitives with
  meta models. `@shared` makes a failure an error. Membership is recorded in
  the interface.
- A report of each definition's membership and, for each non-member, its
  reason.

_Tested by_: a golden membership dump over Std and the examples, with
reasons; termination checker cases ported from Idyll's corpus, accepted and
rejected; the corpus regression, because classification must never change a
scheme.

_Done when_: the report for Std is reviewed. It is the first real evidence
for or against the design.

### L3. The meta level

_Proves_: a total dependent theory checks and stages inside MeadowBoot.

- Meta definitions, Π, Σ, meta data types, `Nat`, equality, universes,
  bidirectional checking with NbE and pattern unification, and `Code` with
  quote and splice inserted by checking.
- Staging by evaluation, and `meadow build --emit staged`, which prints the
  object program after staging.
- No traits at the meta level yet.

_Tested by_:

- a suite of programs the checker accepts and rejects, each rejection pinned
  to its message and place;
- staging goldens, comparing `--emit staged` output with the expected object
  program (Kovács's `power`, map fusion, a staged interpreter);
- running every staged test program on the Cut reference interpreter
  against an unstaged evaluation of the same program in the meta evaluator;
- byte-identical Cut for the corpus, which has no meta code.

_Done when_: the examples above stage to the expected code and run.

### L4. Stage polymorphism

_Proves_: one definition, two copies, one meaning.

- Shared data types and functions get meta copies, callable from meta code,
  with lifting of first-order shared values into code.
- `Int`'s meta model is arithmetic modulo 2^64; `Nat` is unbounded.

_Tested by_ the meta-against-object differential. For each shared function in
Std, generated inputs are given to the meta copy in the compile-time
evaluator, and to the object copy on the Cut reference interpreter, on Glade
and on Silo, and the answers are compared. The inputs are made with Clover,
Meadow's quickcheck, and weighted to the edges: overflow, `minInt`, shifts,
empty and long lists. This is the test that makes "a law about the meta copy
holds at run time" credible. It runs at every optimisation level, so the
optimiser is held to it too.

_Done when_: every shared function in Std passes it.

### L5. Run-time associated types, existentials, and the strategy setting

_Proves_: dictionaries carry what generic code needs, and the two strategies
agree. This needs only L1 and can start alongside L2.

- Dictionaries carry associated types' descriptors. Cut gains a
  representation variable bound from a field, and the existential package,
  reviewed with Idyll under [CUT.md](CUT.md)'s versioning.
- Trait objects in the surface language ([question 12](#q12-trait-objects-in-the-surface-language)).
- The setting `dictionaries` or `specialise`, with profile defaults and the
  per-definition markers.

_Tested by_:

- every program of the back-end differential corpus, and Std's tests, under
  both settings, with answers compared on the reference interpreter, Glade
  and Silo;
- a lint that, under `specialise`, no dictionary is left at a monomorphic
  call;
- polymorphic recursion and existentials run under both settings;
- descriptor correctness through MeadowBoot's `Lint` (representations that
  meet agree) and through Silo under the cycle collector and the smallest
  nursery, where a wrong descriptor shows up as a collector fault.

_Done when_: the whole corpus agrees under both settings, and the release
default is `specialise` with no benchmark slower.

### L6. Traits at the meta level and in the shared fragment

_Proves_: `Ord Int` is written once, its laws are proved once, and coherence
holds across levels and packages.

- Meta traits as records with instance search.
- Shared traits and impls.
- Erased members (`@law`, meta associated items) and code-generating
  members, with forced specialisation and the refusal of polymorphic
  recursion through them.
- The orphan rule.

_Tested by_: golden coherence and orphan errors; laws proved for Std's
`Eq`/`Ord` on `Int`, `Bool`, `Char`, `List` and tuples, checked as part of
Std's build; the L4 differential extended to shared methods; L5's
both-settings differential with erased members present, to show they leave no
trace in Cut.

_Done when_: Std states and proves its order laws, and the Cut it compiles to
is unchanged by them.

### L7. Indices

_Proves_: invariants in types at no run-time cost.

- Object type formers with meta parameters, index variables generalised as
  type variables, index equations solved by normalisation, GADT-style
  refinement, and erased rank-2 quantification for dynamic facts.

_Tested by_: accepted and rejected programs (`Vec`, a socket typestate, a
bounds-checked buffer); the erasure check, where an indexed program's Cut is
byte-identical to the same program written without indices; the corpus
regression.

_Done when_: those examples check, and their Cut matches the unindexed
versions.

### L8. Macros on the meta level

_Proves_: typed code generation replaces token macros where it should, and is
not slower.

- The built-in derives (`Derive.mw`) rewritten as meta functions, and
  procedural macros allowed to return `Code a`.
- Compile-time bindings as typed meta definitions.
- The token interface kept.

_Tested by_: for every derive in the repository, the old path against the new
must give byte-identical Cut, for the whole repository and MeadowBoot's
self-compile; build times by `scripts/bench-meadowboot.py`, which must not
regress; error placement, where a mistake in a derive is reported in the
derive's own code.

_Done when_: the built-in derives are meta functions, and the textual
generator is deleted.

### L9. Proofs

_Proves_: the routes of [Proofs](#proofs-three-routes) work on real code.

- Std's algebraic laws (monoid, `map` composition, `reverse` involution) on
  shared definitions.
- One correct-by-construction library: a staged parser or binary
  serialiser, proved to round-trip.
- One `Mem` example built with Idyll, if Idyll's `Mem` exists by then.
- A spike on the reflected semantics: Cut's meaning as a meta definition, and
  one staging step proved against it.

_Tested by_: the proofs are checked in CI as part of their packages' builds,
within a time budget; the generated code runs in the existing differentials.

_Done when_: the libraries above are proved, and the spike says whether
route 3 is worth a milestone of its own.

## Risks and open problems

- **The border between inference and checking.** Hindley–Milner and
  bidirectional dependent checking in one elaborator must stay predictable.
  A meta call whose expected type is an unsolved variable has to wait, and
  the order things wait in must not change what a program means. Meadow's
  trait predicates already wait this way, which is the precedent.
- **Membership drift.** An edit that adds an effect to a shared function
  takes it out of the fragment, and breaks proofs downstream, far from the
  edit. `@shared` and the interface record are the mitigation, and the error
  must name the edit's cause.
- **The transfer of laws is trusted.** That a shared definition means the
  same at both levels is a property of the compiler, held up by the L4
  differential, not proved. Every optimisation must keep it. Route 3 is the
  long-term way to prove it.
- **Numeric semantics.** `Int` must be modelled as wrapping. Floats must be
  computed bit for bit as the target computes them, including when
  cross-compiling. A meta model of the text primitives does not exist yet.
- **Termination is weaker than people expect.** Std's `range` and
  `replicate` recurse on an `Int` that is not structurally smaller, so they
  are not shared until termination measures exist.
- **Compile time.** Meta evaluation in an interpreter, the size of staged
  code, and interfaces carrying meta terms all cost build time. MeadowBoot is
  already slower than the Rust compiler on cold builds (`bootstrap/README.md`,
  _How fast it is_). L8 has a performance gate for that reason.
- **Separate compilation and staging.** A dependent stages its dependencies'
  meta functions, so a change to a meta function rebuilds every unit that
  splices it. Lingua's build fingerprints must cover this.
- **Logic.** `Type : Type` is inconsistent, so proofs need a universe
  hierarchy before L9. Axiom K, function extensionality and a `Prop` universe
  are open, and agreeing with Idyll is worth more than any one choice.
- **Typestate needs linearity.** `Socket Closed -> Socket Open` is unsound if
  the old handle can still be used. Indexed effects, or linear handles for
  the indexed types only, are the options. Neither is designed.
- **Run-time-sized indices.** Plain 2LTT cannot index by a run-time value. The
  erased rank-2 quantifiers proposed here are an extension of it whose
  metatheory has not been checked.
- **Reflected semantics** breaks the opacity of `Code` that staging's
  soundness rests on. Doing it without losing that is open research.
- **Grammar.** Quote and splice must be readable by an LL(k) automaton with
  no backtracking. `<e>` is ambiguous with comparison operators, which is one
  reason the sketch uses `quote { }` and `$`.
- **Two dependent checkers in one ecosystem**, Idyll's and Meadow's, unless
  they share one.
- **The oracle disappears.** With the Rust front end deleted, the
  differentials compare MeadowBoot with its own earlier output. A bug present
  in both versions is invisible to them, so the Cut reference interpreter
  becomes the main independent check.

## Prior art

- **2LTT.** Annenkov, Capriotti, Kraus and Sattler, _Two-Level Type Theory and
  Applications_ (Mathematical Structures in Computer Science 33(8), 2023;
  arXiv 1705.03307), after Voevodsky's Homotopy Type System. There the inner
  level is homotopy type theory and the outer level its internalised
  metatheory.
- **2LTT for staging.** Kovács, _Staged Compilation with Two-Level Type
  Theory_ (ICFP 2022): staging by evaluation, a staging-correctness proof,
  and a prototype (`AndrasKovacs/staged`, `demo/`) whose README describes
  inference of staging operations by coercive subtyping and bidirectional
  elaboration, with no stage metavariables and with explicit stages on
  `let`s. Kovács, _Closure-Free Functional Programming in a Two-Level Type
  Theory_ (ICFP 2024): a first-order, closure-free object language under a
  dependent meta level, implemented in Agda and in typed Template Haskell.
  His `elaboration-zoo` is the reference for the bidirectional elaboration
  and pattern unification the meta checker needs.
- **Stage polymorphism.** Ofenbeck, Rompf and Püschel, _Staging for Generic
  Programming in Space and Time_ (GPCE 2017), which abstracts over the stage
  of expressions in Scala's LMS. The shared fragment here is a narrower form:
  a definition is at both stages, not abstract over which.
- **Elaboration of dependent languages.** Lean 4 keeps what the kernel checks
  apart from what is compiled: `partial def` is opaque to the kernel,
  `@[implemented_by]` swaps in other code unchecked, and `@[csimp]` swaps in
  code proved equal. Each is a way of relating a run-time copy to a checked
  one. The shared fragment aims to make the two copies one definition.
  Idris 2 erases by quantities (Brady, _Idris 2: Quantitative Type Theory in
  Practice_, ECOOP 2021), as Idyll does by grades.
- **Dictionaries and specialisation.** GHC passes dictionaries and
  specialises with `SPECIALISE` and `INLINABLE`. Swift passes witness tables
  and type metadata, with associated types' metadata in the witness table,
  and specialises as an optimisation. Rust monomorphises, has associated
  types, and requires a trait object to fix them (`dyn Iterator<Item = T>`).
- **Representation polymorphism.** Eisenberg and Peyton Jones, _Levity
  Polymorphism_ (PLDI 2017). Meadow's descriptors are the run-time
  alternative to GHC's restriction.
- **Abstract indices from run-time checks.** Noonan, _Ghosts of Departed
  Proofs_ (Haskell Symposium 2018). `runSt`'s rank-2 state variable is
  Meadow's own instance.
- **Low-level code as an effect.** F\*'s Low\* and KaRaMeL (Protzenko et al.,
  ICFP 2017), the model for Idyll's `Mem`.

## Questions for review

Each question is a decision for review. The options are listed, then a
recommendation.

### Q1. Where the prototype lives

- **(a)** Inside `bootstrap/`, as new modules behind an option that is off by
  default.
- **(b)** A separate package in this repository holding the core calculus
  (the checker, NbE, staging) with no dependency on MeadowBoot, which
  MeadowBoot depends on and wires in behind an option.
- **(c)** A separate repository.

**Recommendation: (b).** The calculus can be built and tested without
touching the compiler that ships as `meadow` and gates releases, and the
wiring into MeadowBoot (L1) is small and lands behind an option. It is also
the shape that lets Idyll share the kernel later (Q8). The package needs a
PascalCase name.

### Q2. Surface syntax for the meta level and for stages

- **Meta definitions:**
  - **(a)** the attribute `@compileTime fun` / `@compileTime def`, extending
    the `@compileTime def` that exists today;
  - **(b)** a new keyword (`meta fun`);
  - **(c)** inferred from a signature that mentions a meta type.
- **Quote and splice where inference cannot decide:**
  - **(d)** `quote { e }` and `$x` / `$(e)`, the spelling `quote!` and macro
    templates use now;
  - **(e)** Kovács's `<e>` and `~e`;
  - **(f)** MetaOCaml's `.<e>.` and `.~e`.
- **Laws:** `@law fun`, or a keyword.

**Recommendation: (a) with (c) as well, (d), and `@law`.** Attributes add no
reserved words, so no existing program that uses `meta`, `law` or `quote` as a
name breaks. (d) reuses a spelling Meadow programmers already know, and it is
LL-decidable where `<e>` is not.

### Q3. Does the meta level get effects?

- **(a)** None: the meta level is pure and total, so code generators must be
  total too.
- **(b)** Two tiers. What a type or a proof depends on must be pure and total,
  as Idyll requires (types depend only on terms with an empty row). Code
  generators may also perform `Div` (fuel, as macros have now), `Fail` (a
  compile error at a place), and `Expand` (compile-time bindings). They may
  never perform `Fs`, `Time` or `Random`, so their answers stay cacheable.
- **(c)** Any effect at compile time.

**Recommendation: (b).** It keeps today's procedural macros expressible on
the meta level (they loop under fuel, fail at places, and talk through
`Expand`), keeps proofs sound, and matches Idyll's rule.

### Q4. How totality is checked

- **(a)** Structural and size-change termination on case trees, as Idyll and
  Agda check it, with `decreases` measures later.
- **(b)** Fuel, which is unsound for proofs.
- **(c)** Measures discharged by an SMT solver from the start.
- **(d)** Trust an annotation.

**Recommendation: (a)**, ported from or shared with Idyll's checker, plus an
`@assumeTotal` escape that makes a definition trusted, listed and warned
about, so that a definition the checker cannot see through can still be
used. Measures come with Idyll's solver work.

### Q5. Universes and the logic

- **(a)** `Type : Type` for good (simpler, inconsistent).
- **(b)** `Type : Type` while prototyping (L3 to L6), then a predicative
  hierarchy with level inference before proofs (L9).
- **(c)** A hierarchy from the start.
- **Also:** Axiom K (Idyll keeps it), function extensionality, a `Prop`
  universe.

**Recommendation: (b), with K kept, as Idyll has it.** Kovács's demo is
type-in-type, and nothing before L9 depends on consistency. Funext and `Prop`
can wait until a proof needs them.

### Q6. How `Mem` regions are indexed

- **(a)** Abstract, erased index variables introduced by rank-2 functions, as
  `runSt` introduces `s`. These carry identity and lifetime.
- **(b)** Meta-level values: static region names, sizes and layouts.
- **(c)** Idyll's ghost heap, with pre- and postconditions on `Mem`'s
  operations.
- **(d)** Wait: Meadow takes no `Mem` until Idyll's design is built, then
  adopts it.

**Recommendation: (a) for identity and lifetime and (b) for static sizes and
layouts, with (c) only inside proof-carrying modules and as Idyll designs it.**
In practice that means (d) for the effect itself: Meadow's indices (L7) are
built so that Idyll's `Mem` fits them, and the effect is not designed twice.

### Q7. The default strategy, and markers

- **(a)** Debug uses dictionaries (with today's O1 known-dictionary reads), and
  release specialises wherever it can.
- **(b)** Dictionaries everywhere unless a definition is marked.
- **(c)** Specialise everywhere, with debug too.
- **Markers:** `@specialise` (always, in every profile), and `@dictionaries`
  (never copied, for code size).
- **The setting:** `traits = "dictionaries" | "specialise"` in `[profile.*]`
  and by `--set`, replacing `dictionaries : Bool`.

**Recommendation: (a), both markers, and the setting as written.**
`@specialise` is also implied by a code-generating member, so a user rarely
writes it.

### Q8. How much Idyll shares

- **(a)** Only Cut, as today.
- **(b)** A kernel library extracted from Idyll's `front/` (NbE, unification,
  case trees, termination, positivity) into a `meadow-lang` package that both
  depend on.
- **(c)** Idyll's theory as Meadow's meta level: meta code written in, or
  imported from, Idyll.

**Recommendation: (b), agreed with Idyll's session**, which owns that code.
It avoids two dependent checkers, and it keeps the logics in step (K, `Int`,
`Div`, the rule on rows in types). (c) is attractive later. Grades stay out
of Meadow's meta level unless `Mem` needs them.

(b) is not only moving files, because `front/`'s kernel is graded
throughout. `Value.Pi` and `Core.Pi` each carry two grades as terms, one
for run-time use and one for type-level use (as in graded type theory).
Grades are polynomial normal forms that unification solves, and
`Usage.mw`, `Grade.mw` and the effect rows share that machinery. So (b)
comes in two forms:

- **(b1)** Meadow's meta level runs the graded kernel with every grade fixed
  at ω. This is cheaper, and keeps the two logics in step.
- **(b2)** The kernel is parameterised over a grade algebra. This is cleaner,
  but it means a real refactor of `Value`, `Unify` and `Check`.

(b1) is the place to start; (b2) can follow if `Mem` brings grades into
Meadow.

### Q9. Partial primitives in the shared fragment

- **(a)** Definitions using `/`, `%`, out-of-bounds indexing or a partial
  match are object-only.
- **(b)** Allowed, with the meta copy treating failure as a compile error
  when it is evaluated, and laws carrying side conditions.
- **(c)** Checked forms that take a proof of the precondition (`div x y {y != 0}`).

**Recommendation: (a) now, (c) after L6.** (b) makes the meta level partial,
which is what the design exists to avoid.

### Q10. Shared-fragment membership: inferred or declared

- **(a)** Inferred silently, with `@shared` to assert it.
- **(b)** Only what is marked `@shared` is shared.
- **(c)** Inferred, with a warning when something a proof uses leaves the
  fragment.

**Recommendation: (a), plus the error of (c) at the point of use.** Inference
is what keeps existing code annotation-free and makes most of Std shared on
day one. The marker and the use-site error stop silent drift.

### Q11. Laws on impls outside the shared fragment

- **(a)** Refused.
- **(b)** Assumed, and listed as trusted.
- **(c)** Turned into a property test generated from the law and run by
  `meadow test` with Clover.

**Recommendation: (b) and (c) together.** An object-only impl states its law,
the law is trusted for proofs, and it is tested at run time. That is useful
evidence for people and agents, and honest about what is proved.

### Q12. Trait objects in the surface language

- **(a)** A `dyn Trait` type, with associated types optional
  (`dyn Stream`, `dyn Stream { Token = Char }`).
- **(b)** General existentials, `exists a. C a => T a`, with explicit pack and
  open.
- **(c)** None for now, with descriptors in dictionaries only for what
  already exists.

**Recommendation: (a) in L5**, with (b) left to later. Trait objects are the
common case. Making their associated types optional is what descriptors in
dictionaries buy, and Rust cannot offer it.
