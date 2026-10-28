//! `--emit`: what a build writes in place of what it writes by default, and
//! an executable not written again when the one there is this build's.

use meadow::{OptLevel, pipeline};
use std::path::{Path, PathBuf};
use std::process::Command;

const PROGRAM: &str = "fun fact n = if n == 0 then 1 else n * fact (n - 1)\n\
                       fun main () = println (fact 10)\n";

fn scratch(who: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-emit-{}-{who}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn meadow(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the meadow binary runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `--emit image` writes the image the VM runs, where a release build would
/// write an executable; what the old runtime wrote is refused, saying why.
#[test]
fn build_writes_what_it_is_asked_for() {
    let dir = scratch("build");
    let root = dir.join("fact");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Meadow.toml"),
        "[package]
name = \"Fact\"
",
    )
    .unwrap();
    std::fs::write(root.join("src/Main.mw"), PROGRAM).unwrap();

    let (ok, err) = meadow(&root, &["build", "--release", "--emit", "image"]);
    assert!(ok, "{err}");
    let native = root.join("target/release/native/silo");
    assert!(native.join("Fact.image").exists(), "{err}");
    assert!(
        !native
            .join(format!("Fact{}", std::env::consts::EXE_SUFFIX))
            .exists(),
        "an executable was written though only the image was asked for"
    );

    for retired in ["bytecode", "asm"] {
        let (ok, err) = meadow(&root, &["build", "--emit", retired]);
        assert!(!ok && err.contains("which is gone"), "{err}");
    }
    let (ok, err) = meadow(&root, &["build", "--emit", "elf"]);
    assert!(
        !ok && err.contains("expected image, exe or expanded"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--emit expanded`: the package's sources again, with each macro call
/// replaced by what it produced, and nothing else written. A second build
/// writes them again, though nothing has changed and the package would
/// otherwise be read back.
#[test]
fn build_writes_what_macros_produced() {
    let dir = buildable(
        "expanded",
        "macro swap\n  | ($a, $b) -> { ($b, $a) }\n\
         fun main () = println (swap!(1, \"two\"))\n",
    );
    for _ in 0..2 {
        let (ok, said) = meadow(&dir, &["build", ".", "--emit", "expanded"]);
        assert!(ok, "{said}");
        assert!(
            said.contains("swap!"),
            "what each macro wrote is said: {said}"
        );
        let text = std::fs::read_to_string(dir.join("target/debug/expanded/src/Lib.mw"))
            .expect("the expanded module");
        assert!(text.contains("-- swap! produced"), "{text}");
        assert!(text.contains("(\"two\", 1)"), "what it produced: {text}");
        assert!(!text.contains("swap!(1"), "the call is gone: {text}");
        assert!(
            text.contains("fun main () = println ("),
            "the rest is as written: {text}"
        );
    }
    assert!(
        !dir.join("target/debug/native").exists(),
        "nothing else was asked for"
    );
}

#[test]
fn a_lone_file_has_nowhere_to_emit_to() {
    let dir = scratch("lone");
    std::fs::write(dir.join("one.mw"), PROGRAM).unwrap();
    let (ok, err) = meadow(&dir, &["build", "--emit", "image", "one.mw"]);
    assert!(!ok, "{err}");
    assert!(err.contains("a lone file has none"), "{err}");
    // What it would run is printed instead.
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(&dir)
        .args(["dis", "one.mw"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("fact"));
    let _ = std::fs::remove_dir_all(&dir);
}

// --- not relinking what is already there -------------------------------------

/// A package whose executable can be built twice over.
fn buildable(what: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-silo-{}-{what}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("a scratch package");
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\nname = \"Prog\"\nversion = \"0.1.0\"\n",
    )
    .expect("a manifest");
    std::fs::write(dir.join("src/Lib.mw"), body).expect("a module");
    dir
}

/// Build `dir` as a native executable, answering where it went and when it was
/// last written.
fn link_once(dir: &Path) -> (PathBuf, std::time::SystemTime) {
    let out = pipeline::build(
        dir,
        meadow::profile::Profile::Release.options().entry("result"),
    );
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    let linked = out.linked.expect("a linked program");
    // As `meadow build` builds it: what the entry reaches, keyed by the
    // build's inputs.
    let program = meadow_compiler::core::prune::prune(&linked.program);
    let exe = meadow::aot::build_native(
        dir,
        meadow::profile::Profile::Release,
        OptLevel::O2,
        "prog",
        &program,
        meadow::aot::Target::host().expect("a host target"),
        out.fingerprint,
    )
    .expect("it links");
    let when = std::fs::metadata(&exe)
        .expect("it exists")
        .modified()
        .unwrap();
    (exe, when)
}

#[test]
fn an_executable_is_not_relinked_when_nothing_has_changed() {
    // Linking is the slow part of a native build, and on some systems running a
    // newly written binary costs more still -- so a `run` that changed nothing
    // used to be an order of magnitude slower than the same program on the JIT.
    let dir = buildable("fresh", "def result = 1 + 1\n");
    let (first, when) = link_once(&dir);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (again, and_then) = link_once(&dir);
    assert_eq!(first, again);
    assert_eq!(when, and_then, "it was written a second time");
}

#[test]
fn an_executable_is_relinked_when_the_program_changes() {
    let dir = buildable("stale", "def result = 1 + 1\n");
    let (exe, when) = link_once(&dir);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(dir.join("src/Lib.mw"), "def result = 2 + 2\n").expect("a module");
    let (again, and_then) = link_once(&dir);
    assert_eq!(exe, again);
    assert_ne!(when, and_then, "a different program, the same executable");
}

#[test]
fn an_executable_that_is_gone_is_linked_again() {
    let dir = buildable("deleted", "def result = 1 + 1\n");
    let (exe, _) = link_once(&dir);
    std::fs::remove_file(&exe).expect("it was there");
    let (again, _) = link_once(&dir);
    assert!(again.exists(), "it came back");
}

// --- what Silo runs ------------------------------------------------------------

/// What a Silo program writes after its last newline: kept. The process is
/// the emitted module's `main`, not Rust's, so nothing flushed stdout's
/// buffer at exit and the end of the output was lost.
#[test]
fn silo_keeps_what_follows_the_last_newline() {
    let dir = std::env::temp_dir().join(format!("meadow-silo-{}-flush", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("a scratch package");
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]
name = \"Flush\"
version = \"0.1.0\"

[profile.release]
runtime = \"silo\"
",
    )
    .expect("a manifest");
    std::fs::write(
        dir.join("src/Main.mw"),
        "use Std.Console (writeOutput)

fun main () = writeOutput \"line\nno newline\"
",
    )
    .expect("a module");
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .args(["run", "--release"])
        .arg(&dir)
        .output()
        .expect("the meadow binary runs");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "line
no newline"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A record's row through a copy of a function of dictionaries, run natively:
/// a release build copied `settle` at the caller's row taken as empty, and
/// Silo, which lays a record out by what it holds, read `{ items, name }` as
/// `{ items }` -- a vector where the record was, and a match that fitted
/// nothing in it. The machines that do not lay records out agreed all along.
#[test]
fn silo_reads_a_record_through_a_copy_at_the_callers_row() {
    let dir = std::env::temp_dir().join(format!("meadow-silo-{}-row", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("a scratch package");
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\nname = \"Row\"\nversion = \"0.1.0\"\n\n[profile.release]\nruntime = \"silo\"\n",
    )
    .expect("a manifest");
    std::fs::write(
        dir.join("src/Main.mw"),
        "use Std.Collections.Vector as V\nuse Std.Console (writeOutput)\nuse T.*\ndata T = A Int | B String\n\nfun firsts spec = settle spec 0 0\n\nfun settle spec acc n =\n  let next = V.foldl (\\a t -> match t with | A i -> a + i | B s -> a + 1) acc spec.items in\n  if n > 5 then next else settle spec next (n + 1)\n\nfun main () =\n  let spec = { name = \"G\", items = V.map (\\i -> if i % 2 == 0 then A i else B \"s\") (V.range 0 100) } in\n  writeOutput (show (firsts spec))\n",
    )
    .expect("a module");
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .args(["run", "--release"])
        .arg(&dir)
        .output()
        .expect("the meadow binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("17500"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
