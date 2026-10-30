//! A package's features: `[features]` in its `Meadow.toml`, turned on by the
//! command line or by a package that depends on it, and asked after with
//! `@cfg(feature = "…")`.
//!
//! As Cargo has them. A feature is a name; its entry lists the other
//! features of the package it turns on with it; `default` is the ones on
//! unless a build says not. What a feature turns on in the source is
//! whatever is written under a `@cfg` that names it.

use meadow::package::{AskedFeatures, ask_features};
use meadow::pipeline;
use meadow_eval as eval;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// What the command line asked for is the process's, so the tests take turns.
fn turn() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|p| p.into_inner())
}

fn scratch(what: &str) -> meadow_scratch::Scratch {
    meadow_scratch::scratch!("meadow-feat-{}-{what}", std::process::id())
}

fn package(dir: &Path, manifest: &str, file: &str, source: &str) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Meadow.toml"), manifest).unwrap();
    std::fs::write(dir.join("src").join(file), source).unwrap();
}

const SHAPES: &str = "\
[package]
name = \"Shapes\"
version = \"0.1.0\"

[features]
default = [\"round\"]
round = []
fancy = [\"round\"]
";

const SHAPES_SRC: &str = "\
@cfg(feature = \"round\")
@pub def round = \"round\"

@cfg(not(feature = \"round\"))
@pub def round = \"square\"

@cfg(feature = \"fancy\")
@pub def fancy = \"fancy\"

@cfg(not(feature = \"fancy\"))
@pub def fancy = \"plain\"
";

const APP_SRC: &str = "\
use Shapes (round, fancy)

@cfg(feature = \"loud\")
def voice = \"LOUD\"

@cfg(not(feature = \"loud\"))
def voice = \"quiet\"

def result = (voice, round, fancy)
";

/// An app at `what/app` depending on Shapes at `what/lib` as `dep` says.
fn app(what: &str, dep: &str) -> meadow_scratch::Scratch {
    let root = scratch(what);
    package(&root.join("lib"), SHAPES, "Lib.mw", SHAPES_SRC);
    let manifest = format!(
        "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[features]\ndefault = []\nloud = []\n\n\
         [dependencies]\nShapes = {dep}\n"
    );
    package(&root.join("app"), &manifest, "Main.mw", APP_SRC);
    root.child("app")
}

fn built(app: &Path, asked: AskedFeatures) -> Result<String, String> {
    ask_features(asked);
    let out = pipeline::build(app, meadow::Options::debug().entry("result"));
    if !out.diagnostics.is_empty() {
        return Err(out
            .diagnostics
            .iter()
            .map(|d| d.msg.clone())
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let linked = out.linked.expect("a linked program");
    Ok(eval::run(&linked.program).expect("it runs").to_string())
}

fn named(names: &[&str]) -> AskedFeatures {
    AskedFeatures {
        named: names.iter().map(|n| n.to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn a_package_is_built_with_its_default_features() {
    let _turn = turn();
    let app = app("defaults", "{ path = \"../lib\" }");
    assert_eq!(
        built(&app, AskedFeatures::default()).unwrap(),
        "(\"quiet\", \"round\", \"plain\")"
    );
}

#[test]
fn the_command_line_turns_on_a_feature_of_the_package_it_builds() {
    let _turn = turn();
    let app = app("named", "{ path = \"../lib\" }");
    assert_eq!(
        built(&app, named(&["loud"])).unwrap(),
        "(\"LOUD\", \"round\", \"plain\")"
    );
    // Every one it has; and that is the package's, not its dependency's.
    let all = AskedFeatures {
        all: true,
        ..Default::default()
    };
    assert_eq!(
        built(&app, all).unwrap(),
        "(\"LOUD\", \"round\", \"plain\")"
    );
}

#[test]
fn a_dependent_says_which_of_a_dependencys_features_it_wants() {
    let _turn = turn();
    // `fancy` turns `round` on with it, though the defaults were declined.
    let app = app(
        "dependent",
        "{ path = \"../lib\", features = [\"fancy\"], default-features = false }",
    );
    assert_eq!(
        built(&app, AskedFeatures::default()).unwrap(),
        "(\"quiet\", \"round\", \"fancy\")"
    );
}

#[test]
fn declining_the_defaults_leaves_them_off() {
    let _turn = turn();
    let app = app(
        "declined",
        "{ path = \"../lib\", default-features = false }",
    );
    assert_eq!(
        built(&app, AskedFeatures::default()).unwrap(),
        "(\"quiet\", \"square\", \"plain\")"
    );
}

#[test]
fn a_feature_nobody_declared_is_said_with_the_ones_there_are() {
    let _turn = turn();
    let app1 = app("unknown", "{ path = \"../lib\" }");
    let said = built(&app1, named(&["nope"])).unwrap_err();
    assert!(
        said.contains("`App` has no feature `nope`, which the command line asks for")
            && said.contains("`loud`"),
        "{said}"
    );
    let app2 = app(
        "unknown-dep",
        "{ path = \"../lib\", features = [\"shiny\"] }",
    );
    let said = built(&app2, AskedFeatures::default()).unwrap_err();
    assert!(
        said.contains("`Shapes` has no feature `shiny`, which `App` asks for")
            && said.contains("`round`, `fancy`"),
        "{said}"
    );
}
