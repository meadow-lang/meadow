//! Top-level definitions are pure to evaluate.
//!
//! A `def` is evaluated once, the first time it is needed, and kept. That is
//! the same program as evaluating it at every use only if evaluating it does
//! nothing a program can see, so a top-level `def` other than `main` may not
//! perform effects. A function may -- its effects happen when it is called.

mod common;
use common::{errors, eval_main_std, schemes_std};

const RULE: &str = "a top-level `def` cannot perform effects";

#[test]
fn a_def_that_prints_while_being_made_is_refused() {
    let out = schemes_std("def greeting = let _ = println \"hi\" in \"hi\"\n");
    assert!(
        out.contains(&format!("{RULE}, and this one performs `Console`")),
        "{out}"
    );
}

#[test]
fn a_def_that_allocates_a_cell_is_refused() {
    let out = errors("def counter = newRef (toInt 0)\n");
    assert!(
        out.contains(&format!("{RULE}, and this one performs `Mut`")),
        "{out}"
    );
}

#[test]
fn every_effect_is_named() {
    let src = "effect Log { log : String -> () }\n\
               def both = let _ = log \"x\" in newRef (toInt 0)\n";
    assert!(
        errors(src).contains("performs `Log`, `Mut`"),
        "{}",
        errors(src)
    );
}

#[test]
fn a_def_may_be_a_function_whose_calls_have_effects() {
    // Making the closure does nothing; its effects are on its arrow.
    let src = "effect Log { log : String -> () }\n\
               def logIt = \\s -> log s\n\
               def logAll = forEach (\\s -> log s)\n";
    let out = schemes_std(src);
    assert!(!out.contains(RULE), "{out}");
    assert!(
        out.contains("logIt : forall e. String -> () ! { Log | e }"),
        "{out}"
    );
}

#[test]
fn a_def_may_use_local_state_that_cannot_be_seen() {
    let src = "fun total (n : Int) = runSt (\\() -> let r = stNewRef (toInt 0) in \
               let _ = stSetRef r n in stGetRef r)\n\
               def forty = total 40\n";
    assert_eq!(errors(src), "");
}

#[test]
fn main_is_run_and_may_do_anything() {
    let src = "def result = let _ = println \"hello\" in newRef (toInt 1) |> getRef\n";
    assert_eq!(eval_main_std(src), "1");
}

#[test]
fn a_def_in_a_recursive_group_is_held_to_the_same_rule() {
    let src = "fun f (n : Int) = if n == 0 then 0 else g\n\
               def g = let _ = newRef (toInt 0) in f 0\n";
    assert!(errors(src).contains(RULE), "{}", errors(src));
}

// Making a value that needs itself cannot finish: it used to recurse until the
// runtime ran out of address space, or spin for ever.

#[test]
fn a_def_has_no_value_of_its_own_to_read() {
    for src in [
        "def x : Int = x * 10\n",
        "def x = x\n",
        "def x = let y = x in y\n",
        "def (x, y) = (1, x)\n",
    ] {
        let out = errors(src);
        assert!(out.contains("undefined variable: x"), "{src}: {out}");
    }
}

#[test]
fn a_def_reads_what_its_name_meant_before_it() {
    let src = "def min = min 1 2\n\
               def result = min\n";
    assert_eq!(eval_main_std(src), "1");
}

#[test]
fn a_def_that_is_a_function_can_call_itself() {
    let src = "def fact = \\n -> if n == 0 then 1 else n * fact (n - 1)\n\
               def even = \\n -> if n == 0 then True else odd (n - 1)\n\
               def odd = \\n -> if n == 0 then False else even (n - 1)\n\
               def inPair = (\\n -> if n == 0 then 0 else fst inPair (n - 1), 2)\n\
               def result = (fact 5, even 4, fst inPair 3)\n";
    assert_eq!(eval_main_std(src), "(120, True, 0)");
}
