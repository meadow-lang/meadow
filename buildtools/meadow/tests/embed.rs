//! `includeStr`: a file read at compile time and written into the program as
//! a string, and the rebuild that editing it has to cause.

use meadow::package::ProfileConfig;
use meadow::pipeline;
use meadow::profile::{Profile, Resolved};
use std::path::Path;

fn scratch(who: &str) -> meadow_scratch::Scratch {
    let dir = meadow_scratch::scratch!("meadow-embed-{}-{who}", std::process::id());
    dir.canonical()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A package whose `Main.mw` is `main`, beside a `note.txt` of `note`.
fn package(who: &str, main: &str, note: &str) -> meadow_scratch::Scratch {
    let root = scratch(who).child("app");
    write(
        &root.join("Meadow.toml"),
        "[package]\nname = \"App\"\nversion = \"0.1.0\"\n",
    );
    write(&root.join("src/Main.mw"), main);
    write(&root.join("src/note.txt"), note);
    root
}

fn options(root: &Path) -> meadow::Options {
    Resolved::resolve(Profile::Debug, root, ProfileConfig::default())
        .options
        .entry("result")
}

/// Build it and run it, or give back what went wrong.
fn run(root: &Path) -> Result<String, String> {
    let out = pipeline::build(root, options(root));
    if !out.diagnostics.is_empty() {
        return Err(out
            .diagnostics
            .iter()
            .map(|d| d.msg.clone())
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let linked = out.linked.expect("a linked program");
    Ok(meadow_eval::run(&linked.program).unwrap().to_string())
}

#[test]
fn a_file_beside_the_source_is_read_at_compile_time() {
    let root = package(
        "reads",
        "def note = includeStr \"note.txt\"\n\ndef result = note\n",
        "hello from a file\n",
    );
    assert_eq!(run(&root).unwrap(), "\"hello from a file\\n\"");
}

#[test]
fn the_path_is_taken_beside_the_file_that_wrote_it() {
    // Not beside wherever the build was started: `src/Main.mw` says
    // `note.txt`, and that is `src/note.txt`.
    let root = package(
        "beside",
        "def note = includeStr \"note.txt\"\n\ndef result = note\n",
        "beside\n",
    );
    let elsewhere = root.join("note.txt");
    write(&elsewhere, "at the root\n");
    assert_eq!(run(&root).unwrap(), "\"beside\\n\"");
}

#[test]
fn editing_the_embedded_file_is_a_rebuild() {
    // The module's own text is untouched, so nothing in the fingerprint has
    // changed: without the embedded file being checked, the cache would serve
    // the old program.
    let root = package(
        "rebuild",
        "def note = includeStr \"note.txt\"\n\ndef result = note\n",
        "first\n",
    );
    assert_eq!(run(&root).unwrap(), "\"first\\n\"");
    write(&root.join("src/note.txt"), "second\n");
    assert_eq!(run(&root).unwrap(), "\"second\\n\"");
}

#[test]
fn a_file_that_is_not_there_says_so_with_the_path_it_looked_for() {
    let root = package(
        "missing",
        "def note = includeStr \"nope.txt\"\n\ndef result = note\n",
        "unused\n",
    );
    let said = run(&root).unwrap_err();
    assert!(said.contains("cannot embed"), "{said}");
    assert!(said.contains("nope.txt"), "{said}");
}

#[test]
fn the_path_has_to_be_written_out() {
    // It is read before the program runs, so it cannot come from the program.
    let root = package(
        "computed",
        "def place = \"note.txt\"\n\ndef note = includeStr place\n\ndef result = note\n",
        "unused\n",
    );
    let said = run(&root).unwrap_err();
    assert!(said.contains("written out in full"), "{said}");
}

#[test]
fn one_argument_is_what_it_takes() {
    let root = package(
        "arity",
        "def note = includeStr \"note.txt\" \"more.txt\"\n\ndef result = note\n",
        "unused\n",
    );
    let said = run(&root).unwrap_err();
    assert!(said.contains("one argument"), "{said}");
}

#[test]
fn a_binding_of_your_own_wins() {
    let root = package(
        "shadow",
        "fun includeStr s = \"mine: ${s}\"\n\ndef result = includeStr \"note.txt\"\n",
        "unused\n",
    );
    assert_eq!(run(&root).unwrap(), "\"mine: note.txt\"");
}
