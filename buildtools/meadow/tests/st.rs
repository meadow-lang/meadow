//! `runSt`: local mutable state, and the check that keeps it local.
//!
//! Everything made inside a `runSt` is tied to a state type the checker makes
//! fresh for that `runSt`, one level in, and rigid. The tests below are the ways
//! state could get out -- each must be an error -- and the programs that must
//! still be accepted, with the types they should get.

mod common;
use common::{cek_main_std, errors_std_with, eval_main_std, schemes_std};

const ESCAPES: &str = "state from inside a `runSt` escapes it";

fn errors(src: &str) -> String {
    errors_std_with(src, meadow::Options::debug().entry("result"))
}

fn scheme_of(src: &str, name: &str) -> String {
    let out = schemes_std(src);
    let prefix = format!("{name} : ");
    out.lines()
        .find_map(|l| l.strip_prefix(&prefix).map(str::to_string))
        .unwrap_or(out)
}

fn both(src: &str) -> String {
    let vm = eval_main_std(src);
    assert_eq!(vm, cek_main_std(src), "the engines disagree on\n{src}");
    vm
}

// --- what must not get out ---------------------------------------------------

#[test]
fn a_cell_cannot_be_returned() {
    assert_eq!(errors("def bad = runSt (\\() -> stNewRef 0)\n"), ESCAPES);
}

#[test]
fn an_array_cannot_be_returned() {
    assert_eq!(
        errors("def bad = runSt (\\() -> stNewArray 3 0)\n"),
        ESCAPES
    );
}

#[test]
fn a_closure_over_a_cell_cannot_be_returned() {
    assert_eq!(
        errors("def bad = runSt (\\() -> let r = stNewRef 0 in \\u -> stGetRef r)\n"),
        ESCAPES
    );
}

#[test]
fn a_cell_cannot_be_stored_somewhere_that_outlives_it() {
    // `outer` is an ordinary `Ref`, made before the `runSt`, and not generalized
    // because making it is effectful -- the case that needed `let`s to hand
    // their variables back to the level around them.
    assert_eq!(
        errors(
            "fun bad u =\n\
             \x20 let outer = newRef [;] in\n\
             \x20 runSt (\\() -> let r = stNewRef 1 in setRef outer [r;])\n"
        ),
        ESCAPES
    );
}

#[test]
fn a_body_passed_in_is_not_polymorphic_enough() {
    // `body`'s type is one type, chosen by the caller; `runSt` needs one that
    // works for a state type nobody outside can name.
    assert_eq!(errors("fun bad body = runSt body\n"), ESCAPES);
}

#[test]
fn a_cell_cannot_be_used_after_its_run_st() {
    assert_eq!(
        errors("fun f r = stGetRef r\nfun bad u = f (runSt (\\() -> stNewRef 0))\n"),
        ESCAPES
    );
}

#[test]
fn a_callback_that_touches_the_state_cannot_leave_either() {
    // A closure performing `St s`, handed to a function that keeps it in a
    // `Ref` from outside. Loosening a parameter's effect must not become a way
    // to launder one.
    let errs = errors(
        "fun keep r f = let _ = setRef r f in f ()\n\
             fun bad u =\n\
             \x20 let outer = newRef (\\() -> 0) in\n\
             \x20 runSt (\\() -> let a = stNewArray 1 7 in keep outer (\\() -> stGetArray a 0))\n",
    );
    assert!(
        !errs.is_empty() && errs.lines().all(|l| l == ESCAPES),
        "{errs}"
    );
}

// --- what must still be accepted -----------------------------------------------

#[test]
fn mutation_inside_run_st_leaves_a_pure_type() {
    let src = "fun sumTo n =\n\
               \x20 runSt (\\() ->\n\
               \x20   let total = stNewRef 0 in\n\
               \x20   let rec go i = if i > n then () else let _ = stSetRef total (stGetRef total + i) in go (i + 1) in\n\
               \x20   let _ = go 1 in\n\
               \x20   stGetRef total)\n";
    assert_eq!(
        scheme_of(src, "sumTo"),
        "forall n. (PartialOrd n, Add n) => n -> n"
    );
    assert_eq!(both(&format!("{src}def result = sumTo 100\n")), "5050");
}

#[test]
fn other_effects_pass_through() {
    // `St` is removed and nothing else is: a `Ref` touched inside keeps `Mut`.
    let src = "fun f u = runSt (\\() -> let r = stNewRef 0 in let c = newRef 5 in let _ = stSetRef r (getRef c) in stGetRef r)\n";
    assert_eq!(scheme_of(src, "f"), "forall a n e. a -> n ! { Mut | e }");
}

#[test]
fn a_callers_callback_keeps_its_own_effect() {
    // `loop`'s type says its callback may touch the state; a callback from
    // outside `runSt` cannot, and is tied to the rest of the row instead.
    let src = "fun loop cmp arr = cmp (stGetArray arr 1) (stGetArray arr 0)\n\
               fun viaHelper cmp v = runSt (\\() -> let c = stThaw v in let _ = loop cmp c in stFreeze c)\n";
    assert_eq!(
        scheme_of(src, "viaHelper"),
        "forall a b e. (a -> a -> b ! e) -> #[a] -> #[a] ! e"
    );
}

#[test]
fn run_st_nests_and_an_inner_one_may_use_outer_cells() {
    let src = "def result = runSt (\\() ->\n\
               \x20 let outer = stNewRef 1 in\n\
               \x20 let inner = runSt (\\() -> let r = stNewRef 10 in let _ = stSetRef outer (stGetRef outer + 1) in stGetRef r) in\n\
               \x20 stGetRef outer + inner)\n";
    assert_eq!(errors(src), "");
    assert_eq!(both(src), "12");
}

#[test]
fn a_pure_result_still_generalizes() {
    let src = "def ident = runSt (\\() -> \\x -> x)\ndef result = (ident 1, ident \"s\")\n";
    assert_eq!(errors(src), "");
    assert_eq!(both(src), "(1, \"s\")");
}

#[test]
fn run_st_as_a_value_is_just_application() {
    let src = "def result = map runSt [\\() -> 1, \\() -> 2]\n";
    assert_eq!(errors(src), "");
    assert_eq!(both(src), "[1, 2]");
}

#[test]
fn std_sort_is_pure_now() {
    let src = "use Std.Sort (sortBy)\nfun g cmp v = sortBy cmp v\n";
    assert_eq!(
        scheme_of(src, "g"),
        "forall a e. (a -> a -> Ordering ! e) -> [a] -> [a] ! e"
    );
}

// --- arrays at run time --------------------------------------------------------

#[test]
fn arrays_are_written_in_place_and_copied_at_the_edges() {
    let src = "def result = runSt (\\() ->\n\
               \x20 let a = stThaw #[1, 2, 3] in\n\
               \x20 let before = stFreeze a in\n\
               \x20 let _ = stSetArray a 0 (stGetArray a 2 * 10) in\n\
               \x20 (before, stFreeze a, stArrayLen a))\n";
    assert_eq!(both(src), "(#[1, 2, 3], #[30, 2, 3], 3)");
}

#[test]
fn an_index_out_of_bounds_is_an_error_not_a_crash() {
    let out = both("def result = runSt (\\() -> let a = stNewArray 2 0 in stGetArray a 5)\n");
    assert!(out.contains("out of bounds"), "{out}");
}

#[test]
fn a_parameter_called_under_a_let_keeps_its_own_effect() {
    // `same` is called under a `let` inside the `runSt`. Its effect must stay
    // its own, and not become the `let`'s region, which later performs `St s`.
    assert_eq!(
        both(
            "fun probe same n =\n\
               runSt (\\() ->\n\
                 let r = stNewRef 0 in\n\
                 let rec go i =\n\
                   if i > n then () else let v = same i in let _ = stSetRef r v in go (i + 1)\n\
                 in\n\
                 let _ = go 0 in\n\
                 stGetRef r)\n\
             def result = probe (\\i -> i + 1) 4\n"
        ),
        "5"
    );
}
