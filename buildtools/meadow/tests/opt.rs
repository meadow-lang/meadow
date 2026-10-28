//! What the optional and the unconditional back-end passes must not change.
//!
//! Every case here runs the same program on the CEK machine and on the VM at
//! each optimization level, and requires one answer from all four. That shape is
//! the point: these passes rewrite *operands* and *control flow*, and the way
//! they go wrong is a program that still runs and computes something else.
//!
//! The three that are always on:
//!
//! * a literal operand folded into the instruction that uses it — which must not
//!   quietly swap the operands of something that cares which side they are on;
//! * a comparison fused into the branch testing it — which must not invert;
//! * arguments read where they already sit instead of being copied into a fresh
//!   window — which must not read the wrong registers.
//!
//! And the one `-O2` turns on: a `match` compiled to a decision tree, which must
//! pick the same arm the chain would have, including when the arms overlap.

use meadow::{Engine, OptLevel, Options, pipeline, runtime};

/// Every level's answer, and the CEK's, when they agree — and a panic naming
/// the culprit when they do not.
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

    for opt in [OptLevel::O0, OptLevel::O1, OptLevel::O2] {
        let vm = runtime::run(&program, Engine::Vm, opt)
            .unwrap_or_else(|e| panic!("the VM at {} failed on\n{src}\n{e}", opt.name()));
        assert_eq!(
            vm,
            cek,
            "the VM at {} disagrees with the CEK machine on\n{src}",
            opt.name()
        );
    }
    cek
}

/// `agreed`, and the answer is what it should be — so a test cannot pass by
/// every machine being wrong in the same way.
fn is(src: &str, expected: &str) {
    assert_eq!(agreed(src), expected, "{src}");
}

/// An expression, evaluated against `Std`.
fn expr_is(expr: &str, expected: &str) {
    is(&format!("def result = {expr}\n"), expected);
}

// --- folding a literal operand ------------------------------------------------

/// The operand that folds is the **right** one, and only an operation that does
/// not care may be given its literal from the left.
///
/// This is the whole risk of the pass in one test. `1 - n` and `n - 1` are
/// different numbers, and the folded instruction has exactly one slot for a
/// constant — so a fold that reaches for a left-hand literal without checking
/// turns one into the other, silently, in every program that counts down.
#[test]
fn a_folded_constant_keeps_the_side_it_was_written_on() {
    expr_is("10 - 3", "7");
    expr_is("3 - 10", "-7");
    expr_is("10 / 3", "3");
    expr_is("3 / 10", "0");
    expr_is("10 % 3", "1");
    expr_is("3 % 10", "3");
    expr_is("2 ^ 5", "32");
    expr_is("5 ^ 2", "25");

    // Through a variable, which is the shape that actually folds: one operand
    // in a register and one in the instruction.
    is("def n = 10\ndef result = n - 3\n", "7");
    is("def n = 10\ndef result = 3 - n\n", "-7");
    is("def n = 10\ndef result = n / 3\n", "3");
    is("def n = 10\ndef result = 3 / n\n", "0");
    is("def n = 10\ndef result = n % 3\n", "1");
    is("def n = 10\ndef result = 3 % n\n", "3");
    is("def n = 3\ndef result = n ^ 2\n", "9");
    is("def n = 3\ndef result = 2 ^ n\n", "8");

    // Bit shifts are the other asymmetric pair.
    is("def n = 8\ndef result = shl n 2\n", "32");
    is("def n = 8\ndef result = shr n 2\n", "2");
    is("def n = 2\ndef result = shl 8 n\n", "32");
}

/// The commutative ones may take their literal from either side, and must come
/// out the same when they do.
#[test]
fn a_commutative_operation_folds_from_either_side() {
    is("def n = 10\ndef result = n + 3\n", "13");
    is("def n = 10\ndef result = 3 + n\n", "13");
    is("def n = 10\ndef result = n * 3\n", "30");
    is("def n = 10\ndef result = 3 * n\n", "30");
    is("def n = 10\ndef result = n == 10\n", "True");
    is("def n = 10\ndef result = 10 == n\n", "True");
    is("def n = 10\ndef result = n != 10\n", "False");
    is("def n = 10\ndef result = 10 != n\n", "False");
}

/// Every literal type the constant table holds, on the folded path.
#[test]
fn a_folded_constant_can_be_any_literal() {
    is("def c = 'a'\ndef result = c == 'a'\n", "True");
    is("def s = \"hi\"\ndef result = s == \"hi\"\n", "True");
    is("def s = \"hi\"\ndef result = s == \"ho\"\n", "False");
    is("def x = 1.5\ndef result = x +. 0.25\n", "1.75");
    is("def x = 1.5\ndef result = x -. 0.25\n", "1.25");
    is("def x = 0.25\ndef result = 1.5 -. x\n", "1.25");
    is("def b = True\ndef result = b == True\n", "True");
    // `()` compares equal to itself and nothing else can be written.
    is("def u = ()\ndef result = u == ()\n", "True");
}

// --- fusing a comparison into its branch --------------------------------------

/// A fused comparison must not change sense, and `<` against a literal is the
/// case where getting the operands backwards still type-checks and still runs.
#[test]
fn a_fused_comparison_does_not_invert() {
    for (n, expect) in [(1, "\"below\""), (5, "\"equal\""), (9, "\"above\"")] {
        is(
            &format!(
                "def n = {n}\n\
                 def result = if n < 5 then \"below\" else if n > 5 then \"above\" else \"equal\"\n"
            ),
            expect,
        );
        // The same tests written with the literal on the left.
        is(
            &format!(
                "def n = {n}\n\
                 def result = if 5 > n then \"below\" else if 5 < n then \"above\" else \"equal\"\n"
            ),
            expect,
        );
        // And the inclusive forms, where an off-by-one would show at 5.
        is(
            &format!(
                "def n = {n}\n\
                 def result = if n <= 5 then \"le\" else \"gt\"\n"
            ),
            if n <= 5 { "\"le\"" } else { "\"gt\"" },
        );
        is(
            &format!(
                "def n = {n}\n\
                 def result = if n >= 5 then \"ge\" else \"lt\"\n"
            ),
            if n >= 5 { "\"ge\"" } else { "\"lt\"" },
        );
    }
}

/// Two registers rather than a register and a literal — the other fused form.
#[test]
fn a_fused_comparison_of_two_variables() {
    is(
        "def a = 3\ndef b = 7\ndef result = if a < b then \"lt\" else \"ge\"\n",
        "\"lt\"",
    );
    is(
        "def a = 7\ndef b = 3\ndef result = if a < b then \"lt\" else \"ge\"\n",
        "\"ge\"",
    );
    is(
        "def a = 3\ndef b = 3\ndef result = if a < b then \"lt\" else \"ge\"\n",
        "\"ge\"",
    );
    is(
        "def a = 3\ndef b = 3\ndef result = if a == b then \"eq\" else \"ne\"\n",
        "\"eq\"",
    );
}

/// A condition that is not a comparison at all still has to work: `and` is not a
/// primitive, a call is not fusable, and a plain `Bool` has nothing to fuse.
#[test]
fn a_condition_that_cannot_fuse_still_branches() {
    is("def b = True\ndef result = if b then 1 else 2\n", "1");
    is(
        "def n = 4\ndef result = if n > 0 and n < 10 then 1 else 2\n",
        "1",
    );
    is(
        "def n = 40\ndef result = if n > 0 and n < 10 then 1 else 2\n",
        "2",
    );
    is(
        "def n = 4\ndef result = if not (n == 4) then 1 else 2\n",
        "2",
    );
}

/// Floats and `BigInt`s compare too, and their comparisons are on the fused
/// list — which the runtime relies on for a reason that has nothing to do with
/// the answer: it keeps the result in a register the collector does not scan, so
/// a fused primitive must not allocate.
#[test]
fn fusing_covers_the_other_numeric_types() {
    is(
        "def x = 1.5\ndef result = if x <. 2.0 then \"lt\" else \"ge\"\n",
        "\"lt\"",
    );
    is(
        "def x = 2.5\ndef result = if x <. 2.0 then \"lt\" else \"ge\"\n",
        "\"ge\"",
    );
    is(
        "def x = 2.0\ndef result = if x <=. 2.0 then \"le\" else \"gt\"\n",
        "\"le\"",
    );
    is(
        "def x = toBigInt 5\ndef result = if x < toBigInt 9 then \"lt\" else \"ge\"\n",
        "\"lt\"",
    );
    is(
        "def x = toBigInt 50\ndef result = if x < toBigInt 9 then \"lt\" else \"ge\"\n",
        "\"ge\"",
    );
    is(
        "def x = toUInt8 200\ndef result = if x > 100 then \"gt\" else \"le\"\n",
        "\"gt\"",
    );
    is(
        "def x = toInt8 (0 - 1)\ndef result = if x < 0 then \"lt\" else \"ge\"\n",
        "\"lt\"",
    );
    is(
        "def x = toUInt64 (toInt (0 - 1))\ndef result = if x > 0 then \"gt\" else \"le\"\n",
        "\"gt\"",
    );
}

// --- reading arguments where they already are ---------------------------------

/// Closures capture, and calls pass, whole stretches of the environment — which
/// is exactly the case the window optimisation reads in place. Getting the base
/// wrong shifts every captured value by one, which a closure over several
/// variables would notice and a closure over one would not.
#[test]
fn a_closure_over_many_values_captures_them_in_order() {
    is(
        "def result =\n\
         \x20 let a = 1 in let b = 2 in let c = 3 in let d = 4 in let e = 5 in\n\
         \x20 let f = \\u -> a * 10000 + b * 1000 + c * 100 + d * 10 + e in\n\
         \x20 f ()\n",
        "12345",
    );
}

/// The same for a call: five arguments, each distinguishable from its
/// neighbours, so a window read one register off is a different number.
#[test]
fn arguments_arrive_in_the_order_they_were_written() {
    is(
        "fun five a b c d e = a * 10000 + b * 1000 + c * 100 + d * 10 + e\n\
         def result = five 1 2 3 4 5\n",
        "12345",
    );
    // Under-applied, so the arguments arrive across several closures rather
    // than one window.
    is(
        "fun five a b c d e = a * 10000 + b * 1000 + c * 100 + d * 10 + e\n\
         fun apply f x = f x\n\
         def result = apply (five 1 2 3 4) 5\n",
        "12345",
    );
}

// --- case trees ---------------------------------------------------------------

/// The tree must pick the arm the chain would have picked, and arms overlap in
/// ways the tree does not cover: a constructor pattern with a *literal* inside
/// can match the tag and then fail, and what follows is another arm for the same
/// constructor. Getting this wrong needs `-O2` and a `match` someone actually
/// writes, which is why it is worth stating.
#[test]
fn a_case_tree_picks_the_same_arm_the_chain_would() {
    let src = |v: &str| {
        format!(
            "use Shape.*\ndata Shape = Dot | Line Int | Box Int Int\n\
             fun name s =\n\
             \x20 match s with\n\
             \x20 | Line 0 -> \"degenerate line\"\n\
             \x20 | Dot -> \"dot\"\n\
             \x20 | Box 0 0 -> \"degenerate box\"\n\
             \x20 | Line n -> \"line\"\n\
             \x20 | Box w h -> \"box\"\n\
             def result = name ({v})\n"
        )
    };
    is(&src("Dot"), "\"dot\"");
    is(&src("Line 0"), "\"degenerate line\"");
    is(&src("Line 7"), "\"line\"");
    is(&src("Box 0 0"), "\"degenerate box\"");
    is(&src("Box 1 0"), "\"box\"");
    is(&src("Box 0 1"), "\"box\"");
}

/// A wildcard in the middle stops the tree, and everything after it has to keep
/// its order behind the wildcard rather than being hoisted into the switch.
#[test]
fn a_wildcard_still_shadows_the_arms_after_it() {
    let src = |v: &str| {
        format!(
            "use Shape.*\ndata Shape = Dot | Line Int | Box Int Int\n\
             fun name s =\n\
             \x20 match s with\n\
             \x20 | Dot -> \"dot\"\n\
             \x20 | Line n -> \"line\"\n\
             \x20 | _ -> \"anything\"\n\
             \x20 | Box w h -> \"box\"\n\
             def result = name ({v})\n"
        )
    };
    is(&src("Dot"), "\"dot\"");
    is(&src("Line 1"), "\"line\"");
    // The wildcard comes first, so `Box` never reaches its own arm.
    is(&src("Box 1 2"), "\"anything\"");
}

/// A binding arm is not a constructor arm, and the value it binds is the whole
/// scrutinee — not a field of it.
#[test]
fn a_binding_arm_sees_the_whole_scrutinee() {
    is(
        "use Shape.*\ndata Shape = Dot | Line Int\n\
         fun size s = match s with | Dot -> 0 | other -> (match other with | Line n -> n | _ -> -1)\n\
         def result = size (Line 9)\n",
        "9",
    );
}

/// Nested constructors: the tree covers the outer `match`, and the inner one is
/// a separate decision with its own fallback.
#[test]
fn nested_matches_each_get_their_own_tree() {
    let src = |v: &str| {
        format!(
            "use Inner.*\ndata Inner = A | B Int\n\
             use Outer.*\ndata Outer = P Inner | Q Inner | R\n\
             fun go o =\n\
             \x20 match o with\n\
             \x20 | P A -> 1\n\
             \x20 | Q (B 0) -> 2\n\
             \x20 | P (B n) -> 3 + n\n\
             \x20 | Q x -> 100\n\
             \x20 | R -> 200\n\
             def result = go ({v})\n"
        )
    };
    is(&src("P A"), "1");
    is(&src("Q (B 0)"), "2");
    is(&src("P (B 5)"), "8");
    is(&src("Q A"), "100");
    is(&src("Q (B 1)"), "100");
    is(&src("R"), "200");
}

/// Lists are the `match` everything else is built on, and the standard library's
/// own containers go through it constantly.
#[test]
fn matching_a_list_agrees_at_every_level() {
    is(
        "use Std.Collections.List as L
def result = L.sum [1; 2; 3; 4]
",
        "10",
    );
    expr_is(
        "match [1; 2; 3] with | [;] -> 0 | x :: [;] -> 1 | x :: y :: rest -> 2",
        "2",
    );
    expr_is(
        "match [9;] with | [;] -> 0 | x :: [;] -> 1 | x :: y :: rest -> 2",
        "1",
    );
    expr_is(
        "match [;] with | [;] -> 0 | x :: [;] -> 1 | x :: y :: rest -> 2",
        "0",
    );
}

/// A literal pattern is a fused comparison against a constant, which is the same
/// machinery as `if n == 0` reached from the other direction.
#[test]
fn literal_patterns_match_every_literal_type() {
    let int = |v: &str| {
        format!(
            "fun f n = match n with | 0 -> \"zero\" | 1 -> \"one\" | _ -> \"many\"\ndef result = f {v}\n"
        )
    };
    is(&int("0"), "\"zero\"");
    is(&int("1"), "\"one\"");
    is(&int("7"), "\"many\"");

    let ch = |v: &str| {
        format!("fun f c = match c with | 'a' -> 1 | 'b' -> 2 | _ -> 0\ndef result = f {v}\n")
    };
    is(&ch("'a'"), "1");
    is(&ch("'b'"), "2");
    is(&ch("'z'"), "0");

    let s = |v: &str| {
        format!(
            "fun f s = match s with | \"yes\" -> 1 | \"no\" -> 2 | _ -> 0\ndef result = f {v}\n"
        )
    };
    is(&s("\"yes\""), "1");
    is(&s("\"no\""), "2");
    is(&s("\"maybe\""), "0");
}

/// A `match` with no arm for the value is an error at every level, and the same
/// error — the tree's fallback has to reach the same place the chain's did.
#[test]
fn a_non_exhaustive_match_fails_the_same_way_everywhere() {
    let src = "use Shape.*\ndata Shape = Dot | Line Int\n\
               fun name s = match s with | Dot -> \"dot\"\n\
               def result = name (Line 1)\n";
    let (program, diags) =
        pipeline::compile_str_with_std("test", src, Options::debug().entry("result"));
    assert!(diags.is_empty(), "{:?}", diags);

    let program = meadow_compiler::core::prune::prune(&program);
    let cek = runtime::run(&program, Engine::Cek, OptLevel::O1).unwrap_err();
    for opt in [OptLevel::O0, OptLevel::O1, OptLevel::O2] {
        let vm = runtime::run(&program, Engine::Vm, opt).unwrap_err();
        assert_eq!(vm, cek, "at {}", opt.name());
    }
    assert!(cek.contains("non-exhaustive"), "{cek}");
}

// --- the awkward corners ------------------------------------------------------

/// A constant index that does not fit the fused branch's operand byte.
///
/// `jumpunlessprimk` holds its constant in a `u8`, so past 256 entries the
/// compiler has to fall back to loading the constant and comparing two
/// registers. The standard library is large enough to reach that on its own,
/// but not in a way anyone reading this file would know — so here it is
/// deliberately, with the comparison done against a constant that cannot
/// possibly be near the front of the table.
#[test]
fn a_comparison_works_past_the_two_hundred_and_fifty_sixth_constant() {
    // Several hundred distinct literals, so the interesting ones are pushed
    // well past the operand byte.
    let mut src = String::new();
    for i in 0..400 {
        src.push_str(&format!("def k{i} = {}\n", 100_000 + i));
    }
    src.push_str("fun classify n = match n with | 100399 -> 1 | 100000 -> 2 | _ -> 3\n");
    // Four digits: the last constant, the first, one in between, and a fused
    // comparison against a constant that is certainly not near the front of the
    // table.
    src.push_str(
        "def result =\n\
         \x20 classify k399 * 1000 + classify k0 * 100 + classify k7 * 10\n\
         \x20 + (if k399 > 100398 then 1 else 0)\n",
    );
    is(&src, "1231");
}

/// Folding must not change what an error says, or when it happens.
#[test]
fn a_folded_operation_still_fails_where_it_should() {
    for (src, want) in [
        ("def n = 1\ndef result = n / 0\n", "division by zero"),
        ("def n = 1\ndef result = n % 0\n", "modulo by zero"),
        ("def n = 0\ndef result = 1 / n\n", "division by zero"),
    ] {
        let (program, diags) =
            pipeline::compile_str_with_std("test", src, Options::debug().entry("result"));
        assert!(diags.is_empty(), "{:?}", diags);
        let program = meadow_compiler::core::prune::prune(&program);
        let cek = runtime::run(&program, Engine::Cek, OptLevel::O1).unwrap_err();
        assert!(cek.contains(want), "the CEK machine said {cek:?}");
        for opt in [OptLevel::O0, OptLevel::O1, OptLevel::O2] {
            let vm = runtime::run(&program, Engine::Vm, opt).unwrap_err();
            assert!(vm.contains(want), "the VM at {} said {vm:?}", opt.name());
        }
    }
}

/// Effects run through `handle` / `perform`, which the folded and fused forms
/// sit inside as readily as anything else — and a resumption restores registers,
/// so a value read from the wrong one would surface here first.
#[test]
fn folding_and_fusing_survive_a_resumption() {
    is(
        "effect Tick { tick : () -> Int }\n\
         fun counting u =\n\
         \x20 handle\n\
         \x20   let a = tick () in\n\
         \x20   let b = tick () in\n\
         \x20   a * 100 + b\n\
         \x20 with { tick u r -> r 7 + 1 }\n\
         def result = counting ()\n",
        "709",
    );
}
