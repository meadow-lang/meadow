//! What a program *costs*, asserted rather than timed.
//!
//! Instruction counts and allocation counts are deterministic — the same program
//! costs the same every run, on every machine — so they can be asserted exactly
//! where a wall clock could only be eyeballed. That makes this a regression test
//! for optimisations rather than a benchmark: a change that quietly reintroduces
//! an allocation per loop iteration fails here, on a laptop, in a second.
//!
//! The bounds are set a little above what the compiler currently achieves, so
//! ordinary noise in code generation does not trip them and a real regression
//! does. If a change *improves* one, tighten it — the number in the assertion is
//! a claim about the compiler, and it should stay true.

use meadow_compiler::{Options, compile_str_with, core};

fn program(src: &str) -> core::Program {
    let (pkg, diags) = compile_str_with("cost", src, Options::default().entry("result"));
    let msgs: Vec<_> = diags.iter().map(|d| d.msg.clone()).collect();
    assert!(msgs.is_empty(), "{}", msgs.join("\n"));
    let entry = pkg.value_entry;
    core::Program {
        defs: pkg.defs.clone(),
        entry,
        ctor_fields: pkg.ctor_fields.clone(),
        variants: pkg.variants.clone(),
        origins: Default::default(),
    }
}

/// Run `src` and answer `(result, instructions retired, heap slots allocated)`.
fn cost(src: &str) -> (String, u64, u64) {
    let prog = program(src);
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::default());
    assert!(lowered.unsupported.is_empty(), "{:?}", lowered.unsupported);
    let image = meadow_codegen::compile(&lowered.program).expect("codegen");
    let mut vm = meadow_glade::Vm::new(&image);
    let v = vm
        .run(image.entry.expect("an entry point"), 500_000_000)
        .unwrap_or_else(|e| panic!("{}: {}", e.msg, src));
    let out = vm.show(v);
    let (_, allocated) = vm.heap_stats();
    (out, vm.steps, allocated)
}

#[test]
fn a_tail_recursive_loop_allocates_nothing() {
    // The one that matters most. `count` is a known function called with exactly
    // its arity, and every operand is a primitive on values already in
    // registers, so the whole loop is a jump back to itself over registers.
    //
    // It used to cost 20 heap slots and 50 instructions *per iteration*: a
    // closure for the continuation of every operand, and another per curried
    // argument. Nothing here should allocate at all.
    let (out, steps, allocated) = cost(
        "fun count (n : Int) acc = if n == 0 then acc else count (n - 1) (acc + n)
         def result = count 100000 0",
    );
    assert_eq!(out, "5000050000");
    assert_eq!(
        allocated, 3,
        "a loop must not allocate; only the halt closure: two words of header, and the descriptor of the answer it waits for"
    );
    assert!(
        steps < 1_100_000,
        "{steps} instructions for 100_000 iterations — about 9 is right"
    );
}

#[test]
fn a_known_call_does_not_build_a_closure() {
    // A saturated call to a top-level function is a jump. Under-applying it is
    // what a closure is *for*, and that path still allocates — the point is that
    // the ordinary case does not pay for it.
    let (_, _, direct) = cost(
        "fun add a b = a + b
         fun go (n : Int) acc = if n == 0 then acc else go (n - 1) (add acc n)
         def result = go 1000 0",
    );
    // A real partial application — `(add acc)` is one argument short, so it has
    // to become a closure. Parentheses alone would not do it: `(add acc) n` is
    // the same application spine as `add acc n`.
    let (_, _, curried) = cost(
        "fun add a b = a + b
         fun apply f x = f x
         fun go (n : Int) acc = if n == 0 then acc else go (n - 1) (apply (add acc) n)
         def result = go 1000 0",
    );
    // Four words an iteration: one continuation object for the non-tail call
    // -- a two-word header and two captures -- which a machine with no call
    // stack has to put somewhere. The curried form pays that *and* a closure
    // per argument.
    assert!(
        direct <= 4 * 1000 + 3,
        "{direct} slots for 1000 saturated calls"
    );
    assert!(
        curried > direct,
        "a partial application has to build more than a saturated call: {curried} vs {direct}"
    );
}

#[test]
fn arithmetic_is_three_address() {
    // `a + b` is one instruction, not a move, a move and an add. Six operations
    // plus the loop's own overhead, over one iteration.
    let (out, steps, _) = cost("def result = 1 + 2 * 3 - 4 + 5 * 6 - 7");
    assert_eq!(out, "26");
    assert!(steps < 20, "{steps} instructions for six operations");
}

#[test]
fn building_a_list_costs_the_list_and_little_else() {
    // Four words per `Cons` — two of header and two fields — and nothing per
    // call on top of it. Walking it back should allocate nothing at all.
    let (out, _, allocated) = cost(
        "use Chain.*\ndata Chain = Nil | Cons Int Chain
         fun build n = if n == 0 then Nil else Cons n (build (n - 1))
         fun total xs = match xs with | Nil -> 0 | Cons x r -> x + total r
         def result = total (build 10000)",
    );
    assert_eq!(out, "50005000");
    // 10_000 conses at 4 words, one `Nil`, and the continuations `build` and
    // `total` need because neither is tail recursive.
    assert!(
        allocated < 270_000,
        "{allocated} slots to build and walk a 10_000-element list"
    );
}

#[test]
fn a_closure_still_captures_what_it_should() {
    // The optimisations must not have made functions-as-values worse or wrong.
    let (out, _, _) = cost(
        "fun twice f x = f (f x)
         fun adder n = \\m -> n + m
         def result = twice (adder 10) 1",
    );
    assert_eq!(out, "21");
}

/// A comparison allocates nothing, whatever it compares.
///
/// Not a performance claim — a correctness one. [`Op::JumpUnlessPrim`] puts its
/// result in `meadow_glade::vm::TEMP`, a register the collector deliberately does
/// not scan, and that is sound only because the primitive cannot allocate while
/// the machine is holding a value there. `Prim::compares` is the list the
/// compiler checks a fused branch against; this is the check that the list is
/// still telling the truth about what those primitives do.
///
/// Each case is run at two iteration counts. What matters is that the total does
/// not grow with the number of comparisons, not what it is — the loop itself has
/// to build its arguments once, and `toBigInt` is an allocation on any reading.
/// The shape is deliberate too: both arms of the `if` are saturated tail calls,
/// so nothing *else* in the loop can allocate and take the blame.
#[test]
fn a_comparison_allocates_nothing() {
    let cases: &[(&str, &str, &str)] = &[
        ("", "a < b", "(toInt 3) (toInt 4)"),
        ("", "a > b", "(toInt 3) (toInt 4)"),
        ("", "a <= b", "(toInt 3) (toInt 4)"),
        ("", "a >= b", "(toInt 3) (toInt 4)"),
        ("", "a == 3", "(toInt 3) (toInt 4)"),
        ("", "a != 3", "(toInt 3) (toInt 4)"),
        // A sized integer is as immediate as an `Int`.
        ("", "a < b", "(toUInt8 3) (toUInt8 4)"),
        ("", "a == 3", "(toInt16 3) (toInt16 4)"),
        ("", "_primLtF a b", "1.5 2.5"),
        ("", "_primGeF a b", "1.5 2.5"),
        ("", "a == b", "'x' 'y'"),
        ("", "a == 'x'", "'x' 'y'"),
        ("", "a != b", "\"ab\" \"ab\""),
        ("", "a == \"ab\"", "\"ab\" \"ab\""),
        // Structural equality over data, which walks the heap without touching
        // the allocator.
        (
            "use Pair.*\ndata Pair = Pair Int Int\n",
            "a == b",
            "(Pair 1 2) (Pair 1 2)",
        ),
        // A `BigInt` is on the heap, and comparing two is reading them.
        ("", "a < b", "(toBigInt 5) (toBigInt 9)"),
        ("", "a >= b", "(toBigInt 5) (toBigInt 9)"),
    ];

    for (prelude, cond, args) in cases {
        let src = |n: i64| {
            format!(
                "{prelude}fun go (i : Int) (acc : Int) a b =\n\
                 \x20 if i == 0 then acc\n\
                 \x20 else if {cond} then go (i - 1) (acc + 1) a b else go (i - 1) acc a b\n\
                 def result = go {n} 0 {args}\n"
            )
        };
        let (_, _, few) = cost(&src(3));
        let (_, _, many) = cost(&src(300));
        assert_eq!(
            few, many,
            "`{cond}` cost {few} slots over 3 iterations and {many} over 300 — it \
             allocates, so it must not be on `Prim::compares`"
        );
    }
}
