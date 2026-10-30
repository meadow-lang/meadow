//! `meadow test` runs tests side by side: the results are the ones running
//! them in order gives, in the order they were declared, and what a test
//! prints stays with that test.

use meadow::{Engine, Options, pipeline, runtime};
use std::sync::Mutex;

fn scratch(who: &str) -> meadow_scratch::Scratch {
    let dir = meadow_scratch::scratch!("meadow-partest-{}-{who}", std::process::id());
    std::fs::create_dir_all(dir.join("src")).unwrap();
    dir.canonical()
}

/// Twelve tests that each print their own number a few times, with work in
/// between so that they overlap; every third fails.
fn package(who: &str) -> meadow_scratch::Scratch {
    let root = scratch(who);
    std::fs::write(root.join("Meadow.toml"), "[package]\nname = \"Par\"\n").unwrap();
    let mut src = String::from(
        "use Std.Test (assertEq)\n\
         fun fib (n : Int) : Int = if n < 2 then n else fib (n - 1) + fib (n - 2)\n\
         fun say (k : Int) = let _ = println (\"line from \" ++ show k) in fib 15\n",
    );
    for k in 0..12 {
        let want = if k % 3 == 2 { 0 } else { 610 };
        src.push_str(&format!(
            "@test fun t{k} () = let _ = say {k} in let _ = say {k} in assertEq (say {k}) {want} \"t{k}\"\n"
        ));
    }
    src.push_str("def result = ()\n");
    std::fs::write(root.join("src/Main.mw"), src).unwrap();
    root
}

#[test]
fn tests_run_side_by_side_answer_as_they_do_in_order() {
    let root = package("agree");
    let out = pipeline::build(&root, Options::debug().entry("result"));
    assert!(
        out.diagnostics.is_empty(),
        "{:?}",
        out.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let linked = out.linked.expect("linked");
    let vars: Vec<_> = linked
        .tests
        .iter()
        .filter(|t| &*t.package == "Par")
        .map(|t| t.var)
        .collect();
    assert_eq!(vars.len(), 12);
    let opt = Options::debug().entry("result").opt;

    for engine in [Engine::Vm, Engine::Jit, Engine::Cek] {
        let in_order: Vec<Result<String, String>> =
            runtime::run_tests_parallel(&linked.program, &vars, engine, opt, 1, true, &|_, _| {})
                .unwrap()
                .into_iter()
                .map(|t| t.result)
                .collect();
        let heard = Mutex::new(Vec::new());
        let together =
            runtime::run_tests_parallel(&linked.program, &vars, engine, opt, 6, true, &|i, t| {
                heard.lock().unwrap().push((i, t.result.is_ok()));
            })
            .unwrap();

        // Declaration order, whatever order they finished in.
        let results: Vec<_> = together.iter().map(|t| t.result.clone()).collect();
        assert_eq!(results, in_order, "{engine:?}");
        for (k, told) in together.iter().enumerate() {
            assert_eq!(told.result.is_ok(), k % 3 != 2, "{engine:?}: t{k}");
            // Its own three lines, and nobody else's.
            assert_eq!(
                told.output,
                format!("line from {k}\n").repeat(3),
                "{engine:?}: t{k}"
            );
        }
        // Every test was reported once, as it finished.
        let mut heard = heard.into_inner().unwrap();
        heard.sort();
        let expected: Vec<(usize, bool)> = (0..12).map(|k| (k, k % 3 != 2)).collect();
        assert_eq!(heard, expected, "{engine:?}");
    }
}

#[test]
fn the_command_keeps_a_passing_tests_output_and_shows_a_failing_ones() {
    let root = package("cli");
    let run = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_meadow"))
            .current_dir(&root)
            .args(args)
            .output()
            .expect("the meadow binary runs");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let (ok, stdout) = run(&["test"]);
    assert!(!ok, "four tests fail");
    assert!(
        stdout.contains("test result: FAILED. 8 passed; 4 failed"),
        "{stdout}"
    );
    // A failing test's output is under its name; a passing one's is nowhere.
    assert!(
        stdout.contains("---- t2 output ----\nline from 2\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("line from 1\n"), "{stdout}");
    // Failures are listed as declared: t2, t5, t8, t11.
    let at = |name: &str| stdout.find(&format!("---- {name} ----")).expect(name);
    assert!(at("t2") < at("t5") && at("t5") < at("t8") && at("t8") < at("t11"));

    // Asked not to keep it, everything a test prints is written.
    let (_, stdout) = run(&["test", "--no-capture", "--test-threads", "1"]);
    assert_eq!(stdout.matches("line from 1\n").count(), 3, "{stdout}");
}
