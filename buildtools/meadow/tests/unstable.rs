//! What is not settled is asked for by name, in the source: `@!feature(ffi)`
//! at the top of a package's root module, as `#![feature(…)]` is at the top
//! of a crate's. See `meadow::channel`.
//!
//! These run as the `meadow` they are built with: a build from a checkout
//! takes what a nightly takes, and a release's stable one refuses what is
//! asked for (`accepted`). What a stable `meadow` says is `channel`'s own
//! tests'.

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

/// What a package that asks for `ffi` rightly is told: nothing, by a build
/// from a checkout or a nightly; and by a release's stable `meadow`, which
/// takes no unstable feature, only that.
fn accepted(said: &str) {
    if meadow::channel::CHANNEL == "stable" {
        assert!(
            said.contains("the feature `ffi` is unstable, and this is a stable `meadow`"),
            "{said}"
        );
    } else {
        assert_eq!(said, "");
    }
}

const CALLS: &str = "use Std.Ffi as Ffi\n\ndef result = 1\n";

#[test]
fn a_package_that_calls_c_says_so_at_the_top_of_its_root_module() {
    let without = package("without", MANIFEST, &[("Main.mw", CALLS)]);
    let problem = said(&without);
    assert!(
        problem.contains("`Std.Ffi` is unstable") && problem.contains("`@!feature(ffi)`"),
        "{problem}"
    );

    let asked = format!("@!feature(ffi)\n\n{CALLS}");
    let with = package("with", MANIFEST, &[("Main.mw", &asked)]);
    accepted(&said(&with));
}

#[test]
fn the_root_module_asks_for_the_whole_package() {
    let dir = package(
        "whole",
        MANIFEST,
        &[
            ("Main.mw", "@!feature(ffi)\n\ndef result = 1\n"),
            ("Other.mw", "use Std.Ffi as Ffi\n\n@pub def two = 2\n"),
        ],
    );
    accepted(&said(&dir));
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
    // And so nothing asked: the `use` is refused as well.
    assert!(problem.contains("`Std.Ffi` is unstable"), "{problem}");
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
            && problem.contains("`@!feature(ffi)`"),
        "{problem}"
    );
}

#[test]
fn a_lone_file_asks_as_a_package_does() {
    let dir = scratch("lone");
    let file = dir.join("calls.mw");
    std::fs::write(&file, CALLS).unwrap();
    assert!(said(&file).contains("`@!feature(ffi)`"));
    std::fs::write(&file, format!("@!feature(ffi)\n\n{CALLS}")).unwrap();
    accepted(&said(&file));
}
