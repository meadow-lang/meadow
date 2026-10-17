//! The standard library, through the whole back end.
//!
//! `meadow_glade`'s own differential tests are hand-written programs, one
//! construct at a time. This is the other end: the largest body of Meadow that
//! exists, compiled the way `meadow test --std` compiles it, and put through
//! `core` → AxCut → bytecode → the VM.
//!
//! It answers three separate questions, and they are worth keeping apart —
//! when one fails, which one it was says where to look.
//!
//! * **Does everything lower?** [`the_whole_standard_library_lowers`] asserts
//!   the `Unsupported` set is empty across the entire library. A statement about
//!   the pass, not about any run, and the one that would catch a `core`
//!   construct nobody thought to test.
//! * **Does it compile?** [`the_whole_standard_library_reaches_bytecode`] runs
//!   the register allocator over every block. What does not fit in the file is
//!   spilled, so what is left to fail is a block it still cannot express --
//!   a labelled block of more than 255 parameters -- and this would notice.
//! * **Does it mean the same thing?** [`the_vm_agrees_with_the_cek`] runs each
//!   `@test` on both machines and requires the same answer.
//!
//! The last has to allow for the one documented divergence: the VM does not
//! discharge an *unhandled* `Fs`, `Process`, `Random` or `Time` operation, where
//! the CEK reaches the real world. Those are counted and reported rather than
//! ignored, and a floor on the number that *do* agree keeps the allowance from
//! quietly swallowing everything.

use meadow::{Engine, Options, linker::Linker, runtime, stdlib};
use meadow_compiler::core;

struct Std {
    program: core::Program,
    tests: Vec<(String, core::Var)>,
}

fn std_program() -> Std {
    let (packages, diags) = stdlib::std_packages(Options::debug().entry("result"));
    assert!(
        diags.is_empty(),
        "the standard library should compile cleanly: {:?}",
        diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let linked = Linker::link(packages);
    let tests = linked
        .tests
        .iter()
        .map(|t| (format!("{}.{}", t.package, t.name), t.var))
        .collect();
    Std {
        program: linked.program,
        tests,
    }
}

#[test]
fn the_whole_standard_library_lowers() {
    let std = std_program();
    let lowered = meadow_seq::lower_program(&std.program, Options::debug().entry("result").opt);
    assert!(
        lowered.unsupported.is_empty(),
        "the standard library uses `core` constructs the lowering does not \
         translate: {:?}",
        lowered.unsupported
    );
    // A guard against the assertion above being vacuous.
    assert!(
        lowered.program.defs.len() > 500,
        "only {} blocks — did the library fail to link?",
        lowered.program.defs.len()
    );
}

#[test]
fn the_whole_standard_library_reaches_bytecode() {
    let std = std_program();
    let image = runtime::compile(&std.program, Options::debug().entry("result").opt)
        .expect("the standard library should compile");
    assert!(
        image.code.len() > 10_000,
        "only {} instructions",
        image.code.len()
    );
    assert!(
        image.regs as usize <= 256,
        "a block wanted {} registers",
        image.regs
    );
    // Worth printing: it is the number that says whether the missing register
    // allocator is a real problem yet.
    println!(
        "{} instructions, {} registers at the high-water mark",
        image.code.len(),
        image.regs
    );
}

#[test]
fn the_vm_agrees_with_the_cek() {
    let std = std_program();
    let vars: Vec<core::Var> = std.tests.iter().map(|(_, v)| *v).collect();

    let cek = runtime::run_tests(
        &std.program,
        &vars,
        Engine::Cek,
        Options::debug().entry("result").opt,
    )
    .expect("the CEK runner");
    let vm = runtime::run_tests(
        &std.program,
        &vars,
        Engine::Vm,
        Options::debug().entry("result").opt,
    )
    .expect("the VM runner");

    let mut agreed = 0;
    let mut native = Vec::new();
    let mut disagreed = Vec::new();

    for ((name, _), (a, b)) in std.tests.iter().zip(cek.iter().zip(vm.iter())) {
        match (a, b) {
            (Ok(x), Ok(y)) if x == y => agreed += 1,
            // Both failed. The messages need not match — the CEK reports a
            // failed assertion and the VM may report the same thing a different
            // way — but a test that fails on both is not a translation bug.
            (Err(_), Err(_)) => agreed += 1,
            // The one documented divergence.
            (Ok(_), Err(e)) if e.starts_with("unhandled effect") => {
                native.push(format!("{name}: {e}"));
            }
            (Ok(x), Ok(y)) => disagreed.push(format!("{name}: CEK {x}, VM {y}")),
            (Ok(x), Err(e)) => disagreed.push(format!("{name}: CEK {x}, VM failed: {e}")),
            (Err(e), Ok(y)) => disagreed.push(format!("{name}: CEK failed: {e}, VM {y}")),
        }
    }

    println!(
        "{agreed} agreed, {} needed a native effect, {} disagreed",
        native.len(),
        disagreed.len()
    );
    assert!(
        disagreed.is_empty(),
        "{} of {} standard library tests disagree:\n{}",
        disagreed.len(),
        std.tests.len(),
        disagreed.join("\n")
    );
    // The allowance above must not be doing the work.
    assert!(
        agreed * 4 > std.tests.len() * 3,
        "only {agreed} of {} agreed; {} needed a native effect:\n{}",
        std.tests.len(),
        native.len(),
        native.join("\n")
    );
}

/// Native code compiled in the process answers every standard library test
/// exactly as the bytecode it was compiled from does -- failures included,
/// word for word -- at both levels a user builds at.
#[test]
fn the_jit_agrees_with_the_vm() {
    let std = std_program();
    let vars: Vec<core::Var> = std.tests.iter().map(|(_, v)| *v).collect();
    for opt in [meadow::OptLevel::O1, meadow::OptLevel::O2] {
        let vm = runtime::run_tests(&std.program, &vars, Engine::Vm, opt).expect("the VM runner");
        // Every block that runs, compiled the first time it does.
        let jit = runtime::run_tests_jit_at(&std.program, &vars, Engine::Jit, opt, 1)
            .expect("the JIT runner");
        let disagreed: Vec<String> = std
            .tests
            .iter()
            .zip(vm.iter().zip(jit.iter()))
            .filter(|(_, (a, b))| a != b)
            .map(|((name, _), (a, b))| format!("{name}: VM {a:?}, JIT {b:?}"))
            .collect();
        assert!(
            disagreed.is_empty(),
            "at {}, {} of {} tests disagree:\n{}",
            opt.name(),
            disagreed.len(),
            std.tests.len(),
            disagreed.join("\n")
        );
    }
}

#[test]
fn the_axcut_machine_agrees_too() {
    // The middle of the pipeline, so a disagreement above can be attributed.
    // If this passes and `the_vm_agrees_with_the_cek` does not, the bug is in
    // code generation; if both fail, it is in the lowering.
    //
    // At both levels, because the levels differ *here* — `-O2`'s decision trees
    // are a lowering decision, and this is the test that can say so.
    for opt in [meadow::OptLevel::O1, meadow::OptLevel::O2] {
        let std = std_program();
        let lowered = meadow_seq::lower_program(&std.program, opt);

        let mut checked = 0;
        for (name, var) in &std.tests {
            // Only what the test reaches, as a build lowers: lowering all of
            // `Std` once per test of it was most of an eleven-minute test.
            let program = core::prune::prune(&calling(&std.program, *var));
            let lowered_one = meadow_seq::lower_program(&program, opt);
            let cek = meadow_eval::run(&program);
            let axcut = meadow_seq::machine::Machine::run(&lowered_one.program, 200_000_000);
            let at = opt.name();
            match (&cek, &axcut) {
                (Ok(a), Ok(b)) if a.to_string() == b.to_string() => checked += 1,
                // This machine has no scheduler. Threads and transactions are
                // checked between the CEK and the VM instead, in
                // `glade/tests/differential.rs`.
                (Ok(_), Err(e))
                    if e.msg.contains("green threads are not supported")
                        || e.msg.contains("transactions are not supported") =>
                {
                    checked += 1
                }
                // Nor does it reach outside itself: `Std.Ffi`'s tests call C,
                // and `Std.Net`'s open sockets, and are checked between the
                // CEK and the VM above.
                (Ok(_), Err(e))
                    if e.msg.starts_with("unhandled effect Process.ffi")
                        || e.msg.starts_with("unhandled effect Net.") =>
                {
                    checked += 1
                }
                (Err(_), Err(_)) => checked += 1,
                (Ok(a), Err(e)) => panic!("{name} at {at}: CEK {a}, AxCut failed: {}", e.msg),
                (Ok(a), Ok(b)) => panic!("{name} at {at}: CEK {a}, AxCut {b}"),
                (Err(e), Ok(b)) => panic!("{name} at {at}: CEK failed: {}, AxCut {b}", e.msg),
            }
        }
        assert_eq!(checked, std.tests.len());
        assert!(!lowered.program.defs.is_empty());
    }
}

/// Every value the standard library's tests put anywhere is represented the
/// way the lowering says it is.
///
/// The AxCut machine's values still carry what they are, so it can compare each
/// binding with the name's declared [`meadow_seq::Rep`] -- which is what the
/// collector and native code will go by once values stop carrying it. A test
/// the machine cannot run at all (a thread, a file) is not a failure here; a
/// value in the wrong representation is.
#[test]
fn every_standard_library_value_is_where_its_representation_says() {
    let std = std_program();
    let base = std.program.defs.len();
    let mut defs = std.program.defs.clone();
    for (i, (_, var)) in std.tests.iter().enumerate() {
        defs.push(core::Def {
            var: meadow_compiler::hir::VarId::synthetic(i as u32),
            name: "<test>".into(),
            module: Default::default(),
            poly: std.program.result_of_calling(*var),
            term: core::Term::App(
                std::sync::Arc::new(core::Term::Var(*var)),
                std::sync::Arc::new(core::Term::Lit(core::Lit::Unit)),
            ),
        });
    }
    let whole = core::Program {
        defs,
        ..std.program.clone()
    };
    let lowered = meadow_seq::lower_program(&whole, Options::debug().entry("result").opt);
    let mut wrong = Vec::new();
    let mut ran = 0;
    for (i, (name, _)) in std.tests.iter().enumerate() {
        let label = meadow_seq::Label((base + i) as u32);
        let result = meadow_seq::machine::Machine::at(&lowered.program, label).and_then(|mut m| {
            loop {
                if m.steps > 50_000_000 {
                    break Ok(());
                }
                match m.step() {
                    Ok(Some(_)) => break Ok(()),
                    Ok(None) => {}
                    Err(e) => break Err(e),
                }
            }
        });
        match result {
            Ok(()) => ran += 1,
            Err(e)
                if e.msg.contains(" is declared ") || e.msg.contains("has no representation") =>
            {
                wrong.push(format!("{name}: {}", e.msg))
            }
            Err(_) => {}
        }
    }
    assert!(
        wrong.is_empty(),
        "{} tests hold a value against its representation:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    assert!(
        ran > 150,
        "only {ran} tests ran to the end on the AxCut machine"
    );
}

/// A program whose entry point calls `test` with `()`, which is what the test
/// runner does.
fn calling(program: &core::Program, test: core::Var) -> core::Program {
    let entry = meadow_compiler::hir::VarId::synthetic(0);
    let mut defs = program.defs.clone();
    defs.push(core::Def {
        var: entry,
        name: "<test>".into(),
        module: Default::default(),
        poly: program.result_of_calling(test),
        term: core::Term::App(
            std::sync::Arc::new(core::Term::Var(test)),
            std::sync::Arc::new(core::Term::Lit(core::Lit::Unit)),
        ),
    });
    core::Program {
        defs,
        entry: Some(entry),
        ctor_fields: program.ctor_fields.clone(),
        variants: program.variants.clone(),
        origins: Default::default(),
    }
}

/// Every optimization level computes the same answers.
///
/// The back end's optional passes are the ones nothing else checks: a program
/// only reaches `-O2` when someone builds for release, and a decision tree that
/// picks the wrong arm is a wrong answer rather than a crash. So the standard
/// library's own tests are run at each level and required to agree with the CEK
/// machine, which has no levels at all.
#[test]
fn every_opt_level_agrees_with_the_cek() {
    use meadow::OptLevel;

    let std = std_program();
    let vars: Vec<core::Var> = std.tests.iter().map(|(_, v)| *v).collect();
    let cek =
        runtime::run_tests(&std.program, &vars, Engine::Cek, OptLevel::O1).expect("the CEK runner");

    for opt in [OptLevel::O0, OptLevel::O1, OptLevel::O2] {
        let vm = runtime::run_tests(&std.program, &vars, Engine::Vm, opt)
            .unwrap_or_else(|e| panic!("the VM runner at {}: {e}", opt.name()));

        let mut agreed = 0;
        let mut disagreed = Vec::new();
        for ((name, _), (a, b)) in std.tests.iter().zip(cek.iter().zip(vm.iter())) {
            match (a, b) {
                (Ok(x), Ok(y)) if x == y => agreed += 1,
                (Err(_), Err(_)) => agreed += 1,
                // The documented divergence: no native effects in the VM.
                (Ok(_), Err(e)) if e.starts_with("unhandled effect") => {}
                (Ok(x), Ok(y)) => disagreed.push(format!("{name}: CEK {x}, {} {y}", opt.name())),
                (Ok(x), Err(e)) => {
                    disagreed.push(format!("{name}: CEK {x}, {} failed: {e}", opt.name()))
                }
                (Err(e), Ok(y)) => {
                    disagreed.push(format!("{name}: CEK failed: {e}, {} {y}", opt.name()))
                }
            }
        }
        assert!(
            disagreed.is_empty(),
            "{} of {} standard library tests disagree at {}:\n{}",
            disagreed.len(),
            std.tests.len(),
            opt.name(),
            disagreed.join("\n")
        );
        assert!(
            agreed * 4 > std.tests.len() * 3,
            "only {agreed} of {} agreed at {}",
            std.tests.len(),
            opt.name()
        );
    }
}

/// `-O2` compiles a `match` to one `switch` rather than one per arm, where it
/// is not one already.
///
/// A switch testing several constructors is the observable difference, and it
/// is what would notice the gate being wired to nothing — which is the way an
/// option like this usually fails. Not the number of switches: `-O2` also
/// copies generic code per representation, so it has more code to count.
#[test]
fn case_trees_are_what_o2_turns_on() {
    use meadow::OptLevel;

    let std = std_program();
    let counts: Vec<usize> = [OptLevel::O1, OptLevel::O2]
        .iter()
        .map(|opt| {
            let lowered = meadow_seq::lower_program(&std.program, *opt);
            lowered
                .program
                .defs
                .iter()
                .map(|d| switches(&d.block.body, 3))
                .sum()
        })
        .collect();

    // A chain tests one constructor a switch; a tree tests them all in one.
    // (Every `perform`'s search block tests two, at every level.) A `match`
    // with an unguarded arm for every constructor is one `switch` at every
    // level -- it is smaller as well as faster -- so `-O1` has some; what `-O2`
    // adds is the tree for the rest, whose arms can fall through.
    assert!(
        counts[1] > counts[0],
        "O2 builds no more decision trees than O1 ({} against {}) — the gate is doing nothing",
        counts[1],
        counts[0]
    );
}

/// How many `switch` statements a block contains, counting into every branch.
/// The switches in `s` testing at least `least` constructors.
fn switches(s: &meadow_seq::Statement, least: usize) -> usize {
    use meadow_seq::Statement::*;
    match s {
        Substitute(_, b) => switches(&b.body, least),
        Jump(_) => 0,
        Let { rest, .. } => switches(rest, least),
        Switch { arms, default, .. } => {
            usize::from(arms.len() >= least)
                + arms
                    .iter()
                    .map(|(_, b)| switches(&b.body, least))
                    .sum::<usize>()
                + switches(&default.body, least)
        }
        New { methods, rest, .. } => {
            methods
                .iter()
                .map(|b| switches(&b.body, least))
                .sum::<usize>()
                + switches(rest, least)
        }
        Invoke(..) => 0,
        Extern { blocks, .. } => blocks.iter().map(|b| switches(&b.body, least)).sum(),
        Mark(_, rest) => switches(rest, least),
        Error(_) => 0,
    }
}

/// A debug build keeps source positions all the way to the bytecode, and must
/// mean exactly what an ordinary build means: a debugger that changed the
/// answer would be worse than none.
#[test]
fn a_debug_build_runs_the_standard_library_the_same() {
    let plain = std_program();
    let mut options = Options::debug().entry("result");
    options.debug_info = true;
    let (packages, diags) = stdlib::std_packages(options);
    assert!(
        diags.is_empty(),
        "{:?}",
        diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let linked = Linker::link(packages);
    let debug_tests: Vec<(String, core::Var)> = linked
        .tests
        .iter()
        .map(|t| (format!("{}.{}", t.package, t.name), t.var))
        .collect();
    assert_eq!(
        plain.tests.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        debug_tests.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );

    let opt = options.opt;
    let plain_vars: Vec<core::Var> = plain.tests.iter().map(|(_, v)| *v).collect();
    let debug_vars: Vec<core::Var> = debug_tests.iter().map(|(_, v)| *v).collect();
    let before = runtime::run_tests(&plain.program, &plain_vars, Engine::Vm, opt).expect("plain");
    let after = runtime::run_tests(&linked.program, &debug_vars, Engine::Vm, opt).expect("debug");
    let differ: Vec<String> = plain
        .tests
        .iter()
        .zip(before.iter().zip(&after))
        .filter(|(_, (a, b))| a != b)
        .map(|((name, _), (a, b))| format!("{name}: {a:?} vs {b:?}"))
        .collect();
    assert!(
        differ.is_empty(),
        "a debug build changed the answer:\n{}",
        differ.join("\n")
    );

    // And recording the debug information does not change a single instruction.
    let lowered = meadow_seq::lower_program(&linked.program, opt);
    let code = meadow_codegen::compile(&lowered.program).expect("compiles");
    let with = meadow_codegen::compile_with_debug_info(&lowered.program).expect("compiles");
    assert_eq!(code.code, with.code);
    let debug = with.debug.expect("debug information");
    assert_eq!(debug.locs.len(), with.code.len());
    let known = debug.locs.iter().filter(|l| l.is_some()).count();
    assert!(
        known > with.code.len() / 2,
        "most instructions should know where they came from"
    );
}
