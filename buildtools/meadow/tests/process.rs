//! `Std.Process`: subprocesses and process-level operations.

mod common;
use common::eval_main_std;

#[test]
fn run_captures_output() {
    insta::assert_snapshot!(eval_main_std(
        "def result =\n\
        \x20 match run \"echo\" [\"hi\"] with\n\
        \x20 | Ok o -> (outputStatus o, outputStdout o, succeeded o)\n\
        \x20 | Err e -> (0 - 1, e, False)\n"
    ), @"(0, \"hi\\n\", True)");
}

#[test]
fn nonexistent_program_is_an_err() {
    insta::assert_snapshot!(eval_main_std(
        "def result = match run \"this-program-does-not-exist-xyz\" [] with | Ok o -> False | Err e -> True\n"
    ), @"True");
}

#[test]
fn effect_can_be_handled() {
    // a handler intercepts `Process` so the runtime never spawns anything
    insta::assert_snapshot!(eval_main_std(
        "def result =\n\
        \x20 handle run \"real\" [] with {\n\
        \x20   spawn cmd k -> k (Ok (0, \"mocked\", \"\")),\n\
        \x20   return x -> x\n\
        \x20 }\n"
    ), @"Ok((0, \"mocked\", \"\"))");
}

#[test]
fn command_builder() {
    insta::assert_snapshot!(eval_main_std(
        "def cmd = withArg \"hi\" (withArg \"-n\" (command \"echo\"))\n\
         def result = match spawn cmd with | Ok o -> outputStdout o | Err e -> e\n"
    ), @"\"hi\"");
}

#[test]
fn is_terminal_answers_for_the_three_streams_and_no_other() {
    // Whether stdout is a terminal depends on how the tests are run, so only
    // that it answers is asserted; a descriptor past stderr is never one.
    insta::assert_snapshot!(eval_main_std(
        "use Std.Process (isTerminal)\n\
         def result = (isTerminal 1 or True, isTerminal 9)\n"
    ), @"(True, False)");
}

#[test]
fn current_pid_is_positive() {
    insta::assert_snapshot!(eval_main_std("def result = currentPid () > 0\n"), @"True");
}

/// The directory a program runs in, on every backend: each answers it from
/// code of its own, and Silo's and Glade's executables run apart from `meadow`.
#[test]
fn every_backend_says_where_the_program_runs() {
    let dir = std::env::temp_dir().join(format!("meadow-current-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Meadow.toml"), "[package]\nname = \"Here\"\n").unwrap();
    std::fs::write(
        dir.join("src/Main.mw"),
        "use Std.Process (currentDir)\n\nfun main () = println (currentDir ())\n",
    )
    .unwrap();
    let here = std::fs::canonicalize(&dir).unwrap();
    for engine in [
        &["--backend", "vm"][..],
        &["--backend", "jit"],
        &["--backend", "aot"],
        &["--cek"],
        &["--runtime", "silo"],
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_meadow"))
            .current_dir(&dir)
            .arg("run")
            .args(engine)
            .arg(".")
            .output()
            .expect("the meadow binary runs");
        assert!(
            out.status.success(),
            "{engine:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let said = String::from_utf8_lossy(&out.stdout);
        let said = std::path::Path::new(said.trim());
        assert!(said.is_absolute(), "{engine:?} said {said:?}");
        assert_eq!(std::fs::canonicalize(said).unwrap(), here, "{engine:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn env_roundtrip() {
    insta::assert_snapshot!(eval_main_std(
        "def result =\n\
        \x20 let a = getEnv \"MEADOW_TEST_VAR_DOES_NOT_EXIST\" in\n\
        \x20 let b = setEnv (\"MEADOW_TEST_VAR_DOES_NOT_EXIST\", \"x\") in\n\
        \x20 let c = getEnv \"MEADOW_TEST_VAR_DOES_NOT_EXIST\" in\n\
        \x20 let d = removeEnv \"MEADOW_TEST_VAR_DOES_NOT_EXIST\" in\n\
        \x20 (a, c)\n"
    ), @"(None, Just(\"x\"))");
}

#[test]
fn many_arguments_cross_the_boundary_as_a_vector() {
    // Forty arguments is past one chunk, and `withArg` builds the vector by
    // pushing, so the runtime reads a shape it did not build itself.
    let src = "def cmd = V.foldl (\\c i -> withArg (show i) c) (command \"echo\") (V.range 0 40)\n\
               def result = match spawn cmd with | Ok o -> String.byteLength (outputStdout o) | Err e -> 0\n";
    let src = format!("use Std.Collections.Vector as V\nuse Std.String as String\n{src}");
    // "0 1 ... 39\n": ten one-digit and thirty two-digit numbers, 39 spaces, a newline.
    assert_eq!(eval_main_std(&src), "110");
    assert_eq!(common::cek_main_std(&src), "110");
}
