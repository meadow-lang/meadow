//! A program of Cut built for debugging, debugged on Glade through the
//! session another program holds open (`meadow::dap::hosted`, which
//! `meadow-host`'s `meadow_debug` hands requests to): where its positions
//! say it is, the stack by the names its definitions are shown by, and its
//! locals by theirs.

use serde_json::{Value, json};
use std::path::PathBuf;

/// The source the positions are in, and where each line of it starts.
const SOURCE: &str = "fun double x =
  x + x

fun main () =
  let y = double 21 in
  let z = y + 1 in
  ()
";

/// Bytes `from..to` of `what`, where it is first found in [`SOURCE`].
fn span(what: &str) -> (usize, usize) {
    let at = SOURCE.find(what).expect("in the source");
    (at, at + what.len())
}

fn ask(req: Value) -> Value {
    let answer = meadow::dap::hosted::request(&req);
    assert!(answer.get("error").is_none(), "{req} was refused: {answer}");
    answer
}

/// The program, written to a directory of its own: its source, and its Cut
/// with a position on each statement a step may stop at.
fn written(tag: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("meadow-dap-cut-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let source = dir.join("src").join("Main.mw");
    std::fs::write(&source, SOURCE).unwrap();
    let at = |what: &str| {
        let (a, b) = span(what);
        format!("@t:Main/\"src/Main.mw\":{a}-{b}")
    };
    let cut = format!(
        "cut 0
entry t:Main/main
answer none
source t:Main/\"src/Main.mw\" = {path:?}

def t:Main/double @\"double\" (x: i64 @\"x\" \"Int\"; k: ptr) =
  {body} prim add(x, x; k)

def t:Main/main @\"main\" (; k: ptr) =
  {call} t:Main/double(21; μ̃ y: i64 @\"y\" \"Int\".
    {next} prim add(y, 1; μ̃ z: i64 @\"z\" \"Int\".
      {end} <unit | k>))
",
        path = source.display().to_string(),
        body = at("x + x"),
        call = at("double 21"),
        next = at("y + 1"),
        end = at("()\n"),
    );
    let program = dir.join("Main.cut");
    std::fs::write(&program, cut).unwrap();
    (program, source)
}

#[test]
fn a_program_of_cut_is_stepped_and_shown_by_its_source() {
    let (program, source) = written("stepped");
    // Launched, it stands at its first boundary: the call in `main`.
    ask(json!({ "do": "launch", "program": program }));
    let frames = ask(json!({ "do": "frames", "max": 10 }));
    let top = &frames["frames"][0];
    assert_eq!(top["name"], "main", "{frames}");
    assert_eq!(top["line"], 5, "{frames}");

    // A breakpoint on the body of `double`, and one where there is nothing.
    let placed = ask(json!({ "do": "breakpoints", "path": source, "lines": [2, 40] }));
    assert_eq!(placed["lines"], json!([2, null]), "{placed}");

    ask(json!({ "do": "resume", "mode": "continue" }));
    let ran = ask(json!({ "do": "run", "budget": 100_000 }));
    assert_eq!(ran["stop"]["reason"], "breakpoint", "{ran}");
    let frames = ask(json!({ "do": "frames", "max": 10 }));
    let names: Vec<&str> = frames["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["double", "main"], "{frames}");
    assert_eq!(frames["frames"][0]["line"], 2);
    assert_eq!(frames["frames"][1]["line"], 5, "where main waits");
    assert!(
        frames["frames"][0]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("Main.mw")),
        "{frames}"
    );

    let scopes = ask(json!({ "do": "scopes", "frame": 0 }));
    let locals = scopes["scopes"][0]["reference"].as_u64().unwrap();
    let vars = ask(json!({ "do": "variables", "reference": locals }));
    let x = vars["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "x")
        .unwrap_or_else(|| panic!("x among the locals: {vars}"));
    assert_eq!(x["value"], "21");
    assert_eq!(x["type"], "Int");
    let evaluated = ask(json!({ "do": "evaluate", "frame": 0, "expression": "x" }));
    assert_eq!(evaluated["variable"]["value"], "21", "{evaluated}");

    // A line with no code is moved to the next that has some.
    let placed = ask(json!({ "do": "breakpoints", "path": source, "lines": [3] }));
    assert_eq!(placed["lines"], json!([5]), "{placed}");

    // Out of `double`, back in `main` with `y` bound.
    ask(json!({ "do": "resume", "mode": "stepOut" }));
    let ran = ask(json!({ "do": "run", "budget": 100_000 }));
    assert_eq!(ran["stop"]["reason"], "step", "{ran}");
    let frames = ask(json!({ "do": "frames", "max": 10 }));
    assert_eq!(frames["frames"][0]["name"], "main", "{frames}");
    assert_eq!(frames["frames"][0]["line"], 6, "{frames}");
    let y = ask(json!({ "do": "evaluate", "frame": 0, "expression": "y" }));
    assert_eq!(y["variable"]["value"], "42", "{y}");

    // And to the end.
    ask(json!({ "do": "resume", "mode": "continue" }));
    let ran = ask(json!({ "do": "run", "budget": 100_000 }));
    assert_eq!(ran["stop"]["reason"], "exited", "{ran}");
    ask(json!({ "do": "end" }));
    let _ = std::fs::remove_dir_all(program.parent().unwrap());
}

/// What is there for a debugger changes nothing a program is compiled to:
/// `meadow cut` takes it off, and Glade's bytecode and Silo's LLVM are the
/// same, to the byte, as those of the program written without it.
#[test]
fn a_program_built_for_debugging_runs_as_built_without() {
    let (program, _) = written("built");
    let text = std::fs::read_to_string(&program).unwrap();
    let mut plain = meadow_cut::parse(&text).unwrap();
    meadow_cut::strip::program(&mut plain);
    let bare = program.with_file_name("Bare.cut");
    std::fs::write(&bare, meadow_cut::print::program(&plain)).unwrap();
    assert!(!std::fs::read_to_string(&bare).unwrap().contains('@'));
    let image = |p: &std::path::Path| {
        let lowered = meadow::cut::lowered(p).unwrap();
        meadow_bytecode::image::encode(&meadow_codegen::compile(&lowered).unwrap())
    };
    assert_eq!(image(&program), image(&bare), "Glade's bytecode");
    for what in ["axcut", "llvm"] {
        assert_eq!(
            meadow::cut::emitted(&program, what).unwrap(),
            meadow::cut::emitted(&bare, what).unwrap(),
            "{what}"
        );
    }
    let _ = std::fs::remove_dir_all(program.parent().unwrap());
}
