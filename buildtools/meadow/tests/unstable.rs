//! What is not settled is asked for by name, in the source: `@!feature(…)`
//! at the top of a package's root module, as `#![feature(…)]` is at the top
//! of a crate's. See `meadow::channel`.
//!
//! There is no unstable feature just now. `ffi`, calling C through
//! `Std.Ffi`, was one, and is stable: a package uses `Std.Ffi` without
//! asking, on every channel, and one that still asks builds and is told the
//! line can go. What a stable `meadow` says of a feature still unstable is
//! `channel`'s own tests'.
//!
//! These run as the `meadow` they are built with: a build from a checkout,
//! or a release's stable one when `MEADOW_CHANNEL=stable` built them, as
//! `release.yml` does.

use meadow::pipeline;
use std::path::Path;

fn scratch(what: &str) -> meadow_scratch::Scratch {
    let dir = meadow_scratch::scratch!("meadow-unstable-{}-{what}", std::process::id());
    std::fs::create_dir_all(dir.join("src")).expect("a scratch directory");
    dir
}

const MANIFEST: &str = "[package]\nname = \"Calls\"\nversion = \"0.1.0\"\n";

/// A package of the files given, each a name under `src` and its source.
fn package(what: &str, manifest: &str, files: &[(&str, &str)]) -> meadow_scratch::Scratch {
    let dir = scratch(what);
    std::fs::write(dir.join("Meadow.toml"), manifest).unwrap();
    for (name, source) in files {
        std::fs::write(dir.join("src").join(name), source).unwrap();
    }
    dir
}

/// What building it says, a line for each thing said.
fn said(dir: &Path) -> String {
    let out = pipeline::build(dir, meadow::Options::debug().entry("result"));
    out.diagnostics
        .iter()
        .map(|d| d.msg.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

const CALLS: &str = "use Std.Ffi as Ffi\n\ndef result = 1\n";

/// A package that uses `Std.Ffi` in more than one module, and asks for
/// nothing, builds on the stable channel: what a release's checks run, with
/// the stable `meadow` they build, and what a stable `meadow` that still
/// refused `Std.Ffi` would fail. On any other channel it builds the same.
#[test]
fn a_stable_meadow_builds_a_package_that_calls_c_without_asking() {
    let dir = package(
        "stable",
        MANIFEST,
        &[
            ("Main.mw", CALLS),
            ("Other.mw", "use Std.Ffi (Arg)\n\n@pub def two = 2\n"),
        ],
    );
    assert_eq!(
        said(&dir),
        "",
        "a {} `meadow` refused `Std.Ffi`",
        meadow::channel::CHANNEL
    );
    assert_eq!(meadow::channel::feature_problem("ffi", "stable"), None);
}

#[test]
fn a_package_that_still_asks_for_ffi_builds() {
    let asked = format!("@!feature(ffi)\n\n{CALLS}");
    let with = package("with", MANIFEST, &[("Main.mw", &asked)]);
    assert_eq!(said(&with), "");
}

#[test]
fn a_module_that_is_not_the_root_cannot_ask() {
    let asked = format!("@!feature(ffi)\n\n{CALLS}");
    let dir = package(
        "elsewhere",
        MANIFEST,
        &[("Main.mw", "def result = 1\n"), ("Other.mw", &asked)],
    );
    let problem = said(&dir);
    assert!(
        problem.contains("it goes at the top of the root module"),
        "{problem}"
    );
}

#[test]
fn a_feature_nobody_knows_and_an_attribute_no_module_has_are_said() {
    let dir = package(
        "unknown",
        MANIFEST,
        &[("Main.mw", "@!feature(warp)\n@!pub\n\ndef result = 1\n")],
    );
    let problem = said(&dir);
    assert!(
        problem.contains("`warp` is not a feature this `meadow` knows"),
        "{problem}"
    );
    assert!(
        problem.contains("`@!pub` is not an attribute a module has"),
        "{problem}"
    );
}

#[test]
fn the_manifest_is_not_where_it_is_asked_any_more() {
    let manifest = format!("{MANIFEST}features = [\"ffi\"]\n");
    let dir = package("manifest", &manifest, &[("Main.mw", CALLS)]);
    let problem = said(&dir);
    assert!(
        problem.contains("`features` under `[package]` is not read any more")
            && problem.contains("`ffi`, which was asked for there, is stable"),
        "{problem}"
    );
}

#[test]
fn a_lone_file_calls_c_as_a_package_does() {
    let dir = scratch("lone");
    let file = dir.join("calls.mw");
    std::fs::write(&file, CALLS).unwrap();
    assert_eq!(said(&file), "");
    std::fs::write(&file, format!("@!feature(ffi)\n\n{CALLS}")).unwrap();
    assert_eq!(said(&file), "");
}
