//! What the lowering actually emits.
//!
//! `meadow_glade`'s differential tests already check that the translation *means*
//! the same thing as the CEK machine. These check that it means it in the
//! intended shape — that returning is an `invoke` with no arguments, that
//! calling is a `substitute` and an `invoke`, and above all that a block's
//! parameters cover the whole environment. The last one is the property the rest
//! of the pipeline is going to rely on, and a test that reads the dump is the
//! cheapest way to notice it slipping.

use meadow_core::{Def, Lit, Prim, Program, Term};
use meadow_hir::VarId;
use meadow_seq::{Block, Statement, lower_program};
use std::sync::Arc;

/// A choice between `a` and `b` that `core::simplify` cannot make.
///
/// These tests are about the shape lowering emits, and several of them used a
/// literal `true` or a constructor built one line above the `match` on it to
/// say what was being lowered. The simplifier now settles both at compile time
/// -- correctly, and leaving nothing for the test to look at. So the condition
/// is a comparison instead: nothing here folds one, and the shape under test is
/// the shape a real program has.
fn opaque(a: Term, b: Term) -> Term {
    let cond = Term::Prim(
        Prim::Lt,
        vec![Term::Lit(Lit::Int(0)), Term::Lit(Lit::Int(1))],
        meadow_core::unknown(),
    );
    Term::If(Arc::new(cond), Arc::new(a), Arc::new(b))
}

fn main_def(term: Term) -> Program {
    let var = VarId(100);
    Program {
        defs: vec![Def::untyped(var, "main", term)],
        entry: Some(var),
        ctor_fields: Default::default(),
        variants: Default::default(),
        origins: Default::default(),
        migrations: Vec::new(),
    }
}

#[test]
fn a_literal_is_an_extern_and_a_return() {
    let lowered = lower_program(
        &main_def(Term::Lit(Lit::Int(42))),
        meadow_core::OptLevel::default(),
    );
    assert!(lowered.unsupported.is_empty());
    let text = lowered.program.pretty();
    // The literal producer, then the universal return sequence: arrange the
    // environment as exactly [k, v] and invoke the continuation's only method.
    assert!(text.contains("extern lit Int(42)()"), "{text}");
    assert!(text.contains("substitute ["), "{text}");
    assert!(text.contains("invoke "), "{text}");
    assert!(text.contains("#0"), "{text}");
}

#[test]
fn every_block_binds_the_whole_environment() {
    // `(1 + 2) * 3` — three operands, so the continuation of each has to keep
    // what came before it alive. If a `keep` set were wrong, some block's
    // parameter list would be shorter than the environment reaching it, and the
    // AxCut machine would refuse to enter it. Here we check the static side:
    // every `substitute` supplies exactly as many values as its block takes.
    let term = Term::prim(
        Prim::Mul,
        vec![
            Term::prim(
                Prim::Add,
                vec![Term::Lit(Lit::Int(1)), Term::Lit(Lit::Int(2))],
            ),
            Term::Lit(Lit::Int(3)),
        ],
    );
    let lowered = lower_program(&main_def(term), meadow_core::OptLevel::default());
    assert!(lowered.unsupported.is_empty());

    let mut checked = 0;
    for def in &lowered.program.defs {
        walk(&def.block.body, &mut |s| {
            if let Statement::Substitute(sel, block) = s {
                assert_eq!(
                    sel.len(),
                    block.params.len(),
                    "substitute of {} into a block of {}",
                    sel.len(),
                    block.params.len()
                );
                checked += 1;
            }
        });
    }
    assert!(checked > 0, "no substitutions to check");
}

#[test]
fn calling_a_lambda_is_a_permutation_and_a_branch() {
    // `(if 1 == 2 then \x -> x else \x -> x) 1`: a call of a function the
    // simplifier cannot see through -- a lambda applied where it stands is a
    // `let` to it. The call site should end in exactly the two statements the
    // IR promises: rearrange the registers, then jump into the method. The
    // registers are the function, its argument, the continuation, and the
    // evidence -- the handlers the callee runs under.
    let x = VarId(1);
    let y = VarId(2);
    let cond = Term::Prim(
        Prim::Eq,
        vec![Term::Lit(Lit::Int(1)), Term::Lit(Lit::Int(2))],
        meadow_core::Ty::Con(meadow_intern::InternedString::from("Bool"), Vec::new()),
    );
    let f = Term::If(
        Arc::new(cond),
        Arc::new(Term::lam(x, Term::Var(x))),
        Arc::new(Term::lam(y, Term::Var(y))),
    );
    let term = Term::App(Arc::new(f), Arc::new(Term::Lit(Lit::Int(1))));
    let lowered = lower_program(&main_def(term), meadow_core::OptLevel::default());
    assert!(lowered.unsupported.is_empty());

    let mut found = false;
    for def in &lowered.program.defs {
        walk(&def.block.body, &mut |s| {
            if let Statement::Substitute(sel, block) = s
                && sel.len() == 4
                && matches!(block.body, Statement::Invoke(f, 0) if f == sel[0])
            {
                found = true;
            }
        });
    }
    assert!(
        found,
        "expected `substitute [f, arg, k, ev] in {{ invoke f#0 }}`\n{}",
        lowered.program.pretty()
    );
}

#[test]
fn a_global_reference_is_a_jump_with_the_environment_untouched() {
    let g = VarId(50);
    let m = VarId(51);
    // Not a literal: `globals::inline_literals` would put one at the mention
    // and there would be no reference left to be a jump. See
    // `a_global_literal_is_put_where_it_is_mentioned`.
    let program = Program {
        defs: vec![
            Def::untyped(
                g,
                "g",
                Term::Prim(
                    Prim::Add,
                    vec![Term::Lit(Lit::Int(3)), Term::Lit(Lit::Int(4))],
                    meadow_core::unknown(),
                ),
            ),
            Def::untyped(m, "main", Term::Var(g)),
        ],
        entry: Some(m),
        ctor_fields: Default::default(),
        variants: Default::default(),
        origins: Default::default(),
        migrations: Vec::new(),
    };
    let lowered = lower_program(&program, meadow_core::OptLevel::default());
    assert!(lowered.unsupported.is_empty());

    // `substitute [k] in {(k) => jump #0}` — a one-element environment, which is
    // exactly what `g`'s block takes, so the jump needs no shuffling.
    let main = &lowered.program.defs[1].block.body;
    match main {
        Statement::Substitute(sel, block) => {
            assert_eq!(sel.len(), 1);
            assert_eq!(block.params.len(), 1);
            assert!(matches!(block.body, Statement::Jump(l) if l.0 == 0));
        }
        other => panic!("expected a substitute and a jump, got {other:?}"),
    }
}

/// A global whose value is a literal is not a jump at all: the literal goes
/// where the mention was.
///
/// This is what lets a bound written `def limit : Int = 100` be fused into the
/// branch that tests it. Without it, reading one in a loop condition costs two
/// heap-allocated continuations and an unfused compare every time round.
#[test]
fn a_global_literal_is_put_where_it_is_mentioned() {
    let g = VarId(50);
    let m = VarId(51);
    let program = Program {
        defs: vec![
            Def::untyped(g, "g", Term::Lit(Lit::Int(7))),
            Def::untyped(m, "main", Term::Var(g)),
        ],
        entry: Some(m),
        ctor_fields: Default::default(),
        variants: Default::default(),
        origins: Default::default(),
        migrations: Vec::new(),
    };
    let lowered = lower_program(&program, meadow_core::OptLevel::default());
    assert!(lowered.unsupported.is_empty());

    let main = &lowered.program.defs[1].block.body;
    assert!(
        !matches!(main, Statement::Substitute(_, b) if matches!(b.body, Statement::Jump(_))),
        "the literal should be here, not behind a jump: {main:?}"
    );
    assert!(
        format!("{main:?}").contains("Lit(Int(7))"),
        "expected the literal itself, got {main:?}"
    );
}

/// A `let`-bound function that is only tail-called becomes a block and a jump,
/// not a closure and an invoke.
///
/// `core::joins` finds it and `lower` gives it a label. Both branches of the
/// `if` enter the same block, which is the whole point: the continuation is
/// named once instead of copied into each branch or built as an object.
#[test]
fn a_join_point_becomes_a_label_and_jumps_to_it() {
    let f = VarId(60);
    let x = VarId(61);

    let call = |n: i64| Term::App(Arc::new(Term::Var(f)), Arc::new(Term::Lit(Lit::Int(n))));
    let body = opaque(call(1), call(2));
    let term = Term::Let(
        f,
        meadow_core::Poly::mono(meadow_core::unknown()),
        Arc::new(Term::Lam(x, meadow_core::unknown(), Arc::new(Term::Var(x)))),
        Arc::new(body),
    );
    let lowered = lower_program(&main_def(term), meadow_core::OptLevel::default());
    assert!(lowered.unsupported.is_empty());

    let joins: Vec<_> = lowered
        .program
        .defs
        .iter()
        .filter(|d| &*d.name == "<join>")
        .collect();
    assert_eq!(joins.len(), 1, "one block for the join point");

    // And both branches jump to it, rather than making an object and invoking.
    let label = joins[0].label;
    let text = lowered.program.pretty();
    let jumps = text.matches(&format!("jump #{}", label.0)).count();
    assert!(jumps >= 2, "both branches jump to it:\n{text}");
}

#[test]
fn a_variable_from_nowhere_is_reported() {
    // Every `core` construct lowers now, so the only thing left to report is a
    // variable that is neither in scope nor a definition — a bug upstream. It
    // still has to be *said* rather than turned into a plausible statement.
    let lowered = lower_program(
        &main_def(Term::Var(VarId(999))),
        meadow_core::OptLevel::default(),
    );
    assert!(
        lowered
            .unsupported
            .contains(&meadow_seq::Unsupported::UnboundVar),
        "an unbound variable should be reported, not silently mistranslated"
    );
}

#[test]
fn a_match_becomes_a_switch_with_a_default() {
    // `match x with | Just y -> y | _ -> 0`, as core builds it.
    let x = VarId(1);
    let y = VarId(2);
    let term = Term::let_(
        x,
        opaque(
            Term::ctor("Just", vec![Term::Lit(Lit::Int(9))]),
            Term::ctor("Nothing", vec![]),
        ),
        Term::case(
            Term::Var(x),
            vec![
                (
                    meadow_core::Pat::Ctor("Just".into(), vec![meadow_core::Pat::var(y)]),
                    Term::Var(y),
                ),
                (meadow_core::Pat::Wild, Term::Lit(Lit::Int(0))),
            ],
        ),
    );
    let lowered = lower_program(&main_def(term), meadow_core::OptLevel::default());
    assert!(lowered.unsupported.is_empty());

    let mut switches = 0;
    for def in &lowered.program.defs {
        walk(&def.block.body, &mut |s| {
            if let Statement::Switch { arms, default, .. } = s {
                // One arm — the constructor the pattern names — and a default
                // that binds no fields, because it knows of none. `Just` has one
                // field, and the scrutinee stays, so the arm binds exactly one
                // more value than the default does.
                assert_eq!(arms.len(), 1);
                assert_eq!(arms[0].1.params.len(), default.params.len() + 1);
                switches += 1;
            }
        });
    }
    assert_eq!(switches, 1, "{}", lowered.program.pretty());
}

#[test]
fn a_letrec_becomes_labels_sharing_one_parameter_list() {
    // `let n = 1 in letrec f = \x -> g x; g = \x -> f n in f 0`
    //
    // Both bindings capture `n`, so both blocks take `[n, k, ev]` and a reference to
    // either is a substitute of that shape followed by a jump. The point is that
    // mutual recursion needs no object pointing at itself.
    let n = VarId(1);
    let f = VarId(2);
    let g = VarId(3);
    let a = VarId(4);
    let b = VarId(5);
    let term = Term::let_(
        n,
        // Not a literal: one of those is copied to where it is read, and then
        // the group has no capture to share.
        opaque(Term::Lit(Lit::Int(1)), Term::Lit(Lit::Int(2))),
        Term::letrec(
            vec![
                (
                    f,
                    Term::lam(a, Term::App(Arc::new(Term::Var(g)), Arc::new(Term::Var(a)))),
                ),
                (
                    g,
                    Term::lam(b, Term::App(Arc::new(Term::Var(f)), Arc::new(Term::Var(n)))),
                ),
            ],
            Term::App(Arc::new(Term::Var(f)), Arc::new(Term::Lit(Lit::Int(0)))),
        ),
    );
    // At `-O0`: above it, the group is lifted to definitions of its own before
    // lowering sees it -- see the next test.
    let lowered = lower_program(&main_def(term), meadow_core::OptLevel::O0);
    assert!(lowered.unsupported.is_empty());

    // One definition for `main`, one per `letrec` binding.
    assert_eq!(
        lowered.program.defs.len(),
        3,
        "{}",
        lowered.program.pretty()
    );
    for def in &lowered.program.defs[1..] {
        assert_eq!(
            def.block.params,
            vec![n, def.block.params[1], def.block.params[2]],
            "a lifted binding takes the group's captures, then the continuation and the evidence"
        );
    }
}

#[test]
fn a_local_function_is_lifted_to_a_definition_with_a_direct_entry() {
    // `let n = … in letrec go = \x -> go n in go 0`, above `-O0`: `go` is a
    // definition of its own taking `n` first, so the call in `main` is a jump
    // to its direct entry, with no closure built for it.
    let n = VarId(1);
    let go = VarId(2);
    let a = VarId(3);
    let term = Term::let_(
        n,
        opaque(Term::Lit(Lit::Int(1)), Term::Lit(Lit::Int(2))),
        Term::letrec(
            vec![(
                go,
                Term::lam(
                    a,
                    Term::App(Arc::new(Term::Var(go)), Arc::new(Term::Var(n))),
                ),
            )],
            Term::App(Arc::new(Term::Var(go)), Arc::new(Term::Lit(Lit::Int(0)))),
        ),
    );
    let lowered = lower_program(&main_def(term), meadow_core::OptLevel::default());
    assert!(lowered.unsupported.is_empty());
    let text = lowered.program.pretty();
    // `main`, and `go` lifted: its entry as a value and its direct entry, which
    // takes the captured `n`, the argument and the continuation.
    let lifted: Vec<_> = lowered
        .program
        .defs
        .iter()
        .filter(|d| d.name.ends_with("Local1"))
        .collect();
    assert_eq!(lifted.len(), 2, "{text}");
    assert!(lifted.iter().any(|d| d.block.params.len() == 3), "{text}");
}

/// Every statement in a tree, including the bodies of nested blocks.
fn walk(s: &Statement, f: &mut impl FnMut(&Statement)) {
    f(s);
    let blocks: Vec<&Block> = match s {
        Statement::Substitute(_, b) => vec![b],
        Statement::Switch { arms, default, .. } => {
            let mut bs: Vec<&Block> = arms.iter().map(|(_, b)| b).collect();
            bs.push(default);
            bs
        }
        Statement::Extern { blocks, .. } => blocks.iter().collect(),
        Statement::New { methods, rest, .. } => {
            walk(rest, f);
            methods.iter().collect()
        }
        Statement::Let { rest, .. } => {
            walk(rest, f);
            vec![]
        }
        Statement::Mark(_, rest) => {
            walk(rest, f);
            vec![]
        }
        Statement::Jump(_) | Statement::Invoke(..) | Statement::Error(_) => vec![],
    };
    for b in blocks {
        walk(&b.body, f);
    }
}

/// A literal operand rides inside the `extern` instead of becoming a value.
///
/// `n - 1` used to be two statements and two environment slots: one to produce
/// the `1`, one to subtract it, and the literal stayed live until something
/// dropped it. The folded form is what removes the register, not only the
/// instruction.
#[test]
fn a_literal_operand_is_folded_into_the_primitive() {
    let term = Term::prim(
        Prim::Sub,
        vec![Term::Lit(Lit::Int(7)), Term::Lit(Lit::Int(1))],
    );
    let lowered = lower_program(&main_def(term), meadow_core::OptLevel::default());

    let mut ops = Vec::new();
    for def in &lowered.program.defs {
        walk(&def.block.body, &mut |s| {
            if let Statement::Extern { op, args, .. } = s {
                ops.push((op.clone(), args.len()));
            }
        });
    }
    // The `7` is still produced — only the right operand folds — and the
    // subtraction takes it as its one argument.
    assert!(
        ops.iter().any(
            |(op, n)| matches!(op, meadow_seq::Extern::PrimK(Prim::Sub, Lit::Int(1))) && *n == 1
        ),
        "expected a folded `Sub .. 1`, got {ops:?}"
    );
    assert!(
        !ops.iter()
            .any(|(op, _)| matches!(op, meadow_seq::Extern::Prim(Prim::Sub))),
        "the unfolded form should be gone: {ops:?}"
    );
}

/// `if n == 0` is one statement: the comparison, the literal and the branch.
#[test]
fn a_comparison_fuses_into_the_branch_that_tests_it() {
    let n = VarId(1);
    let term = Term::let_(
        n,
        Term::Lit(Lit::Int(3)),
        Term::If(
            Arc::new(Term::prim(
                Prim::Eq,
                vec![Term::Var(n), Term::Lit(Lit::Int(0))],
            )),
            Arc::new(Term::Lit(Lit::Int(1))),
            Arc::new(Term::Lit(Lit::Int(2))),
        ),
    );
    let lowered = lower_program(&main_def(term), meadow_core::OptLevel::default());

    let mut found = false;
    for def in &lowered.program.defs {
        walk(&def.block.body, &mut |s| {
            if let Statement::Extern { op, args, blocks } = s {
                if let meadow_seq::Extern::BranchPrimK(Prim::Eq, Lit::Int(0)) = op {
                    assert_eq!(args.len(), 1, "one operand; the other is the literal");
                    assert_eq!(blocks.len(), 2, "false and true");
                    found = true;
                }
                assert!(
                    !matches!(op, meadow_seq::Extern::Branch),
                    "the unfused branch should be gone"
                );
            }
        });
    }
    assert!(found, "{}", lowered.program.pretty());
}

/// At `-O2` a `match` over distinct constructors is one `switch`.
#[test]
fn case_trees_replace_the_chain_at_o2() {
    use meadow_core::{OptLevel, Pat};

    // `match x with | Nothing -> 0 | Just y -> y | _ -> 0`
    let x = VarId(1);
    let y = VarId(2);
    let case = Term::case(
        Term::Var(x),
        vec![
            (Pat::Ctor("Nothing".into(), vec![]), Term::Lit(Lit::Int(0))),
            (Pat::Ctor("Just".into(), vec![Pat::var(y)]), Term::Var(y)),
            (Pat::Wild, Term::Lit(Lit::Int(0))),
        ],
    );
    let term = Term::let_(
        x,
        opaque(
            Term::ctor("Just", vec![Term::Lit(Lit::Int(9))]),
            Term::ctor("Nothing", vec![]),
        ),
        case,
    );

    let arms_at = |opt| {
        let lowered = lower_program(&main_def(term.clone()), opt);
        let mut widths = Vec::new();
        for def in &lowered.program.defs {
            walk(&def.block.body, &mut |s| {
                if let Statement::Switch { arms, .. } = s {
                    widths.push(arms.len());
                }
            });
        }
        widths.sort_unstable();
        widths
    };

    // Neither arm can fail once its tag has matched, so there is nothing to
    // fall back to and it is one switch that tests both, at any level: the
    // chain's switch an arm, each testing one tag, came with an object an arm.
    assert_eq!(arms_at(OptLevel::O1), vec![2]);
    assert_eq!(arms_at(OptLevel::O2), vec![2]);

    // `match x with | Nothing -> 0 | Just (Just z) -> z | _ -> 0`: the second
    // arm can fail after its tag has matched, and has to go on to the third.
    let z = VarId(3);
    let nested = Term::case(
        Term::Var(x),
        vec![
            (Pat::Ctor("Nothing".into(), vec![]), Term::Lit(Lit::Int(0))),
            (
                Pat::Ctor(
                    "Just".into(),
                    vec![Pat::Ctor("Just".into(), vec![Pat::var(z)])],
                ),
                Term::Var(z),
            ),
            (Pat::Wild, Term::Lit(Lit::Int(0))),
        ],
    );
    let term = Term::let_(
        x,
        opaque(
            Term::ctor(
                "Just",
                vec![Term::ctor("Just", vec![Term::Lit(Lit::Int(9))])],
            ),
            Term::ctor("Nothing", vec![]),
        ),
        nested,
    );
    let widest = |opt| {
        let lowered = lower_program(&main_def(term.clone()), opt);
        let mut widest = 0;
        for def in &lowered.program.defs {
            walk(&def.block.body, &mut |s| {
                if let Statement::Switch { arms, .. } = s {
                    widest = widest.max(arms.len());
                }
            });
        }
        widest
    };
    // The chain: a switch an arm, each testing one tag ...
    assert_eq!(widest(OptLevel::O1), 1);
    // ... against a tree, whose first switch tests both.
    assert_eq!(widest(OptLevel::O2), 2);
}
