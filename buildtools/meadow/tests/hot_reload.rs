//! Loading a new build into a program running on Silo's interpreter
//! (`meadow-silo-vm --watch`, which `meadow run --watch` drives): a call by
//! name runs the new version from then on, what is running finishes as it
//! was, and a value of a type the new build changed is taken only by the
//! code made for it -- old values by old code, new by new.
//!
//! Each program waits, before it goes on, for a file the test writes once
//! the reload is in: what it prints after says which code ran.
//!
//! These need Silo's interpreter built (`meadow-silo-vm`, which
//! `meadow::aot::interpreter` finds); without it they say so and pass.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// A directory of the test's own.
fn scratch(who: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-hot-{}-{who}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `src` built as package `Hot` in `dir`, for the interpreter: its image.
fn image(dir: &Path, src: &str) -> PathBuf {
    image_with(dir, src, &[])
}

/// [`image`], built with `flags` besides.
fn image_with(dir: &Path, src: &str, flags: &[&str]) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\nname = \"Hot\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src").join("Main.mw"), src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .args(["build", "--runtime", "silo", "--interpret"])
        .args(flags)
        .arg(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "building:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let at = dir.join("target/debug/native/silo/Hot.image");
    assert!(at.exists(), "no image at {}", at.display());
    at
}

/// The program at `first` run watching its image, which becomes each of
/// `then` in turn -- each reload's report awaited -- before the file the
/// program waits for is written: what each reload said, and what the program
/// printed.
fn run(dir: &Path, first: &Path, then: &[PathBuf]) -> (Vec<String>, String) {
    let running = dir.join("running.image");
    std::fs::copy(first, &running).unwrap();
    let mut child = Command::new(meadow::aot::interpreter().unwrap())
        .arg("--watch")
        .arg(&running)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut said = Vec::new();
    for next in then {
        // Written whole, as `meadow` writes one, and later than the last:
        // the interpreter goes by when its image was written.
        std::thread::sleep(Duration::from_millis(50));
        let part = dir.join("running.part");
        std::fs::copy(next, &part).unwrap();
        std::fs::rename(&part, &running).unwrap();
        let line = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the interpreter says what came of the reload");
        said.push(line);
    }
    std::fs::write(dir.join("go"), "").unwrap();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "the program failed: {out}");
    (said, out)
}

/// A program that waits for `DIR/go` in `slow`, which then calls `greet` and
/// reads `scale`, and makes a closure before it waits that calls `greet`
/// after; what they say, and `Circle`'s field, vary between builds.
fn program(dir: &Path, greet: &str, slow: &str, scale: u32, field: &str) -> String {
    let go = dir.join("go").to_string_lossy().replace('\\', "/");
    format!(
        "\
use Std.Time (sleep)
use Std.String as S
use Shape.*

data Shape = Circle {field} | Square Int

-- Computed, and so cached: a literal would be written in where it is used.
def scale : Int = S.byteLength (S.repeat \"x\" {scale})

fun greet (n : Int) : String = S.concatAll [\"{greet} \", S.fromInt n]

fun wait () = if exists \"{go}\" then () else let _ = sleep 10 in wait ()

fun slow (n : Int) =
  let _ = wait () in
  S.concatAll [\"{slow} \", greet n, \" \", S.fromInt (n * scale)]

fun main () =
  let k = \\n -> S.concatAll [\"{slow}-closure \", greet n] in
  let _ = println (slow 1) in
  let _ = println (k 3) in
  let _ = println (slow 2) in
  ()
"
    )
}

#[test]
fn a_call_by_name_runs_the_new_version_and_what_runs_finishes_as_it_was() {
    if let Err(e) = meadow::aot::interpreter() {
        eprintln!("skipped: {e}");
        return;
    }
    let dir = scratch("versions");
    let old = image(&dir.join("v1"), &program(&dir, "one", "old", 10, "Int"));
    let new = image(&dir.join("v2"), &program(&dir, "two", "new", 100, "Int"));
    let (said, out) = run(&dir, &old, &[new]);
    assert!(said[0].starts_with("reloaded"), "{said:?}");
    // `slow 1` was waiting when the new build came: its own code goes on
    // as it was, and what it calls by name after is the new -- `greet`, and
    // the top-level value, which the new version computes for itself rather
    // than reading what the old one cached. So does the closure made before
    // it, which is the old code's too. `main`, which `slow 2` is called
    // from, is the old version, and `slow 2` is new throughout.
    assert_eq!(out, "old two 1 100\nold-closure two 3\nnew two 2 200\n");
}

/// A program that makes a `Shape` before it waits, and after it shows that
/// one, and then one made afresh by a function whose type does not mention
/// `Shape` -- which a new build's version replaces. What a `Circle` holds,
/// and how its area is worked out, vary between builds.
fn shapes(dir: &Path, say: &str, field: &str, made: &str, area: &str) -> String {
    let go = dir.join("go").to_string_lossy().replace('\\', "/");
    format!(
        "\
use Std.Time (sleep)
use Std.String as S
use Shape.*

data Shape = Circle {field} | Square Int

fun area (s : Shape) : Int =
  match s with
  | Circle r -> {area}
  | Square w -> w * w

fun describe (s : Shape) : String = S.concatAll [\"{say} \", S.fromInt (area s)]

fun make () : Shape = Circle {made}

fun fresh () : String = describe (make ())

fun wait () = if exists \"{go}\" then () else let _ = sleep 10 in wait ()

fun main () =
  let old = make () in
  let _ = wait () in
  let _ = println (describe old) in
  let _ = println (fresh ()) in
  ()
"
    )
}

#[test]
fn a_value_of_a_changed_type_goes_to_the_code_made_for_it() {
    if let Err(e) = meadow::aot::interpreter() {
        eprintln!("skipped: {e}");
        return;
    }
    let dir = scratch("types");
    let old = image(
        &dir.join("v1"),
        &shapes(&dir, "one", "Int", "2", "3 * r * r"),
    );
    let new = image(
        &dir.join("v2"),
        &shapes(&dir, "two", "String", "\"abc\"", "S.byteLength r * 100"),
    );
    let (said, out) = run(&dir, &old, &[new]);
    assert!(said[0].starts_with("reloaded"), "{said:?}");
    // The `Circle 2` made before is shown by the old `describe`: it is a
    // `Shape` of the old build's, and the new `describe` takes the new one.
    // `fresh`, whose type mentions neither, is the new build's, and makes a
    // `Circle "abc"` and shows it with the new code.
    assert_eq!(out, "one 12\ntwo 300\n");
}

#[test]
fn a_build_without_reloading_is_not_reloaded() {
    if let Err(e) = meadow::aot::interpreter() {
        eprintln!("skipped: {e}");
        return;
    }
    let dir = scratch("unversioned");
    let old = image(&dir.join("v1"), &program(&dir, "one", "old", 10, "Int"));
    let plain = image_with(
        &dir.join("v2"),
        &program(&dir, "two", "new", 100, "Int"),
        &["--no-reload"],
    );
    let (said, out) = run(&dir, &old, &[plain]);
    assert!(
        said[0].starts_with("not reloaded: it was built without hot loading"),
        "{said:?}"
    );
    assert_eq!(out, "old one 1 10\nold-closure one 3\nold one 2 20\n");
}

#[test]
fn reloads_follow_one_another() {
    if let Err(e) = meadow::aot::interpreter() {
        eprintln!("skipped: {e}");
        return;
    }
    let dir = scratch("again");
    let v1 = image(&dir.join("v1"), &program(&dir, "one", "old", 10, "Int"));
    let v2 = image(&dir.join("v2"), &program(&dir, "two", "mid", 100, "Int"));
    let v3 = image(&dir.join("v3"), &program(&dir, "three", "new", 1000, "Int"));
    let (said, out) = run(&dir, &v1, &[v2, v3]);
    assert!(said.iter().all(|s| s.starts_with("reloaded")), "{said:?}");
    assert_eq!(
        out,
        "old three 1 1000\nold-closure three 3\nnew three 2 2000\n"
    );
}
