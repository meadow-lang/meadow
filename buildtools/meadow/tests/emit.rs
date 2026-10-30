//! `--emit`: a build's bytecode and native code written as text, in place of
//! the binaries -- and the listings themselves.

use meadow::listing;
use meadow::{OptLevel, Options, pipeline, runtime};
use meadow_glade::codegen::{self, Arch};
use std::path::{Path, PathBuf};
use std::process::Command;

fn image(src: &str) -> meadow_bytecode::Program {
    let (program, diags) =
        pipeline::compile_str_with_std("test", src, Options::debug().entry("result"));
    assert!(
        diags.is_empty(),
        "{:?}",
        diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    // What `main` reaches, as a build compiles: not all of `Std`.
    let program = meadow_compiler::core::prune::prune(&program);
    runtime::compile(&program, OptLevel::O1).unwrap()
}

const PROGRAM: &str = "fun fact n = if n == 0 then 1 else n * fact (n - 1)\n\
                       fun main () = println (fact 10)\n";

/// Every function gets its label, where its code starts; every byte of the
/// code is on some line; and what comes out reads as each architecture's
/// instructions.
#[test]
fn a_listing_accounts_for_all_the_code() {
    let image = image(PROGRAM);
    for (arch, ret) in [(Arch::X86_64, "ret"), (Arch::Aarch64, "ret")] {
        let compiled = codegen::compile(&image, arch, OptLevel::O2);
        let text = listing::listing(&compiled, OptLevel::O2);
        let labels: std::collections::HashSet<&str> =
            text.lines().filter_map(|l| l.strip_suffix(':')).collect();
        assert!(!labels.is_empty());
        for &(pc, at) in &compiled.blocks {
            // Two blocks can share an offset; one of them names it.
            assert!(
                compiled
                    .blocks
                    .iter()
                    .any(|&(p, a)| a == at && labels.contains(format!("fn_pc{p}").as_str())),
                "{arch:?}: no label for pc {pc}"
            );
        }
        let bytes: usize = text
            .lines()
            .filter(|l| l.starts_with("  "))
            .map(|l| {
                l.split_whitespace()
                    .skip(1)
                    .take_while(|t| t.len() == 2 && t.bytes().all(|b| b.is_ascii_hexdigit()))
                    .count()
            })
            .sum();
        assert_eq!(
            bytes,
            compiled.code.len(),
            "{arch:?}: every byte listed once"
        );
        assert!(
            text.lines().any(|l| l.trim_end().ends_with(ret)),
            "{arch:?}:\n{text}"
        );
        assert!(
            !text.contains(".word"),
            "{arch:?}: undecodable words:\n{text}"
        );
    }
}

/// A branch says where it goes as a place in a function: `fact` calling itself
/// jumps back into its own code.
#[test]
fn a_branch_is_named_by_where_it_lands() {
    let image = image(PROGRAM);
    let text = listing::asm(&image, Arch::Aarch64, OptLevel::O2);
    assert!(
        text.lines()
            .any(|l| l.contains(" b ") && l.contains("; fn_pc")),
        "no branch names its place:\n{text}"
    );
    let text = listing::asm(&image, Arch::X86_64, OptLevel::O2);
    assert!(
        text.lines()
            .any(|l| l.contains("jmp") && l.contains(" fn_pc")),
        "no jump names its place:\n{text}"
    );
}

fn scratch(who: &str) -> meadow_scratch::Scratch {
    meadow_scratch::scratch!("meadow-emit-{}-{who}", std::process::id())
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

#[test]
fn build_writes_text_in_place_of_binaries() {
    let dir = scratch("build");
    let root = dir.join("fact");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Meadow.toml"), "[package]\nname = \"Fact\"\n").unwrap();
    std::fs::write(root.join("src/Main.mw"), PROGRAM).unwrap();

    // Whichever architecture this machine is not: code for the host goes
    // straight into `native/`, and it is another target's that is written
    // beside it under its triple -- the layout this is here to check. Naming
    // `aarch64` outright passed on x86-64 and failed on Apple silicon.
    let foreign = if cfg!(target_arch = "aarch64") {
        "x86_64"
    } else {
        "aarch64"
    };
    let (ok, err) = meadow(
        &root,
        &["build", "--emit", "bytecode,asm", "--target", foreign],
    );
    assert!(ok, "{err}");
    let debug = root.join("target/debug");
    assert!(
        !debug.join("bytecode/Fact.mbc").exists(),
        "the image was written though only text was asked for"
    );
    let bytecode = std::fs::read_to_string(debug.join("bytecode/Fact.mbc.txt")).unwrap();
    assert_eq!(bytecode, image_text(&root));
    let native = std::fs::read_dir(debug.join("native"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .unwrap_or_else(|| panic!("{foreign}'s code beside the host's, under its triple"));
    let asm = std::fs::read_to_string(native.join("Fact.s")).unwrap();
    assert!(
        asm.starts_with(&format!("; meadow native code: {foreign}, -O1")),
        "{asm}"
    );
    assert!(err.contains("bytecode: ") && err.contains("asm: "), "{err}");

    // Both kinds of `--emit`, and the image an ordinary build writes.
    let (ok, err) = meadow(&root, &["build", "--emit", "image", "--emit", "bytecode"]);
    assert!(ok, "{err}");
    assert!(debug.join("bytecode/Fact.mbc").exists());

    // `link` takes an image to text as it would to an executable.
    let (ok, err) = meadow(
        &root,
        &[
            "link",
            "--emit",
            "asm",
            "--target",
            "x86_64",
            "target/debug/bytecode/Fact.mbc",
        ],
    );
    assert!(ok, "{err}");
    let asm = std::fs::read_to_string(debug.join("bytecode/Fact.s")).unwrap();
    assert!(
        asm.starts_with("; meadow native code: x86_64, -O2"),
        "{asm}"
    );

    let (ok, err) = meadow(&root, &["build", "--emit", "elf"]);
    assert!(
        !ok && err.contains("expected image, bytecode, asm, exe or expanded"),
        "{err}"
    );
}

/// What `meadow dis` prints for the package at `root`, which is the text an
/// `--emit bytecode` writes.
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
        !dir.join("target/debug/bytecode").exists(),
        "nothing else was asked for"
    );
}

fn image_text(root: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(root)
        .args(["dis"])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_lone_file_has_nowhere_to_emit_to() {
    let dir = scratch("lone");
    std::fs::write(dir.join("one.mw"), PROGRAM).unwrap();
    let (ok, err) = meadow(&dir, &["build", "--emit", "asm", "one.mw"]);
    assert!(!ok, "{err}");
    assert!(err.contains("a lone file has none"), "{err}");
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(&dir)
        .args(["dis", "--asm", "--target", "x86_64", "one.mw"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("; meadow native code: x86_64"));
}

// --- not relinking what is already there -------------------------------------

/// A package whose executable can be built twice over.
fn buildable(what: &str, body: &str) -> meadow_scratch::Scratch {
    let dir = meadow_scratch::scratch!("meadow-silo-{}-{what}", std::process::id());
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
    let out = pipeline::build(dir, meadow::profile::Profile::Release.options());
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    let linked = out.linked.expect("a linked program");
    let image = runtime::compile(&linked.program, OptLevel::O2).expect("an image");
    let exe = meadow::aot::build(
        dir,
        meadow::profile::Profile::Release,
        OptLevel::O2,
        "prog",
        &image,
        meadow::aot::Target::host().expect("a host target"),
        &Default::default(),
    )
    .expect("it links");
    let when = std::fs::metadata(&exe)
        .expect("it exists")
        .modified()
        .unwrap();
    (exe, when)
}

/// A test's directory goes when the test fails as when it passes: here a
/// native build, the largest thing these tests write, and the executable it
/// made run -- a program that has just exited can hold its image a moment
/// longer on Windows -- before a deliberate panic.
#[test]
fn a_failing_test_leaves_no_directory_behind() {
    let seen = std::sync::Mutex::new(PathBuf::new());
    // On a thread of its own, as the test harness runs a test.
    let failed = std::thread::scope(|s| {
        s.spawn(|| {
            let dir = buildable("panics", "def result = 1 + 1\n");
            *seen.lock().unwrap() = dir.to_path_buf();
            let (exe, _) = link_once(&dir);
            let ran = Command::new(&exe).output().expect("the program runs");
            assert!(ran.status.success(), "{ran:?}");
            panic!("a deliberate failure");
        })
        .join()
        .is_err()
    });
    assert!(failed, "the deliberate failure passed");
    let dir = seen.into_inner().unwrap();
    assert!(
        dir.starts_with(env!("CARGO_TARGET_TMPDIR")),
        "{}",
        dir.display()
    );
    assert!(!dir.exists(), "{} is still there", dir.display());
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

/// Run `main` -- a module of a package built for Silo -- with `meadow run
/// --release`, answering whether it succeeded and what it said on stderr.
fn on_silo(name: &str, main: &str) -> (bool, String) {
    let dir = meadow_scratch::scratch!("meadow-silo-{}-{name}", std::process::id());
    std::fs::create_dir_all(dir.join("src")).expect("a scratch package");
    std::fs::write(
        dir.join("Meadow.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[profile.release]\nruntime = \"silo\"\n"
        ),
    )
    .expect("a manifest");
    std::fs::write(dir.join("src/Main.mw"), main).expect("a module");
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .args(["run", "--release"])
        .arg(&dir)
        .output()
        .expect("the meadow binary runs");
    let _ = std::fs::remove_dir_all(&dir);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A cancelled thread stops at an operation the runtime answers, as at a
/// thread operation -- here the clock, asked over and over under a handler of
/// its own -- and one waiting on a socket is woken; on Silo as on Glade
/// (`tests/threads.rs`).
#[test]
fn silo_stops_a_cancelled_thread_at_an_operation_and_wakes_it_from_a_socket() {
    let (ok, stderr) = on_silo(
        "Clock",
        "use Std.Thread as T
use Std.Time (monotonic)

effect Ask { ask : () -> Int }

fun loop (n : Int) = if monotonic () < 0 then n else loop (n + ask ())

fun main () =
  let t = T.spawn (\\() -> handle loop 0 with { ask u k -> k 1 }) in
  let _ = T.yieldNow () in
  let _ = T.cancel t in
  println (T.await t)
",
    );
    assert!(!ok && stderr.contains("cancelled"), "{stderr}");
    let (ok, stderr) = on_silo(
        "Socket",
        "use Std.Thread as T
use Std.Net as Net
use Std.Time (sleep)

fun main () =
  match Net.listen \"127.0.0.1\" 0 with
  | Err why -> println why
  | Ok l ->
      let t = T.spawn (\\() -> match Net.accept l with | Ok _ -> \"accepted\" | Err why -> why) in
      let _ = T.yieldNow () in
      let _ = sleep 50 in
      let _ = T.cancel t in
      println (T.await t)
",
    );
    assert!(!ok && stderr.contains("cancelled"), "{stderr}");
}

/// What a Silo program writes after its last newline: kept. The process is
/// the emitted module's `main`, not Rust's, so nothing flushed stdout's
/// buffer at exit and the end of the output was lost.
#[test]
fn silo_keeps_what_follows_the_last_newline() {
    let dir = meadow_scratch::scratch!("meadow-silo-{}-flush", std::process::id());
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
}

/// A record's row through a copy of a function of dictionaries, run natively:
/// a release build copied `settle` at the caller's row taken as empty, and
/// Silo, which lays a record out by what it holds, read `{ items, name }` as
/// `{ items }` -- a vector where the record was, and a match that fitted
/// nothing in it. The machines that do not lay records out agreed all along.
#[test]
fn silo_reads_a_record_through_a_copy_at_the_callers_row() {
    let dir = meadow_scratch::scratch!("meadow-silo-{}-row", std::process::id());
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
}
