//! Tail recursion modulo context (`meadow_cut::trmc`): which definitions get
//! a twin in destination-passing style, and that a program with them answers
//! what it answered without -- run by the reference interpreter as written,
//! and by the AxCut machine lowered both ways.

use meadow_axcut::machine::{Machine, Value};
use meadow_cut::interp::{Options, run};
use meadow_cut::{Program, parse, print};

/// Lists and trees, and their text: each element's digits and a space, or a
/// tree as `(left value right)` with `.` for a leaf.
const PRELUDE: &str = "cut 0
entry t:Main/main
answer str

data t:Main/L { Nil; Cons(i64, ptr) }
data t:Main/T { Leaf; Node(ptr, i64, ptr) }
data t:Main/S { Stop; Step(ptr) }

def t:Main/digits (n: i64, acc: ptr; k: ptr) =
  prim mod(n, 10; μ̃ d: i64.
    prim arrayGet([\"0\", \"1\", \"2\", \"3\", \"4\", \"5\", \"6\", \"7\", \"8\", \"9\"], d; μ̃ s: str.
      prim lt(n, 10;
        μ̃ u: unit. prim div(n, 10; μ̃ q: i64. t:Main/digits(q, acc; μ̃ a: ptr. prim arrayPush(a, s; k))),
        μ̃ u: unit. prim arrayPush(acc, s; k))))

def t:Main/text (xs: ptr, acc: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => prim concatStrings(acc; k);
               t:Main/L.Cons(x: i64, r: ptr) =>
                 t:Main/digits(x, acc; μ̃ a: ptr. prim arrayPush(a, \" \"; μ̃ b: ptr. t:Main/text(r, b; k))) }>

def t:Main/tree (t: ptr, acc: ptr; k: ptr) =
  <t | case { t:Main/T.Leaf => prim arrayPush(acc, \".\"; k);
              t:Main/T.Node(l: ptr, v: i64, r: ptr) =>
                prim arrayPush(acc, \"(\"; μ̃ a: ptr. t:Main/tree(l, a; μ̃ b: ptr.
                  t:Main/digits(v, b; μ̃ c: ptr. t:Main/tree(r, c; μ̃ d: ptr. prim arrayPush(d, \")\"; k))))) }>

def t:Main/upto (i: i64, n: i64; k: ptr) =
  prim gt(i, n;
    μ̃ u: unit. prim add(i, 1; μ̃ j: i64. t:Main/upto(j, n; μ̃ r: ptr. <t:Main/L.Cons(i, r) | k>)),
    μ̃ u: unit. <t:Main/L.Nil | k>)

def t:Main/sum (xs: ptr, acc: i64; k: ptr) =
  <xs | case { t:Main/L.Nil => <acc | k>;
               t:Main/L.Cons(x: i64, r: ptr) => prim add(acc, x; μ̃ a: i64. t:Main/sum(r, a; k)) }>

def t:Main/map (f: ptr, xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) =>
                 <f | apply(x; μ̃ y: i64. t:Main/map(f, r; μ̃ ys: ptr. <t:Main/L.Cons(y, ys) | k>))> }>
";

/// `defs` and a `main` that answers `body`'s string to `k`.
fn program(defs: &str, body: &str) -> String {
    format!("{PRELUDE}\n{defs}\ndef t:Main/main (; k: ptr) =\n  {body}\n")
}

/// `call`, a call written up to its last argument, given the continuation
/// that answers the list it makes, as text, to `k`.
fn shown(call: &str) -> String {
    format!("{call}; μ̃ out: ptr. t:Main/text(out, []; k))")
}

/// What the AxCut machine answers of `p`, lowered as it is.
fn machine(p: &Program) -> String {
    let lowered = meadow_cut::lower::lower(p).unwrap_or_else(|e| panic!("not lowered: {e}"));
    // Its continuations are on the heap, but letting go of a long chain of
    // them, or of a long list, is as deep as it is.
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || match Machine::run(&lowered, 2_000_000_000) {
            Ok(Value::Str(s)) => s.to_string(),
            Ok(v) => format!("answered {v}"),
            Err(e) => format!("fails: {}", e.msg),
        })
        .unwrap()
        .join()
        .unwrap()
}

/// The twins `p` was given, by their definitions' last names.
fn twins(p: &Program) -> Vec<String> {
    let mut out: Vec<String> = p
        .defs
        .iter()
        .filter(|d| d.symbol.last() == "#dps")
        .map(|d| d.symbol.path[d.symbol.path.len() - 2].clone())
        .collect();
    out.sort();
    out
}

/// `text` rewritten: read back from its print the same, answering what the
/// interpreter answers of it as written, on the AxCut machine both ways.
/// The twins it was given, and what it answers.
#[track_caller]
fn rewritten(text: &str) -> (Vec<String>, String) {
    let p = parse(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let want = run(&p, &Options::default())
        .unwrap_or_else(|(e, _)| panic!("the interpreter: {e}"))
        .output;
    let after = meadow_cut::trmc::program(&p);
    let shown = print::program(&after);
    let again = parse(&shown).unwrap_or_else(|e| panic!("rewritten, read back: {e}\n{shown}"));
    assert_eq!(after, again, "printed and read back:\n{shown}");
    assert_eq!(machine(&p), want, "as written, on the AxCut machine");
    assert_eq!(
        machine(&after),
        want,
        "rewritten, on the AxCut machine:\n{shown}"
    );
    // What the back end runs: the program inlined first.
    let optimized = meadow_cut::lower::optimized(&p);
    assert_eq!(machine(&optimized), want, "optimized, on the AxCut machine");
    (twins(&after), want)
}

#[test]
fn a_map_and_the_list_it_maps_are_built_front_to_back() {
    let text = program(
        "",
        &(shown(
            "t:Main/upto(1, 5; μ̃ xs: ptr.\n    \
             t:Main/map(cocase { apply(x: i64; j: ptr) => prim mul(x, 10; j) }, xs",
        ) + ")"),
    );
    let (twins, out) = rewritten(&text);
    assert_eq!(twins, ["map", "upto"]);
    assert_eq!(out, "10 20 30 40 50 ");
}

#[test]
fn a_filter_passes_its_destination_on_where_it_skips() {
    let text = program(
        "def t:Main/evens (xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) =>
                 prim mod(x, 2; μ̃ m: i64. prim eq(m, 0;
                   μ̃ u: unit. t:Main/evens(r; k),
                   μ̃ u: unit. t:Main/evens(r; μ̃ ys: ptr. <t:Main/L.Cons(x, ys) | k>))) }>
",
        &(shown("t:Main/upto(1, 9; μ̃ xs: ptr. t:Main/evens(xs") + ")"),
    );
    let (twins, out) = rewritten(&text);
    assert_eq!(twins, ["evens", "map", "upto"]);
    assert_eq!(out, "2 4 6 8 ");
}

#[test]
fn an_append_ends_with_the_list_it_did_not_build() {
    let text = program(
        "def t:Main/append (xs: ptr, ys: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <ys | k>;
               t:Main/L.Cons(x: i64, r: ptr) => t:Main/append(r, ys; μ̃ zs: ptr. <t:Main/L.Cons(x, zs) | k>) }>
",
        &(shown(
            "t:Main/upto(1, 3; μ̃ a: ptr. t:Main/upto(7, 9; μ̃ b: ptr. t:Main/append(a, b",
        ) + "))"),
    );
    let (twins, out) = rewritten(&text);
    assert_eq!(twins, ["append", "map", "upto"]);
    assert_eq!(out, "1 2 3 7 8 9 ");
}

#[test]
fn a_take_stops_at_a_count() {
    let text = program(
        "def t:Main/take (n: i64, xs: ptr; k: ptr) =
  prim eq(n, 0;
    μ̃ u: unit. <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
                            t:Main/L.Cons(x: i64, r: ptr) =>
                              prim sub(n, 1; μ̃ m: i64. t:Main/take(m, r; μ̃ ys: ptr. <t:Main/L.Cons(x, ys) | k>)) }>,
    μ̃ u: unit. <t:Main/L.Nil | k>)
",
        &(shown("t:Main/upto(1, 100; μ̃ xs: ptr. t:Main/take(4, xs") + ")"),
    );
    let (twins, out) = rewritten(&text);
    assert_eq!(twins, ["map", "take", "upto"]);
    assert_eq!(out, "1 2 3 4 ");
}

/// A tree fills its last subtree in place, and makes its first by a call
/// that returns, which is to the definition itself, and so builds that
/// subtree in place too.
#[test]
fn a_tree_is_built_with_its_last_subtree_in_place() {
    let text = program(
        "def t:Main/build (d: i64, v: i64; k: ptr) =
  prim eq(d, 0;
    μ̃ u: unit. prim sub(d, 1; μ̃ e: i64. prim mul(v, 2; μ̃ w: i64.
      t:Main/build(e, w; μ̃ l: ptr. prim add(w, 1; μ̃ w1: i64.
        t:Main/build(e, w1; μ̃ r: ptr. <t:Main/T.Node(l, v, r) | k>))))),
    μ̃ u: unit. <t:Main/T.Leaf | k>)
",
        "t:Main/build(3, 1; μ̃ t: ptr. t:Main/tree(t, []; μ̃ a: ptr. prim concatStrings(a; k)))",
    );
    let (twins, out) = rewritten(&text);
    assert_eq!(twins, ["build", "map", "upto"]);
    assert_eq!(out, "(((.4.)2(.5.))1((.6.)3(.7.)))");
}

/// Two cells at once, written nested or by `let`: the inner one is the
/// destination. And a tuple on the way is a cell like any constructor.
#[test]
fn nested_constructors_and_tuples_are_one_context() {
    let text = program(
        "def t:Main/twice (xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) =>
                 t:Main/twice(r; μ̃ ys: ptr. <t:Main/L.Cons(x, t:Main/L.Cons(x, ys)) | k>) }>

def t:Main/thrice (xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) =>
                 t:Main/thrice(r; μ̃ ys: ptr. let a: ptr = t:Main/L.Cons(x, ys) in
                   let b: ptr = t:Main/L.Cons(x, a) in <t:Main/L.Cons(x, b) | k>) }>

def t:Main/steps (n: i64; k: ptr) =
  prim eq(n, 0;
    μ̃ u: unit. prim sub(n, 1; μ̃ m: i64. t:Main/steps(m; μ̃ s: ptr. <t:Main/S.Step(#tuple(n, s)) | k>)),
    μ̃ u: unit. <t:Main/S.Stop | k>)

def t:Main/unstep (s: ptr; k: ptr) =
  <s | case { t:Main/S.Stop => <t:Main/L.Nil | k>;
              t:Main/S.Step(p: ptr) => <p | case { #tuple(n: i64, rest: ptr) =>
                t:Main/unstep(rest; μ̃ ns: ptr. <t:Main/L.Cons(n, ns) | k>) }> }>
",
        &(shown(
            "t:Main/upto(1, 2; μ̃ xs: ptr. t:Main/twice(xs; μ̃ a: ptr.\n    \
             t:Main/thrice(xs; μ̃ b: ptr. t:Main/steps(3; μ̃ s: ptr. t:Main/unstep(s; μ̃ c: ptr.\n    \
             t:Main/append3(a, b, c",
        ) + ")))))"),
    )
    .replace(
        "\ndef t:Main/main",
        "\ndef t:Main/append3 (a: ptr, b: ptr, c: ptr; k: ptr) =
  <a | case { t:Main/L.Nil => <b | case { t:Main/L.Nil => <c | k>;
                                          t:Main/L.Cons(x: i64, r: ptr) => t:Main/append3(t:Main/L.Nil, r, c; μ̃ ys: ptr. <t:Main/L.Cons(x, ys) | k>) }>;
              t:Main/L.Cons(x: i64, r: ptr) => t:Main/append3(r, b, c; μ̃ ys: ptr. <t:Main/L.Cons(x, ys) | k>) }>

def t:Main/main",
    );
    let (twins, out) = rewritten(&text);
    assert_eq!(
        twins,
        [
            "append3", "map", "steps", "thrice", "twice", "unstep", "upto"
        ]
    );
    assert_eq!(out, "1 1 2 2 1 1 1 2 2 2 3 2 1 ");
}

/// Definitions that call each other, one of them under a context: both are
/// given twins, and the list is built across them.
#[test]
fn mutually_recursive_definitions_build_one_list_between_them() {
    let text = program(
        "def t:Main/odds (xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) => t:Main/evens(r; μ̃ ys: ptr. <t:Main/L.Cons(x, ys) | k>) }>

def t:Main/evens (xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) => t:Main/odds(r; k) }>
",
        &(shown("t:Main/upto(1, 9; μ̃ xs: ptr. t:Main/odds(xs") + ")"),
    );
    let (twins, out) = rewritten(&text);
    assert_eq!(twins, ["evens", "map", "odds", "upto"]);
    assert_eq!(out, "1 3 5 7 9 ");
}

/// What is not a context is left as written: a continuation that computes
/// after the call answers, uses the answer twice, puts it where no
/// constructor is, or is under a handler -- and a definition that only
/// calls itself in tail position, which needs nothing.
#[test]
fn what_is_not_a_context_is_left_as_it_is() {
    let text = program(
        "data t:Main/P { P(ptr, ptr) }

effect t:Main/Ask { ask(unit) -> i64 }

def t:Main/plus (xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) =>
                 t:Main/plus(r; μ̃ ys: ptr. prim add(x, 1; μ̃ y: i64. <t:Main/L.Cons(y, ys) | k>)) }>

def t:Main/both (n: i64; k: ptr) =
  prim eq(n, 0;
    μ̃ u: unit. prim sub(n, 1; μ̃ m: i64. t:Main/both(m; μ̃ p: ptr. <t:Main/P.P(p, p) | k>)),
    μ̃ u: unit. <t:Main/L.Nil | k>)

def t:Main/boxed (n: i64; k: ptr) =
  prim eq(n, 0;
    μ̃ u: unit. prim sub(n, 1; μ̃ m: i64. t:Main/boxed(m; μ̃ p: ptr. <record { inner = p } | k>)),
    μ̃ u: unit. <t:Main/L.Nil | k>)

def t:Main/asked (n: i64; k: ptr) =
  prim eq(n, 0;
    μ̃ u: unit. prim sub(n, 1; μ̃ m: i64.
      handle {
        t:Main/Ask.ask(u: unit; r: ptr, h: ptr) => <r | apply(7; h)>;
        return(x: ptr; h: ptr) => <x | h>
      } in μ b.
        perform t:Main/Ask.ask(unit; μ̃ a: i64. t:Main/asked(m; μ̃ ys: ptr. <t:Main/L.Cons(a, ys) | b>))
      ; k),
    μ̃ u: unit. <t:Main/L.Nil | k>)
",
        "t:Main/upto(1, 3; μ̃ xs: ptr. t:Main/plus(xs; μ̃ a: ptr.\n    \
         t:Main/both(2; μ̃ p: ptr. t:Main/boxed(2; μ̃ q: ptr. t:Main/asked(3; μ̃ c: ptr.\n    \
         t:Main/sum(a, 0; μ̃ s: i64. t:Main/text(t:Main/L.Cons(s, c), []; k)))))))",
    );
    let (twins, out) = rewritten(&text);
    // `text` calls itself in tail position only; `digits` computes after
    // its call; `sum` accumulates.
    assert_eq!(twins, ["map", "upto"]);
    assert_eq!(out, "9 7 7 7 ");
}

/// A program that may resume an operation more than once has only the
/// definitions no handler can capture rewritten: running the rest of a
/// recursion twice would write the same cells twice.
#[test]
fn where_a_resumption_may_run_twice_only_what_cannot_be_captured_is_rewritten() {
    let text = program(
        "effect t:Main/Choose { @many choose(unit) -> bool }
",
        &(shown(
            "t:Main/upto(1, 3; μ̃ xs: ptr.\n    \
             t:Main/map(cocase { apply(x: i64; j: ptr) => prim mul(x, 2; j) }, xs",
        ) + ")"),
    );
    let (twins, out) = rewritten(&text);
    // `map` invokes the function it is given, which might perform.
    assert_eq!(twins, ["upto"]);
    assert_eq!(out, "2 4 6 ");
}

/// What the rewrite is for: a list of a million built and mapped, and
/// answering what the program as written answers.
#[test]
fn a_long_list_is_built_and_mapped() {
    let text = program(
        "",
        "t:Main/upto(1, 1000000; μ̃ xs: ptr.\n    \
         t:Main/map(cocase { apply(x: i64; j: ptr) => prim mul(x, 3; j) }, xs; μ̃ ys: ptr.\n    \
         t:Main/sum(ys, 0; μ̃ s: i64. t:Main/digits(s, []; μ̃ a: ptr. prim concatStrings(a; k)))))",
    );
    let p = parse(&text).unwrap();
    let after = meadow_cut::trmc::program(&p);
    assert_eq!(twins(&after), ["map", "upto"]);
    assert_eq!(machine(&after), "1500001500000");
}
