//! The CEK machine is the specification; this checks the whole back end against
//! it.
//!
//! Each case is Meadow source, compiled once to `core` and then run **three
//! ways**:
//!
//! | | what it tests |
//! |---|---|
//! | `meadow_eval` | nothing — it *is* the specification |
//! | `meadow_seq::machine` | the lowering to AxCut, with no registers involved |
//! | `meadow_glade` | that, plus code generation, register allocation and the GC |
//!
//! Running the middle one matters even though the VM is what ships. When a case
//! fails, which of the two disagrees says immediately whether the bug is in the
//! translation or in the back end — and the AxCut machine additionally refuses
//! to enter a block whose parameters do not cover the whole environment, so a
//! lowering that miscounts what is live fails there with a shape error rather
//! than a wrong number.
//!
//! Agreement is on the printed result, because all three print the same way on
//! purpose. Anything the lowering cannot translate is asserted *loudly*: a case
//! that silently reported `Unsupported` and passed would be worse than no case.

use meadow_compiler::{Options, compile_str_with, core};
use meadow_seq::machine;

/// How many steps a case may take. Generous — a bug that loops should fail a
/// test, not hang one.
const FUEL: u64 = 20_000_000;

/// Compile a source string to a `core::Program` whose entry point is the
/// value `result`.
fn program(src: &str) -> core::Program {
    let (pkg, diags) = compile_str_with("diff", src, Options::default().entry("result"));
    let hard: Vec<_> = diags.iter().map(|d| d.msg.clone()).collect();
    assert!(hard.is_empty(), "compile errors:\n{}", hard.join("\n"));
    let entry = pkg.value_entry;
    assert!(entry.is_some(), "the case has no `result`");
    core::Program {
        defs: pkg.defs.clone(),
        entry,
        ctor_fields: pkg.ctor_fields.clone(),
        variants: pkg.variants.clone(),
        origins: Default::default(),
    }
}

/// Every optimization level, since the lowering differs between them.
const LEVELS: [meadow_core::OptLevel; 3] = [
    meadow_core::OptLevel::O0,
    meadow_core::OptLevel::O1,
    meadow_core::OptLevel::O2,
];

/// Run `src` all three ways, at every optimization level, and require the same
/// answer from all of them.
///
/// Returns the shared result, so a case can also assert what it should be —
/// otherwise machines that are wrong in the same way would agree and pass.
///
/// Every level rather than the default, because the levels are a *lowering*
/// decision: `-O2` compiles a `match` to a decision tree, so the cases below
/// are the only place each construct is put through both shapes one at a time.
/// On the whole standard library a disagreement says "something disagrees"; here
/// it says which construct.
#[track_caller]
fn agree(src: &str) -> String {
    let prog = program(src);

    let cek = meadow_eval::run(&prog).unwrap_or_else(|e| panic!("CEK failed: {}\n{src}", e.msg));
    let want = cek.to_string();

    for opt in LEVELS {
        let at = opt.name();
        let lowered = meadow_seq::lower_program(&prog, opt);
        assert!(
            lowered.unsupported.is_empty(),
            "lowering gave up at {at} on {:?}\n{src}",
            lowered.unsupported
        );

        let axcut = machine::Machine::run(&lowered.program, FUEL)
            .unwrap_or_else(|e| panic!("AxCut at {at} failed: {}\n{src}", e.msg));

        let image = meadow_codegen::compile(&lowered.program)
            .unwrap_or_else(|e| panic!("codegen at {at} failed: {}\n{src}", e.msg));
        let vm = meadow_glade::run(&image, FUEL).unwrap_or_else(|e| {
            panic!(
                "VM at {at} failed: {}\n{src}\n{}",
                e.msg,
                image.disassemble()
            )
        });

        assert_eq!(want, axcut.to_string(), "CEK vs AxCut at {at}\n{src}");
        assert_eq!(want, vm, "CEK vs VM at {at}\n{src}");
        assert_eq!(
            Ok(want.clone()),
            jit(&image, 1),
            "CEK vs JIT at {at}\n{src}"
        );
    }
    want
}

/// Run `image` as native code compiled in this process, on `workers` threads.
///
/// At every level the code generator has, which must all answer alike: the
/// passes above `O0` change what native code costs, never what it does.
#[track_caller]
fn jit(image: &meadow_bytecode::Program, workers: usize) -> Result<String, String> {
    let entry = image.entry.ok_or("no entry point")?;
    let mut answer = None;
    // Every block that runs, compiled the first time it does, so all of the
    // code the program reaches runs natively; and compiled the fourth time,
    // so that its calls have been seen first and are guarded on what they
    // entered -- see `meadow_glade::codegen::Known`.
    for threshold in [1, 4] {
        for opt in LEVELS {
            let jit = meadow_glade::jit::Native::jit(image, threshold, opt)?;
            let got = meadow_glade::sched::run_native(image, Some(&jit), entry, FUEL, workers)
                .result
                .map_err(|e| e.msg);
            match &answer {
                None => answer = Some(got),
                Some(want) => assert_eq!(
                    want,
                    &got,
                    "JIT at O0 vs {} after {threshold} entries",
                    opt.name()
                ),
            }
        }
    }
    answer.expect("a level")
}

/// Compile all the way down, for the cases that check a *failure* — where the
/// harness above cannot help because there is no answer to compare.
fn image(prog: &core::Program) -> meadow_bytecode::Program {
    let lowered = meadow_seq::lower_program(prog, meadow_core::OptLevel::default());
    assert!(lowered.unsupported.is_empty(), "{:?}", lowered.unsupported);
    meadow_codegen::compile(&lowered.program).expect("codegen")
}

// --- the straight-line core ----------------------------------------------

// `compile_str` builds a package with no dependencies, so there is no `Std` and
// therefore no `true` / `false` — those are `Std.Bool` constructors, not
// literals. Booleans come from the comparison primitives instead, which is what
// `if` branches on anyway.

#[test]
fn interpolated_strings() {
    assert_eq!(
        agree(r#"def result = "a ${1 + 2} b ${"c"} ${'d'} ${(1, 2.5)} \${e}""#),
        r#""a 3 b c 'd' (1, 2.5) ${e}""#
    );
    // Holes in holes, and a hole with nothing around it.
    assert_eq!(
        agree(
            r#"fun f (n : Int) = "<${n}>"
                 def result = "${f 1}${"${f 2}${f 3}"}""#
        ),
        r#""<1><2><3>""#
    );
}

#[test]
fn literals() {
    assert_eq!(agree("def result = 42"), "42");
    assert_eq!(agree("def result = ()"), "()");
    assert_eq!(agree("def result = 'q'"), "'q'");
    assert_eq!(agree(r#"def result = "hi""#), "\"hi\"");
    assert_eq!(agree("def result = _primAddF 1.5 1.0"), "2.5");
}

#[test]
fn arithmetic_nests_and_keeps_its_order() {
    assert_eq!(agree("def result = 2 + 2"), "4");
    assert_eq!(agree("def result = 1 + 2 * 3 - 4"), "3");
    // Left to right, and each operand's continuation has to keep the operands
    // already computed alive — the case that catches a wrong `keep` set.
    assert_eq!(agree("def result = (1 + 2) * (3 + 4) - (5 + 6)"), "10");
}

#[test]
fn comparisons_and_conditionals() {
    assert_eq!(agree("def result = if 1 < 2 then 10 else 20"), "10");
    assert_eq!(agree("def result = if 1 > 2 then 10 else 20"), "20");
    // Both branches have to see the same environment, and it is not the one the
    // scrutinee was computed in.
    assert_eq!(
        agree("def result = let x = 5 in if x < 3 then x * 2 else x * 3"),
        "15"
    );
}

#[test]
fn let_bindings_nest_and_shadow() {
    assert_eq!(agree("def result = let x = 1 in x + 1"), "2");
    assert_eq!(agree("def result = let x = 1 in let y = 2 in x + y"), "3");
    // The inner `x` must not be captured as the outer one.
    assert_eq!(agree("def result = let x = 1 in let x = x + 10 in x"), "11");
}

// --- codata ---------------------------------------------------------------

#[test]
fn a_lambda_is_codata_with_one_method() {
    assert_eq!(agree("def result = (\\x -> x + 1) 41"), "42");
    assert_eq!(agree("def result = (\\x -> \\y -> x - y) 10 3"), "7");
}

#[test]
fn a_closure_captures_exactly_what_it_needs() {
    assert_eq!(
        agree("def result = let n = 10 in let f = \\x -> x + n in f 32"),
        "42"
    );
    // Two closures over different bindings, alive at the same time: the
    // capture lists have to be independent.
    assert_eq!(
        agree(
            "def result =
               let a = 1 in
               let f = \\x -> x + a in
               let b = 100 in
               let g = \\x -> x + b in
               f 0 + g 0"
        ),
        "101"
    );
}

/// A call site the JIT has seen enter one thing, and so guards on it, and
/// then enters something else: the guard has to fail and the call go the
/// general way. `apply` sees `inc` for its first dozen calls and `dec` after,
/// and returns into two different continuations; the closures capture more
/// than eight values, so their headers are the longer kind, and one captures
/// more than twenty-four.
#[test]
fn a_call_that_stops_entering_what_it_was_seen_to_enter() {
    assert_eq!(
        agree(
            "fun apply f (x : Int) = f x
             fun run (n : Int) (acc : Int) =
               if n == 0 then acc
               else
                 let f = if n > 12 then (\\y -> y + 1) else (\\y -> y - 1) in
                 run (n - 1) (apply f acc)
             fun wide (a : Int) (b : Int) (c : Int) (d : Int) (e : Int) =
               let f = \\y -> y + a + b + c + d + e + a * b + c * d + e * a + b * c in
               let g = \\y -> y + a + b + c + d + e in
               apply f 1 + apply g 2
             fun many (n : Int) =
               let a = n in let b = n + 1 in let c = n + 2 in let d = n + 3 in
               let e = n + 4 in let f = n + 5 in let g = n + 6 in let h = n + 7 in
               let i = n + 8 in let j = n + 9 in let k = n + 10 in let l = n + 11 in
               let m = n + 12 in let o = n + 13 in let p = n + 14 in let q = n + 15 in
               let r = n + 16 in let s = n + 17 in let t = n + 18 in let u = n + 19 in
               let v = n + 20 in let w = n + 21 in let x = n + 22 in let y = n + 23 in
               let z = n + 24 in
               apply (\\q0 -> q0 + a + b + c + d + e + f + g + h + i + j + k + l + m + o
                                  + p + q + r + s + t + u + v + w + x + y + z) 0
             fun loop (n : Int) (acc : Int) =
               if n == 0 then acc else loop (n - 1) (acc + wide n 2 3 4 5 + many n)
             def result = (run 40 0, loop 30 0)"
        ),
        "(16, 26280)"
    );
}

#[test]
fn functions_are_values() {
    assert_eq!(
        agree(
            "fun twice f x = f (f x)
             def result = twice (\\n -> n * 3) 2"
        ),
        "18"
    );
    // A function returned from a function, applied later — the continuation of
    // the outer call has to survive the inner one.
    assert_eq!(
        agree(
            "fun adder n = \\m -> n + m
             def result = (adder 40) 2"
        ),
        "42"
    );
}

// --- globals and recursion ------------------------------------------------

#[test]
fn a_global_reference_is_a_jump() {
    assert_eq!(
        agree(
            "def x = 21
             def result = x + x"
        ),
        "42"
    );
    // Definition order in the source is not call order.
    assert_eq!(
        agree(
            "def result = a + b
             def a = 1
             def b = 2"
        ),
        "3"
    );
}

#[test]
fn recursion_needs_no_back_patching() {
    assert_eq!(
        agree(
            "fun fact n = if n <= 1 then 1 else n * fact (n - 1)
             def result = fact 10"
        ),
        "3628800"
    );
    assert_eq!(
        agree(
            "fun fib n = if n < 2 then n else fib (n - 1) + fib (n - 2)
             def result = fib 20"
        ),
        "6765"
    );
    // Mutual recursion, which is the case a back-patching scheme has to think
    // about and a jump table does not.
    assert_eq!(
        agree(
            "fun isEven n = if n == 0 then 0 == 0 else isOdd (n - 1)
             fun isOdd n = if n == 0 then 0 == 1 else isEven (n - 1)
             def result = isEven 100"
        ),
        "True"
    );
}

#[test]
fn a_deep_tail_call_does_not_grow_anything() {
    // 100_000 iterations. The AxCut machine has no stack at all — the whole
    // state is one environment — so this is a test that the translation of a
    // tail call really is a `substitute` and an `invoke`, with nothing left over.
    assert_eq!(
        agree(
            "fun count n acc = if n == 0 then acc else count (n - 1) (acc + n)
             def result = count 100000 0"
        ),
        "5000050000"
    );
}

// --- data -----------------------------------------------------------------

#[test]
fn constructors_build_data() {
    assert_eq!(
        agree(
            "use Shape.*\ndata Shape = Circle Int | Rect Int Int
             def result = Rect 3 4"
        ),
        "Rect(3, 4)"
    );
    assert_eq!(
        agree(
            "use Shape.*\ndata Shape = Circle Int | Rect Int Int
             def result = Circle (1 + 2)"
        ),
        "Circle(3)"
    );
}

#[test]
fn tuples_are_data_too() {
    assert_eq!(agree("def result = (1, 2)"), "(1, 2)");
    assert_eq!(agree("def result = (1 + 1, (2, 3))"), "(2, (2, 3))");
}

// --- the shape of the failure --------------------------------------------

// --- pattern matching -----------------------------------------------------

#[test]
fn matching_on_a_constructor() {
    assert_eq!(
        agree(
            "use Shape.*\ndata Shape = Circle Int | Rect Int Int
             fun area s = match s with
               | Circle r -> 3 * r * r
               | Rect w h -> w * h
             def result = area (Rect 3 4) + area (Circle 2)"
        ),
        "24"
    );
}

#[test]
fn arms_are_tried_in_order_and_a_wildcard_catches() {
    assert_eq!(
        agree(
            "fun classify n = match n with
               | 0 -> \"zero\"
               | 1 -> \"one\"
               | _ -> \"many\"
             def result = (classify 0, classify 1, classify 7)"
        ),
        "(\"zero\", \"one\", \"many\")"
    );
}

#[test]
fn nested_patterns_backtrack_into_the_next_arm() {
    // The first arm matches `Just` and then fails on the inner literal. That
    // failure has to restore the whole environment — including the scrutinee,
    // which the `switch` for `Just` consumed — before the second arm can look
    // at it again. It is the case the failure-continuation chain exists for.
    assert_eq!(
        agree(
            "use Opt.*\ndata Opt = None | Some Int
             fun f x = match x with
               | Some 0 -> \"zero\"
               | Some n -> \"some\"
               | None   -> \"none\"
             def result = (f (Some 0), f (Some 5), f None)"
        ),
        "(\"zero\", \"some\", \"none\")"
    );
}

#[test]
fn deeply_nested_constructor_patterns() {
    assert_eq!(
        agree(
            "use Tree.*\ndata Tree = Leaf | Node Tree Int Tree
             fun sum t = match t with
               | Leaf -> 0
               | Node l v r -> sum l + v + sum r
             def result = sum (Node (Node Leaf 1 Leaf) 2 (Node Leaf 3 Leaf))"
        ),
        "6"
    );
}

#[test]
fn tuple_and_as_patterns() {
    assert_eq!(
        agree(
            "fun swap p = match p with
               | (a, b) -> (b, a)
             def result = swap (1, 2)"
        ),
        "(2, 1)"
    );
    // An `as` pattern binds the whole and then keeps matching the part.
    assert_eq!(
        agree(
            "use Opt.*\ndata Opt = None | Some Int
             fun f x = match x with
               | Some n -> n
               | None -> 0
             def result = f (Some 7)"
        ),
        "7"
    );
}

#[test]
fn a_non_exhaustive_match_fails_the_same_way_everywhere() {
    // All three must *fail*, and `agree` only compares answers — so this one is
    // checked by hand.
    let prog = program(
        "use Opt.*\ndata Opt = None | Some Int
         fun f x = match x with | Some n -> n
         def result = f None",
    );
    let cek = meadow_eval::run(&prog).expect_err("CEK should fail");
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::default());
    let axcut = machine::Machine::run(&lowered.program, FUEL).expect_err("AxCut should fail");
    let vm = meadow_glade::run(&image(&prog), FUEL).expect_err("the VM should fail");
    assert!(cek.msg.contains("non-exhaustive"), "{}", cek.msg);
    assert!(axcut.msg.contains("non-exhaustive"), "{}", axcut.msg);
    assert!(vm.msg.contains("non-exhaustive"), "{}", vm.msg);
}

// --- lists, and the recursion that builds them ---------------------------

#[test]
fn a_recursive_data_type_end_to_end() {
    assert_eq!(
        agree(
            "use Chain.*\ndata Chain = Nil | Cons Int Chain
             fun range n = if n == 0 then Nil else Cons n (range (n - 1))
             fun total xs = match xs with
               | Nil -> 0
               | Cons x rest -> x + total rest
             def result = total (range 100)"
        ),
        "5050"
    );
}

/// A type of one's own named `Nil` / `Cons` is **not** the builtin list.
///
/// Both `Display`s special-case `List.Nil` / `List.Cons` to print `[1; 2; 3]`.
/// This used to fire for *any* type whose constructors were spelled that way,
/// because constructors shared one global namespace and there was nothing to
/// tell `Chain`'s `Cons` from the list's. Now a constructor's name is
/// `Type.Ctor`, so the special case applies to the list and to nothing else —
/// and a `Chain` prints as the `Chain` it is.
#[test]
fn a_lookalike_type_is_not_mistaken_for_the_builtin_list() {
    assert_eq!(
        agree(
            "use Chain.*\ndata Chain = Nil | Cons Int Chain
             def result = Cons 1 (Cons 2 (Cons 3 Nil))"
        ),
        "Cons(1, Cons(2, Cons(3, Nil)))"
    );
}

// --- records, arrays, projection -----------------------------------------

#[test]
fn records_build_and_select() {
    assert_eq!(agree("def result = { x = 1, y = 2 }"), "{ x = 1, y = 2 }");
    assert_eq!(agree("def result = { x = 1, y = 2 }.y"), "2");
    // Row polymorphism: one accessor, records of different shapes.
    assert_eq!(
        agree(
            "fun getX r = r.x
             def result = (getX { x = 1 }, getX { x = 5, other = 9 })"
        ),
        "(1, 5)"
    );
}

#[test]
fn record_extension_has_no_surface_syntax_but_lowers() {
    // `{ r | x = 9 }` does not parse — the language has no record update. The
    // `core` term exists all the same, so the only way to check it lowers is to
    // build one, which is worth doing: `Extend` is a construct the front end
    // could start producing at any time.
    use meadow_compiler::core::{Def, Lit, Term};
    use std::sync::Arc;

    let var = meadow_compiler::hir::VarId(1);
    let term = Term::Extend(
        Arc::new(Term::Record(vec![
            ("x".into(), Term::Lit(Lit::Int(1))),
            ("y".into(), Term::Lit(Lit::Int(2))),
        ])),
        "x".into(),
        Arc::new(Term::Lit(Lit::Int(9))),
    );
    let prog = core::Program {
        defs: vec![Def::untyped(var, "main", term)],
        entry: Some(var),
        ctor_fields: Default::default(),
        variants: Default::default(),
        origins: Default::default(),
    };

    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::default());
    assert!(lowered.unsupported.is_empty());
    let cek = meadow_eval::run(&prog).expect("CEK");
    let axcut = machine::Machine::run(&lowered.program, FUEL).expect("AxCut");
    let vm = meadow_glade::run(&image(&prog), FUEL).expect("the VM");
    assert_eq!(cek.to_string(), "{ x = 9, y = 2 }");
    assert_eq!(cek.to_string(), axcut.to_string());
    assert_eq!(cek.to_string(), vm);
}

#[test]
fn a_record_pattern_matches_the_fields_it_names() {
    assert_eq!(
        agree(
            "fun getX r = match r with | { x = v } -> v
             def result = getX { x = 42 }"
        ),
        "42"
    );
}

#[test]
fn arrays_build_and_match_by_length() {
    assert_eq!(agree("def result = #[1, 2, 3]"), "#[1, 2, 3]");
    assert_eq!(agree("def result = arrayLen #[1, 2, 3]"), "3");
    assert_eq!(
        agree(
            "fun f a = match a with
               | #[x, y] -> x + y
               | _ -> 0
             def result = (f #[3, 4], f #[1, 2, 3])"
        ),
        "(7, 0)"
    );
}

#[test]
fn tuple_projection_does_not_need_the_arity() {
    // An irrefutable tuple parameter is where `core` produces `Proj`. It carries
    // only an index, so it cannot become a `switch` arm — every arm would have
    // to name every field, and the arity is not in the term. It becomes a field
    // extern instead.
    assert_eq!(
        agree("fun addPair (a, b) = a + b\ndef result = addPair (1, 2)"),
        "3"
    );
    assert_eq!(
        agree("def result = let (a, b, c) = (10, 20, 30) in b"),
        "20"
    );
}

// --- effects --------------------------------------------------------------

#[test]
fn a_handler_that_never_resumes_aborts_the_body() {
    assert_eq!(
        agree(
            "effect Abort { bail : () -> Int }
             def result =
               handle 1 + bail () with {
                 bail u k -> 99
               }"
        ),
        "99"
    );
}

#[test]
fn a_handler_that_resumes_continues_the_body_in_place() {
    // The case that pins down what a resumption restores. The body computes
    // `5 + 1`, and that 6 comes back *inside* the clause, where 100 is added —
    // so the handler's continuation after resuming is the resume site, not
    // where the handler was installed.
    assert_eq!(
        agree(
            "effect Ask { ask : () -> Int }
             def result =
               handle ask () + 1 with {
                 ask u k -> k 5 + 100
               }"
        ),
        "106"
    );
}

/// A handler in tail position of a function called from another handler's
/// body -- `map` inside `toVec` in `Std.Stream` -- whose clause performs to the
/// outer one and then resumes. The inner handler's own continuation is a heap
/// closure, not a frame, so the frame stack has to know the inner handler's
/// boundary from where its `handle` was *entered*, not from where its
/// continuation lives: the first frame-stack cut took the outer handler's
/// frames along and the outer perform had nothing to return into.
#[test]
fn a_handler_inside_another_handlers_body_performs_outwards() {
    let src = "effect Yield { yield : Int -> () }
               data L = Nil | Cons Int L
               use L.*
               fun range (lo : Int) (hi : Int) = if lo >= hi then () else let _ = yield lo in range (lo + 1) hi
               fun map f producer =
                 handle producer () with { yield x k -> let _ = yield (f x) in k (), return r -> () }
               fun toList producer =
                 handle producer () with { yield x k -> Cons x (k ()), return r -> Nil }
               def result = toList (\\() -> map (\\x -> x * 2) (\\() -> range 0 4))";
    assert_eq!(agree(src), "Cons(0, Cons(2, Cons(4, Cons(6, Nil))))");
}

#[test]
fn handlers_are_deep() {
    // `ask` is performed twice, and the second one must find the same handler —
    // which only happens if resuming puts the handler frame back.
    assert_eq!(
        agree(
            "effect Ask { ask : () -> Int }
             def result =
               handle ask () + ask () with {
                 ask u k -> k 5
               }"
        ),
        "10"
    );
    // And through a function call, which is what "deep" is usually about.
    assert_eq!(
        agree(
            "effect Ask { ask : () -> Int }
             fun twice u = ask () + ask ()
             def result = handle twice () with { ask u k -> k 3 }"
        ),
        "6"
    );
}

#[test]
fn a_return_clause_transforms_the_final_value() {
    assert_eq!(
        agree(
            "effect Ask { ask : () -> Int }
             def result =
               handle ask () with {
                 ask u k -> k 2,
                 return x -> x * 10
               }"
        ),
        "20"
    );
}

#[test]
fn state_by_hand_is_a_handler_returning_a_function() {
    // The standard encoding: each clause returns a function of the state, and
    // the `return` clause ignores it. It exercises a handler whose clauses build
    // closures over the resumption, which is where a wrong capture list shows.
    assert_eq!(
        agree(
            "effect St { get : () -> Int, put : Int -> () }
             def result =
               let f =
                 handle
                   let a = get () in
                   let u = put (a + 1) in
                   let b = get () in
                   a + b
                 with {
                   get u k -> \\s -> (k s) s,
                   put v k -> \\s -> (k ()) v,
                   return x -> \\s -> x
                 }
               in f 10"
        ),
        "21"
    );
}

#[test]
fn a_nested_handler_takes_precedence_over_an_outer_one() {
    assert_eq!(
        agree(
            "effect Ask { ask : () -> Int }
             def result =
               handle
                 (handle ask () with { ask u k -> k 1 }) + ask ()
               with {
                 ask u k -> k 100
               }"
        ),
        "101"
    );
}

/// What a program does happens in the order it is written, on every engine
/// and at every level -- the rule `docs/TUTORIAL.md` states under
/// "Evaluation order". Each number is the order one construct's parts were
/// evaluated in: `evaluation_order.mw` says which. Silo runs the same program
/// (`compiler/meadow-llvm/tests/native.rs`), and MeadowBoot's CEK machine
/// does too, as it does every case here.
#[test]
fn everything_is_evaluated_in_the_order_it_is_written() {
    assert_eq!(agree(include_str!("evaluation_order.mw")), EVALUATION_ORDER);
}

/// What `evaluation_order.mw` answers.
const EVALUATION_ORDER: &str = "(123, 123, 123, 12, 12, 123, 123, 12, 12, 123, 12, 12, 12, \
     1234, 12, 1234, 123, 12, 12, 12, 122, 1234567, 12)";

#[test]
fn resuming_twice_is_refused_everywhere() {
    let prog = program(
        "effect Ask { ask : () -> Int }
         def result =
           handle ask () with {
             ask u k -> k 1 + k 2
           }",
    );
    let cek = meadow_eval::run(&prog).expect_err("CEK should refuse");
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::default());
    let axcut = machine::Machine::run(&lowered.program, FUEL).expect_err("AxCut should refuse");
    let vm = meadow_glade::run(&image(&prog), FUEL).expect_err("the VM should refuse");
    assert!(cek.msg.contains("more than once"), "{}", cek.msg);
    assert!(axcut.msg.contains("more than once"), "{}", axcut.msg);
    assert!(vm.msg.contains("more than once"), "{}", vm.msg);
}

// --- everything lowers ----------------------------------------------------

#[test]
fn nothing_in_core_is_left_untranslated() {
    // One program touching every `core` construct the front end can produce.
    // The assertion is on the *lowering*, not the answer: `Unsupported` has one
    // variant left and it means a bug upstream, so an empty set here is the
    // statement that this pass is complete.
    let prog = program(
        "use Opt.*\ndata Opt = None | Some Int
         use Chain.*\ndata Chain = Nil | Cons Int Chain
         effect Ask { ask : () -> Int }

         fun chain n = if n == 0 then Nil else Cons n (chain (n - 1))

         fun sum xs = match xs with
           | Nil -> { total = 0 }
           | Cons x rest -> { total = x + (sum rest).total }

         fun addPair (a, b) = a + b

         def result =
           let opt = Some 3 in
           let arr = #[1, 2, 3] in
           let r = sum (chain 4) in
           handle
             (match opt with | Some n -> n | None -> 0)
               + addPair (1, 2)
               + arrayLen arr
               + r.total
               + ask ()
           with {
             ask u k -> k 1,
             return x -> x
           }",
    );
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::default());
    assert!(
        lowered.unsupported.is_empty(),
        "still untranslated: {:?}",
        lowered.unsupported
    );

    let cek = meadow_eval::run(&prog).expect("CEK");
    let axcut = machine::Machine::run(&lowered.program, FUEL).expect("AxCut");
    let vm = meadow_glade::run(&image(&prog), FUEL).expect("the VM");
    assert_eq!(cek.to_string(), axcut.to_string());
    assert_eq!(cek.to_string(), vm);
}

// --- the heap -------------------------------------------------------------

/// Compile `src` and run it on the VM, answering both the result and the
/// machine, so a case can look at what collection did.
#[track_caller]
fn run_on_vm(src: &str) -> (String, u64, u64) {
    let prog = program(src);
    let img = image(&prog);
    let mut vm = meadow_glade::Vm::new(&img);
    let entry = img.entry.expect("an entry point");
    let v = vm
        .run(entry, FUEL)
        .unwrap_or_else(|e| panic!("VM failed: {}\n{src}", e.msg));
    let out = vm.show(v);
    let (collections, allocated) = vm.heap_stats();
    (out, collections, allocated)
}

#[test]
fn the_collector_runs_and_the_answer_survives_it() {
    // Builds a 200_000-element list and walks it. That is well past the initial
    // heap, so collection is not optional — and every one of them moves the list
    // the program is still holding.
    let src = "use Chain.*\ndata Chain = Nil | Cons Int Chain
               fun build n = if n == 0 then Nil else Cons n (build (n - 1))
               fun total xs = match xs with
                 | Nil -> 0
                 | Cons x rest -> x + total rest
               def result = total (build 200000)";
    let (out, collections, allocated) = run_on_vm(src);
    assert_eq!(out, "20000100000");
    assert!(
        collections > 0,
        "the heap never filled — is the test too small?"
    );
    // The list alone is 200_000 cells of four slots; the recursion's
    // continuations no longer add to it, since a branch's context is a join
    // point rather than an object.
    assert!(allocated >= 800_000, "only {allocated} slots allocated");

    // And the specification agrees, which is the point.
    assert_eq!(agree(src), "20000100000");
}

#[test]
fn garbage_is_actually_reclaimed() {
    // Allocates a fresh tuple every iteration and keeps none of them. If the
    // collector were retaining what it should not — a stale register, a root set
    // that is too wide — this would grow without bound and run out of memory
    // rather than finishing.
    let src = "fun loop n acc = if n == 0 then acc else loop (n - 1) (acc + 1)
               fun step n = let pair = (n, n) in loop 10 0
               fun outer n acc =
                 if n == 0 then acc else outer (n - 1) (acc + step n)
               def result = outer 20000 0";
    let (out, collections, _) = run_on_vm(src);
    assert_eq!(out, "200000");
    assert!(collections > 0);
}

#[test]
fn a_cycle_through_a_mutable_cell_does_not_leak_the_program() {
    // Reference counting cannot free a cell that points at something holding the
    // cell. A copying collector never has the question — this runs, where the
    // `Rc`-based machine would simply keep every one of them alive.
    let src = "fun spin n =
                 if n == 0 then 0
                 else
                   let cell = newRef 0 in
                   let ignored = setRef cell (getRef cell + n) in
                   getRef cell + spin (n - 1)
               def result = spin 20000";
    assert_eq!(agree(src), "200010000");
}

#[test]
fn a_deep_tail_loop_costs_the_vm_no_stack() {
    // The VM has no call stack at all, so this is not tail-call *optimisation* —
    // there is nothing to optimise away. It is also a GC test: the loop keeps
    // one accumulator alive and everything else has to go.
    let (out, _, _) = run_on_vm(
        "fun count n acc = if n == 0 then acc else count (n - 1) (acc + n)
         def result = count 100000 0",
    );
    assert_eq!(out, "5000050000");
}

/// A constructor prints the same on both machines, and prints *bare*.
///
/// A constructor's real name is `Type.Ctor` — two types may each own a `Leaf`,
/// and every pass after the resolver keys on the qualified form. Display is the
/// one place that has to undo it, in two separate implementations
/// (`meadow_eval`'s `Display` and `meadow_glade::show`), which is exactly the
/// shape of thing that drifts: the CEK machine kept printing `Maybe.Just(3)`
/// for a while after the VM had stopped.
#[test]
fn constructors_print_bare_and_agree() {
    // No `Std` here, so every constructor is one these cases declare.
    assert_eq!(
        agree("use Box.*\ndata Box = Wrap Int\ndef result = Wrap 3"),
        "Wrap(3)"
    );
    assert_eq!(
        agree("use Colour.*\ndata Colour = Red | Green\ndef result = (Red, Green)"),
        "(Red, Green)"
    );
    // Two types owning one constructor name — the reason the qualified form
    // exists. Both print as themselves.
    assert_eq!(
        agree(
            "use Tree.*\ndata Tree = Leaf | Node Tree Tree\n\
             use Rope.*\ndata Rope = Leaf String | Node Rope Rope\n\
             def result = (Tree.Leaf, Rope.Leaf \"s\")"
        ),
        "(Leaf, Leaf(\"s\"))"
    );
}

/// A definition that is a constructor applied to nothing is written in where
/// it is mentioned, as a literal is -- including a generic one's copy at each
/// type it is used at -- rather than kept and asked for.
#[test]
fn a_constant_constructor_is_the_same_wherever_it_is_mentioned() {
    assert_eq!(
        agree(
            "use M.*\ndata M = A | B Int\n\
             def none : M = A\n\
             def nothing : [a;] = [;]\n\
             fun count (n : Int) (acc : Int) : Int =\n\
             \x20 if n == 0 then acc else count (n - 1) (match none with | A -> acc + 1 | B _ -> acc)\n\
             def result = (count 10 0, 1 :: nothing, \"x\" :: nothing)"
        ),
        "(10, [1], [\"x\"])"
    );
}

/// `.field` on a `record` declaration's value, which is constructor data with
/// named fields rather than an anonymous record. The CEK machine looked the
/// name up in the constructor's field list; the VM and the sequent machine only
/// knew anonymous records, and failed with "selected `.x` from object".
#[test]
fn a_records_fields_select_by_name_everywhere() {
    assert_eq!(
        agree(
            "record Point = { x : Int, y : Int }\n\
             def origin = Point { x = 3, y = 4 }\n\
             def result = (origin.y, origin.x, (Point { x = 1, y = 2 }).y)"
        ),
        "(4, 3, 2)"
    );
    // Fields declared in a different order from the one they are written in.
    assert_eq!(
        agree(
            "record Pair = { second : Int, first : Int }\n\
             def result = let p = Pair { first = 1, second = 2 } in (p.first, p.second)"
        ),
        "(1, 2)"
    );
    // Selected where only the type says which record it is -- a field read,
    // not a search by name -- of a record with parameters and an unboxed field
    // beside boxed ones.
    assert_eq!(
        agree(
            "record Box a = { item : a, count : Int, weight : Float }\n\
             fun weigh (b : Box a) : Float = _primMulF b.weight 2.0\n\
             fun first (b : Box [Int;]) : [Int;] = b.item\n\
             def result = let b = Box { weight = 1.5, item = [4; 5], count = 3 } in\n\
             \x20 (weigh b, first b, b.count)"
        ),
        "(3.0, [4; 5], 3)"
    );
}

/// `hash` is one algorithm fed the same way by every engine, so all three give
/// the same number for the same value -- and a program that prints one, or
/// lays a table out by one, behaves the same wherever it runs.
#[test]
fn hashes_agree_everywhere() {
    agree(
        "use Shape.*\ndata Shape = Circle Int | Rect Int Int\n\
         record Point = { x : Int, y : Int }\n\
         def result =\n\
         \x20 ( hash 0, hash (-7), hash 1.5, hash (-0.0), hash True, hash 'q'\n\
         \x20 , hash \"a string past eight bytes\", hash ()\n\
         \x20 , hash (1, \"two\", 3.0), hash [1; 2; 3], hash #[4, 5]\n\
         \x20 , hash (Rect 2 3), hash { a = 1, b = \"x\" }, hash (Point { x = 1, y = 2 })\n\
         \x20 , hash (toBigInt 12345678901234) )",
    );
}

/// Two values `==` calls equal hash alike, on every engine.
#[test]
fn equal_values_hash_alike() {
    assert_eq!(
        agree(
            "def result =\n\
             \x20 ( hash 0.0 == hash (-0.0)\n\
             \x20 , hash { a = 1, b = 2 } == hash { b = 2, a = 1 }\n\
             \x20 , hash (1 :: 2 :: [;]) == hash [1; 2]\n\
             \x20 , hash 1 == hash 2 )"
        ),
        "(True, True, True, False)"
    );
}

/// `runSt` and its mutable arrays behave alike everywhere: written in place,
/// copied by `stFreeze` / `stThaw`, and printed as what they hold.
#[test]
fn mutable_arrays_agree_everywhere() {
    assert_eq!(
        agree(
            "fun fill a i n = if i >= n then () else let _ = stSetArray a i (i * i) in fill a (i + 1) n\n\
             def result = runSt (\\() ->\n\
             \x20 let a = stNewArray 5 0 in\n\
             \x20 let _ = fill a 0 5 in\n\
             \x20 let frozen = stFreeze a in\n\
             \x20 let b = stThaw frozen in\n\
             \x20 let _ = stSetArray b 0 99 in\n\
             \x20 let r = stNewRef 1 in\n\
             \x20 let _ = stSetRef r (stGetRef r + stArrayLen b) in\n\
             \x20 (frozen, stFreeze b, stGetRef r))"
        ),
        "(#[0, 1, 4, 9, 16], #[99, 1, 4, 9, 16], 6)"
    );
}

// --- sized numbers -------------------------------------------------------

// A fixed-width integer is an unboxed word in every engine: in a register, in a
// constructor field, and in an array slot. A disagreement in wrapping, sign,
// display or equality between the three shows up here first.

#[test]
fn a_fixed_width_integer_wraps_at_its_width() {
    assert_eq!(agree("def result = toUInt8 250 + toUInt8 10"), "4");
    assert_eq!(agree("def result = toInt8 127 + toInt8 1"), "-128");
    assert_eq!(agree("def result = toUInt16 0 - toUInt16 1"), "65535");
    assert_eq!(agree("def result = toInt32 65536 * toInt32 65536"), "0");
    assert_eq!(
        agree("def result = toUInt64 (toInt (0 - 1))"),
        "18446744073709551615"
    );
    // A conversion keeps the low bits, from a `BigInt` as from anything else.
    assert_eq!(agree("def result = toUInt8 (2 ^ 100 + 5)"), "5");
    assert_eq!(agree("def result = toInt16 40000"), "-25536");
}

/// A number nothing pins down is an `Int`, and an `Int` wraps: `2 ^ 64` is
/// `0`. Where a result has to be exact past 2^63 the program says `BigInt`,
/// and gets it. (It was the other way round: the default was `BigInt`, and
/// every program that never said what its numbers were paid for the heap.)
#[test]
fn an_unconstrained_literal_is_an_int_and_wraps() {
    assert_eq!(agree("def result = 2 ^ 64"), "0");
    assert_eq!(
        agree("def result = toBigInt 2 ^ 64"),
        "18446744073709551616"
    );
    assert_eq!(
        agree("def big : BigInt = 2 ^ 64\ndef result = big"),
        "18446744073709551616"
    );
}

#[test]
fn shifts_and_bits_know_the_width_and_the_sign() {
    assert_eq!(agree("def result = toUInt32 (0 - 1) >> 28"), "15");
    assert_eq!(agree("def result = toInt32 (0 - 16) >> 2"), "-4");
    assert_eq!(agree("def result = bitNot (toUInt8 0)"), "255");
    assert_eq!(agree("def result = popCount (toInt16 (0 - 1))"), "16");
    assert_eq!(agree("def result = bitWidth (toUInt64 0)"), "64");
}

#[test]
fn sized_integers_compare_and_match_by_value() {
    assert_eq!(
        agree("def result = if toUInt8 200 > 100 then 1 else 0"),
        "1"
    );
    assert_eq!(
        agree("def result = if toInt8 (0 - 1) < 0 then 1 else 0"),
        "1"
    );
    assert_eq!(agree("def result = if toUInt16 5 == 5 then 1 else 0"), "1");
    assert_eq!(
        agree("def result = match toUInt8 3 with | 2 -> 20 | 3 -> 30 | _ -> 0"),
        "30"
    );
}

#[test]
fn sized_numbers_are_stored_in_fields_and_arrays() {
    assert_eq!(
        agree(
            "use P.*\ndata P = P UInt8 Int16 Float32
             fun total p = match p with | P a b c -> (toInt a + toInt b, c)
             def result = total (P (toUInt8 255) (toInt16 (0 - 5)) (toFloat32 0.5))"
        ),
        "(250, 0.5)"
    );
    assert_eq!(
        agree("def result = let (b : #[UInt8]) = #[1, 255, 3] in (b, arrayGet b 1 + toUInt8 1)"),
        "(#[1, 255, 3], 0)"
    );
}

#[test]
fn float32_keeps_single_precision() {
    assert_eq!(
        agree("def result = _primAddF (toFloat32 1.5) (toFloat32 0.25)"),
        "1.75"
    );
    // Printed as the shortest text that reads back as the same `Float32`.
    assert_eq!(agree("def result = toFloat32 0.1"), "0.1");
    assert_eq!(
        agree("def result = toFloat64 (toFloat32 0.1)"),
        "0.10000000149011612"
    );
    assert_eq!(agree("def result = toFloat32 16777217.0"), "16777216.0");
}

#[test]
fn equal_integers_hash_alike_whatever_their_type() {
    assert_eq!(
        agree("def result = if hash (toUInt8 7) == hash (toInt 7) then 1 else 0"),
        "1"
    );
    assert_eq!(
        agree("def result = if hash (toInt32 7) == hash (toBigInt 7) then 1 else 0"),
        "1"
    );
}

// --- compact regions -----------------------------------------------------

// On the VM a compacted value lives outside the collected heap; on the other
// two nothing moves. The cases bind with `let` rather than `def`: the sequent
// machine evaluates a top-level `def` again at each use, so `def c = compact x`
// would be a new region every time `c` is mentioned, and the sizes would count
// that rather than what is being tested. Everything a program can see must still agree -- the
// value, `==`, `hash`, what is refused, and even `compactSize`, which the
// reference-counted engines estimate by counting what the VM would copy.

const CHAIN: &str = "use Chain.*\ndata Chain = End | Link Int Chain
                     fun build n = if n == 0 then End else Link n (build (n - 1))
                     fun total xs = match xs with
                       | End -> 0
                       | Link x rest -> x + total rest\n";

#[test]
fn a_compacted_value_is_the_value() {
    let src = format!(
        "{CHAIN}def result =
           let xs = build 100 in
           let c = compact xs in
           (total (getCompact c), if getCompact c == xs then 1 else 0)"
    );
    assert_eq!(agree(&src), "(5050, 1)");
    assert_eq!(
        agree("def result = compact (1, \"two\", #[3.0])"),
        "compact (1, \"two\", #[3.0])"
    );
}

#[test]
fn compacts_compare_and_hash_by_what_they_hold() {
    let src = format!(
        "{CHAIN}def a = compact (build 10)
         def b = compact (build 10)
         def result = (if a == b then 1 else 0, if hash a == hash b then 1 else 0,
                     if compact (build 3) == a then 1 else 0)"
    );
    assert_eq!(agree(&src), "(1, 1, 0)");
}

#[test]
fn adding_to_a_region_keeps_both_values_and_shares_the_old_one() {
    let src = format!(
        "{CHAIN}def result =
           let c = compact (build 100) in
           let before = compactSize c in
           let c2 = compactAdd c (Link 0 (getCompact c)) in
           (total (getCompact c), total (getCompact c2),
            compactSize c2 - before, compactSize c == compactSize c2)"
    );
    // One `Link` is four slots -- two of header, two fields -- and that is all
    // the add copied.
    let bytes = meadow_core::compact::object_slots(false, 2) * meadow_core::compact::SLOT_BYTES;
    assert_eq!(agree(&src), format!("(5050, 5050, {bytes}, True)"));
}

#[test]
fn compact_size_is_the_same_on_every_engine() {
    let src = format!(
        "{CHAIN}def result =
           let xs = build 1000 in
           (compactSize (compact xs), compactSize (compact (xs, xs)),
            compactSize (compact (toInt 7)), compactSize (compact #[xs, xs, xs]))"
    );
    agree(&src);
}

#[test]
fn a_compact_can_hold_a_compact() {
    let src = format!(
        "{CHAIN}def result =
           let inner = compact (build 4) in
           let outer = compact (inner, inner) in
           match getCompact outer with
           | (a, b) -> (total (getCompact a), if a == b then 1 else 0)"
    );
    assert_eq!(agree(&src), "(10, 1)");
}

#[test]
fn what_cannot_be_compacted_fails_the_same_way_everywhere() {
    for (value, what) in [
        ("(1, newRef 2)", "a Ref"),
        ("Just (\\x -> x)", "a function"),
        ("#[stNewArray 2 0]", "a mutable array"),
    ] {
        // Where the type at `compact` is known, this is refused before it runs
        // (`meadow_exhaust`). Through a function that only knows it as a
        // variable, and does more with it than hand it on, it reaches each
        // runtime -- whose own check is what this is about.
        let prog = program(&format!(
            "use Opt.*\ndata Opt a = None | Just a\n\
             fun later x = let y = x in compact y\n\
             def result = later ({value})"
        ));
        let cek = meadow_eval::run(&prog).expect_err("CEK should fail");
        let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::default());
        let axcut = machine::Machine::run(&lowered.program, FUEL).expect_err("AxCut should fail");
        let vm = meadow_glade::run(&image(&prog), FUEL).expect_err("the VM should fail");
        let want = meadow_core::compact::uncompactable(what);
        assert_eq!(cek.msg, want);
        assert_eq!(axcut.msg, want);
        assert_eq!(vm.msg, want);
    }
}

#[test]
fn a_compacted_value_survives_collections_without_being_copied() {
    // A 50_000-link chain kept alive through a loop that allocates enough to
    // collect many times over. Compacted, the collections copy almost nothing;
    // left in the heap, every one of them copies the whole chain.
    let churn = "fun churn n acc = if n == 0 then acc else churn (n - 1) (acc + total (build 50))";
    use meadow_glade::heap::{Collector, GcConfig, Heap};
    let run = |keep: &str, collector: Collector| {
        let src = format!(
            "{CHAIN}{churn}
             def result = let xs = {keep} in (churn 20000 0, total xs)"
        );
        let prog = program(&src);
        let img = image(&prog);
        let config = GcConfig {
            collector,
            ..GcConfig::from_env()
        };
        let mut vm = meadow_glade::Vm::with_heap(&img, Heap::with_config(1 << 16, config));
        let v = vm
            .run(img.entry.unwrap(), u64::MAX)
            .unwrap_or_else(|e| panic!("{}", e.msg));
        let h = vm.heap();
        (
            vm.show(v),
            h.collections,
            h.copied,
            h.promoted,
            h.region_slots(),
        )
    };

    // Copying: every collection copies the chain, unless it is compacted.
    let (plain, plain_gcs, plain_copied, _, _) = run("build 50000", Collector::Copying);
    let (compacted, gcs, copied, _, region) =
        run("getCompact (compact (build 50000))", Collector::Copying);
    assert_eq!(plain, "(25500000, 1250025000)");
    assert_eq!(compacted, plain);
    assert!(
        plain_gcs > 5 && gcs > 5,
        "both should collect: {plain_gcs}, {gcs}"
    );
    assert_eq!(
        region,
        2 + 50_000 * 4,
        "the chain is in a region, and nothing else is"
    );
    assert!(
        copied * 4 < plain_copied,
        "compacting should spare most of the copying: {copied} slots against {plain_copied}"
    );

    // Generational: the chain is promoted once, as it is built, and never
    // copied again either way -- what compacting spares there is marking it at
    // every cycle, which this program is too small to run. The same answer,
    // and the chain where it should be.
    let (plain, _, _, plain_promoted, _) = run("build 50000", Collector::Generational);
    let (compacted, _, _, _, region) = run(
        "getCompact (compact (build 50000))",
        Collector::Generational,
    );
    assert_eq!(compacted, plain);
    assert!(
        plain_promoted >= 50_000 * 4,
        "the chain outgrew the nursery"
    );
    assert_eq!(region, 2 + 50_000 * 4);
}

#[test]
fn regions_nothing_refers_to_are_freed() {
    // A thousand regions of a thousand links each, each dropped straight away.
    // Kept, they would be three million slots; collecting frees them.
    let src = format!(
        "{CHAIN}fun again n acc = if n == 0 then acc
                 else again (n - 1) (acc + total (getCompact (compact (build 1000))))
         def result = again 1000 0"
    );
    let prog = program(&src);
    let img = image(&prog);
    let mut vm = meadow_glade::Vm::new(&img);
    let v = vm
        .run(img.entry.unwrap(), u64::MAX)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(vm.show(v), "500500000");
    assert!(
        vm.heap().collections > 0,
        "writing regions should have asked for a collection"
    );
    assert!(
        vm.heap().region_slots() < 1_000_000,
        "dead regions were kept: {} slots",
        vm.heap().region_slots()
    );
}

// --- number-generic code at a number type -----------------------------------

// A function generic over its number type is copied for each type it is used
// at (`meadow_core::specialize`), so its literals are made as that type. Before
// that, a literal in generic code was an `Int` at run time whatever the call
// said, and `fib 100` -- typed `BigInt` -- wrapped at 64 bits.

const FIB: &str = "fun fib n =
                     let rec loop a b i =
                       if i == 0 then a
                       else loop b (a + b) (i - 1)
                     in loop 0 1 n\n";

#[test]
fn a_generic_function_computes_at_the_type_it_is_called_at() {
    // `fib 100` defaults to `Int`: wrapped. Where its answer meets a
    // `BigInt`, it is computed as one: exact.
    assert_eq!(
        agree(&format!("{FIB}def result = toBigInt 0 + fib 100")),
        "354224848179261915075"
    );
    assert_eq!(
        agree(&format!(
            "{FIB}def result = (fib 100, fib (toUInt8 13) + toUInt8 0)"
        )),
        "(3736710778780434371, 233)"
    );
}

/// `BigInt` arithmetic as the runtime does it: on the heap's own limbs for
/// `+`, `-` and comparison, through num-bigint for the rest -- and the same
/// answer as the CEK machine either way.
///
/// The numbers are chosen to run across limbs and through every sign case the
/// limb code has to get right: carries out of the top limb, borrows that empty
/// it, a result of exactly zero, a negative minus a larger negative, and an
/// `Int` on either side of a `BigInt`.
#[test]
fn big_integers_agree_across_every_sign_and_limb_boundary() {
    let src = format!(
        "{FIB}def big = fib 400
         def neg = toBigInt 0 - big
         def two64 = toBigInt 9223372036854775807 * toBigInt 2 + toBigInt 2
         def result =
           ( ( big - big, neg + big, big - (big + toBigInt 1), neg - neg
             , two64 - toBigInt 1, two64 * two64 - toBigInt 1, neg - big )
           , ( 7 + big - big, big + 9223372036854775807 - big, (big * big) / big == big
             , big % toBigInt 1000000007, neg / toBigInt 3 )
           , ( neg < big, big > neg, big <= big, neg >= big, neg < neg - toBigInt 1
             , two64 > two64 - toBigInt 1, big == fib 400, neg == toBigInt 0 - fib 400 ) )"
    );
    let got = agree(&src);
    // Not only agreeing: right. `fib 400` has 84 digits, so it spans limbs.
    assert!(
        got.contains(
            "(0, 0, -1, 0, 18446744073709551615, 340282366920938463463374607431768211455, -"
        ),
        "{got}"
    );
    assert!(
        got.ends_with("(True, True, True, False, False, True, True, True))"),
        "{got}"
    );
}

#[test]
fn a_literal_in_generic_code_wraps_at_the_width_it_is_used_at() {
    let src = "fun bump x = x + 200
               def result = (bump (toUInt8 100), bump (toInt16 32600), bump 1)";
    assert_eq!(agree(src), "(44, -32736, 201)");
}

#[test]
fn a_generic_local_is_copied_for_each_type_in_its_scope() {
    let src = "fun f u =
                 let g x = x + 1 in
                 (g (toUInt8 255), g (toInt8 127), g (toBigInt 9223372036854775807), g 9223372036854775807)
               def result = f ()";
    assert_eq!(
        agree(src),
        "(0, -128, 9223372036854775808, -9223372036854775808)"
    );
}

#[test]
fn a_generic_function_passed_as_a_value_is_copied_too() {
    let src = "fun addOne x = x + 1
               fun apply f x = f x
               def result = (apply addOne (toUInt8 255), apply addOne (toBigInt 9223372036854775807), apply addOne 9223372036854775807)";
    assert_eq!(agree(src), "(0, 9223372036854775808, -9223372036854775808)");
}

#[test]
fn mutually_recursive_generic_functions_are_copied_together() {
    let src = "fun countDown n acc = if n == 0 then acc else countUp (n - 1) (acc + 1)
               fun countUp n acc = if n == 0 then acc else countDown (n - 1) (acc + 1)
               def result = (countDown (toInt 5) (toUInt8 254), countUp 3 0)";
    assert_eq!(agree(src), "(3, 3)");
}

#[test]
fn a_float_literal_in_generic_code_takes_the_float_type() {
    let src = "fun third x = _primDivF x 3.0
               def result = (third (toFloat32 1.0), third 1.0)";
    assert_eq!(agree(src), "(0.33333334, 0.3333333333333333)");
}

// --- green threads -----------------------------------------------------------

// The sequent machine has no scheduler, so these compare the CEK machine -- one
// thread at a time, in order -- against the VM at every optimization level,
// on one OS thread and on several. Only programs whose answer does not depend
// on the interleaving can agree, so that is what these are.

/// Run `src` on the CEK machine and on the VM with 1 and 8 workers, require
/// one answer -- or one failure -- from all of them, and return it.
#[track_caller]
fn threads_agree(src: &str) -> Result<String, String> {
    let prog = program(src);
    let want = meadow_eval::run(&prog)
        .map(|v| v.to_string())
        .map_err(|e| e.msg);
    for opt in LEVELS {
        let lowered = meadow_seq::lower_program(&prog, opt);
        assert!(lowered.unsupported.is_empty(), "{:?}", lowered.unsupported);
        let image = meadow_codegen::compile(&lowered.program).expect("codegen");
        for workers in [1, 8] {
            let got = meadow_glade::sched::run_with(&image, image.entry.unwrap(), FUEL, workers)
                .result
                .map_err(|e| e.msg);
            assert_eq!(
                want,
                got,
                "CEK vs VM at {} with {workers} workers\n{src}",
                opt.name()
            );
            assert_eq!(
                want,
                jit(&image, workers),
                "CEK vs JIT at {} with {workers} workers\n{src}",
                opt.name()
            );
        }
    }
    want
}

const FIB_INT: &str = "fun fib (n : Int) = if n < 2 then n else fib (n - 1) + fib (n - 2)\n";

#[test]
fn a_spawned_thread_answers_through_await() {
    let src =
        format!("{FIB_INT}def result = let t = threadSpawn (\\() -> fib 20) in threadAwait t");
    assert_eq!(threads_agree(&src), Ok("6765".into()));
}

#[test]
fn threads_run_side_by_side_and_are_awaited_in_any_order() {
    let src = format!(
        "{FIB_INT}use Ts.*\ndata Ts = Done | More (Task Int) Ts
         fun spawnAll (n : Int) acc = if n == 0 then acc else spawnAll (n - 1) (More (threadSpawn (\\() -> fib n)) acc)
         fun sumAll ts acc = match ts with | Done -> acc | More t rest -> sumAll rest (acc + threadAwait t)
         def result = sumAll (spawnAll 20 Done) 0"
    );
    assert_eq!(threads_agree(&src), Ok("17710".into()));
}

#[test]
fn a_channel_carries_values_between_threads_in_order() {
    let src = "use L.*\ndata L = Nil | Cons Int L
               def result =
                 let ch = channelNew () in
                 let producer = threadSpawn (\\() ->
                   let rec go (i : Int) = if i > 5 then () else let _ = channelSend ch i in go (i + 1)
                   in go 1) in
                 let rec take (k : Int) acc = if k == 0 then acc else take (k - 1) (Cons (channelReceive ch) acc) in
                 let got = take 5 Nil in
                 let _ = threadAwait producer in
                 got";
    assert_eq!(
        threads_agree(src),
        Ok("Cons(5, Cons(4, Cons(3, Cons(2, Cons(1, Nil)))))".into())
    );
}

#[test]
fn channels_and_threads_can_themselves_be_sent() {
    // A worker is handed the channel to answer on, over another channel.
    let src = "def result =
                 let jobs = channelNew () in
                 let worker = threadSpawn (\\() ->
                   let job = channelReceive jobs in
                   match job with
                   | (n, reply) -> channelSend reply (n * 2)) in
                 let reply = channelNew () in
                 let _ = channelSend jobs (toInt 21, reply) in
                 let answer = channelReceive reply in
                 let _ = threadAwait worker in
                 answer";
    assert_eq!(threads_agree(src), Ok("42".into()));
}

#[test]
fn every_thread_has_its_own_mutable_state() {
    // Each thread makes a cell of its own and counts to a different number in
    // it. Nothing is shared, so nothing is lost however they interleave.
    let src = "fun count (n : Int) = let r = newRef (toInt 0) in
                 let rec go (i : Int) = if i == 0 then getRef r else let _ = setRef r (getRef r + 1) in go (i - 1)
                 in go n
               def result =
                 let a = threadSpawn (\\() -> count 3000) in
                 let b = threadSpawn (\\() -> count 5000) in
                 let c = threadSpawn (\\() -> count 7000) in
                 (threadAwait a, threadAwait b, threadAwait c)";
    assert_eq!(threads_agree(src), Ok("(3000, 5000, 7000)".into()));
}

#[test]
fn many_producers_one_consumer() {
    // Arrival order depends on scheduling; the total does not.
    let src = "fun produce ch (from : Int) = let rec go (i : Int) = if i == 0 then () else
                   let _ = channelSend ch (from + i) in go (i - 1) in go 100
               fun spawnProducers ch (k : Int) = if k == 0 then () else
                   let _ = threadSpawn (\\() -> produce ch (k * 1000)) in spawnProducers ch (k - 1)
               fun consume ch (n : Int) acc = if n == 0 then acc else consume ch (n - 1) (acc + channelReceive ch)
               def result = let ch = channelNew () in let _ = spawnProducers ch 16 in consume ch 1600 0";
    assert_eq!(threads_agree(src), Ok("13680800".into()));
}

#[test]
fn mutable_state_and_continuations_cannot_cross() {
    for (value, what) in [("newRef 1", "a Ref"), ("stNewArray 2 0", "a mutable array")] {
        let src = format!(
            "use L.*\ndata L = Nil | Cons Int L
             def result = let x = {value} in threadAwait (threadSpawn (\\() -> let y = x in 0))"
        );
        let src = src.replace("[;]", "Nil");
        assert_eq!(
            threads_agree(&src),
            Err(meadow_core::thread::unsendable(what)),
            "{value}"
        );
    }
    let src = "effect E { e : () -> Int }
               def result = handle (let n = e () in n) with {
                 e u k -> let _ = threadAwait (threadSpawn (\\() -> let j = k in 0)) in 0 }";
    assert_eq!(
        threads_agree(src),
        Err(meadow_core::thread::unsendable("a continuation"))
    );
}

#[test]
fn a_ref_in_scope_but_unused_does_not_stop_a_spawn() {
    // What crosses is what the function uses -- which the CEK machine, whose
    // closures hold their whole environment, has to work out to agree.
    let src = "def result = let r = newRef (toInt 1) in let x = toInt 41 in threadAwait (threadSpawn (\\() -> x + 1))";
    assert_eq!(threads_agree(src), Ok("42".into()));
}

#[test]
fn a_message_cannot_carry_a_ref_either() {
    let src = "def result = let ch = channelNew () in channelSend ch (newRef (toInt 0))";
    assert_eq!(
        threads_agree(src),
        Err(meadow_core::thread::unsendable("a Ref"))
    );
}

#[test]
fn a_failed_thread_fails_its_await_with_the_same_message() {
    let src = "def result = let t = threadSpawn (\\() -> toInt 1 / toInt 0) in threadAwait t + 1";
    assert_eq!(threads_agree(src), Err("division by zero".into()));
    // Nobody waiting: the failure stays in the thread.
    let src = "def result = let t = threadSpawn (\\() -> toInt 1 / toInt 0) in 7";
    assert_eq!(threads_agree(src), Ok("7".into()));
}

#[test]
fn waiting_on_what_can_never_answer_is_a_deadlock() {
    let src = "def result = let ch = channelNew () in threadAwait (threadSpawn (\\() -> channelReceive ch + toInt 1))";
    assert_eq!(
        threads_agree(src),
        Err(meadow_core::thread::DEADLOCK.into())
    );
}

#[test]
fn a_thread_that_never_waits_does_not_starve_the_others() {
    // `spin` would run forever; the answer comes from the other thread, and
    // `main` ending ends the program.
    let src = "fun spin (n : Int) = spin (n + 1)
               def result = let _ = threadSpawn (\\() -> spin 0) in
                 threadAwait (threadSpawn (\\() -> toInt 99))";
    let prog = program(src);
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::default());
    let image = meadow_codegen::compile(&lowered.program).unwrap();
    for workers in [1, 2] {
        let got = meadow_glade::sched::run_with(&image, image.entry.unwrap(), FUEL, workers).result;
        assert_eq!(got.map_err(|e| e.msg), Ok("99".into()), "{workers} workers");
    }
    assert_eq!(
        meadow_eval::run(&prog)
            .map(|v| v.to_string())
            .map_err(|e| e.msg),
        Ok("99".into())
    );
}

#[test]
fn threads_collect_their_own_heaps() {
    // Every thread allocates enough to collect several times. The collections
    // are each thread's own -- the stats count one heap per thread.
    let src = "use L.*\ndata L = Nil | Cons Int L
               fun build (n : Int) = if n == 0 then Nil else Cons n (build (n - 1))
               fun len xs = match xs with | Nil -> toInt 0 | Cons _ r -> 1 + len r
               fun churn (k : Int) acc = if k == 0 then acc else churn (k - 1) (acc + len (build 2000))
               def result =
                 let a = threadSpawn (\\() -> churn 50 0) in
                 let b = threadSpawn (\\() -> churn 50 0) in
                 threadAwait a + threadAwait b";
    let prog = program(src);
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::default());
    let image = meadow_codegen::compile(&lowered.program).unwrap();
    let out = meadow_glade::sched::run_with(&image, image.entry.unwrap(), FUEL, 4);
    assert_eq!(out.result.map_err(|e| e.msg), Ok("200000".into()));
    assert_eq!(out.stats.threads, 3, "main and two spawned");
    assert!(
        out.stats.collections >= 2,
        "only {} collections",
        out.stats.collections
    );
}

// --- top-level values -------------------------------------------------------

// A top-level `def` is evaluated once, when first needed, on every engine. The
// VM used to evaluate one at every mention and the CEK machine all of them at
// load, which nothing could see while defs were pure -- except `compactSize`,
// which counts a region the mentions share or do not.

#[test]
fn a_top_level_value_is_evaluated_once() {
    let src = format!(
        "{CHAIN}def c = compact (build 100)
         def result = let c2 = compactAdd c (Link 0 End) in (compactSize c, compactSize c2)"
    );
    // One region, grown by the add: both mentions of `c` are the same value.
    let slots = meadow_core::compact::object_slots;
    let bytes = (slots(false, 0) + 100 * slots(false, 2) + slots(false, 2) + slots(false, 0))
        * meadow_core::compact::SLOT_BYTES;
    let (cek, vm) = (cek_and_vm(&src), format!("({bytes}, {bytes})"));
    assert_eq!(cek, vm);
}

#[test]
fn a_top_level_value_nobody_uses_is_never_evaluated() {
    let src = "def boom = toInt 1 / toInt 0
               def result = 7";
    assert_eq!(agree(src), "7");
}

#[test]
fn a_top_level_value_used_by_many_threads_is_the_same_value_in_each() {
    let src = format!(
        "{CHAIN}def big = build 1000
         def result =
           let a = threadSpawn (\\() -> total big) in
           let b = threadSpawn (\\() -> total big) in
           threadAwait a + threadAwait b + total big"
    );
    assert_eq!(threads_agree(&src), Ok("1501500".into()));
}

/// The CEK machine's answer, required to be the VM's too.
#[track_caller]
fn cek_and_vm(src: &str) -> String {
    let prog = program(src);
    let cek = meadow_eval::run(&prog)
        .unwrap_or_else(|e| panic!("CEK: {}", e.msg))
        .to_string();
    let vm = meadow_glade::run(&image(&prog), FUEL).unwrap_or_else(|e| panic!("VM: {}", e.msg));
    assert_eq!(cek, vm, "CEK vs VM\n{src}");
    cek
}

#[test]
fn spawned_work_spreads_to_idle_workers() {
    // Every thread is spawned by `main`, onto `main`'s worker. The others only
    // get any by stealing -- so with CPU-bound work and idle workers, they must.
    let src = format!(
        "{FIB_INT}use Ts.*\ndata Ts = Done | More (Task Int) Ts
         fun spawnAll (n : Int) acc = if n == 0 then acc else spawnAll (n - 1) (More (threadSpawn (\\() -> fib 22)) acc)
         fun sumAll ts acc = match ts with | Done -> acc | More t rest -> sumAll rest (acc + threadAwait t)
         def result = sumAll (spawnAll 16 Done) 0"
    );
    let prog = program(&src);
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::default());
    let image = meadow_codegen::compile(&lowered.program).unwrap();
    let out = meadow_glade::sched::run_with(&image, image.entry.unwrap(), FUEL, 4);
    assert_eq!(out.result.map_err(|e| e.msg), Ok((17711 * 16).to_string()));
    assert!(out.stats.stolen > 0, "no worker took any work from main's");
}

#[test]
fn a_compact_crosses_threads_without_being_copied() {
    // On the VM the region is shared: the spawned threads read the chain where
    // `main` compacted it. Every engine gives the same answer.
    let src = format!(
        "{CHAIN}def result =
           let c = compact (build 5000) in
           let a = threadSpawn (\\() -> total (getCompact c)) in
           let b = threadSpawn (\\() -> compactSize c) in
           let ch = channelNew () in
           let _ = channelSend ch c in
           let d = channelReceive ch in
           (threadAwait a, threadAwait b == compactSize c, total (getCompact d))"
    );
    assert_eq!(threads_agree(&src), Ok("(12502500, True, 12502500)".into()));
}

#[test]
fn a_compacted_value_sent_to_a_thread_is_not_copied_into_its_heap() {
    // The same chain handed to a thread twice over: plain, it is copied into
    // the thread's heap; compacted, the thread reads the shared region. The
    // difference in what was allocated is the copy.
    let run = |value: &str| {
        let src = format!(
            "{CHAIN}def result =
               let c = {value} in
               threadAwait (threadSpawn (\\() -> total (getCompact c)))"
        );
        let prog = program(&src);
        let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::default());
        let image = meadow_codegen::compile(&lowered.program).unwrap();
        let out = meadow_glade::sched::run_with(&image, image.entry.unwrap(), FUEL, 2);
        assert_eq!(out.result.map_err(|e| e.msg), Ok("50005000".into()));
        out.stats.allocated
    };
    // The second also hands the plain chain to a thread before compacting it,
    // so it differs from the first only by that copy.
    let compacted = run("compact (build 10000)");
    let copied = run(
        "compact (let x = build 10000 in let t = threadSpawn (\\() -> total x) in let _ = threadAwait t in x)",
    );
    assert!(
        copied >= compacted + 30_000,
        "sending the plain chain should have copied 30_000 slots more: {copied} vs {compacted}"
    );
}

// --- software transactional memory ----------------------------------------

// `Std.Stm`'s handlers, as the cases below need them: these programs are
// compiled without the standard library.
const STM: &str = "use Maybe.*\ndata Maybe a = None | Just a
    effect Stm { retrySignal : () -> (), conflictSignal : () -> () }
    use Attempt.*\ndata Attempt a = Done a | Retried | Conflicted
    fun unreachable u = unreachable u
    fun readTVar tv = match stmRead tv with | Just x -> x | None -> let _ = conflictSignal () in unreachable ()
    fun retry u = let _ = retrySignal () in unreachable ()
    fun orElse (first : () -> a ! { Stm }) (second : () -> a ! { Stm }) =
      let _ = stmNest () in
      match handle Done (first ()) with { retrySignal u k -> Retried, conflictSignal u k -> Conflicted } with
      | Done x -> let _ = stmMerge () in x
      | Retried -> let _ = stmRollback () in second ()
      | Conflicted -> let _ = stmRollback () in let _ = conflictSignal () in unreachable ()
    fun atomically (f : () -> a ! { Stm }) =
      let _ = stmBegin () in
      match handle Done (f ()) with { retrySignal u k -> Retried, conflictSignal u k -> Conflicted } with
      | Done x -> if stmCommit () then x else atomically f
      | Retried -> let _ = stmWait () in atomically f
      | Conflicted -> atomically f\n";

#[test]
fn increments_from_many_threads_are_none_of_them_lost() {
    // Sixteen threads each add one a thousand times to one `TVar`. On the VM
    // they run in parallel and collide constantly; every collision is a
    // transaction run again, and the total is still exact.
    let src = format!(
        "{STM}use Ts.*\ndata Ts = End | More (Task ()) Ts
         fun bump tv (k : Int) = if k == 0 then () else
           let _ = atomically (\\() -> stmWrite tv (readTVar tv + 1)) in bump tv (k - 1)
         fun spawnAll tv (n : Int) acc = if n == 0 then acc else spawnAll tv (n - 1) (More (threadSpawn (\\() -> bump tv 1000)) acc)
         fun awaitAll ts = match ts with | End -> () | More t rest -> let _ = threadAwait t in awaitAll rest
         def result =
           let tv = stmNewIO (toInt 0) in
           let _ = awaitAll (spawnAll tv 16 End) in
           atomically (\\() -> readTVar tv)"
    );
    assert_eq!(threads_agree(&src), Ok("16000".into()));
}

#[test]
fn a_transaction_never_sees_half_of_another() {
    // Two `TVar`s whose sum is always 100, moved between by one thread and read
    // together by another. Any read that saw one side updated and not the other
    // would count as a mismatch.
    let src = format!(
        "{STM}fun shuffle a b (k : Int) = if k == 0 then () else
           let _ = atomically (\\() ->
             let x = readTVar a in
             let _ = stmWrite a (x - 1) in
             stmWrite b (readTVar b + 1)) in shuffle a b (k - 1)
         fun watch a b (k : Int) (bad : Int) = if k == 0 then bad else
           let s = atomically (\\() -> readTVar a + readTVar b) in
           watch a b (k - 1) (if s == 100 then bad else bad + 1)
         def result =
           let a = stmNewIO (toInt 100) in
           let b = stmNewIO (toInt 0) in
           let mover = threadSpawn (\\() -> shuffle a b 2000) in
           let watcher = threadSpawn (\\() -> watch a b 2000 0) in
           let _ = threadAwait mover in
           (threadAwait watcher, atomically (\\() -> (readTVar a, readTVar b)))"
    );
    assert_eq!(threads_agree(&src), Ok("(0, (-1900, 2000))".into()));
}

#[test]
fn retry_blocks_until_there_is_something_to_take() {
    // A one-slot mailbox: `take` retries while it is empty, `put` while it is
    // full. The consumer starts first and has to wait for every value.
    let src = format!(
        "{STM}fun take box = atomically (\\() -> match readTVar box with | Just x -> let _ = stmWrite box None in x | None -> retry ())
         fun put box x = atomically (\\() -> match readTVar box with | None -> stmWrite box (Just x) | Just _ -> retry ())
         fun consume box (k : Int) acc = if k == 0 then acc else consume box (k - 1) (acc + take box)
         fun produce box (k : Int) = if k == 0 then () else let _ = put box k in produce box (k - 1)
         def result =
           let box = stmNewIO None in
           let consumer = threadSpawn (\\() -> consume box 500 (toInt 0)) in
           let producer = threadSpawn (\\() -> produce box 500) in
           let _ = threadAwait producer in
           threadAwait consumer"
    );
    assert_eq!(threads_agree(&src), Ok("125250".into()));
}

#[test]
fn or_else_falls_back_and_keeps_only_what_succeeded() {
    let src = format!(
        "{STM}def result =
           let tv = stmNewIO (toInt 1) in
           let got = atomically (\\() ->
             orElse (\\() -> let _ = stmWrite tv 50 in retry ()) (\\() -> let _ = stmWrite tv (readTVar tv + 1) in readTVar tv)) in
           (got, atomically (\\() -> readTVar tv))"
    );
    assert_eq!(threads_agree(&src), Ok("(2, 2)".into()));
}

#[test]
fn a_tvar_holds_only_what_a_compact_can() {
    for (value, what) in [("newRef 1", "a Ref"), ("\\x -> x", "a function")] {
        let src = format!("{STM}def result = let tv = stmNewIO ({value}) in 0");
        assert_eq!(
            threads_agree(&src),
            Err(meadow_core::stm::unstorable(what)),
            "{value}"
        );
    }
}

#[test]
fn a_transaction_operation_outside_atomically_says_so() {
    let src = format!("{STM}def result = let tv = stmNewIO (toInt 1) in stmRead tv");
    assert_eq!(
        threads_agree(&src),
        Err(meadow_core::stm::outside("readTVar"))
    );
}

#[test]
fn waiting_on_a_tvar_nobody_writes_is_a_deadlock() {
    let src = format!(
        "{STM}def result = let tv = stmNewIO (toInt 0) in
           atomically (\\() -> if readTVar tv == 0 then retry () else readTVar tv)"
    );
    assert_eq!(
        threads_agree(&src),
        Err(meadow_core::thread::DEADLOCK.into())
    );
}

// --- cancellation ----------------------------------------------------------------

// A cancelled thread fails at its next thread operation, and only there: see
// `meadow_core::thread`. Most of these cancel a thread that is held up by
// something only `main` can let go of, so it has either not got there yet --
// and fails on arriving -- or is waiting there and is woken to fail: the same
// answer whatever the interleaving.

fn cancelled() -> Result<String, String> {
    Err(meadow_core::thread::CANCELLED.into())
}

/// [`threads_agree`], on the VM and the JIT with one worker only: where the
/// answer depends on a spawned thread not having started before `main`'s next
/// few steps, which is so when nothing else can run it.
#[track_caller]
fn one_worker_agrees(src: &str) -> Result<String, String> {
    let prog = program(src);
    let want = meadow_eval::run(&prog)
        .map(|v| v.to_string())
        .map_err(|e| e.msg);
    for opt in LEVELS {
        let lowered = meadow_seq::lower_program(&prog, opt);
        assert!(lowered.unsupported.is_empty(), "{:?}", lowered.unsupported);
        let image = meadow_codegen::compile(&lowered.program).expect("codegen");
        let got = meadow_glade::sched::run_with(&image, image.entry.unwrap(), FUEL, 1)
            .result
            .map_err(|e| e.msg);
        assert_eq!(want, got, "CEK vs VM at {}\n{src}", opt.name());
        assert_eq!(want, jit(&image, 1), "CEK vs JIT at {}\n{src}", opt.name());
    }
    want
}

#[test]
fn every_thread_operation_fails_in_a_cancelled_thread() {
    // Cancelled before it starts, so the operation is the first it makes.
    for op in [
        "threadYield ()",
        "channelNew ()",
        "channelSend ch (toInt 1)",
        "channelReceive ch",
        "threadSpawn (\\() -> toInt 0)",
        "threadAwait other",
        "threadCancel other",
    ] {
        let src = format!(
            "def result =
               let ch = channelNew () in
               let other = threadSpawn (\\() -> toInt 0) in
               let t = threadSpawn (\\() -> let _ = {op} in toInt 1) in
               let _ = threadCancel t in
               threadAwait t"
        );
        assert_eq!(one_worker_agrees(&src), cancelled(), "{op}");
    }
}

#[test]
fn a_cancelled_receiver_is_woken_to_fail() {
    let src = format!(
        "{FIB_INT}def result =
           let ch = channelNew () in
           let t = threadSpawn (\\() -> let x = fib 15 in channelReceive ch + x) in
           let _ = threadCancel t in
           let _ = channelSend ch (toInt 1) in
           threadAwait t"
    );
    assert_eq!(threads_agree(&src), cancelled());
    // Nothing is ever sent: unless the cancel wakes it, this is a deadlock.
    let src = "def result =
                 let ch = channelNew () in
                 let t = threadSpawn (\\() -> channelReceive ch + toInt 1) in
                 let _ = threadYield () in
                 let _ = threadCancel t in
                 threadAwait t";
    assert_eq!(threads_agree(src), cancelled());
}

#[test]
fn a_thread_cancelled_while_it_awaits_another_is_woken_to_fail() {
    // `blocker` never finishes: only the cancel can end `waiter`'s wait.
    let src = "def result =
                 let gate = channelNew () in
                 let blocker = threadSpawn (\\() -> channelReceive gate) in
                 let waiter = threadSpawn (\\() -> threadAwait blocker + toInt 1) in
                 let _ = threadYield () in
                 let _ = threadCancel waiter in
                 threadAwait waiter";
    assert_eq!(threads_agree(src), cancelled());
    // And when it does finish, after the cancel, `waiter` still failed: its
    // wait was over before there was an answer to take.
    let src = "def result =
                 let gate = channelNew () in
                 let blocker = threadSpawn (\\() -> channelReceive gate) in
                 let waiter = threadSpawn (\\() -> threadAwait blocker + toInt 1) in
                 let _ = threadCancel waiter in
                 let _ = channelSend gate (toInt 41) in
                 (threadAwait blocker, threadAwait waiter)";
    assert_eq!(threads_agree(src), cancelled());
}

#[test]
fn a_thread_cancelled_in_retry_is_woken_to_fail() {
    let src = format!(
        "{STM}def result =
           let box = stmNewIO (toInt 0) in
           let t = threadSpawn (\\() -> atomically (\\() -> if readTVar box == 0 then retry () else readTVar box)) in
           let _ = threadYield () in
           let _ = threadCancel t in
           threadAwait t"
    );
    assert_eq!(threads_agree(&src), cancelled());
    // A commit after the cancel finds nobody waiting to wake -- and one the
    // thread sees before it ever waits sends it to wait all the same, which
    // is where it fails. A transaction that does not wait is not a place a
    // thread is stopped.
    let src = format!(
        "{STM}def result =
           let box = stmNewIO (toInt 0) in
           let t = threadSpawn (\\() -> atomically (\\() -> if readTVar box < 10 then retry () else readTVar box)) in
           let _ = threadCancel t in
           let _ = atomically (\\() -> stmWrite box 5) in
           (atomically (\\() -> readTVar box), threadAwait t)"
    );
    assert_eq!(threads_agree(&src), cancelled());
}

#[test]
fn cancelling_a_finished_thread_changes_nothing() {
    let src = "def result =
                 let t = threadSpawn (\\() -> toInt 7) in
                 let a = threadAwait t in
                 let _ = threadCancel t in
                 let _ = threadCancel t in
                 (a, threadAwait t)";
    assert_eq!(threads_agree(src), Ok("(7, 7)".into()));
    // A failure stays the failure it was.
    let src = "def result =
                 let t = threadSpawn (\\() -> toInt 1 / toInt 0) in
                 let gate = channelNew () in
                 let w = threadSpawn (\\() -> let _ = threadAwait t in channelReceive gate) in
                 let _ = threadYield () in
                 let _ = threadCancel t in
                 threadAwait t";
    assert_eq!(threads_agree(src), Err("division by zero".into()));
}

#[test]
fn cancelling_twice_is_cancelling_once() {
    let src = "def result =
                 let ch = channelNew () in
                 let t = threadSpawn (\\() -> channelReceive ch) in
                 let _ = threadCancel t in
                 let _ = threadCancel t in
                 let _ = channelSend ch (toInt 1) in
                 let _ = threadCancel t in
                 threadAwait t";
    assert_eq!(threads_agree(src), cancelled());
}

#[test]
fn a_thread_in_a_pure_loop_is_ended_at_a_preemption_point() {
    // No operation in it, so nowhere to stop softly; it is preempted, and
    // the second time after the cancel it is ended there.
    let src = "fun spin (n : Int) : Int = spin (n + 1)
               def result =
                 let t = threadSpawn (\\() -> spin 0) in
                 let _ = threadCancel t in
                 threadAwait t";
    assert_eq!(threads_agree(src), cancelled());
    // Under a handler it never performs, the same.
    let src = "effect Ask { ask : () -> Int }
               fun spin (n : Int) : Int = spin (n + 1)
               def result =
                 let t = threadSpawn (\\() -> handle spin 0 with { ask u k -> k 1 }) in
                 let _ = threadYield () in
                 let _ = threadCancel t in
                 threadAwait t";
    assert_eq!(threads_agree(src), cancelled());
}

#[test]
fn a_thread_can_ask_whether_it_was_cancelled() {
    // It asks until it is, then returns: that is its answer, not a failure.
    let src = "fun spin (n : Int) = if threadCancelled () then n else spin (n + 1)
               def result =
                 let t = threadSpawn (\\() -> spin 0) in
                 let _ = threadCancel t in
                 threadAwait t >= 0";
    assert_eq!(threads_agree(src), Ok("True".into()));
    // Nobody is, unasked -- `main` included.
    let src =
        "def result = (threadCancelled (), threadAwait (threadSpawn (\\() -> threadCancelled ())))";
    assert_eq!(threads_agree(src), Ok("(False, False)".into()));
    // Asking does not use the cancel up: the next operation still fails.
    let src = "fun spin (n : Int) = if threadCancelled () then let _ = threadYield () in n else spin (n + 1)
               def result =
                 let t = threadSpawn (\\() -> spin 0) in
                 let _ = threadCancel t in
                 threadAwait t";
    assert_eq!(threads_agree(src), cancelled());
}

#[test]
fn a_thread_may_cancel_itself() {
    let src = "def result =
                 let ch = channelNew () in
                 let t = threadSpawn (\\() ->
                   let me = channelReceive ch in
                   let _ = threadCancel me in
                   threadCancelled ()) in
                 let _ = channelSend ch t in
                 threadAwait t";
    assert_eq!(threads_agree(src), Ok("True".into()));
}

#[test]
fn no_handler_in_the_thread_catches_the_cancel() {
    // An exception handler around the work, and a handler whose operation the
    // thread was cancelled under: the thread fails through both, and the
    // `return` clause never runs.
    let src = "effect Ask { ask : () -> Int }
               effect Exn { throw : Int -> () }
               def result =
                 let ch = channelNew () in
                 let t = threadSpawn (\\() ->
                   handle
                     (handle (let x = channelReceive ch in x + ask ())
                      with { ask u k -> k 1 + 100, return r -> r * 1000 })
                   with { throw e k -> 0 - 1 }) in
                 let _ = threadCancel t in
                 let _ = channelSend ch (toInt 41) in
                 threadAwait t";
    assert_eq!(threads_agree(src), cancelled());
    // Cancelled while a handler's clause waits.
    let src = "effect Ask { ask : () -> Int }
               def result =
                 let ch = channelNew () in
                 let t = threadSpawn (\\() ->
                   handle (ask () + 1) with { ask u k -> k (channelReceive ch) }) in
                 let _ = threadCancel t in
                 let _ = channelSend ch (toInt 41) in
                 threadAwait t";
    assert_eq!(threads_agree(src), cancelled());
}

#[test]
fn many_threads_cancelled_at_once_all_let_go() {
    // Two hundred readers, waiting or not yet started, all cancelled. If any
    // were still among the channel's receivers, the message would go to it
    // and `main` would wait for ever.
    let src = "use Ts.*\ndata Ts = End | More (Task Int) Ts
               fun readers gate (n : Int) acc = if n == 0 then acc else
                 readers gate (n - 1) (More (threadSpawn (\\() -> channelReceive gate + n)) acc)
               fun cancelAll ts = match ts with | End -> () | More t rest -> let _ = threadCancel t in cancelAll rest
               def result =
                 let gate = channelNew () in
                 let ts = readers gate 200 End in
                 let _ = threadYield () in
                 let _ = cancelAll ts in
                 let _ = channelSend gate (toInt 42) in
                 channelReceive gate";
    assert_eq!(threads_agree(src), Ok("42".into()));
    // Each of them fails, and `await` says so.
    let src = "use Ts.*\ndata Ts = End | More (Task Int) Ts
               fun readers gate (n : Int) acc = if n == 0 then acc else
                 readers gate (n - 1) (More (threadSpawn (\\() -> channelReceive gate + n)) acc)
               fun cancelAll ts = match ts with | End -> () | More t rest -> let _ = threadCancel t in cancelAll rest
               fun sumAll ts acc = match ts with | End -> acc | More t rest -> sumAll rest (acc + threadAwait t)
               def result =
                 let gate = channelNew () in
                 let ts = readers gate 200 End in
                 let _ = cancelAll ts in
                 sumAll ts 0";
    assert_eq!(threads_agree(src), cancelled());
}

// --- typed instructions --------------------------------------------------------

// Operands the compiler knows are both `Int`, or both `Float`, run as typed
// instructions on words rather than as primitives. They have to mean exactly
// what the primitives do, edges included.

#[test]
fn typed_int_arithmetic_wraps_and_compares_as_the_primitives_do() {
    let src = "fun f (a : Int) (b : Int) =
                 (a + b, a - b, a * b, a / b, a % b, a < b, a >= b, a == b, a != b)
               fun g (a : Int) = (a + 5000000000, a * 3, a - 1, a < 2, a == 7, a > 3000000000)
               def result = (f 9223372036854775807 3, f (0 - 7) 2, g 7, g 9223372036854775807)";
    assert_eq!(
        agree(src),
        "((-9223372036854775806, 9223372036854775804, 9223372036854775805, 3074457345618258602, 1, False, True, False, True), \
         (-5, -9, -14, -3, -1, True, False, False, True), \
         (5000000007, 21, 6, False, True, False), \
         (-9223372031854775809, 9223372036854775805, 9223372036854775806, False, False, True))"
    );
}

#[test]
fn typed_division_by_zero_fails_the_same_way_everywhere() {
    for (op, what) in [("/", "division by zero"), ("%", "modulo by zero")] {
        let prog = program(&format!(
            "fun f (a : Int) (b : Int) = a {op} b\ndef result = f 1 0"
        ));
        let cek = meadow_eval::run(&prog).expect_err("CEK should fail");
        let vm = meadow_glade::run(&image(&prog), FUEL).expect_err("the VM should fail");
        assert!(cek.msg.contains(what), "{}", cek.msg);
        assert_eq!(cek.msg, vm.msg);
    }
}

#[test]
fn typed_float_arithmetic_is_ieee() {
    let src = "fun f (a : Float) (b : Float) =
                 (_primAddF a b, _primSubF a b, _primMulF a b, _primDivF a b, _primLtF a b, _primGeF a b, a == b, a != b)
               def result = let nan = _primDivF 0.0 0.0 in (f 1.5 0.25, f nan nan, f 0.0 (_primSubF 0.0 0.0))";
    let out = agree(src);
    assert!(
        out.starts_with("((1.75, 1.25, 0.375, 6.0, False, True, False, True)"),
        "{out}"
    );
}

#[test]
fn typed_literal_patterns_and_equality_on_immediates() {
    let src = "fun c x = let _ = x == 'q' in match x with | 'a' -> 1 | 'λ' -> 2 | _ -> 0
               fun s x = let _ = x == \"q\" in match x with | \"hi\" -> 1 | _ -> 0
               fun b x = if x == (1 < 2) then 1 else 0
               def result = (c 'a', c 'λ', c 'z', s \"hi\", s \"ho\", b (1 < 2), b (2 < 1), \"x\" == \"x\")";
    assert_eq!(agree(src), "(1, 2, 0, 1, 0, 1, 0, True)");
}

// --- release specialization -----------------------------------------------------

#[test]
fn a_release_build_copies_generic_code_once_per_representation() {
    // `id` is used at four types and two representations: `Int`, and three
    // that are references and share a copy -- a `String` among them, since a
    // string is an object on the heap.
    let src = "use L.*\ndata L = Nil | Cons Int L
               fun id x = x
               def result = (id (toInt 1), id \"s\", id (Cons 1 Nil), id (id 2, Nil))";
    let prog = program(src);
    let id = prog
        .defs
        .iter()
        .find(|d| &*d.name == "id")
        .map(|d| d.var)
        .expect("an `id`");
    let released = meadow_core::specialize::release(&prog);
    let copies: Vec<&meadow_core::Def> = released
        .defs
        .iter()
        .filter(|d| &*d.name == "id" && d.var != id)
        .collect();
    let mut at: Vec<String> = copies.iter().map(|d| format!("{:?}", d.poly.ty)).collect();
    at.sort();
    assert_eq!(copies.len(), 2, "{at:?}");
    for d in &copies {
        assert!(
            d.poly
                .binders
                .iter()
                .all(|b| b.kind != meadow_compiler::infer::VarKind::Type),
            "a copy abstracts over no type: {:?}",
            d.poly
        );
    }
    assert!(
        at.iter().any(|t| t.contains(meadow_core::specialize::REF)),
        "{at:?}"
    );
    // And it means what it did.
    for opt in LEVELS {
        let lowered = meadow_seq::lower_program(&prog, opt);
        let image = meadow_codegen::compile(&lowered.program).expect("codegen");
        assert_eq!(
            meadow_glade::run(&image, FUEL).map_err(|e| e.msg),
            Ok("(1, \"s\", Cons(1, Nil), (2, Nil))".into())
        );
    }
}

// --- tail recursion modulo cons -----------------------------------------------
//
// `core::trmc` builds a constructor before the recursive call that fills one of
// its fields, and writes the field in place. Every case below is checked on
// every machine at every level; O0 is the one without the rewrite.

/// How many destination-passing twins the pass makes for `src`.
fn twins(src: &str) -> usize {
    let prog = program(src);
    let after = core::trmc::program(&prog, meadow_core::OptLevel::O1);
    after.defs.len() - prog.defs.len()
}

const LIST: &str = "use L.*\ndata L = E | C Int L\n";

#[test]
fn a_map_is_built_front_to_back() {
    let src = format!(
        "{LIST}fun map f xs = match xs with | E -> E | C x r -> C (f x) (map f r)
         fun upto i n = if i > n then E else C i (upto (i + 1) n)
         fun sum acc xs = match xs with | E -> acc | C x r -> sum (acc + x) r
         def result = (sum 0 (map (\\x -> x * 2) (upto 1 100000)), map (\\x -> x + 1) (upto 1 3))"
    );
    assert_eq!(twins(&src), 2, "`map` and `upto`");
    assert_eq!(agree(&src), "(10000100000, C(2, C(3, C(4, E))))");
}

#[test]
fn a_filter_skips_by_calling_itself_in_tail_position() {
    // One arm builds a cell, one passes the same destination on, one ends.
    let src = format!(
        "{LIST}fun filter p xs = match xs with
           | E -> E
           | C x r -> if p x then C x (filter p r) else filter p r
         def result = filter (\\x -> x % 2 == 0) (C 1 (C 2 (C 3 (C 4 (C 5 (C 6 E))))))"
    );
    assert_eq!(twins(&src), 1);
    assert_eq!(agree(&src), "C(2, C(4, C(6, E)))");
}

#[test]
fn an_append_ends_with_a_list_it_did_not_build() {
    let src = format!(
        "{LIST}fun append xs ys = match xs with | E -> ys | C x r -> C x (append r ys)
         def shared = C 9 E
         def result = (append (C 1 (C 2 E)) shared, append E shared, shared)"
    );
    assert_eq!(twins(&src), 1);
    assert_eq!(agree(&src), "(C(1, C(2, C(9, E))), C(9, E), C(9, E))");
}

#[test]
fn a_tree_fills_its_last_field_and_recurses_for_the_others() {
    let src = "use T.*\ndata T = Leaf | Node T Int T
               fun mirror t = match t with
                 | Leaf -> Leaf
                 | Node l v r -> Node (mirror r) v (mirror l)
               def result = mirror (Node (Node Leaf 1 Leaf) 2 (Node Leaf 3 (Node Leaf 4 Leaf)))";
    assert_eq!(twins(src), 1);
    assert_eq!(
        agree(src),
        "Node(Node(Node(Leaf, 4, Leaf), 3, Leaf), 2, Node(Leaf, 1, Leaf))"
    );
}

#[test]
fn work_after_the_call_is_not_reordered() {
    // `f x` after the recursive call is not a value: running it first would
    // print in the wrong order, so the pass leaves this alone.
    let src = format!(
        "{LIST}data P = P L Int
         fun go f xs = match xs with | E -> P E 0 | C x r -> P (step f r) (f x)
         fun step f xs = match go f xs with | P l _ -> l
         def result = go (\\x -> x) (C 1 (C 2 E))"
    );
    assert_eq!(twins(&src), 0);
    agree(&src);
}

#[test]
fn a_type_with_no_nullary_constructor_is_left_alone() {
    let src = "use S.*\ndata S = S Int S | End Int
               fun bump s = match s with | End n -> End (n + 1) | S n r -> S (n + 1) (bump r)
               def result = bump (S 1 (S 2 (End 3)))";
    assert_eq!(
        twins(src),
        0,
        "`End` has a field, so there is no placeholder"
    );
    assert_eq!(agree(src), "S(2, S(3, End(4)))");
}

#[test]
fn an_effect_performed_mid_list_resumes_into_the_half_built_cell() {
    let src = format!(
        "{LIST}effect Ask {{ ask : () -> Int }}
         fun tag xs = match xs with | E -> E | C x r -> C (x + ask ()) (tag r)
         def result =
           handle tag (C 1 (C 2 (C 3 E))) with {{
             ask u k -> k 10
           }}"
    );
    assert_eq!(twins(&src), 1);
    assert_eq!(agree(&src), "C(11, C(12, C(13, E)))");
}

// --- more values live than there are registers ------------------------------------
//
// An operand names one of 255 registers. What does not fit is spilled into a
// heap block and reloaded where it is read (see `meadow_codegen`, "Spilling"),
// and an array longer than one instruction builds is built in pieces. Each of
// these failed to compile before, with "the allocator does not spill yet".

/// `let x0 = f 0 in ... let x{n-1} = f (n-1) in`, each a call.
fn many_lets(n: usize) -> String {
    (0..n).map(|i| format!("  let x{i} = f {i} in\n")).collect()
}

#[test]
fn an_array_literal_longer_than_the_register_file() {
    let items: Vec<String> = (1..=800).map(|i| i.to_string()).collect();
    let src = format!(
        "def result = let a = #[{}] in (arrayLen a, arrayGet a 0, arrayGet a 399, arrayGet a 799)",
        items.join(", ")
    );
    assert_eq!(agree(&src), "(800, 1, 400, 800)");
}

#[test]
fn an_array_literal_of_computed_elements_keeps_their_order() {
    // Elements that are calls, each live until its piece is built.
    let items: Vec<String> = (0..300).map(|i| format!("f {i}")).collect();
    let src = format!(
        "fun f (i : Int) : Int = i * 3\n\
         fun sum a (i : Int) (acc : Int) : Int = if i == arrayLen a then acc else sum a (i + 1) (acc + arrayGet a i * (i + 1))\n\
         def result = sum #[{}] 0 0",
        items.join(", ")
    );
    let want: i64 = (0..300i64).map(|i| i * 3 * (i + 1)).sum();
    assert_eq!(agree(&src), want.to_string());
}

#[test]
fn a_list_literal_longer_than_the_register_file() {
    let items: Vec<String> = (1..=600).map(|i| i.to_string()).collect();
    let src = format!(
        "fun sum xs = match xs with | [;] -> 0 | x :: rest -> x + sum rest\n\
         def result = sum [{}]",
        items.join("; ")
    );
    assert_eq!(agree(&src), (600 * 601 / 2).to_string());
}

#[test]
fn three_hundred_values_live_across_calls() {
    // Every `f i` is a non-tail call, whose continuation captures everything
    // still live: spill blocks, rather than three hundred values.
    let sum: Vec<String> = (0..300).map(|i| format!("x{i}")).collect();
    let src = format!(
        "fun f (i : Int) : Int = i * 2 + 1\n\
         def result =\n{}  {}",
        many_lets(300),
        sum.join(" + ")
    );
    assert_eq!(agree(&src), "90000");
}

#[test]
fn spilled_values_of_a_type_variable_keep_their_descriptors() {
    // A value of type `a` is read through its descriptor, which is never
    // spilled; the values themselves are.
    let items: Vec<String> = (0..280).map(|i| format!("x{i}")).collect();
    let lets: String = (0..280)
        .map(|i| format!("  let x{i} = id2 a in\n"))
        .collect();
    let src = format!(
        "fun id2 x = x\n\
         fun many a =\n{lets}  #[{}]\n\
         def result = (arrayLen (many \"s\"), arrayGet (many (1, 2)) 279)",
        items.join(", ")
    );
    assert_eq!(agree(&src), "(280, (1, 2))");
}

#[test]
fn spilled_values_reach_branches_matches_closures_and_handlers() {
    let evens: Vec<String> = (0..300).step_by(2).map(|i| format!("x{i}")).collect();
    let odds: Vec<String> = (1..300).step_by(2).map(|i| format!("x{i}")).collect();
    let src = format!(
        "use Opt.*\ndata Opt a = None | Some a\n\
         effect Ask {{ ask : Int -> Int }}\n\
         fun f (i : Int) : Int = i * 2 + 1\n\
         fun pick (n : Int) = if n > 100 then Some n else None\n\
         fun body u =\n{}\
         \x20 let a = if x7 > x3 then {} else 0 in\n\
         \x20 let b = match pick x250 with | Some v -> v + x1 | None -> x2 in\n\
         \x20 let g = \\y -> x0 + x299 + y in\n\
         \x20 let c = g x150 + ask x10 in\n\
         \x20 a + b + c + {}\n\
         def result = handle body 0 with {{ ask n k -> k (n * 1000) }}",
        many_lets(300),
        evens.join(" + "),
        odds.join(" + ")
    );
    assert_eq!(agree(&src), "112405");
}

#[test]
fn spilled_references_survive_collections() {
    // Lists held only in spill blocks while a loop allocates enough to collect
    // every few slots, the heap verified at each collection.
    use meadow_glade::heap::{Collector, GcConfig, Heap};
    let lengths: Vec<String> = (0..300).map(|i| format!("len x{i}")).collect();
    let lets: String = (0..300)
        .map(|i| format!("  let x{i} = build {} in\n", i % 7 + 1))
        .collect();
    let src = format!(
        "fun build (n : Int) = if n == 0 then [;] else n :: build (n - 1)\n\
         fun len xs = match xs with | [;] -> 0 | x :: rest -> 1 + len rest\n\
         fun churn (n : Int) (acc : Int) : Int = if n == 0 then acc else churn (n - 1) (acc + len (build 20))\n\
         def result =\n{lets}  let c = churn 2000 0 in\n  (c, {})",
        lengths.join(" + ")
    );
    assert_eq!(agree(&src), "(40000, 1197)");
    let prog = program(&src);
    let img = image(&prog);
    for collector in [Collector::Generational, Collector::Copying] {
        let config = GcConfig {
            collector,
            nursery: 64,
            min_trigger: 64,
            verify: true,
            ..GcConfig::from_env()
        };
        let mut vm = meadow_glade::Vm::with_heap(&img, Heap::with_config(1 << 12, config));
        let v = vm
            .run(img.entry.unwrap(), u64::MAX)
            .unwrap_or_else(|e| panic!("{collector:?}: {}", e.msg));
        assert_eq!(vm.show(v), "(40000, 1197)", "{collector:?}");
        assert!(vm.heap().collections > 10, "{collector:?} should collect");
    }
}

// --- wider than one instruction --------------------------------------------------
//
// A constructor, a record, or a call's arguments more than one instruction can
// name at once: built blank and filled, extended a field at a time, or handed
// over with the rest packed (see `meadow_codegen`, "Spilling").

fn ints(n: usize, sep: &str) -> String {
    (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(sep)
}

fn names(n: usize, sep: &str) -> String {
    (0..n)
        .map(|i| format!("x{i}"))
        .collect::<Vec<_>>()
        .join(sep)
}

#[test]
fn a_function_of_three_hundred_parameters() {
    let params: String = (0..300).map(|i| format!("(x{i} : Int) ")).collect();
    let src = format!(
        "fun f {params}: Int = {}\n\
         def result = f {}",
        names(300, " + "),
        ints(300, " ")
    );
    assert_eq!(agree(&src), "44850");
}

#[test]
fn a_function_of_three_hundred_parameters_that_loops() {
    // Each turn jumps back into itself with three hundred values.
    let params: String = (0..300).map(|i| format!("(x{i} : Int) ")).collect();
    let next: String = (0..300).map(|i| format!("(x{i} + 1) ")).collect();
    let src = format!(
        "fun f (n : Int) {params}: Int = if n == 0 then x0 + x150 + x299 else f (n - 1) {next}\n\
         def result = f 10 {}",
        ints(300, " ")
    );
    assert_eq!(agree(&src), (10 + 160 + 309).to_string());
}

#[test]
fn a_lambda_of_three_hundred_parameters_invoked() {
    // Called through a value, so an `invoke` rather than a `jump`: the method
    // unpacks by the same rule the call packs by.
    let lam = format!("\\{} -> x0 + x299 * 2", names(300, " "));
    let src = format!(
        "fun call g = g {}\n\
         def result = call ({lam})",
        ints(300, " ")
    );
    assert_eq!(agree(&src), "598");
}

#[test]
fn a_constructor_of_three_hundred_fields() {
    let fields: String = (0..300).map(|_| "Int ").collect();
    let src = format!(
        "use Big.*\ndata Big = Big {fields}\n\
         fun total b = match b with | Big {} -> {}\n\
         def result = total (Big {})",
        names(300, " "),
        names(300, " + "),
        ints(300, " ")
    );
    assert_eq!(agree(&src), "44850");
}

#[test]
fn a_wide_constructor_holding_references_survives_collections() {
    // Fields that are lists, filled one at a time into a block that may have
    // been promoted by then.
    let fields: String = (0..260).map(|_| "L ").collect();
    let builds: String = (0..260)
        .map(|i| format!("(build {}) ", i % 5 + 1))
        .collect();
    let src = format!(
        "use L.*\ndata L = N | C Int L\n\
         use Big.*\ndata Big = Big {fields}\n\
         fun build (n : Int) = if n == 0 then N else C n (build (n - 1))\n\
         fun len xs = match xs with | N -> 0 | C _ r -> 1 + len r\n\
         fun total b = match b with | Big {} -> {}\n\
         def result = total (Big {builds})",
        names(260, " "),
        (0..260)
            .map(|i| format!("len x{i}"))
            .collect::<Vec<_>>()
            .join(" + "),
    );
    let want: usize = (0..260).map(|i| i % 5 + 1).sum();
    assert_eq!(agree(&src), want.to_string());
}

#[test]
fn a_record_of_three_hundred_fields() {
    let fields: String = (0..300)
        .map(|i| format!("f{i} = {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let src = format!("def result = let r = {{ {fields} }} in r.f0 + r.f150 + r.f299");
    assert_eq!(agree(&src), "449");
}
