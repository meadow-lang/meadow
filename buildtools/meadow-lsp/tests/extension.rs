//! The VS Code extension's declarative parts.
//!
//! The grammar and the manifest are JSON that nothing else validates: a stray
//! comma ships an extension that fails to load, and the failure only shows up in
//! an editor. These are cheap to check here.

use serde_json::Value;
use std::path::PathBuf;

fn editor_file(rel: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../editors/vscode")
        .join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_manifest_is_valid_and_points_at_files_that_exist() {
    let pkg = editor_file("package.json");
    assert_eq!(pkg["contributes"]["languages"][0]["id"], "meadow");
    assert_eq!(pkg["contributes"]["languages"][0]["extensions"][0], ".mw");

    // What makes opening a `.mw` file start the extension. Since VS Code 1.74
    // the `onLanguage:meadow` activation event is generated from the language
    // contribution above, so the manifest does not list it — but only if the
    // engine requirement is high enough for that to be true.
    assert!(
        pkg["activationEvents"].is_null(),
        "activation events are generated from `contributes.languages`; listing \
         them again is what VS Code warns about"
    );
    let engine = pkg["engines"]["vscode"].as_str().unwrap();
    let major_minor: Vec<u32> = engine
        .trim_start_matches('^')
        .split('.')
        .take(2)
        .map(|p| p.parse().unwrap())
        .collect();
    assert!(
        major_minor >= vec![1, 74],
        "generated activation events need VS Code 1.74; the manifest asks for {engine}"
    );

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../editors/vscode");
    for rel in [
        pkg["main"].as_str().unwrap(),
        pkg["contributes"]["languages"][0]["configuration"]
            .as_str()
            .unwrap(),
        pkg["contributes"]["grammars"][0]["path"].as_str().unwrap(),
    ] {
        let p = root.join(rel.trim_start_matches("./"));
        assert!(p.exists(), "the manifest points at {rel}, which is missing");
    }
}

/// The theme is a second extension, in `editors/vscode-theme`, and as much
/// unvalidated JSON as the first: a theme whose file fails to parse is listed
/// in the picker and colours nothing.
#[test]
fn the_theme_manifest_points_at_themes_that_are_valid() {
    let pkg = editor_file("../vscode-theme/package.json");
    assert_eq!(
        pkg["version"],
        editor_file("package.json")["version"],
        "the theme is released with the language extension, at its version"
    );
    let themes = pkg["contributes"]["themes"].as_array().unwrap();
    assert_eq!(themes.len(), 2, "a dark theme and a light one");
    for entry in themes {
        let path = entry["path"].as_str().unwrap().trim_start_matches("./");
        let theme = editor_file(&format!("../vscode-theme/{path}"));
        // The picker shows the manifest's label; the file's own name is what
        // a `[Meadow Dark]` block in someone's settings is matched against.
        assert_eq!(theme["name"], entry["label"], "{path}");
        let kind = match entry["uiTheme"].as_str().unwrap() {
            "vs-dark" => "dark",
            "vs" => "light",
            other => panic!("{path}: unexpected uiTheme {other}"),
        };
        assert_eq!(theme["type"], kind, "{path}");
        // Without this the server's tokens are ignored, and a constructor is
        // coloured as the type the grammar guessed it to be.
        assert_eq!(theme["semanticHighlighting"], true, "{path}");

        let colours = theme["colors"]
            .as_object()
            .unwrap()
            .values()
            .chain(theme["semanticTokenColors"].as_object().unwrap().values())
            .chain(
                theme["tokenColors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|rule| &rule["settings"]["foreground"]),
            );
        for colour in colours {
            let hex = colour.as_str().unwrap();
            let digits = hex.strip_prefix('#').unwrap_or("");
            assert!(
                matches!(digits.len(), 6 | 8) && digits.chars().all(|c| c.is_ascii_hexdigit()),
                "{path}: {hex} is not a colour"
            );
        }
    }
}

#[test]
fn the_grammar_is_valid_and_matches_the_declared_scope() {
    let grammar = editor_file("syntaxes/meadow.tmLanguage.json");
    let pkg = editor_file("package.json");
    assert_eq!(
        grammar["scopeName"], pkg["contributes"]["grammars"][0]["scopeName"],
        "the grammar's scope must be the one the manifest declares"
    );
    // Every `include` has to name a rule that exists, or highlighting silently
    // stops at that point.
    let repo = grammar["repository"].as_object().unwrap();
    for pat in grammar["patterns"].as_array().unwrap() {
        let name = pat["include"].as_str().unwrap();
        let key = name.trim_start_matches('#');
        assert!(repo.contains_key(key), "no rule named {name}");
    }
}

#[test]
fn the_grammar_covers_the_languages_keywords() {
    let grammar = editor_file("syntaxes/meadow.tmLanguage.json");
    let text = grammar.to_string();
    for kw in [
        "fun", "def", "let", "in", "match", "with", "if", "then", "else", "data", "record",
        "effect", "handle", "use", "mod",
    ] {
        assert!(text.contains(kw), "the grammar never mentions `{kw}`");
    }
}

#[test]
fn the_language_configuration_uses_meadows_comment_syntax() {
    let cfg = editor_file("language-configuration.json");
    assert_eq!(cfg["comments"]["lineComment"], "--");
}
