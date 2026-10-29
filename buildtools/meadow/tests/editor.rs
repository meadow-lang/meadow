//! What the language server is given: a package, its dependencies, and what can
//! run a macro one of them exports.
//!
//! The server cannot build a package itself -- manifests and module discovery
//! live in this crate -- so it asks through `editor::load_package`. What that
//! hands over is what an editor knows about, and for a long time it left the
//! dependencies out: every name one provided was reported undefined.

use meadow::Options;
use std::path::{Path, PathBuf};

/// A package written out in a directory of its own.
fn package(what: &str, manifest: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-editor-{}-{what}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("a scratch package");
    std::fs::write(dir.join("Meadow.toml"), manifest).expect("a manifest");
    for (name, text) in files {
        std::fs::write(dir.join("src").join(name), text).expect("a module");
    }
    dir
}

/// The standard library, as the server holds it.
fn std_lib() -> meadow_lsp::analysis::Std {
    let opts = Options::debug().entry("result");
    let modules = meadow::stdlib::std_modules(opts)
        .0
        .into_iter()
        .map(|(dotted, pkg)| (dotted.to_string(), pkg))
        .collect();
    meadow_lsp::analysis::Std::new(
        meadow::stdlib::std_packages(opts).0,
        modules,
        None,
        meadow::stdlib::MODULES,
    )
}

/// What the server would report for `file`, loaded the way it loads one.
fn diagnostics(file: &Path) -> Vec<String> {
    let sources = meadow::editor::load_package(file).expect("the file is in a package");
    let text = std::fs::read_to_string(file).expect("the file reads");
    let analysis = std_lib()
        .analyse_package(&sources, file, &text)
        .expect("the file is one of the package's modules");
    analysis.diagnostics.iter().map(|d| d.msg.clone()).collect()
}

#[test]
fn a_packages_dependencies_are_loaded_with_it() {
    let lib = package(
        "dep",
        "[package]\nname = \"Util\"\nversion = \"0.1.0\"\n",
        &[("Lib.mw", "@pub fun double x = x + x\n")],
    );
    let app = package(
        "dependent",
        &format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nutil = {{ path = \"{}\" }}\n",
            lib.display()
        ),
        &[("Lib.mw", "use util (double)\n\ndef result = double 21\n")],
    );
    let file = app
        .join("src/Lib.mw")
        .canonicalize()
        .expect("the file is there");
    let sources = meadow::editor::load_package(&file).expect("the file is in a package");
    assert_eq!(
        sources
            .deps
            .iter()
            .map(|(n, _)| n.to_string())
            .collect::<Vec<_>>(),
        vec!["util".to_string()],
        "the dependency comes with the package"
    );
    assert!(diagnostics(&file).is_empty(), "{:?}", diagnostics(&file));
}

#[test]
fn a_name_a_dependency_does_not_have_is_still_undefined() {
    // The point is not that errors go away: it is that the right ones are left.
    let lib = package(
        "dep-partial",
        "[package]\nname = \"Util\"\nversion = \"0.1.0\"\n",
        &[("Lib.mw", "@pub fun double x = x + x\n")],
    );
    let app = package(
        "dependent-partial",
        &format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nutil = {{ path = \"{}\" }}\n",
            lib.display()
        ),
        &[("Lib.mw", "use util (double)\n\ndef result = treble 21\n")],
    );
    let file = app
        .join("src/Lib.mw")
        .canonicalize()
        .expect("the file is there");
    let errs = diagnostics(&file);
    assert!(errs.iter().any(|e| e.contains("treble")), "{errs:?}");
}

#[test]
fn a_procedural_macro_runs_for_the_editor() {
    // The server is given something that can run one, so a document using a
    // macro is analysed as what the macro made of it -- not as a file full of
    // undefined names.
    let lib = package(
        "macro-dep",
        "[package]\nname = \"Maker\"\nversion = \"0.1.0\"\n",
        &[(
            "Lib.mw",
            "use Std.Macro.TokenTree.*\n\n@macro\n@pub fun defineOne ts = [Code \"def one = 1\"]\n",
        )],
    );
    let app = package(
        "macro-user",
        &format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nmaker = {{ path = \"{}\" }}\n",
            lib.display()
        ),
        &[(
            "Lib.mw",
            "use maker (defineOne!)\n\ndefineOne!()\n\ndef result = one\n",
        )],
    );
    let file = app
        .join("src/Lib.mw")
        .canonicalize()
        .expect("the file is there");
    assert!(diagnostics(&file).is_empty(), "{:?}", diagnostics(&file));
}

// --- opening a file fetches nothing -------------------------------------------------
//
// A manifest is text anyone may have written, and opening a file in an editor
// is not asking for a build. The server used to resolve dependencies the way a
// build does: a git dependency was cloned, compiled, and its macros run, on
// opening any file of the package.

#[test]
fn a_git_dependency_is_not_fetched_for_the_editor() {
    // `.invalid` never resolves: were it fetched, git would be run and fail
    // with its own message, after trying the network.
    let dir = package(
        "git-dep",
        "[package]\nname = \"Victim\"\nversion = \"0.1.0\"\n\n[dependencies]\n\
         Far = { git = \"https://example.invalid/far.git\" }\n",
        &[("Main.mw", "def result = 1\n")],
    );
    let file = dir.join("src").join("Main.mw");
    let started = std::time::Instant::now();
    let got = meadow::editor::find_package(&file).expect("the file is in a package");
    let why = match got {
        Ok(_) => panic!("a dependency that is not fetched cannot be loaded"),
        Err(why) => why,
    };
    // What is missing, what fetches it, and why the editor did not -- and not
    // the resolver's words about `--offline`, which nobody here passed.
    assert!(why.contains("`Far`"), "{why}");
    assert!(why.contains("has not been fetched yet"), "{why}");
    assert!(why.contains("trust this one"), "{why}");
    assert!(why.contains("`meadow build`"), "{why}");
    assert!(!why.contains("offline"), "{why}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "nothing was waited on: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_path_dependency_is_still_loaded_for_the_editor() {
    // What is on disk already is not fetched: it is read, as before.
    let lib = package(
        "offline-lib",
        "[package]\nname = \"Near\"\nversion = \"0.1.0\"\n",
        &[("Lib.mw", "@pub def near = 41\n")],
    );
    let app = package(
        "offline-app",
        &format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nNear = {{ path = \"{}\" }}\n",
            lib.display().to_string().replace('\\', "/")
        ),
        &[("Lib.mw", "use Near (near)\n\ndef result = near + 1\n")],
    );
    let file = app
        .join("src/Lib.mw")
        .canonicalize()
        .expect("the file is there");
    let diags = diagnostics(&file);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn what_a_derive_writes_does_not_make_its_declaration_a_link() {
    // A procedural derive's output has the declaration's span, the type it
    // names included: every character of `data Colour = ...` -- the keyword
    // too -- went to `Colour`, and an editor offered a link on all of it.
    let lib = package(
        "derive-dep",
        "[package]\nname = \"Maker\"\nversion = \"0.1.0\"\n",
        &[(
            "Lib.mw",
            "use Std.Macro.TokenTree.*\n\n@macro\n\
             @pub fun describe ts = [Code \"fun describe (c : Colour) : Int = 1\"]\n",
        )],
    );
    let src = "use maker (describe!)\n\n@derive(Describe)\ndata Colour = Red | Green\n\n\
               def result = describe Colour.Red\n";
    let app = package(
        "derive-user",
        &format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nmaker = {{ path = \"{}\" }}\n",
            lib.display()
        ),
        &[("Lib.mw", src)],
    );
    let file = app
        .join("src/Lib.mw")
        .canonicalize()
        .expect("the file is there");
    let sources = meadow::editor::load_package(&file).expect("a package");
    let std = std_lib();
    let a = std.analyse_package(&sources, &file, src).expect("analysis");
    assert!(
        a.diagnostics.is_empty(),
        "{:?}",
        a.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let decl = src.find("data Colour").unwrap();
    let line = "data Colour = Red | Green";
    for (off, c) in line.char_indices() {
        // The keyword, and what stands between the names.
        if !(off < "data".len() || "=| ".contains(c)) {
            continue;
        }
        let found = a.definition_at(decl + off, std.definitions(), std.declared_names());
        assert!(
            found.is_none(),
            "{c:?} at {off} is a link to {:?}",
            found.map(|l| l.source.content[l.span.start as usize..l.span.end as usize].to_string())
        );
    }
    // The name it declares is itself, as any declaration's is -- not the
    // whole declaration the derive's output stands at.
    let name = decl + "data ".len();
    let found = a
        .definition_at(name, std.definitions(), std.declared_names())
        .expect("the name");
    assert_eq!(
        &src[found.span.start as usize..found.span.end as usize],
        "Colour"
    );
    // What is written still is: `describe` and `Colour` in `result`.
    let at = src.rfind("Colour.Red").unwrap();
    assert!(
        a.definition_at(at, std.definitions(), std.declared_names())
            .is_some()
    );
    let at = src.rfind("describe").unwrap();
    assert!(
        a.definition_at(at, std.definitions(), std.declared_names())
            .is_some()
    );
}
