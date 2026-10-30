//! **Macros**: the built-in ones, and what expansion does with a call that is
//! wrong.
//!
//! There are no `macro` declarations yet (see `docs/MACROS.md`), so what is
//! tested here is the machinery every macro will go through: a call parsed as
//! an opaque argument, expanded to tokens, and those tokens parsed back with
//! the entry point for the position the call was in.

mod common;
use meadow::{Engine, OptLevel, Options, pipeline, runtime};
use std::path::Path;

/// What `src` evaluates to, required to be the same on every engine -- a macro
/// is gone by the time anything runs, so all of them must agree.
fn agreed(src: &str) -> String {
    let (program, diags) =
        pipeline::compile_str_with_std("test", src, Options::debug().entry("result"));
    assert!(
        diags.is_empty(),
        "compile errors in\n{src}\n{}",
        diags
            .iter()
            .map(|d| d.msg.clone())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let program = meadow_compiler::core::prune::prune(&program);
    let cek = runtime::run(&program, Engine::Cek, OptLevel::O1)
        .unwrap_or_else(|e| panic!("the CEK machine failed on\n{src}\n{e}"));
    for engine in [Engine::Vm, Engine::Jit] {
        let got = runtime::run(&program, engine, OptLevel::O1)
            .unwrap_or_else(|e| panic!("{engine:?} failed: {e}"));
        assert_eq!(got, cek, "{engine:?} on\n{src}");
    }
    cek
}

fn is(src: &str, expected: &str) {
    assert_eq!(agreed(src), expected, "{src}");
}

fn errors(src: &str) -> String {
    common::errors_std_with(src, Options::debug().entry("result"))
}

// --- what the built-ins answer ------------------------------------------------

#[test]
fn stringify_gives_back_the_tokens_it_was_given() {
    is(r#"def result = stringify!(1 + f x)"#, r#""1 + f x""#);
}

#[test]
fn stringify_normalises_the_spacing_rather_than_remembering_it() {
    // A token tree does not know what whitespace it was written with, so the
    // text is the tokens written back, not the source.
    is(r#"def result = stringify!( 1   +    2 )"#, r#""1 + 2""#);
}

#[test]
fn stringify_takes_anything_that_balances_its_brackets() {
    // Not an expression, and never parsed as one.
    is(r#"def result = stringify!(fun let ->)"#, r#""fun let ->""#);
}

#[test]
fn concat_joins_its_literals() {
    is(r#"def result = concat!("a", "b", "c")"#, r#""abc""#);
    is(r#"def result = concat!("n = ", 42)"#, r#""n = 42""#);
    is(r#"def result = concat!('x', 'y')"#, r#""xy""#);
}

#[test]
fn concat_of_nothing_is_the_empty_string() {
    is(r#"def result = concat!()"#, r#""""#);
}

#[test]
fn line_is_the_line_the_call_is_written_on() {
    // Line 1 is empty -- the literal starts with a newline -- so `main` is on
    // line 2 and the call on line 3.
    is("\ndef result =\n  line!()\n", "3");
}

#[test]
fn file_is_the_module_the_call_is_written_in() {
    is(r#"def result = file!()"#, r#""test""#);
}

// --- where a call may stand ---------------------------------------------------

#[test]
fn the_three_brackets_mean_the_same_thing() {
    let round = agreed(r#"def result = stringify!(a b)"#);
    let square = agreed(r#"def result = stringify![a b]"#);
    let curly = agreed(r#"def result = stringify!{a b}"#);
    assert_eq!(round, square);
    assert_eq!(round, curly);
}

#[test]
fn a_call_is_an_expression_like_any_other() {
    // Application is juxtaposition, so a call is an *atom*: `twice stringify!(…)`
    // is `twice` applied to the expansion, not to `twice stringify` and a `!`.
    is(
        r#"
def result =
  let twice s = s ++ s in
  twice stringify!(a b)
"#,
        r#""a ba b""#,
    );
}

#[test]
fn a_call_can_stand_where_a_pattern_does() {
    // `concat!` expands to a string literal, and a string literal is a pattern.
    is(
        r#"
def result =
  match "ab" with
  | concat!("a", "b") -> "matched"
  | _ -> "no"
"#,
        r#""matched""#,
    );
}

#[test]
fn a_call_inside_an_interpolation_hole_is_expanded() {
    is(r#"def result = "n = ${stringify!(a b)}""#, r#""n = a b""#);
}

#[test]
fn a_call_is_expanded_wherever_an_expression_is_nested() {
    is(
        r#"
def result =
  let go x = if x then stringify!(yes) else stringify!(no) in
  go True
"#,
        r#""yes""#,
    );
}

// --- what expansion says when a call is wrong ---------------------------------

#[test]
fn an_unknown_macro_is_reported_by_name() {
    let e = errors(r#"def result = nope!(1)"#);
    assert!(e.contains("there is no macro `nope!`"), "{e}");
}

#[test]
fn a_macro_that_takes_nothing_says_so_when_given_something() {
    let e = errors(r#"def result = line!(1)"#);
    assert!(e.contains("`line!` takes no arguments"), "{e}");
}

#[test]
fn concat_of_something_that_is_not_a_literal_is_reported() {
    let e = errors(r#"def result = concat!("a", f x)"#);
    assert!(e.contains("`concat!` takes literals"), "{e}");
}

#[test]
fn a_macro_that_does_not_expand_to_a_declaration_is_reported_there() {
    // `line!()` is an integer, which is an expression and not a declaration.
    let e = errors("line!()\ndef result = 1");
    assert!(e.contains("`line!` did not expand to declarations"), "{e}");
}

#[test]
fn a_failed_call_does_not_hide_the_rest_of_the_module() {
    // The call is replaced by something harmless, so the undefined name after
    // it is still reported: one mistake should not swallow the next.
    let e = errors(
        r#"
def result =
  let a = nope!(1) in
  undefinedName
"#,
    );
    assert!(e.contains("there is no macro `nope!`"), "{e}");
    assert!(e.contains("undefinedName"), "{e}");
}

// --- how expansion sits with the passes around it -----------------------------

#[test]
fn a_call_under_a_cfg_that_does_not_hold_is_never_expanded() {
    // `@cfg` is stripped first, so the unknown macro is gone before expansion
    // looks -- exactly as an unparseable body under a false `@cfg` would be.
    let e = errors(
        r#"
@cfg(not(debug))
def unused = nope!(1)

def result = 1
"#,
    );
    assert_eq!(e, "", "a stripped declaration was still expanded");
}

#[test]
fn a_macro_call_is_not_confused_with_a_not_equal() {
    // `!=` is one token, so `foo != x` can never be read as a call to `foo!`.
    is(
        r#"
def result = if 1 != 2 then "differ" else "same"
"#,
        r#""differ""#,
    );
}

// --- `macro` declarations -----------------------------------------------------

#[test]
fn a_rule_binds_what_its_matcher_stood_for() {
    is(
        r#"
macro swap
  | ($a, $b) -> { ($b, $a) }

def result = swap!(1, "two")
"#,
        r#"("two", 1)"#,
    );
}

#[test]
fn the_first_rule_that_fits_is_the_one_taken() {
    is(
        r#"
macro pick
  | ()       -> { "none" }
  | ($x)     -> { "one" }
  | ($x, $y) -> { "two" }

def result = (pick!(), pick!(1), pick!(1, 2))
"#,
        r#"("none", "one", "two")"#,
    );
}

#[test]
fn a_macro_can_be_called_above_where_it_is_written() {
    // Definitions are read before anything is expanded, so a macro is in scope
    // in its whole module -- as every other top-level name is.
    is(
        r#"
def result = later!(1)

macro later
  | ($x) -> { $x + 1 }
"#,
        "2",
    );
}

#[test]
fn a_repetition_matches_a_run_and_writes_one_back() {
    is(
        r#"
macro listOf
  | ($( $x ),*) -> { [ $( $x );* ] }

def result = listOf!(1, 2, 3)
"#,
        "[1; 2; 3]",
    );
}

#[test]
fn the_template_may_separate_a_run_differently_from_the_matcher() {
    // `,` going in, `;` coming out: the separator is part of how each side is
    // written, not part of what was matched.
    is(
        r#"
macro sumOf
  | ($( $x ),*) -> { [ $( $x );* ] }

def result = sumOf!(1, 2)
"#,
        "[1; 2]",
    );
}

#[test]
fn a_repetition_of_none_is_still_a_match() {
    // With nothing to write, the template's separator is not written either --
    // so these brackets come out empty, which is the empty `Vector` rather than
    // the empty `List` a `;` would have made it. That is the template's doing,
    // not the repetition's: a macro that wants `[;]` writes a rule for none.
    is(
        r#"
macro listOf
  | ($( $x ),*) -> { [ $( $x );* ] }

def result = listOf!()
"#,
        "[]",
    );
}

#[test]
fn a_template_may_call_the_macro_it_is_in() {
    is(
        r#"
macro sum
  | ()              -> { 0 }
  | ($x)            -> { $x }
  | ($x, $( $r ),+) -> { $x + sum!($( $r ),+) }

def result = sum!(1, 2, 3, 4)
"#,
        "10",
    );
}

#[test]
fn a_macro_can_expand_to_declarations() {
    is(
        r#"
macro constant
  | ($name, $value) -> { fun $name = $value }

constant!(four, 4)

def result = four
"#,
        "4",
    );
}

#[test]
fn a_call_on_a_line_of_its_own_begins_a_declaration() {
    // Not one more argument to the expression above it: nothing but a
    // keyword or an `@` ends that expression otherwise, and a call written in
    // column 0 is a declaration.
    is(
        r#"
macro constant
  | ($name, $value) -> { fun $name = $value }

fun twice x = x + x
def result = twice four
constant!(four, 4)
"#,
        "8",
    );
}

#[test]
fn a_call_that_is_indented_is_still_an_argument() {
    is(
        "fun twice x = x ++ x\ndef result =\n  twice\n  stringify!(ab)\n",
        r#""abab""#,
    );
}

#[test]
fn a_double_dollar_writes_one_dollar_token() {
    // `$$` is an escape between *tokens*, so it has nothing to do with a `$`
    // inside a string literal, which is already just text.
    //
    // What it writes is a bare `$`, and a bare `$` is not an expression in
    // Meadow -- it is only ever macro syntax. So the escape has nothing to be
    // useful for until a macro can write another macro, and until then this is
    // what it does. See `docs/MACROS.md`.
    let e = errors(
        r#"
macro dollars
  | () -> { $$ }

def result = dollars!()
"#,
    );
    assert!(
        e.contains("`dollars!` did not expand to an expression"),
        "{e}"
    );
}

#[test]
fn a_dollar_inside_a_string_a_template_writes_is_text() {
    is(
        r#"
macro dollars
  | () -> { "$$" }

def result = dollars!()
"#,
        r#""$$""#,
    );
}

// --- fragments ----------------------------------------------------------------

#[test]
fn an_ident_fragment_takes_only_an_identifier() {
    is(
        r#"
macro name
  | ($x : ident) -> { stringify!($x) }

def result = name!(hello)
"#,
        r#""hello""#,
    );
}

#[test]
fn a_lit_fragment_takes_only_a_literal() {
    is(
        r#"
macro twice
  | ($x : lit) -> { ($x, $x) }

def result = twice!(7)
"#,
        "(7, 7)",
    );
}

#[test]
fn a_fragment_that_is_the_wrong_sort_does_not_match() {
    let e = errors(
        r#"
macro name
  | ($x : ident) -> { $x }

def result = name!(1)
"#,
    );
    assert!(e.contains("no rule of `name!` matches"), "{e}");
}

#[test]
fn a_bare_metavariable_is_a_token_tree() {
    // `$x` with no kind is `$x : tt`, so it takes a bracketed run whole.
    is(
        r#"
macro first
  | ($x, $y) -> { $x }

def result = first!((1 + 2), 9)
"#,
        "3",
    );
}

// --- hygiene ------------------------------------------------------------------

#[test]
fn a_template_cannot_capture_a_name_the_caller_passed_in() {
    // The template binds `tmp` and also uses the caller's `$x`, which is the
    // caller's own `tmp`. Hygienic: 10 + 100. Captured: 10 + 10.
    is(
        r#"
macro addTen
  | ($x) -> { let tmp = 10 in tmp + $x }

def result =
  let tmp = 100 in
  addTen!(tmp)
"#,
        "110",
    );
}

#[test]
fn two_expansions_do_not_share_the_locals_they_introduce() {
    is(
        r#"
macro twice
  | ($x) -> { let t = $x in t + t }

def result = twice!(1) + twice!(2)
"#,
        "6",
    );
}

#[test]
fn a_name_the_template_does_not_bind_is_an_ordinary_one() {
    // `helper` is not bound by the template, so it is the function declared
    // here: items are not hygienic.
    is(
        r#"
fun helper n = n * 3

macro tripled
  | ($x) -> { helper $x }

def result = tripled!(5)
"#,
        "15",
    );
}

#[test]
fn a_record_label_a_template_writes_is_still_that_label() {
    // A label is not a variable, so it must not be marked -- it is matched by
    // name against the record's field.
    is(
        r#"
macro named
  | ($v) -> { { name = $v } }

def result = (named!("a")).name
"#,
        r#""a""#,
    );
}

#[test]
fn a_type_a_template_writes_is_still_that_type() {
    // Everything lowercase in this signature is written by the template: the
    // row variable `r`, and `name` as a field of the record type. Neither is a
    // variable, so neither may keep a hygiene mark -- a marked `name` would no
    // longer be the field the record has, and this would not type-check.
    is(
        r#"
macro getter
  | () -> { fun getName : { name : String | r } -> String = \p -> p.name }

getter!()

def result = getName { name = "x", age = 1 }
"#,
        r#""x""#,
    );
}

// --- what is said when a macro is written wrong -------------------------------

#[test]
fn a_call_that_fits_no_rule_is_reported() {
    let e = errors(
        r#"
macro pair
  | ($a, $b) -> { ($a, $b) }

def result = pair!(1)
"#,
    );
    assert!(e.contains("no rule of `pair!` matches this call"), "{e}");
}

#[test]
fn a_template_naming_something_its_matcher_does_not_bind_is_reported() {
    let e = errors(
        r#"
macro wrong
  | ($a) -> { $b }

def result = wrong!(1)
"#,
    );
    assert!(
        e.contains("`$b` is not bound by this rule's matcher"),
        "{e}"
    );
}

#[test]
fn a_fragment_kind_that_does_not_exist_is_reported() {
    let e = errors(
        r#"
macro wrong
  | ($a : ty) -> { $a }

def result = wrong!(1)
"#,
    );
    assert!(e.contains("is not a fragment kind"), "{e}");
}

#[test]
fn a_repetition_that_does_not_say_how_many_is_reported() {
    let e = errors(
        r#"
macro wrong
  | ($( $a )) -> { 1 }

def result = wrong!(1)
"#,
    );
    assert!(e.contains("does not say how many"), "{e}");
}

#[test]
fn a_run_written_without_a_repetition_around_it_is_reported() {
    let e = errors(
        r#"
macro wrong
  | ($( $a ),*) -> { $a }

def result = wrong!(1, 2)
"#,
    );
    assert!(e.contains("stands for a run of things"), "{e}");
}

#[test]
fn one_name_bound_twice_in_a_rule_is_reported() {
    let e = errors(
        r#"
macro wrong
  | ($a, $a) -> { $a }

def result = wrong!(1, 2)
"#,
    );
    assert!(e.contains("is bound twice"), "{e}");
}

#[test]
fn two_macros_of_one_name_are_reported() {
    let e = errors(
        r#"
macro same
  | () -> { 1 }

macro same
  | () -> { 2 }

def result = same!()
"#,
    );
    assert!(e.contains("defined twice"), "{e}");
}

#[test]
fn a_macro_may_not_take_a_built_in_name() {
    let e = errors(
        r#"
macro stringify
  | () -> { 1 }

def result = 1
"#,
    );
    assert!(e.contains("is a built-in macro"), "{e}");
}

/// Every diagnostic, labels and all: what a reader sees, rather than only the
/// headline `errors` gives.
fn errors_in_full(src: &str) -> String {
    let (_, diags) = pipeline::compile_str_with_std("test", src, Options::debug().entry("result"));
    diags
        .iter()
        .map(|d| {
            let labels: Vec<&str> = std::iter::once(d.label.0.as_str())
                .chain(d.extra_labels.iter().map(|l| l.0.as_str()))
                .collect();
            format!("{}\n{}", d.msg, labels.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_unknown_macro_says_which_ones_there_are() {
    let e = errors_in_full(
        r#"
macro known
  | () -> { 1 }

def result = unknown!()
"#,
    );
    assert!(e.contains("there is no macro `unknown!`"), "{e}");
    // The module's own macros and the built-ins, so the reader can see a typo.
    assert!(e.contains("`known!`"), "{e}");
    assert!(e.contains("`stringify!`"), "{e}");
}

#[test]
fn a_repetition_may_be_followed_by_more_of_the_matcher() {
    // The run must stop early enough to leave the `;` and the `$last` for the
    // rest of the matcher, rather than swallowing to the end.
    is(
        r#"
macro lastOf
  | ($( $a ),* ; $last) -> { ($last, [ $( $a );* ]) }

def result = lastOf!(1, 2, 3 ; 9)
"#,
        "(9, [1; 2; 3])",
    );
}

#[test]
fn a_repetition_of_none_before_more_of_the_matcher_still_matches() {
    is(
        r#"
macro lastOf
  | ($( $a ),* ; $last) -> { ($last, [ $( $a );* ]) }

def result = lastOf!( ; 9)
"#,
        "(9, [])",
    );
}

// --- fragments the parser reads -----------------------------------------------

#[test]
fn an_expr_fragment_takes_a_whole_expression() {
    is(
        r#"
macro twice
  | ($e : expr) -> { ($e, $e) }

def result = twice!(1 + 2)
"#,
        "(3, 3)",
    );
}

#[test]
fn an_expr_fragment_stays_one_thing_where_it_is_written() {
    // Application is juxtaposition, so a fragment written into a template has
    // to hold together: `show $x` with `$x` bound to `1 + 2` is `show (1 + 2)`,
    // which is what was passed, and not `(show 1) + 2`.
    is(
        r#"
macro call
  | ($f : expr, $x : expr) -> { $f $x }

def result = call!(show, 1 + 2)
"#,
        r#""3""#,
    );
}

#[test]
fn a_pat_fragment_takes_a_pattern() {
    is(
        r#"
macro matches
  | ($e : expr, $p : pat) -> { match $e with | $p -> True | _ -> False }

def result = (matches!(Just 1, Just x), matches!(None, Just x))
"#,
        "(True, False)",
    );
}

#[test]
fn an_item_fragment_takes_a_declaration() {
    is(
        r#"
macro alsoDefine
  | ($d : item) -> { $d }

alsoDefine! { def seven = 7 }

def result = seven
"#,
        "7",
    );
}

#[test]
fn a_fragment_the_parser_cannot_read_does_not_match() {
    let e = errors(
        r#"
macro twice
  | ($e : expr) -> { ($e, $e) }

def result = twice!(let)
"#,
    );
    assert!(e.contains("no rule of `twice!` matches"), "{e}");
}

#[test]
fn a_fragment_runs_to_the_token_the_matcher_says_follows_it() {
    // The `,` that ends the first fragment is the one at the top: the one
    // inside the brackets is part of the expression.
    is(
        r#"
macro pair
  | ($a : expr, $b : expr) -> { ($a, $b) }

def result = pair!(fst (1, 2), 3)
"#,
        "(1, 3)",
    );
}

#[test]
fn a_run_of_fragments_is_separated_as_the_matcher_says() {
    is(
        r#"
macro total
  | ($( $e : expr ),*) -> { foldl (\a b -> a + b) 0 [$( $e ),*] }

def result = total!(1 + 1, 2, 3)
"#,
        "7",
    );
}

#[test]
fn a_matcher_that_does_not_say_where_a_fragment_ends_is_reported() {
    let e = errors(
        r#"
macro wrong
  | ($f : expr $x : expr) -> { $f $x }

def result = wrong!(id 1)
"#,
    );
    assert!(e.contains("nothing says where `$f : expr` ends"), "{e}");
}

#[test]
fn a_fragment_followed_by_something_that_could_continue_it_is_reported() {
    // `+` could be part of the expression, so it does not end one.
    let e = errors(
        r#"
macro wrong
  | ($a : expr + $b : expr) -> { $a + $b }

def result = wrong!(1 + 2)
"#,
    );
    assert!(e.contains("nothing says where `$a : expr` ends"), "{e}");
}

#[test]
fn a_repeated_fragment_with_nothing_between_is_reported() {
    let e = errors(
        r#"
macro wrong
  | ($( $e : expr )*) -> { [$( $e );*] }

def result = wrong!(1 2)
"#,
    );
    assert!(e.contains("nothing says where `$e : expr` ends"), "{e}");
}

#[test]
fn a_fragment_at_the_end_of_a_matcher_is_ended_by_the_bracket() {
    is(
        r#"
macro discard
  | ($a : expr, $b : expr) -> { $b }

def result = discard!(1, 2 + 3)
"#,
        "5",
    );
}

// --- macros that cross a module or a package ----------------------------------

/// A package written out in a directory of its own. What a macro can be seen
/// from only means something across a boundary, so these tests need real ones.
fn package(what: &str, manifest: &str, files: &[(&str, &str)]) -> meadow_scratch::Scratch {
    let dir = meadow_scratch::scratch!("meadow-macro-{}-{what}", std::process::id());
    std::fs::create_dir_all(dir.join("src")).expect("a scratch package");
    std::fs::write(dir.join("Meadow.toml"), manifest).expect("a manifest");
    for (name, text) in files {
        std::fs::write(dir.join("src").join(name), text).expect("a module");
    }
    dir
}

/// What the package at `dir` evaluates to, or the diagnostics that stopped it.
fn build(dir: &Path) -> Result<String, Vec<String>> {
    let out = pipeline::build(dir, Options::debug().entry("result"));
    if !out.diagnostics.is_empty() {
        return Err(out.diagnostics.iter().map(|d| d.msg.clone()).collect());
    }
    let linked = out.linked.expect("a linked program");
    Ok(meadow_eval::run(&linked.program)
        .expect("the program runs")
        .to_string())
}

#[test]
fn a_macro_is_reached_from_another_module_of_its_package() {
    let dir = package(
        "sibling",
        "[package]\nname = \"Demo\"\nversion = \"0.1.0\"\n",
        &[
            (
                "Helpers.mw",
                "@pub macro twice\n  | ($e : expr) -> { ($e, $e) }\n",
            ),
            (
                "Lib.mw",
                "mod Helpers\n\nuse Demo.Helpers (twice!)\n\ndef result = twice!(1 + 1)\n",
            ),
        ],
    );
    assert_eq!(build(&dir).expect("it builds"), "(2, 2)");
}

#[test]
fn a_macro_reached_through_an_alias_is_written_with_it() {
    let dir = package(
        "alias",
        "[package]\nname = \"Demo\"\nversion = \"0.1.0\"\n",
        &[
            (
                "Helpers.mw",
                "@pub macro twice\n  | ($e : expr) -> { ($e, $e) }\n",
            ),
            (
                "Lib.mw",
                "mod Helpers\n\nuse Demo.Helpers as H\n\ndef result = H.twice!(3)\n",
            ),
        ],
    );
    assert_eq!(build(&dir).expect("it builds"), "(3, 3)");
}

#[test]
fn a_macro_that_is_not_public_stays_in_its_module() {
    let dir = package(
        "private",
        "[package]\nname = \"Demo\"\nversion = \"0.1.0\"\n",
        &[
            (
                "Helpers.mw",
                "@pub macro shown\n  | () -> { 1 }\n\nmacro hidden\n  | () -> { 2 }\n",
            ),
            (
                "Lib.mw",
                "mod Helpers\n\nuse Demo.Helpers (hidden!)\n\ndef result = hidden!()\n",
            ),
        ],
    );
    let errs = build(&dir).expect_err("it does not build");
    assert!(
        errs.iter()
            .any(|e| e.contains("does not export a macro `hidden!`")),
        "{errs:?}"
    );
}

#[test]
fn a_macro_crosses_into_a_package_that_depends_on_it() {
    let lib = package(
        "dep",
        "[package]\nname = \"Demo\"\nversion = \"0.1.0\"\n",
        &[
            (
                "Helpers.mw",
                "@pub macro twice\n  | ($e : expr) -> { ($e, $e) }\n",
            ),
            ("Lib.mw", "mod Helpers\n"),
        ],
    );
    let app = package(
        "dependent",
        &format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nDemo = {{ path = \"{}\" }}\n",
            lib.display()
        ),
        &[(
            "Lib.mw",
            "use Demo.Helpers (twice!)\n\ndef result = twice!(2)\n",
        )],
    );
    assert_eq!(build(&app).expect("it builds"), "(2, 2)");
}

#[test]
fn pkg_is_the_package_the_macro_was_written_in() {
    // Not the one it was expanded in: that is the whole point of `$pkg`.
    let lib = package(
        "pkg-lib",
        "[package]\nname = \"Demo\"\nversion = \"0.1.0\"\n",
        &[
            (
                "Helpers.mw",
                "@pub macro whereFrom\n  | () -> { stringify!($pkg) }\n",
            ),
            ("Lib.mw", "mod Helpers\n"),
        ],
    );
    let app = package(
        "pkg-app",
        &format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nDemo = {{ path = \"{}\" }}\n",
            lib.display()
        ),
        &[(
            "Lib.mw",
            "use Demo.Helpers (whereFrom!)\n\ndef result = whereFrom!()\n",
        )],
    );
    assert_eq!(build(&app).expect("it builds"), r#""Demo""#);
}

#[test]
fn a_metavariable_may_not_be_called_pkg() {
    let e = errors(
        r#"
macro wrong
  | ($pkg : expr) -> { $pkg }

def result = wrong!(1)
"#,
    );
    assert!(e.contains("is the package a macro was written in"), "{e}");
}

#[test]
fn a_use_that_selects_a_macro_selects_nothing_else() {
    // `use M (twice!)` names one macro. It is not a bare `use M`, which would
    // bring in every value the module has.
    let dir = package(
        "selective",
        "[package]\nname = \"Demo\"\nversion = \"0.1.0\"\n",
        &[
            (
                "Helpers.mw",
                "@pub macro twice\n  | ($e : expr) -> { ($e, $e) }\n\n@pub fun helper x = x\n",
            ),
            (
                "Lib.mw",
                "mod Helpers\n\nuse Demo.Helpers (twice!)\n\ndef result = helper (twice!(1))\n",
            ),
        ],
    );
    let errs = build(&dir).expect_err("`helper` was not imported");
    assert!(errs.iter().any(|e| e.contains("helper")), "{errs:?}");
}

#[test]
fn a_template_reaches_its_own_packages_helpers_through_pkg() {
    // The dependent has never heard of `demo.Text`, and does not have to: the
    // `use` the template writes says where `shout` comes from.
    let lib = package(
        "helper-lib",
        "[package]\nname = \"Demo\"\nversion = \"0.1.0\"\n",
        &[
            ("Text.mw", "@pub fun shout s = s ++ \"!\"\n"),
            (
                "Helpers.mw",
                "@pub macro withShout\n  | ($( $d : item );*) -> {\n      use $pkg.Text (shout)\n      $( $d );*\n    }\n",
            ),
            ("Lib.mw", "mod Helpers\nmod Text\n"),
        ],
    );
    let app = package(
        "helper-app",
        &format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nDemo = {{ path = \"{}\" }}\n",
            lib.display()
        ),
        &[(
            "Lib.mw",
            "use Demo.Helpers (withShout!)\n\nwithShout! {\n  def greeting = shout \"hello\"\n}\n\ndef result = greeting\n",
        )],
    );
    assert_eq!(build(&app).expect("it builds"), r#""hello!""#);
}

// --- what an error in expanded code says --------------------------------------

#[test]
fn an_error_in_what_a_macro_wrote_names_the_macro() {
    // The mistake is in the template, and the template is not in the file, so
    // the error is at the call -- which is where the reader has to look, and
    // says nothing on its own about why the code there is wrong.
    let e = errors_in_full(
        r#"
macro addOne
  | ($e : expr) -> { $e + notANumber }

def result = addOne!(1)
"#,
    );
    assert!(e.contains("`addOne!` wrote this"), "{e}");
}

#[test]
fn an_error_in_an_argument_is_the_callers_own() {
    // An argument keeps the span it was written with, so this is reported where
    // it was written and is nobody else's doing.
    let e = errors_in_full(
        r#"
macro twice
  | ($e : expr) -> { ($e, $e) }

def result = twice!(notANumber)
"#,
    );
    assert!(e.contains("notANumber"), "{e}");
    assert!(!e.contains("`twice!` wrote this"), "{e}");
}

#[test]
fn a_nested_expansion_names_both_macros() {
    let e = errors_in_full(
        r#"
macro inner
  | ($e : expr) -> { $e + notANumber }

macro outer
  | ($e : expr) -> { inner!($e) }

def result = outer!(1)
"#,
    );
    assert!(e.contains("`inner!` wrote this"), "{e}");
    assert!(e.contains("`outer!` wrote this"), "{e}");
}

// --- procedural macros ---------------------------------------------------------

/// A package holding a procedural macro, and one that calls it: the caller,
/// which removes the macro's package with it.
fn with_macro(what: &str, macro_src: &str, caller: &str) -> meadow_scratch::Scratch {
    let lib = package(
        &format!("{what}-lib"),
        "[package]\nname = \"Maker\"\nversion = \"0.1.0\"\n",
        &[("Lib.mw", macro_src)],
    );
    package(
        &format!("{what}-app"),
        &format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nMaker = {{ path = \"{}\" }}\n",
            lib.display()
        ),
        &[("Lib.mw", caller)],
    )
    .holding(lib)
}

const SHOUT: &str = r#"
use Std.Macro (spaced)
use Std.Macro.TokenTree.*

@macro
@pub fun shout ts = [Code "\"${spaced ts}!\""]
"#;

#[test]
fn a_procedural_macro_is_a_function_run_while_its_caller_is_compiled() {
    let app = with_macro(
        "proc",
        SHOUT,
        "use Maker (shout!)\n\ndef result = shout!(hello there)\n",
    );
    assert_eq!(build(&app).expect("it builds"), r#""hello there!""#);
}

#[test]
fn a_procedural_macro_can_answer_with_a_bracketed_group() {
    // A group's trees are a `[TokenTree]`, which the compiler once could not
    // read back: the answer now crosses as arrays all the way down.
    let app = with_macro(
        "group",
        r#"
use Std.Macro.TokenTree.*
use Std.Macro.Delim.*
use Std.Macro.Loc.*

@macro
@pub fun sum ts = [Group Paren [Num 40 Nowhere, Punct "+" Nowhere, Num 2 Nowhere] Nowhere, Punct "*" Nowhere, Num 1 Nowhere]
"#,
        "use Maker (sum!)\n\ndef result = sum!()\n",
    );
    assert_eq!(build(&app).expect("it builds"), "42");
}

#[test]
fn an_interpolated_string_comes_back_through_a_macro_whole() {
    // A macro is given an interpolated string as pieces -- `"outer ${`, the
    // hole's tokens, `} end"` -- none of which is Meadow on its own. Handed
    // back, they used to be read one at a time, and `"outer ${` never closed.
    // Quotes inside a hole, and a string nested in one, come back too.
    let app = with_macro(
        "interp",
        "use Std.Macro (TokenTree)\n\n@macro\n@pub fun same (ts : [TokenTree]) : [TokenTree] = ts\n",
        "use Maker (same!)\nuse Std.String as S\n\ndef result = same!(let x = \"a\" in \"outer ${S.join \", \" [x, \"b ${x}\"]} end\")\n",
    );
    assert_eq!(build(&app).expect("it builds"), r#""outer a, b a end""#);
}

#[test]
fn a_procedural_macro_may_not_perform_an_effect() {
    // A macro runs during a build, so anything it could learn about the world
    // would make the build depend on when it ran.
    let app = with_macro(
        "effectful",
        r#"
use Std.Macro.TokenTree.*
use Std.Fs (readToString)

@macro
@pub fun peek ts =
  match readToString "/etc/hosts" with
  | Ok s -> [Code "1"]
  | Err e -> [Code "2"]
"#,
        "use Maker (peek!)\n\ndef result = peek!()\n",
    );
    let errs = build(&app).expect_err("it does not build");
    assert!(
        errs.iter().any(|e| e.contains("it performs `Fs`")),
        "{errs:?}"
    );
}

#[test]
fn a_procedural_macro_may_not_use_the_console() {
    // Under an editor, what the compiler prints goes down the channel the
    // language server answers on: one line from a macro breaks the protocol.
    // And what it reads is the world outside, which a build may not depend on.
    for body in [
        "let u = println \"hello\" in [Code \"1\"]",
        "match readLine () with | Just s -> [Code \"1\"] | None -> [Code \"2\"]",
    ] {
        let app = with_macro(
            "console",
            &format!(
                "use Std.Macro.TokenTree.*\nuse Std.Maybe.Maybe.*\n\n@macro\n@pub fun noisy ts =\n  {body}\n"
            ),
            "use Maker (noisy!)\n\ndef result = noisy!()\n",
        );
        let errs = build(&app).expect_err("it does not build");
        assert!(
            errs.iter().any(|e| e.contains("it performs `Console`")),
            "{body}: {errs:?}"
        );
    }
}

#[test]
fn a_function_is_a_macro_only_if_it_says_so() {
    // Nothing about a function's type makes it a macro: `@macro` does, and a
    // package that means one to be used that way has to say it.
    let app = with_macro(
        "unmarked",
        r#"
use Std.Macro (spaced)
use Std.Macro.TokenTree.*

@pub fun shout ts = [Code "\"${spaced ts}!\""]
"#,
        "use Maker (shout!)\n\ndef result = shout!(hi)\n",
    );
    let errs = build(&app).expect_err("it does not build");
    assert!(
        errs.iter().any(|e| e.contains("`shout` is not a macro")),
        "{errs:?}"
    );
}

#[test]
fn a_macro_whose_type_could_never_run_is_reported_where_it_is_written() {
    // Not where someone imports it and finds out the hard way.
    let lib = package(
        "bad-shape",
        "[package]\nname = \"Maker\"\nversion = \"0.1.0\"\n",
        &[("Lib.mw", "@macro\n@pub fun double x = x + x\n")],
    );
    let errs = build(&lib).expect_err("it does not build");
    assert!(
        errs.iter()
            .any(|e| e.contains("this cannot be a macro: it is not `[TokenTree] -> [TokenTree]`")),
        "{errs:?}"
    );
}

#[test]
fn a_macro_of_ones_own_package_cannot_be_run_there() {
    // It has to be compiled before it can run, and a package is not compiled
    // while it is being compiled.
    let lib = package(
        "own-macro",
        "[package]\nname = \"Maker\"\nversion = \"0.1.0\"\n",
        &[
            (
                "Mac.mw",
                "use Std.Macro.TokenTree.*\n\n@macro\n@pub fun one ts = [Code \"1\"]\n",
            ),
            (
                "Lib.mw",
                "mod Mac\n\nuse Maker.Mac (one!)\n\ndef result = one!()\n",
            ),
        ],
    );
    let errs = build(&lib).expect_err("it does not build");
    assert!(
        errs.iter()
            .any(|e| e.contains("`one` is a macro of this package")),
        "{errs:?}"
    );
}

#[test]
fn a_macro_that_does_not_stop_runs_out_of_the_budget_it_is_given() {
    // A macro runs while its caller is compiled, so one that never finishes
    // has to fail the build rather than hang it. The budget is what does that;
    // this is it, at the size a test can wait for.
    let (program, diags) = pipeline::compile_str_with_std(
        "test",
        "fun spin n = spin (n + 1)\n\ndef result = spin 0\n",
        Options::debug().entry("result"),
    );
    assert!(diags.is_empty(), "{diags:?}");
    let entry = program.entry.expect("an entry point");
    let failed = meadow_eval::eval_with_fuel(
        &program,
        std::sync::Arc::new(meadow_compiler::core::Term::Var(entry)),
        100_000,
    )
    .expect_err("it never finishes");
    assert!(
        failed.msg.contains("did not finish within"),
        "{}",
        failed.msg
    );
}

#[test]
fn a_derive_writes_what_goes_beside_the_declaration() {
    let app = with_macro(
        "derive",
        r#"
use Std.Macro (text)
use Std.Macro.TokenTree.*
use Std.Collections.Vector as V
use Std.String as S
use Std.Maybe.Maybe.*

@macro
@pub fun naming ts =
  let ctors = V.drop (V.filter isUpperWord ts) 1 in
  [Code "fun nameOf x = match x with ${V.foldl (\acc c -> acc ++ arm c) "" ctors}"]

fun isUpperWord t =
  match t with
  | Word w _ -> (match S.byteAt w 0 with | Just b -> b >= 65 and b <= 90 | None -> False)
  | _ -> False

fun arm c = "| ${text c} -> \"${text c}\" "
"#,
        "use Maker (naming!)\n\n@derive(Naming)\ndata Colour = Red | Green | Blue\n\nuse Colour.*\n\ndef result = nameOf Green\n",
    );
    assert_eq!(build(&app).expect("it builds"), r#""Green""#);
}

#[test]
fn a_derive_is_given_the_declaration_as_it_was_written() {
    // Attributes on the variants and all: that is where a derive of any
    // substance keeps what it needs.
    let app = with_macro(
        "derive-attrs",
        r#"
use Std.Macro (spaced)
use Std.Macro.TokenTree.*
use Std.Macro.Loc.*

@macro
@pub fun echo ts = [Code "def given =", Str (spaced ts) Nowhere]
"#,
        "use Maker (echo!)\n\n@derive(Echo)\ndata Token = @token(\"+\") Plus | @regex(\"[0-9]+\") Number\n\ndef result = given\n",
    );
    let given = build(&app).expect("it builds");
    assert!(given.contains("@ token (\\\"+\\\") Plus"), "{given}");
    assert!(given.contains("@ regex (\\\"[0-9]+\\\") Number"), "{given}");
}

#[test]
fn a_derive_that_names_nothing_is_reported() {
    // A derive is a macro, as in Rust: a trait alone is not enough.
    let app = with_macro(
        "derive-missing",
        SHOUT,
        "use Maker (shout!)\n\n@derive(Nothing)\ndata Colour = Red\n\ndef result = 1\n",
    );
    let errs = build(&app).expect_err("it does not build");
    assert!(
        errs.iter()
            .any(|e| e.contains("there is no macro to derive `Nothing` with")),
        "{errs:?}"
    );
}

#[test]
fn a_trait_with_defaults_still_needs_a_macro_to_derive() {
    let src = r#"trait Greet a {
  fun greet : a -> String
    | greet x = "hello"
}

@derive(Greet)
data Who = World

def result = 1
"#;
    let errs = common::errors_std_with(src, meadow::Options::debug().entry("result"));
    assert!(
        errs.contains("there is no macro to derive `Greet` with"),
        "{errs}"
    );
}

#[test]
fn debug_and_display_are_derived_by_the_compilers_own_macros() {
    let src = r#"@derive(Debug, Display)
data Shape = Circle Float | Rect { w : Int, h : Int } | Dot

@derive(Debug)
record Pt = { name : String, at : (Int, Int) }

@derive(Debug, Display)
data Pair a = Pair a a

def result =
  ( "${Shape.Rect { w = 2, h = 3 }} ${Shape.Dot:?} ${Shape.Circle 1.5:?}"
  , "${Pt { name = "o", at = (1, 2) }:?}"
  , "${Pair "a" "b"} ${Pair "a" "b":?}"
  )
"#;
    assert_eq!(
        common::eval_main_std(src),
        r#"("Rect(2, 3) Dot Circle(1.5)", "Pt(\"o\", (1, 2))", "Pair(a, b) Pair(\"a\", \"b\")")"#
    );
}

#[test]
fn an_enumeration_compares_by_where_each_constructor_is() {
    // No constructor has fields, so equality and order are by position in
    // the declaration -- the same answers the constructor-by-constructor
    // comparison gave, without building a pair to match on.
    let src = r#"@derive(PartialEq, PartialOrd)
data Kind = Let | In | Fun | Arrow

def result =
  ( Kind.Let == Kind.Let, Kind.Let == Kind.Fun, Kind.Arrow != Kind.In
  , Kind.Let < Kind.Arrow, Kind.Fun > Kind.In, Kind.In <= Kind.In
  )
"#;
    assert_eq!(
        common::eval_main_std(src),
        "(True, False, True, True, True, True)"
    );
}

#[test]
fn a_derived_impl_asks_for_its_parameters_traits() {
    // `Pair a` is `Debug` when `a` is: a function is not.
    let src = "@derive(Debug)\ndata Pair a = Pair a a\n\ndef result = \"${Pair (\\x -> x) (\\x -> x):?}\"\n";
    let errs = common::errors_std_with(src, meadow::Options::debug().entry("result"));
    assert!(errs.contains("does not implement `Debug`"), "{errs}");
}

// --- where what a macro wrote is ----------------------------------------------

/// Every diagnostic of the app at `dir`, with the text its label is on.
fn located(dir: &Path, caller: &str) -> Vec<(String, String)> {
    pipeline::build(dir, Options::debug().entry("result"))
        .diagnostics
        .iter()
        .map(|d| {
            let (a, b) = (d.label.1.start as usize, d.label.1.end as usize);
            (d.msg.clone(), caller.get(a..b).unwrap_or("?").to_string())
        })
        .collect()
}

#[test]
fn a_macro_can_say_which_token_was_wrong() {
    let caller = "use Maker (strict!)\n\ndef result = strict!(a b c)\n";
    let app = with_macro(
        "fail-at",
        r#"
use Std.Macro (failAt, fail)
use Std.Collections.Vector as V

@macro
@pub fun strict ts = match V.get ts 1 with | Just t -> failAt "not this one" t | None -> fail "empty"
"#,
        caller,
    );
    let got = located(&app, caller);
    assert_eq!(
        got,
        vec![("not this one".to_string(), "b".to_string())],
        "{got:?}"
    );
}

#[test]
fn tokens_passed_through_keep_where_they_were_written() {
    // The macro hands back what it was given; the type error in it is the
    // caller's, on the caller's own tokens, and not on the whole call.
    let caller = "use Maker (same!)\n\ndef result = same!(1 + \"x\")\n";
    let app = with_macro(
        "pass-through",
        "use Std.Macro (TokenTree)\n\n@macro\n@pub fun same (ts : [TokenTree]) : [TokenTree] = ts\n",
        caller,
    );
    let got = located(&app, caller);
    assert!(!got.is_empty(), "it should not build");
    assert!(
        got.iter().all(|(_, at)| at != "(1 + \"x\")" && at != "?"),
        "reported at the whole call rather than inside it: {got:?}"
    );
}

#[test]
fn a_parse_of_the_argument_fails_at_the_token_it_stopped_on() {
    let caller = "use Maker (pair!)\n\ndef result = pair!(first (second third))\n";
    let app = with_macro(
        "parse-tokens",
        r#"
use Std.Macro (TokenTree, Delim, Loc, ToTokens, quoted)
use Std.Macro.Parse (expandWith, ident, punct, group)
use Std.String.Parse as P
use Std.Macro.Delim.*

@macro
@pub fun pair ts =
  expandWith
    (P.bind ident (\a -> P.map (\b -> quote! { ($(TokenTree.Word a Loc.Nowhere), $(TokenTree.Word b Loc.Nowhere)) }) (group Paren (P.thenSkip ident (punct ",")))))
    ts
"#,
        caller,
    );
    let got = located(&app, caller);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(
        got[0].1, "third",
        "at the token inside the brackets: {got:?}"
    );
}

#[test]
fn a_quote_writes_code_with_what_it_splices() {
    let app = with_macro(
        "quote",
        r#"
use Std.Macro (TokenTree, Delim, Loc, ToTokens, quoted)

@macro
@pub fun twice (ts : [TokenTree]) : [TokenTree] = quote! { ($ts, $ts) }

@macro
@pub fun dollars (ts : [TokenTree]) : [TokenTree] = quote! { $$ }
"#,
        "use Maker (twice!)\n\ndef result = twice!(20 + 1)\n",
    );
    assert_eq!(build(&app).expect("it builds"), "(21, 21)");
}

#[test]
fn a_quoted_splice_keeps_where_the_caller_wrote_it() {
    let caller = "use Maker (twice!)\n\ndef result = twice!(1 + \"x\")\n";
    let app = with_macro(
        "quote-spans",
        r#"
use Std.Macro (TokenTree, Delim, Loc, ToTokens, quoted)

@macro
@pub fun twice (ts : [TokenTree]) : [TokenTree] = quote! { ($ts, $ts) }
"#,
        caller,
    );
    let got = located(&app, caller);
    assert!(!got.is_empty(), "it should not build");
    assert!(
        got.iter().all(|(_, at)| at != "(1 + \"x\")" && at != "?"),
        "reported at the whole call rather than inside it: {got:?}"
    );
}

#[test]
fn a_macro_whose_argument_needs_a_trait_is_told_to_say_what_it_takes() {
    // Left to inference, `toTokens ts` makes `ts` anything with `ToTokens`,
    // and a macro is handed tokens with no trait to go with them.
    let app = with_macro(
        "needs-trait",
        r#"
use Std.Macro (TokenTree, Delim, Loc, ToTokens, quoted)

@macro
@pub fun twice ts = quote! { ($ts, $ts) }
"#,
        "use Maker (twice!)\n\ndef result = twice!(1)\n",
    );
    let errs = build(&app).expect_err("it does not build");
    assert!(
        errs.iter()
            .any(|e| e.contains("say what it is: `(ts : [TokenTree])`")),
        "{errs:?}"
    );
}

#[test]
fn a_call_after_a_data_declaration_is_not_one_of_its_fields() {
    // A variant's fields are type atoms, and a name is one: the call on the
    // next line starts a declaration of its own.
    is(
        r#"
macro constant
  | ($name, $value) -> { fun $name = $value }

data Shape = Square Int | Circle Int
constant!(four, 4)

def result = four
"#,
        "4",
    );
}

#[test]
fn what_a_macro_wrote_that_does_not_parse_says_why() {
    let e = errors(
        r#"
macro broken
  | () -> { fun = 1 }

broken!()
def result = 1
"#,
    );
    assert!(
        e.contains("`broken!` did not expand to declarations: found"),
        "{e}"
    );
}

// --- the depth limit ---------------------------------------------------------------
//
// Expanding what a call made used to happen outside the depth count, so the
// count never went past one: a macro that expands to a call of itself went on
// until the compiler's own stack ran out, and took the compiler with it.

const TOO_DEEP: &str = "macro expansion went 128 deep";

#[test]
fn an_expression_macro_that_never_stops_is_an_error() {
    let e = errors(
        r#"
macro forever
  | ($x) -> { forever!($x) + 1 }

def result = forever!(1)
"#,
    );
    assert!(e.contains(TOO_DEEP), "{e}");
    assert_eq!(e.matches(TOO_DEEP).count(), 1, "reported once: {e}");
}

#[test]
fn a_declaration_macro_that_never_stops_is_an_error() {
    let e = errors(
        r#"
macro again
  | ($name) -> { again!($name) }

again!(x)

def result = 0
"#,
    );
    assert!(e.contains(TOO_DEEP), "{e}");
}

#[test]
fn a_pattern_macro_that_never_stops_is_an_error() {
    let e = errors(
        r#"
macro pat
  | ($x) -> { pat!($x) }

def result = match 1 with | pat!(y) -> y | _ -> 0
"#,
    );
    assert!(e.contains(TOO_DEEP), "{e}");
}

#[test]
fn a_macro_that_branches_forever_stops_after_one_report() {
    // Two calls of itself at every step: trying every branch to the limit
    // would be 2^128 expansions. The first to reach it ends them all.
    let started = std::time::Instant::now();
    let e = errors(
        r#"
macro tree
  | ($x) -> { (tree!($x), tree!($x)) }

def result = tree!(1)
"#,
    );
    assert!(e.contains(TOO_DEEP), "{e}");
    assert_eq!(e.matches(TOO_DEEP).count(), 1, "reported once: {e}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_recursive_macro_within_the_limit_still_expands() {
    // One step per argument: 100 arguments is 100 deep, under the limit.
    let args: Vec<String> = (1..=100).map(|i| i.to_string()).collect();
    is(
        &format!(
            r#"
macro sum
  | ()              -> {{ 0 }}
  | ($x)            -> {{ $x }}
  | ($x, $( $r ),+) -> {{ $x + sum!($( $r ),+) }}

def result = sum!({})
"#,
            args.join(", ")
        ),
        "5050",
    );
}

#[test]
fn a_recursive_macro_past_the_limit_is_an_error_not_a_crash() {
    let args: Vec<String> = (1..=200).map(|i| i.to_string()).collect();
    let e = errors(&format!(
        r#"
macro sum
  | ()              -> {{ 0 }}
  | ($x)            -> {{ $x }}
  | ($x, $( $r ),+) -> {{ $x + sum!($( $r ),+) }}

def result = sum!({})
"#,
        args.join(", ")
    ));
    assert!(e.contains(TOO_DEEP), "{e}");
}

// --- a macro's answer says where it is --------------------------------------------
//
// An answer was cached on the macro and its argument's text alone, and it is
// tokens that say where they are: a second call written the same way took the
// first one's, and its errors were reported at the first call.

const SAME: &str =
    "use Std.Macro (TokenTree)\n\n@macro\n@pub fun same (ts : [TokenTree]) : [TokenTree] = ts\n";

/// What each diagnostic's label points at in the caller's `Lib.mw`.
fn pointed_at(app: &Path) -> Vec<(String, String)> {
    let src = std::fs::read_to_string(app.join("src").join("Lib.mw")).unwrap();
    let out = pipeline::build(app, Options::debug().entry("result"));
    out.diagnostics
        .iter()
        .map(|d| {
            let span = d.label.1;
            let text = src
                .get(span.start as usize..span.end as usize)
                .unwrap_or("<not in the file>")
                .to_string();
            (d.msg.clone(), text)
        })
        .collect()
}

#[test]
fn two_calls_written_alike_are_reported_where_each_is() {
    let caller = "use Maker (same!)\n\n\
                  def a = same!(nope1)\n\
                  def b = same!(nope1)\n\
                  def result = 0\n";
    let app = with_macro("same-twice", SAME, caller);
    let got = pointed_at(&app);
    assert_eq!(got.len(), 2, "one error per call: {got:?}");
    let src = std::fs::read_to_string(app.join("src").join("Lib.mw")).unwrap();
    let out = pipeline::build(&app, Options::debug().entry("result"));
    let mut starts: Vec<u32> = out.diagnostics.iter().map(|d| d.label.1.start).collect();
    starts.sort();
    let first = src.find("same!(nope1)").unwrap() as u32;
    let second = src.rfind("same!(nope1)").unwrap() as u32;
    assert!(
        starts[0] >= first && starts[0] < second && starts[1] >= second,
        "the errors are at {starts:?}, the calls at {first} and {second}: {got:?}"
    );
}

#[test]
fn a_call_after_a_longer_one_is_not_given_its_positions() {
    // The first call is further into the file, and the second's answer, if
    // it were the first's, would point past where the second call ends.
    let caller = "use Maker (same!)\n\n\
                  def a = let padding = 1 in same!(nope2)\n\
                  def b = same!(nope2)\n\
                  def result = 0\n";
    let app = with_macro("same-shifted", SAME, caller);
    let got = pointed_at(&app);
    for (msg, text) in &got {
        assert_eq!(text, "nope2", "{msg} points at {text:?}");
    }
    let out = pipeline::build(&app, Options::debug().entry("result"));
    let spans: Vec<_> = out.diagnostics.iter().map(|d| d.label.1).collect();
    assert_eq!(spans.len(), 2, "{got:?}");
    assert_ne!(spans[0], spans[1], "each at its own call: {spans:?}");
}

#[test]
fn a_call_that_answers_the_same_is_still_expanded_right_each_time() {
    // No errors: the cache still serves, and both calls work.
    let caller = "use Maker (same!)\n\n\
                  def a = same!(1 + 1)\n\
                  def b = same!(1 + 1)\n\
                  def result = a + b\n";
    let app = with_macro("same-fine", SAME, caller);
    assert_eq!(build(&app).expect("it builds"), "4");
}
