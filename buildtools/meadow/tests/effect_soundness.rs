//! Effects must not go missing from a type. Each test below was accepted, or
//! printed a type with an effect missing, before the fix it guards.
//!
//! ## Lets
//!
//! A `let` whose right-hand side calls a function *parameter* must keep that
//! call's effect.
//!
//! The parameter's latent effect is a row variable, and a row variable used to
//! count as "pure" when deciding whether a binding contributes its effect to
//! the enclosing function and whether it may be generalized. So in
//! `fun each f xs = ... let _ = f x in ...` the `! e` vanished from `each`'s
//! type: `each (\x -> log x) xs` then type-checked as a pure call, a
//! `Log`-performing closure could be stored where a pure function was required,
//! and a binding like `let r = f ()` was generalized even when `f` allocated.
//!
//! ## Handlers
//!
//! A handler discharges only what it answers. An operation with no clause goes
//! straight past it at run time, so its effect is still performed as far as
//! the enclosing code is concerned. The rule used to take the first clause's
//! effect as handled in full, give clauses of any second effect unchecked
//! types, and drop the body's whole row when there was no clause at all.

mod common;
use common::{errors, schemes};

/// The scheme printed for `name`, or the whole output if it has none.
fn scheme_of(src: &str, name: &str) -> String {
    let out = schemes(src);
    let prefix = format!("{name} : ");
    out.lines()
        .find_map(|l| l.strip_prefix(&prefix).map(str::to_string))
        .unwrap_or(out)
}

const LOG: &str = "effect Log { log : String -> () }\n";

#[test]
fn let_underscore_keeps_the_parameters_effect() {
    let src = "fun go f x = let _ = f x in ()\n";
    assert_eq!(
        scheme_of(src, "go"),
        "forall a b e. (a -> b ! e) -> a -> () ! e"
    );
}

#[test]
fn a_named_let_keeps_the_parameters_effect() {
    let src = "fun go f x = let y = f x in ()\n";
    assert_eq!(
        scheme_of(src, "go"),
        "forall a b e. (a -> b ! e) -> a -> () ! e"
    );
}

#[test]
fn a_let_inside_a_match_arm_keeps_the_parameters_effect() {
    let src = "fun each f xs =\n\
               \x20 match xs with\n\
               \x20 | Nil -> ()\n\
               \x20 | Cons x r -> let _ = f x in each f r\n";
    assert_eq!(
        scheme_of(src, "each"),
        "forall a b e. (a -> b ! e) -> List a -> () ! e"
    );
}

#[test]
fn a_caller_sees_the_effect_through_the_let() {
    let src = format!(
        "{LOG}\
         fun each f xs =\n\
         \x20 match xs with\n\
         \x20 | Nil -> ()\n\
         \x20 | Cons x r -> let _ = f x in each f r\n\
         fun logAll xs = each (\\x -> log x) xs\n"
    );
    assert_eq!(
        scheme_of(&src, "logAll"),
        "forall e. List String -> () ! { Log | e }"
    );
}

#[test]
fn an_effectful_closure_is_not_accepted_as_a_pure_function() {
    // `Pure` holds a function with no effects; `logAll` performs `Log`, so
    // wrapping it must be a type error rather than a way to smuggle `Log` out
    // from under every handler.
    let src = format!(
        "{LOG}\
         data Pure = Pure (List String -> ())\n\
         fun each f xs =\n\
         \x20 match xs with\n\
         \x20 | Nil -> ()\n\
         \x20 | Cons x r -> let _ = f x in each f r\n\
         def smuggled = Pure (\\xs -> each (\\x -> log x) xs)\n"
    );
    let errs = errors(&src);
    assert!(
        errs.contains("type mismatch"),
        "expected a type error, got: {errs:?}"
    );
    assert!(
        errs.contains("Log"),
        "the error should name the effect, got: {errs:?}"
    );
}

#[test]
fn a_let_after_a_parameter_call_is_not_generalized() {
    // If `f` has effects, `x` must stay monomorphic -- `f` might allocate the
    // very `Ref` that `\y -> y` closes over in a less innocent program.
    let src = "fun go f = let x = (let _ = f () in \\y -> y) in (x 1, x \"s\")\n";
    let errs = errors(src);
    assert!(
        errs.contains("type mismatch: `String` is not an integer type"),
        "expected `x` to stay monomorphic, got: {errs:?}"
    );
}

#[test]
fn a_let_after_an_operation_is_not_generalized() {
    let src =
        format!("{LOG}fun go u = let x = (let _ = log \"hi\" in \\y -> y) in (x 1, x \"s\")\n");
    let errs = errors(&src);
    assert!(
        errs.contains("type mismatch: `String` is not an integer type"),
        "expected `x` to stay monomorphic, got: {errs:?}"
    );
}

#[test]
fn a_pure_let_still_generalizes() {
    // The fix must not over-correct: nothing effectful happens here.
    let src = "fun go u = let id = \\y -> y in (id 1, id \"s\")\n";
    assert_eq!(errors(src), "");
    assert_eq!(scheme_of(src, "go"), "forall a n. a -> (n, String)");
}

#[test]
fn a_let_bound_to_a_pure_call_still_generalizes() {
    let src = "fun k x = \\y -> y\n\
               fun go u = let id = k 0 in (id 1, id \"s\")\n";
    assert_eq!(errors(src), "");
}

#[test]
fn a_concrete_effect_is_unaffected() {
    let src = format!("{LOG}fun go u = let _ = log \"hi\" in ()\n");
    assert_eq!(scheme_of(&src, "go"), "forall a e. a -> () ! { Log | e }");
}

// --- handlers ----------------------------------------------------------------

const COUNTER: &str = "effect Counter { next : () -> Int, reset : () -> () }\n\
                       fun job () = let a = next () in let _ = reset () in let b = next () in (a, b)\n";

#[test]
fn a_handler_missing_an_operation_leaves_its_effect_in_the_type() {
    // `reset` has no clause, so `partial` still performs `Counter`.
    let src = format!("{COUNTER}fun partial () = handle job () with {{ next () k -> k 7 }}\n");
    assert_eq!(
        scheme_of(&src, "partial"),
        "forall e. () -> (Int, Int) ! { Counter | e }"
    );
}

#[test]
fn a_handler_covering_every_operation_discharges_its_effect() {
    let src = format!(
        "{COUNTER}fun full () = handle job () with {{ next () k -> k 7, reset () k -> k () }}\n"
    );
    assert_eq!(scheme_of(&src, "full"), "() -> (Int, Int)");
}

#[test]
fn nested_partial_handlers_are_judged_one_at_a_time() {
    // Between them these two answer both operations, but a row names effects,
    // not operations: each handler on its own leaves `Counter` unfinished, so
    // the type keeps it. Conservative -- one handler with both clauses is exact.
    let src = format!(
        "{COUNTER}fun both () = handle (handle job () with {{ next () k -> k 7 }}) with {{ reset () k -> k () }}\n"
    );
    assert_eq!(
        scheme_of(&src, "both"),
        "forall e. () -> (Int, Int) ! { Counter | e }"
    );
}

#[test]
fn a_resumed_continuation_still_performs_the_forwarded_effect() {
    // `k` runs the rest of `job`, and that rest calls `reset`.
    let src = format!(
        "{COUNTER}fun resumer () = handle job () with {{ next () k -> let r = k 1 in r }}\n"
    );
    assert_eq!(
        scheme_of(&src, "resumer"),
        "forall e. () -> (Int, Int) ! { Counter | e }"
    );
}

#[test]
fn a_handler_with_only_a_return_clause_handles_nothing() {
    let src = format!("{LOG}fun onlyReturn () = handle log \"x\" with {{ return x -> 1 }}\n");
    assert_eq!(
        scheme_of(&src, "onlyReturn"),
        "forall n e. () -> n ! { Log | e }"
    );
}

const LOG_AND_ASK: &str = "effect Log { log : String -> () }\n\
                           effect Ask { ask : String -> Int }\n\
                           fun work () = let _ = log \"x\" in ask \"q\"\n";

#[test]
fn one_handler_can_discharge_two_effects() {
    let src = format!(
        "{LOG_AND_ASK}fun answered () = handle work () with {{ log m k -> k (), ask q k -> k 1 }}\n"
    );
    assert_eq!(errors(&src), "");
    assert_eq!(scheme_of(&src, "answered"), "() -> Int");
}

#[test]
fn a_second_effects_clause_is_type_checked() {
    // `q` is `ask`'s `String`, so `q + 1` is an error -- it used to get a fresh,
    // unconstrained type because only the first clause's effect was looked up.
    let src = format!(
        "{LOG_AND_ASK}fun wrong () = handle work () with {{ log m k -> k (), ask q k -> k (q + 1) }}\n"
    );
    let errs = errors(&src);
    assert!(
        errs.contains("type mismatch"),
        "expected a type error, got: {errs:?}"
    );
}

#[test]
fn handling_one_effect_leaves_the_other() {
    let src = format!("{LOG_AND_ASK}fun quiet () = handle work () with {{ log m k -> k () }}\n");
    assert_eq!(
        scheme_of(&src, "quiet"),
        "forall e. () -> Int ! { Ask | e }"
    );
}

#[test]
fn a_clause_that_performs_its_own_effect_reaches_the_enclosing_code() {
    // A clause runs outside its own handler, so the `log` inside it is performed
    // by `relay`, not answered by the handler it sits in.
    let src = format!(
        "{LOG}fun relay () = handle log \"inner\" with {{ log m k -> let _ = log m in k () }}\n"
    );
    assert_eq!(scheme_of(&src, "relay"), "forall e. () -> () ! { Log | e }");
}
