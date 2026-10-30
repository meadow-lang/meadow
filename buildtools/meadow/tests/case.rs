//! Names are spelled exactly, on every platform.
//!
//! macOS and Windows open `layers` where the directory is `Layers`; Linux does
//! not. Anything that reaches the filesystem through a name one letter off
//! therefore works on the machine it was written on and fails on CI -- which
//! is how a fixture called `Layers` came to be opened as `layers` for months.
//! These tests fail the same way everywhere: they compare names with what a
//! directory *lists*, which is exact whatever the filesystem would accept.
//!
//! Three things are held to it: the paths the tests themselves open, the
//! files the Rust sources embed with `include_str!` (the whole standard
//! library arrives that way), and the language's own module names, which a
//! case-insensitive disk must not be allowed to make forgiving.

mod common;

use common::exact_case;
use meadow::{Options, pipeline};
use std::path::{Path, PathBuf};

#[test]
fn a_path_is_exact_or_it_is_refused() {
    assert_eq!(
        exact_case(Path::new("tests/fixtures/workspace/Layers")),
        Ok(())
    );
    let wrong = exact_case(Path::new("tests/fixtures/workspace/layers"))
        .expect_err("`layers` is not how it is spelled");
    assert!(wrong.contains("spelled `Layers`"), "{wrong}");
    let missing = exact_case(Path::new("tests/fixtures/workspace/Nowhere"))
        .expect_err("there is no such fixture");
    assert!(missing.contains("no `Nowhere`"), "{missing}");
    // Through `..` and `.` too, which is how an embedded file is named.
    assert_eq!(
        exact_case(Path::new("tests/./fixtures/../fixtures/workspace/App")),
        Ok(())
    );
}

/// Every Rust source under `dir`, not counting build output.
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                rust_sources(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// The string literal that opens at `text[at..]`, if one does and it is a
/// plain one: no escapes and no `{}` to fill in.
fn literal(text: &str, at: usize) -> Option<&str> {
    let rest = text[at..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let lit = &rest[..end];
    (!lit.contains('\\') && !lit.contains('{')).then_some(lit)
}

#[test]
fn every_embedded_file_is_spelled_as_it_is_on_disk() {
    // `include_str!` is resolved by the compiler through the filesystem, so a
    // path in the wrong case builds on a Mac and not on Linux.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut sources = Vec::new();
    for ws in ["buildtools", "compiler", "glade", "eval"] {
        rust_sources(&root.join(ws), &mut sources);
    }
    assert!(sources.len() > 50, "found only {} sources", sources.len());
    let mut checked = 0;
    let mut wrong = Vec::new();
    for src in &sources {
        // This file spells the macros' names without calling them.
        if src.ends_with("tests/case.rs") {
            continue;
        }
        let text = std::fs::read_to_string(src).unwrap_or_default();
        for mac in ["include_str!(", "include_bytes!("] {
            let mut from = 0;
            while let Some(i) = text[from..].find(mac) {
                let at = from + i + mac.len();
                from = at;
                let Some(rel) = literal(&text, at) else {
                    continue;
                };
                let path = src.parent().expect("a file has a directory").join(rel);
                checked += 1;
                if let Err(e) = exact_case(&path) {
                    wrong.push(format!("{}: {e}", src.display()));
                }
            }
        }
    }
    assert!(checked > 30, "checked only {checked} embedded files");
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn every_fixture_a_test_names_is_spelled_as_it_is_on_disk() {
    // A literal path into `tests/fixtures`, anywhere in the tests.
    let mut sources = Vec::new();
    rust_sources(Path::new("tests"), &mut sources);
    let mut wrong = Vec::new();
    for src in &sources {
        // This file names paths that are wrong on purpose.
        if src.ends_with("case.rs") {
            continue;
        }
        let text = std::fs::read_to_string(src).unwrap_or_default();
        let mut from = 0;
        while let Some(i) = text[from..].find("\"tests/fixtures") {
            let at = from + i;
            from = at + 1;
            let Some(lit) = literal(&text, at) else {
                continue;
            };
            // A directory a test makes for itself is not there to check.
            if Path::new(lit).exists()
                && let Err(e) = exact_case(Path::new(lit))
            {
                wrong.push(format!("{}: {e}", src.display()));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// A package `Cased` with a module `Alpha` and this `Main`, built; its
/// diagnostics.
fn cased(tag: &str, main: &str) -> Vec<String> {
    let dir = meadow_scratch::scratch!("meadow-case-{tag}-{}", std::process::id());
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\nname = \"Cased\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/Alpha.mw"), "def x = 41\n").unwrap();
    std::fs::write(dir.join("src/Main.mw"), main).unwrap();
    let out = pipeline::build(&dir, Options::debug().entry("result"));
    let _ = std::fs::remove_dir_all(&dir);
    out.diagnostics.iter().map(|d| d.msg.clone()).collect()
}

#[test]
fn a_module_is_named_as_its_file_is() {
    let ok = cased("right", "use Cased.Alpha (x)\n\ndef result = x + 1\n");
    assert!(ok.is_empty(), "{ok:?}");
}

#[test]
fn a_module_in_the_wrong_case_is_not_found_on_any_platform() {
    // The file is `Alpha.mw`. A disk that would open it as `alpha.mw` must not
    // make `use Cased.alpha` mean it: the same program has to be the same
    // program on Linux.
    let wrong = cased("module", "use Cased.alpha (x)\n\ndef result = x + 1\n");
    assert!(!wrong.is_empty(), "`use Cased.alpha` found `Alpha.mw`");
}

#[test]
fn a_package_in_the_wrong_case_is_not_found_on_any_platform() {
    let wrong = cased("package", "use cased.Alpha (x)\n\ndef result = x + 1\n");
    assert!(
        !wrong.is_empty(),
        "`use cased.Alpha` found the package `Cased`"
    );
}

#[test]
fn the_standard_library_is_named_exactly() {
    // Embedded, so no filesystem is asked -- which is the point: it has to
    // stay that way, and `std` or `collections` has to stay wrong.
    let right = cased(
        "std-right",
        "use Std.Collections.Vector as V\n\ndef result = V.len [1, 2]\n",
    );
    assert!(right.is_empty(), "{right:?}");
    for (tag, src) in [
        (
            "std-a",
            "use Std.collections.Vector as V\n\ndef result = V.len [1, 2]\n",
        ),
        (
            "std-b",
            "use Std.Collections.vector as V\n\ndef result = V.len [1, 2]\n",
        ),
        (
            "std-c",
            "use std.Collections.Vector as V\n\ndef result = V.len [1, 2]\n",
        ),
    ] {
        let wrong = cased(tag, src);
        assert!(!wrong.is_empty(), "accepted: {src}");
    }
}
