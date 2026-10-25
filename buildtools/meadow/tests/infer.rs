//! Type-inference snapshots: for a representative program, snapshot the inferred
//! scheme of every top-level binding (see `common::schemes`). A trailing `!! ...`
//! line means a diagnostic was produced.

mod common;
use common::{schemes, schemes_std};

#[test]
fn literals_and_arithmetic() {
    insta::assert_snapshot!(schemes("def n = 1 + 2 * 3\ndef s = \"hi\"\ndef u = ()\n"));
}

#[test]
fn numeric_literals_coerce_by_context() {
    // A literal takes the type of what it meets; one nothing pins down is an
    // `Int`. A `def` keeps the one type it gets rather than generalizing.
    insta::assert_snapshot!(schemes(
        "def a = 1 + 2\ndef b = toUInt8 1 + 2\ndef c = 1 + toInt 2\ndef d = 5\ndef e = _primAddF 1.5 2.0\n"
    ));
}

#[test]
fn identity_generalizes() {
    insta::assert_snapshot!(schemes("def id = \\x -> x\n"));
}

#[test]
fn curried_functions() {
    insta::assert_snapshot!(schemes("fun const a b = a\nfun compose f g x = f (g x)\n"));
}

#[test]
fn let_polymorphism() {
    insta::assert_snapshot!(schemes(
        "def usePair =\n  let pair = \\a -> \\b -> (a, b) in\n  (pair 1 2, pair \"x\" \"y\")\n"
    ));
}

#[test]
fn recursion() {
    insta::assert_snapshot!(schemes(
        "fun fib n = if n == 0 then 0 else if n == 1 then 1 else fib (n - 1) + fib (n - 2)\n"
    ));
}

#[test]
fn if_and_match() {
    insta::assert_snapshot!(schemes(
        "fun classify n =\n  match n == 0 with\n  | True -> \"zero\"\n  | False -> \"nonzero\"\n"
    ));
}

#[test]
fn tuples_and_lists() {
    insta::assert_snapshot!(schemes(
        "fun swap p = match p with | (a, b) -> (b, a)\nfun singleton x = [x]\n"
    ));
}

#[test]
fn builtin_list_constructors() {
    insta::assert_snapshot!(schemes(
        "fun map f xs =\n  match xs with\n  | Nil -> Nil\n  | Cons x rest -> Cons (f x) (map f rest)\n"
    ));
}

#[test]
fn row_polymorphic_field_access() {
    insta::assert_snapshot!(schemes("def name = \\r -> r.name\n"));
}

#[test]
fn record_extension() {
    insta::assert_snapshot!(schemes("fun withAge r = { age = 0 | r }\n"));
}

#[test]
fn data_sum_type() {
    insta::assert_snapshot!(schemes(
        "use Shape.*\ndata Shape = Circle Int | Rect Int Int\n\
         fun area s = match s with | Circle r -> r * r | Rect w h -> w * h\n"
    ));
}

#[test]
fn data_polymorphic_recursive() {
    insta::assert_snapshot!(schemes(
        "use Tree.*\ndata Tree a = Tip | Branch (Tree a) a (Tree a)\n\
         fun size t = match t with | Tip -> 0 | Branch l x r -> 1 + size l + size r\n"
    ));
}

#[test]
fn nominal_record_and_field() {
    insta::assert_snapshot!(schemes(
        "record Person = { name : String, age : Int }\n\
         def p = Person { name = \"Ann\", age = 30 }\n\
         def who = p.name\n"
    ));
}

#[test]
fn maybe_type() {
    insta::assert_snapshot!(schemes(
        "use Option.*\ndata Option a = None | Some a\n\
         fun orElse m d = match m with | None -> d | Some x -> x\n"
    ));
}

// --- effect inference ---------------------------------------------------------

#[test]
fn printing_performs_console() {
    // `println` is `Std.Console`'s `writeOutput` underneath, so a function that
    // calls it performs `Console` -- and a handler can take that away.
    assert_eq!(
        schemes_std(
            "use Std.Console (withOutput)\n\
             fun greet name = println name\n\
             fun captured name = withOutput (\\() -> greet name)\n"
        ),
        "greet : forall a e. Display a => a -> () ! { Console | e }\n\
         captured : forall a e. Display a => a -> ((), String) ! { Console | e }\n"
    );
}

#[test]
fn effect_polymorphism_through_higher_order() {
    // `map`'s effect is exactly the effect of its function argument.
    insta::assert_snapshot!(schemes(
        "fun map f xs =\n  match xs with\n  | Nil -> Nil\n  | Cons x r -> Cons (f x) (map f r)\n"
    ));
}

#[test]
fn effectful_binding_is_not_generalized() {
    // `def a` is pure ⇒ polymorphic; `let b` allocates a cell ⇒ monomorphic, so
    // both halves of the pair are the one cell at one type. (A top-level `def`
    // may not allocate at all -- see `a_top_level_def_cannot_perform_effects`.)
    insta::assert_snapshot!(schemes(
        "def a = \\x -> x\nfun g u = let b = newRef (\\y -> y) in (b, b)\n"
    ));
}

/// Every type an instantiation is at mentions only variables something binds.
///
/// A pure `let` right-hand side's effect row is a variable nothing outside can
/// constrain, and it used to stay unsolved: `let r = f x` recorded `f` at an
/// effect no binder anywhere bound, which is an instantiation of nothing in
/// particular. It is empty, and core says so.
#[test]
fn a_let_bound_call_is_instantiated_at_what_is_bound() {
    use meadow_compiler::core::{Term, Ty, rewrite};
    let src = "fun step (c : String) (i : Int) : Int = if i >= 10 then i else step c (i + 1)\n\
               fun fold f (c : String) (i : Int) acc =\n\
               \x20 if i >= 3 then acc else let r = step c i in fold f c (i + 1) (f acc r)\n\
               def result = fold (\\a x -> a + x) \"x\" 0 0\n";
    let (program, diags) = meadow::pipeline::compile_str_with_std(
        "test",
        src,
        meadow::Options::debug().entry("result"),
    );
    assert!(
        diags.is_empty(),
        "{:?}",
        diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    for d in program.defs.iter().filter(|d| &*d.name == "fold") {
        let bound: Vec<u32> = d.poly.binders.iter().map(|b| b.id).collect();
        rewrite::visit(&d.term, &mut |t| {
            if let Term::TyApp(_, tys) = t {
                for ty in tys {
                    let mut vars = Vec::new();
                    fn collect(t: &Ty, out: &mut Vec<u32>) {
                        match t {
                            Ty::Var(v) => out.push(*v),
                            Ty::Con(_, xs) | Ty::Tuple(xs) => {
                                xs.iter().for_each(|x| collect(x, out))
                            }
                            Ty::Fun(ps, r, e) => {
                                ps.iter().for_each(|x| collect(x, out));
                                collect(r, out);
                                collect(e, out);
                            }
                            Ty::Record(r) => collect(r, out),
                            Ty::RowExtend(_, f, r) => {
                                collect(f, out);
                                collect(r, out);
                            }
                            _ => {}
                        }
                    }
                    collect(ty, &mut vars);
                    for v in vars {
                        assert!(
                            bound.contains(&v),
                            "`fold` instantiates at unbound {v}: {tys:?}"
                        );
                    }
                }
            }
            true
        });
    }
}

// --- `: R ! e`: what a function's body performs, said inline -----------------

/// Inline parameters leave the last arrow's effect nowhere to go but after the
/// result, and that is where it is written.
#[test]
fn a_declared_result_can_say_what_the_body_performs() {
    let out = schemes_std("fun say (s : String) : () ! { Console | e } = println s\n");
    assert_eq!(
        out, "say : forall e. String -> () ! { Console | e }\n",
        "{out}"
    );
}

/// The row is the declaration's like every other annotation's variable: the
/// `e` a parameter's arrow names is the `e` the body performs.
#[test]
fn a_declared_effect_shares_its_variable_with_the_parameters() {
    let out = schemes_std("fun run (f : () -> a ! e) : a ! e = f ()\n");
    assert_eq!(out, "run : forall a e. (() -> a ! e) -> a ! e\n", "{out}");
}

/// A closed row is a promise: a body that performs more is refused.
#[test]
fn a_declared_pure_body_may_not_perform() {
    let out = schemes_std("fun noisy (x : Int) : Int ! {} = let u = println \"hi\" in x\n");
    assert!(
        out.contains("!!"),
        "a pure declaration accepted a print: {out}"
    );
}

/// A value has no body that runs when it is used.
#[test]
fn a_def_cannot_declare_an_effect() {
    let out = schemes_std("def x : Int ! e = 1\n");
    assert!(
        out.contains("only a function's result can say what its body performs"),
        "{out}"
    );
}

/// A result written without a `!` is a promise of purity, as an arrow without
/// one is; only a result left unwritten has its effect inferred.
#[test]
fn a_declared_result_without_an_effect_is_pure() {
    let out = schemes_std("fun noisy (x : Int) : Int = let u = println \"hi\" in x\n");
    assert!(
        out.contains("the effect `Console` is not allowed here"),
        "{out}"
    );
    let out = schemes_std("fun noisy (x : Int) = let u = println \"hi\" in x\n");
    assert_eq!(
        out, "noisy : forall e. Int -> Int ! { Console | e }\n",
        "{out}"
    );
}

/// Pure is what a function does, not all a place that uses it allows: one can
/// be handed to a caller whose callback may print without closing that call.
#[test]
fn a_pure_function_can_stand_where_a_callback_may_do_more() {
    let out = schemes_std(
        "use Std.Collections.Vector as V\n\
         fun inc (x : Int) : Int = x + 1\n\
         fun noisy (xs : [Int]) : [Int] ! { Console | e } =\n\
         \x20 let u = println \"mapping\" in V.map inc xs\n",
    );
    assert!(!out.contains("!!"), "{out}");
}

/// A constructor used as a function is one that performs nothing, and can be
/// handed to a caller whose callback may do more -- as a lambda wrapping it can.
#[test]
fn a_constructor_can_stand_where_a_callback_may_do_more() {
    let out = schemes_std(
        "use Std.Collections.Vector as V\n\
         data K = Len Int | Total\n\
         fun lens (xs : [Int]) : [K] ! { Console | e } =\n\
         \x20 let u = println \"mapping\" in V.map K.Len xs\n\
         fun app (f : a -> b ! e) (x : a) : b ! e = f x\n\
         fun one (x : Int) : K ! { Console | e } = app K.Len x\n",
    );
    assert!(!out.contains("!!"), "{out}");
}

/// What a handler leaves of a body that performs the function's own effects
/// is already part of what the function performs: `e` into `{ Console | e }`
/// is a join, not an equation, which as one would have no solution.
#[test]
fn a_handler_can_leave_the_effects_of_the_function_around_it() {
    let out = schemes_std(
        "effect Ask {\n\
         \x20 ask : Int -> Int\n\
         }\n\
         fun asking (body : Int -> Int ! { Ask | e }) : Int ! { Console | e } =\n\
         \x20 let got = handle body 1 with { ask n resume -> resume n, return x -> x } in\n\
         \x20 let u = println \"asked\" in got\n",
    );
    assert!(!out.contains("!!"), "{out}");
    assert!(
        out.contains("asking : forall e. (Int -> Int ! { Ask | e }) -> Int ! { Console | e }"),
        "{out}"
    );
}
