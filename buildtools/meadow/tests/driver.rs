//! What a program run by `meadow` gets from the command line and gives back to
//! it: its arguments, bytes written as they are, a bytecode image run or linked
//! on its own -- and nothing at all when it does not compile.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(who: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-driver-{}-{who}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A package at `dir/name` whose `main` is `main`. The directory is called
/// what this test calls it; the package is called what a package is called.
fn package(dir: &Path, name: &str, main: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Meadow.toml"),
        format!(
            "[package]\nname = \"{}\"\n",
            meadow::package::as_package_name(name)
        ),
    )
    .unwrap();
    std::fs::write(root.join("src/Main.mw"), main).unwrap();
    root
}

fn meadow(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the meadow binary runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Fifty threads, each keeping a compact in a table in a `Ref` that is written
/// under a handler -- what a query's thread keeps its answers in -- and
/// ending. Found by Lingua's session, reduced from a database whose answers
/// were never let go.
const KEPT_IN_A_REF: &str = r#"use Std.Collections.Vector as V
use Std.Collections.HashMap as HashMap
use Std.Collections.HashMap (HashMap)
use Std.Compact as Compact
use Std.Thread as Thread

data Box = Box (Ref (HashMap Int (Compact [Int])))

effect Ask { ask : Int -> Int }

fun through (b : Box) (n : Int) : Int ! { Mut | e } =
  handle (ask n + 1) with {
    ask k resume ->
      (match b with
       | Box r ->
           let c = Compact.make (V.range 0 k) in
           let u = setRef r (HashMap.insert k c (getRef r)) in
           resume (V.len (Compact.get c))),
    return x -> x
  }

fun inBox (n : Int) = through (Box (newRef HashMap.empty)) n

fun main () =
  let tasks = V.map (\i -> Thread.spawn (\() -> inBox (1000 + i))) (V.range 0 50) in
  println (show (V.foldl (\a b -> a + b) 0 (Thread.awaitAll tasks)))
"#;

/// A `Ref` that has been written is a candidate for Silo's cycle collector,
/// and one that dies while it is a candidate is only marked, for the
/// collector to free when it next runs. A thread that ended first never ran
/// it: the `Ref`, the table in it and every region the table held stayed for
/// the rest of the program.
#[test]
fn what_a_thread_kept_in_a_ref_goes_when_the_thread_does() {
    let dir = scratch("kept-in-a-ref");
    let root = package(&dir, "kept-in-a-ref", KEPT_IN_A_REF);
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(&root)
        .env("MEADOW_SILO_LEAKS", "1")
        .args(["run", "--runtime", "silo", "."])
        .output()
        .expect("the meadow binary runs");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert_eq!(answer(&String::from_utf8_lossy(&out.stdout)), "51275");
    assert!(err.contains("aot: 0 regions live at exit"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hundred `TVar`s, made and read and let go of on five threads, each
/// holding a vector.
const VARS_LET_GO: &str = r#"use Std.Stm as T
use Std.Collections.Vector as V
use Std.Thread as Thread

fun round (i : Int) =
  let tv = T.newTVarIO (V.range 0 (100 + i)) in
  let u = T.atomically (\() -> T.modifyTVar tv (\xs -> V.pushBack xs i)) in
  V.len (T.readTVarIO tv)

fun go (i : Int) (acc : Int) = if i == 0 then acc else go (i - 1) (acc + round i)

fun main () =
  let tasks = V.map (\k -> Thread.spawn (\() -> go 20 0)) (V.range 0 4) in
  println (show (go 20 0 + V.foldl (\a b -> a + b) 0 (Thread.awaitAll tasks)))
"#;

/// A `TVar` keeps its value in a region of its own, and was kept for the
/// whole run: nothing said when the last handle to one had gone. A program
/// that made them as it went -- a database with one for each answer -- kept
/// every value it had ever put in one. Handles are counted now, wherever
/// they are, and a `TVar` nothing can reach lets its value go.
#[test]
fn a_tvar_nothing_can_reach_lets_its_value_go() {
    let dir = scratch("vars-let-go");
    let root = package(&dir, "vars-let-go", VARS_LET_GO);
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(&root)
        .env("MEADOW_SILO_LEAKS", "1")
        .args(["run", "--runtime", "silo", "."])
        .output()
        .expect("the meadow binary runs");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert_eq!(answer(&String::from_utf8_lossy(&out.stdout)), "11150");
    assert!(err.contains("aot: 0 regions live at exit"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The last line a run printed.
fn answer(stdout: &str) -> &str {
    stdout.lines().last().unwrap_or("")
}

const ARGS: &str = "use Std.Process\nuse Std.Collections.Vector as V\n\nfun main () = println \"${V.toList (argv ()):?}\"\n";

#[test]
fn a_program_gets_what_follows_the_double_dash() {
    let dir = scratch("args");
    let root = package(&dir, "args", ARGS);
    for engine in [&[][..], &["--backend", "vm"], &["--cek"]] {
        let mut args = vec!["run"];
        args.extend_from_slice(engine);
        args.extend([".", "--", "one", "two words", "--three"]);
        let (ok, out, err) = meadow(&root, &args);
        assert!(ok, "{err}");
        assert_eq!(
            answer(&out),
            r#"["one"; "two words"; "--three"]"#,
            "{engine:?}"
        );
    }
    let (ok, out, err) = meadow(&root, &["run", "."]);
    assert!(ok, "{err}");
    assert_eq!(answer(&out), "[;]", "and nothing, given nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Prints, says why it gives up on standard error, and stops with status 3:
/// giving up, made of the runtime's basics.
const GIVES_UP: &str = "use Std.Process (exit)\n\nfun main () =\n  let _ = println \"before\" in\n  let _ = eprintln \"gave up: no input\" in\n  exit 3\n";

#[test]
fn a_program_gives_up_on_standard_error_on_every_engine() {
    let dir = scratch("gives-up");
    let root = package(&dir, "gives-up", GIVES_UP);
    for engine in [
        &[][..],
        &["--backend", "vm"],
        &["--cek"],
        &["--runtime", "silo"],
    ] {
        let mut args = vec!["run"];
        args.extend_from_slice(engine);
        args.push(".");
        let (ok, out, err) = meadow(&root, &args);
        assert!(!ok, "{engine:?}: exit 3 is a failure");
        assert_eq!(
            answer(&out),
            "before",
            "{engine:?}: stdout keeps what it printed"
        );
        assert!(
            err.contains("gave up: no input\n"),
            "{engine:?}: the message is on stderr: {err}"
        );
        assert!(!out.contains("gave up"), "{engine:?}: and only there");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bytes_are_written_as_they_are() {
    let dir = scratch("bytes");
    let root = package(
        &dir,
        "bytes",
        "use Std.Fs\n\nfun main () =\n  let w = writeBytes (\"out.bin\", #[0xff, 0x00, 0x41, 0x80]) in\n  println \"${(w, readBytes \"out.bin\"):?}\"\n",
    );
    for engine in [&[][..], &["--backend", "vm"], &["--cek"]] {
        let mut args = vec!["run"];
        args.extend_from_slice(engine);
        args.push(".");
        let (ok, out, err) = meadow(&root, &args);
        assert!(ok, "{err}");
        assert_eq!(answer(&out), "(Ok(()), Ok(#[255, 0, 65, 128]))");
        assert_eq!(
            std::fs::read(root.join("out.bin")).unwrap(),
            [0xff, 0, 0x41, 0x80]
        );
        std::fs::remove_file(root.join("out.bin")).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_image_runs_on_its_own() {
    let dir = scratch("exec");
    let root = package(&dir, "img", ARGS);
    let (ok, _, err) = meadow(&root, &["build", "."]);
    assert!(ok, "{err}");
    let image = root.join("target/debug/bytecode/Img.mbc");
    assert!(image.is_file());
    let image = image.to_string_lossy().into_owned();
    for backend in ["jit", "vm"] {
        let (ok, out, err) = meadow(&dir, &["exec", &image, "--backend", backend, "--", "x"]);
        assert!(ok, "{err}");
        assert_eq!(answer(&out), r#"["x"]"#);
    }
    let (ok, _, err) = meadow(&dir, &["exec", &root.join("Meadow.toml").to_string_lossy()]);
    assert!(!ok);
    assert!(err.contains("not a Meadow image"), "{err}");

    // An executable from it, where there is a runtime library to link.
    let exe = dir
        .join("out")
        .join(format!("img{}", std::env::consts::EXE_SUFFIX));
    let (ok, _, err) = meadow(&dir, &["link", &image, "-o", &exe.to_string_lossy()]);
    if !ok {
        assert!(
            err.contains("runtime"),
            "a link failure other than a missing runtime: {err}"
        );
        eprintln!("skipping the executable: {err}");
    } else {
        let out = Command::new(&exe).args(["a", "b"]).output().unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains(r#"["a"; "b"]"#));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_program_that_does_not_compile_is_not_run() {
    let dir = scratch("errors");
    let root = package(&dir, "broken", "fun main () = println (1 + \"one\")\n");
    let (ok, out, err) = meadow(&root, &["run", "."]);
    assert!(!ok);
    assert!(err.contains("mismatch") || err.contains("integer"), "{err}");
    assert!(
        !out.contains("=>") && !err.contains("ill-formed"),
        "{out}{err}"
    );
    let (ok, _, _) = meadow(&root, &["build", "."]);
    assert!(!ok);
    assert!(
        !root.join("target/debug/bytecode/Broken.mbc").exists(),
        "and no image is written for it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `meadow` in `dir`, with `input` on its standard input.
fn meadow_fed(dir: &Path, args: &[&str], input: &str) -> (bool, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(dir)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the meadow binary runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn a_message_framed_by_its_length_is_read_whole() {
    // The Language Server Protocol's framing: a header, a blank line, and a
    // body that no newline ends -- `readLine` for the first two, `readExact`
    // for the last. The two read through one buffer, so they take turns.
    let dir = scratch("framed");
    let root = package(
        &dir,
        "framed",
        "use Std.Console (readLine, readExact)
use Std.String as S

fun loop (n : Int) =
  match readLine () with
  | None -> println \"${show n} read\"
  | Just header ->
      let blank = readLine () in
      match S.toInt (S.trim (S.drop header 15)) with
      | None -> println \"bad header\"
      | Just k -> (match readExact k with
          | Just body -> (let u = println \"[${body}]\" in loop (n + 1))
          | None -> println \"cut short\")

fun main () = loop 0
",
    );
    let input = "Content-Length: 7\r\n\r\n{\"a\":1}Content-Length: 14\r\n\r\n{\"method\":\"x\"}Content-Length: 50\r\n\r\nshort";
    for backend in ["jit", "vm"] {
        let (ok, out) = meadow_fed(&root, &["run", "--backend", backend], input);
        assert!(ok, "{backend}: {out}");
        assert!(
            out.contains("[{\"a\":1}]\n[{\"method\":\"x\"}]\ncut short\n"),
            "{backend}: {out}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// --- deep programs ------------------------------------------------------------------
//
// The compiler walks a program recursively, and ran on a main thread whose
// stack is 1 MB on Windows: a few hundred nested `let`s, three hundred chained
// `+`s or a literal of a few hundred elements killed the process with a stack
// overflow. `meadow` now runs on a thread with room.

/// Build and run `main` with the binary, and answer what it printed last.
#[track_caller]
fn runs(who: &str, main: &str) -> String {
    let dir = scratch(who);
    let root = package(&dir, who, main);
    let (ok, stdout, stderr) = meadow(&root, &["run"]);
    assert!(ok, "{who} failed:\n{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
    answer(&stdout).to_string()
}

#[test]
fn many_nested_lets_compile() {
    let n = 600;
    let mut src = String::from("fun main () =\n");
    for i in 0..n {
        src.push_str(&format!("  let x{i} = {i} in\n"));
    }
    src.push_str(&format!("  println x{}\n", n - 1));
    assert_eq!(runs("nested_lets", &src), format!("{}", n - 1));
}

#[test]
fn a_long_chain_of_additions_compiles() {
    let n = 600;
    let terms: Vec<String> = (1..=n).map(|i| i.to_string()).collect();
    let src = format!("fun main () = println ({})\n", terms.join(" + "));
    assert_eq!(runs("long_sum", &src), format!("{}", n * (n + 1) / 2));
}

#[test]
fn a_long_literal_compiles() {
    // Past the register file: see `meadow_codegen`, "Spilling".
    let n = 800;
    let items: Vec<String> = (1..=n).map(|i| i.to_string()).collect();
    let src = format!(
        "use Std.Collections.Vector as V\n\nfun main () = println (V.len [{}])\n",
        items.join(", ")
    );
    assert_eq!(runs("long_literal", &src), format!("{n}"));
}

#[test]
fn a_macro_a_hundred_calls_deep_compiles() {
    let args: Vec<String> = (1..=120).map(|i| i.to_string()).collect();
    let src = format!(
        "macro sum\n  | ($x) -> {{ $x }}\n  | ($x, $( $r ),+) -> {{ $x + sum!($( $r ),+) }}\n\n\
         fun main () = println (sum!({}))\n",
        args.join(", ")
    );
    assert_eq!(runs("deep_macro", &src), "7260");
}

/// A program as a front end hands one to the back end: data taken apart, an
/// object called, a top-level value, a loop, text printed by the runtime as
/// it goes and a string for an answer.
const CUT: &str = r#"cut 0
entry t:Main/main
answer str

native {
  t:Main/Console.say = Console.writeOutput
}

effect t:Main/Console { say(str) -> unit }

data t:Main/Nat { Z; S(ptr) }

val t:Main/two : ptr =
  <t:Main/Nat.S(t:Main/Nat.S(t:Main/Nat.Z)) | halt>

def t:Main/count (n: ptr, acc: i64; k: ptr) =
  <n | case {
    t:Main/Nat.Z => <acc | k>;
    t:Main/Nat.S(m: ptr) => prim add(acc, 1; μ̃ a: i64. t:Main/count(m, a; k))
  }>

def t:Main/down (n: i64, acc: i64; k: ptr) =
  prim eq(n, 0;
    μ̃ u: unit. prim sub(n, 1; μ̃ m: i64. prim add(acc, 2; μ̃ a: i64. t:Main/down(m, a; k))),
    μ̃ u: unit. <acc | k>)

def t:Main/main (; k: ptr) =
  perform t:Main/Console.say("counting\n"; μ̃ u: unit.
    t:Main/count(t:Main/two, 0; μ̃ c: i64.
      <cocase { apply(x: i64; k1: ptr) => t:Main/down(100000, x; k1) } | apply(c; μ̃ r: i64.
        prim eq(r, 200002; μ̃ w: unit. <"wrong" | k>, μ̃ w: unit. <"200002" | k>))>))
"#;

/// The images kept of the programs of Cut in `dir`, by name.
fn cut_images(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found: Vec<_> = std::fs::read_dir(dir.join("target").join("cut"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    found.retain(|p: &std::path::PathBuf| p.extension().is_some_and(|x| x == "image"));
    found.sort();
    found
}

/// A program of Cut that is run again -- the one a front end builds of a
/// package's macros is run for every round of a build -- is not read and
/// lowered again: its image is kept beside it, named for its text, and that
/// is what runs. A program that changed has an image of its own, and one
/// that cannot be read is made again.
#[test]
fn a_program_of_cut_run_again_runs_the_image_kept_of_it() {
    let dir = scratch("cut-image");
    let other = scratch("cut-image-other");
    let said = "two hundred thousand and two";
    std::fs::write(dir.join("count.cut"), CUT).unwrap();
    std::fs::write(
        other.join("count.cut"),
        CUT.replace("\"200002\"", &format!("\"{said}\"")),
    )
    .unwrap();

    let (ok, out, err) = meadow(&dir, &["cut", "count.cut"]);
    assert!(ok, "{err}");
    assert_eq!(out, "counting\n200002");
    let kept = cut_images(&dir);
    assert_eq!(kept.len(), 1, "{kept:?}");
    let (_, out, _) = meadow(&dir, &["cut", "count.cut"]);
    assert_eq!(out, "counting\n200002");
    assert_eq!(cut_images(&dir), kept, "the same text has the same image");

    // It is the image that runs: with another program's in its place, the
    // other program's answer is what comes back.
    let (ok, out, err) = meadow(&other, &["cut", "count.cut"]);
    assert!(ok, "{err}");
    assert_eq!(out, format!("counting\n{said}"));
    let theirs = cut_images(&other);
    assert_eq!(theirs.len(), 1, "{theirs:?}");
    assert_ne!(
        theirs[0].file_name(),
        kept[0].file_name(),
        "named for its text"
    );
    std::fs::copy(&theirs[0], &kept[0]).unwrap();
    let (_, out, _) = meadow(&dir, &["cut", "count.cut"]);
    assert_eq!(out, format!("counting\n{said}"));

    // One that does not read is made again, and the answer is the program's.
    std::fs::write(&kept[0], b"not an image").unwrap();
    let (ok, out, err) = meadow(&dir, &["cut", "count.cut"]);
    assert!(ok, "{err}");
    assert_eq!(out, "counting\n200002");

    // A program that changed gets its own, and the one before it goes.
    std::fs::write(
        dir.join("count.cut"),
        CUT.replace("\"200002\"", &format!("\"{said}\"")),
    )
    .unwrap();
    let (_, out, _) = meadow(&dir, &["cut", "count.cut"]);
    assert_eq!(out, format!("counting\n{said}"));
    let now = cut_images(&dir);
    assert_eq!(now.len(), 1, "{now:?}");
    assert_ne!(now, kept);
}

/// A program that declares no vector -- a language without Meadow's -- gets
/// `Process.argv` as a plain array, which the array primitives take apart:
/// `docs/CUT.md`. Found by Idyll's session.
#[test]
fn a_program_with_no_vector_gets_its_arguments_as_an_array() {
    let dir = scratch("cut-argv");
    std::fs::write(
        dir.join("argv.cut"),
        "cut 0\nentry t:Main/main\nanswer str\n\n\
         native {\n  t:Main/P.argv = Process.argv\n}\n\n\
         effect t:Main/P { argv(unit) -> ptr }\n\n\
         def t:Main/main (; k: ptr) =\n  perform t:Main/P.argv(unit; μ̃ v: ptr.\n    \
         prim arrayLen(v; μ̃ n: i64.\n      prim eq(n, 2; μ̃ u: unit. <\"not two\" | k>, μ̃ u: unit.\n        \
         prim arrayGet(v, 1; μ̃ s: str. <s | k>))))\n",
    )
    .unwrap();
    for runtime in [&[][..], &["--runtime", "silo"]] {
        let mut args = vec!["cut", "argv.cut"];
        args.extend_from_slice(runtime);
        args.extend_from_slice(&["--", "first", "second"]);
        let (ok, out, err) = meadow(&dir, &args);
        assert!(ok, "{runtime:?}: {err}");
        assert_eq!(out, "second", "{runtime:?}");
    }
}

#[test]
fn a_program_of_cut_runs_on_glade_and_as_an_executable_of_silos() {
    let dir = scratch("cut");
    std::fs::write(dir.join("count.cut"), CUT).unwrap();
    for runtime in [&[][..], &["--runtime", "silo"]] {
        let mut args = vec!["cut", "count.cut"];
        args.extend_from_slice(runtime);
        let (ok, out, err) = meadow(&dir, &args);
        assert!(ok, "{runtime:?}: {err}");
        assert_eq!(out, "counting\n200002", "{runtime:?}");
    }
    // A handler, and what is not lowered yet, which is said: nothing runs.
    std::fs::write(
        dir.join("handles.cut"),
        "cut 0\nentry t:Main/main\nanswer str\n\neffect t:Main/Ask { ask(unit) -> i64 }\n\n\
         def t:Main/main (; k: ptr) =\n  handle {\n    t:Main/Ask.ask(u: unit; r: ptr, h: ptr) => <r | apply(41; h)>;\n    \
         return(x: i64; h: ptr) => prim add(x, 1; h)\n  } in μ b.\n    perform t:Main/Ask.ask(unit; b)\n  ; \
         μ̃ n: i64. prim eq(n, 42; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit. <\"42\" | k>)\n",
    )
    .unwrap();
    for runtime in [&[][..], &["--runtime", "silo"]] {
        let mut args = vec!["cut", "handles.cut"];
        args.extend_from_slice(runtime);
        let (ok, out, err) = meadow(&dir, &args);
        assert!(ok, "{runtime:?}: {err}");
        assert_eq!(out, "42", "{runtime:?}");
    }
    std::fs::write(
        dir.join("generic.cut"),
        "cut 0\nentry t:Main/main\nanswer str\n\n\
         def t:Main/id <'a = d> (d: desc, x: 'a; k: ptr) =\n  <x | k>\n\n\
         def t:Main/main (; k: ptr) =\n  t:Main/id(desc(i64), 1; μ̃ n: i64. t:Main/id(desc(str), \"generic\"; k))\n",
    )
    .unwrap();
    for runtime in [&[][..], &["--runtime", "silo"]] {
        let mut args = vec!["cut", "generic.cut"];
        args.extend_from_slice(runtime);
        let (ok, out, err) = meadow(&dir, &args);
        assert!(ok, "{runtime:?}: {err}");
        assert_eq!(out, "generic", "{runtime:?}");
    }
    // A program with no answer prints what it writes and nothing after:
    // on Silo the `unit` it ended with was shown, as `<continuation>`.
    std::fs::write(
        dir.join("quiet.cut"),
        "cut 0\nentry t:Main/main\nanswer none\n\n\
         native {\n  t:Main/Console.say = Console.writeOutput\n}\n\n\
         effect t:Main/Console { say(str) -> unit }\n\n\
         def t:Main/main (; k: ptr) =\n  perform t:Main/Console.say(\"said\\n\"; μ̃ u: unit. <unit | k>)\n",
    )
    .unwrap();
    for runtime in [&[][..], &["--runtime", "silo"]] {
        let mut args = vec!["cut", "quiet.cut"];
        args.extend_from_slice(runtime);
        let (ok, out, err) = meadow(&dir, &args);
        assert!(ok, "{runtime:?}: {err}");
        assert_eq!(out, "said\n", "{runtime:?}");
    }
    std::fs::write(
        dir.join("refused.cut"),
        "cut 0\nentry t:Main/main\nanswer none\n\n\
         def t:Main/main (; k: ptr) =\n  prim add(μ j. <1 | j>, 1; k)\n",
    )
    .unwrap();
    let (ok, out, err) = meadow(&dir, &["cut", "refused.cut"]);
    assert!(!ok && out.is_empty(), "{out}");
    assert!(err.contains("`μ`"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the REPL prints, given `input` a line at a time.
fn repl(input: &str) -> String {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the meadow binary runs");
    child
        .stdin
        .take()
        .expect("a pipe")
        .write_all(input.as_bytes())
        .expect("written");
    let out = child.wait_with_output().expect("the REPL ends");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_repl_shows_a_definition_in_each_ir() {
    let defined = "fun sumTo (m : Int) : Int = let rec go n acc = if n == 0 then acc else go (n - 1) (acc + n) in go m 0\n";
    let all = repl(&format!("{defined}:ir sumTo\n:q\n"));
    let at = |what: &str| {
        all.find(what)
            .unwrap_or_else(|| panic!("no `{what}` in:\n{all}"))
    };
    // In the order a program passes through them.
    assert!(at("-- core") < at("-- cut") && at("-- cut") < at("-- axcut"));
    // One of them, asked for by name, with no heading: the definition's
    // value, and the definition its local function is lifted to.
    let cut = repl(&format!("{defined}:ir cut sumTo\n:q\n"));
    assert!(
        !cut.contains("-- cut") && !cut.contains("-- axcut"),
        "{cut}"
    );
    let from = cut.find("val meadow:").expect("the definition's value");
    let text = format!("cut 0\n\n{}", &cut[from..]);
    let read = meadow_cut::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(read.vals.len(), 1, "{text}");
    assert_eq!(read.defs.len(), 1, "the local function, at the top: {text}");
    // And something not defined is said to be.
    assert!(repl(":ir nope\n:q\n").contains("nothing called `nope` is defined"));
}

#[test]
fn the_repl_evaluates_on_the_merged_runtime_s_vm() {
    if let Err(e) = meadow::aot::interpreter() {
        eprintln!("skipped: {e}");
        return;
    }
    let out = repl(":silo\nfun f x = x * 2\nf 21\n\"ab\" ++ \"c\"\nprintln \"hi\"\nf\n:q\n");
    for want in ["= 42", "= \"abc\"", "hi\n= ()", "= <closure>"] {
        assert!(out.contains(want), "no `{want}` in:\n{out}");
    }
}

#[test]
fn a_profile_of_a_program_on_the_merged_runtime_s_vm_names_its_functions() {
    if let Err(e) = meadow::aot::interpreter() {
        eprintln!("skipped: {e}");
        return;
    }
    let dir = std::env::temp_dir().join(format!("meadow-vm-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("Prof/src")).unwrap();
    std::fs::write(
        dir.join("Prof/Meadow.toml"),
        "[package]\nname = \"Prof\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("Prof/src/Main.mw"),
        "fun fib (n : Int) : Int = if n < 2 then n else fib (n - 1) + fib (n - 2)\n\n\
         fun main () = println (fib 24)\n",
    )
    .unwrap();
    let to = dir.join("prof.folded");
    let run = |to: &std::path::Path| {
        let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
            .args([
                "run",
                "--runtime",
                "silo",
                "--interpret",
                "--sample-every",
                "1000",
                "--profile-to",
            ])
            .arg(to)
            .arg(dir.join("Prof"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "46368");
        std::fs::read_to_string(to).unwrap()
    };
    let folded = run(&to);
    // Outermost first: `main`, then `fib` as deep as it was.
    assert!(
        folded
            .lines()
            .any(|l| l.starts_with("Prof.main;Prof.fib;Prof.fib")),
        "{folded}"
    );
    // A profile of work is the same profile every time.
    let mut a: Vec<&str> = folded.lines().collect();
    let again = run(&dir.join("again.folded"));
    let mut b: Vec<&str> = again.lines().collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);
}

#[test]
fn the_merged_runtime_s_vm_says_what_a_program_reached_as_glade_does() {
    if let Err(e) = meadow::aot::interpreter() {
        eprintln!("skipped: {e}");
        return;
    }
    let dir = std::env::temp_dir().join(format!("meadow-vm-covered-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("Cov/src")).unwrap();
    std::fs::write(
        dir.join("Cov/Meadow.toml"),
        "[package]\nname = \"Cov\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("Cov/src/Main.mw"),
        "use Std.Process as P\nuse Std.String as S\n\n\
         fun pick (n : Int) : Int = if n > 10 then n * 2 else if n > 5 then n + 1 else 0\n\n\
         fun main () =\n  let a = P.covered True in\n  let _ = pick 1 in\n  \
         let b = P.covered False in\n  let _ = pick 20 in\n  let c = P.covered False in\n  \
         println (S.concatAll [show a, \" \", show (b > a), \" \", show (c > b)])\n",
    )
    .unwrap();
    let said = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
            .arg("run")
            .args(args)
            .arg(dir.join("Cov"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    // Glade's VM, and the merged runtime's: from nothing, and more as the
    // program gets somewhere new. Native code notes nothing.
    assert_eq!(said(&["--backend", "vm"]), "0 True True");
    assert_eq!(said(&["--runtime", "silo", "--interpret"]), "0 True True");
    assert_eq!(said(&["--runtime", "silo"]), "0 False False");
}

#[test]
fn gc_stats_say_what_memory_did_on_the_merged_runtime_s_vm() {
    if let Err(e) = meadow::aot::interpreter() {
        eprintln!("skipped: {e}");
        return;
    }
    let dir = std::env::temp_dir().join(format!("meadow-vm-gc-stats-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("Gc/src")).unwrap();
    std::fs::write(
        dir.join("Gc/Meadow.toml"),
        "[package]\nname = \"Gc\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("Gc/src/Main.mw"),
        "use Std.Collections.Vector as V\n\n\
         fun main () = println (V.len (V.map (\\x -> [x, x + 1]) (V.range 0 20000)))\n",
    )
    .unwrap();
    for (gc, says) in [("rc", "gc: counted:"), ("tracing", "gc: traced:")] {
        let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
            .args([
                "run",
                "--runtime",
                "silo",
                "--interpret",
                "--gc",
                gc,
                "--gc-stats",
            ])
            .arg(dir.join("Gc"))
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "20000");
        assert!(
            err.contains(says) && err.contains("gc: heaps"),
            "{gc}: {err}"
        );
    }
}
