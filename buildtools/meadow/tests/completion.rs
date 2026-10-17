//! REPL tab completion, against the real embedded `Std`.
//!
//! `src/complete.rs` unit-tests the context logic on synthetic name tables; this
//! checks that a live `Std` actually populates each namespace, and that a name
//! only shows up in the namespace it belongs to.

use meadow::complete::{self, Names};
use meadow_compiler::{lexer::tokenize, parser, source::Source, source::SourceKind};

fn std_names() -> Names {
    let (pkgs, diags) = meadow::stdlib::std_packages(meadow::Options::debug().entry("result"));
    assert!(diags.is_empty(), "Std should compile cleanly: {diags:?}");
    complete::snapshot(&pkgs, &[])
}

/// `snapshot` with the given `use` lines already in effect.
fn names_with_uses(lines: &[&str]) -> Names {
    let (pkgs, _) = meadow::stdlib::std_packages(meadow::Options::debug().entry("result"));
    let uses: Vec<_> = lines.iter().map(|l| parse_decl(l)).collect();
    complete::snapshot(&pkgs, &uses)
}

fn parse_decl(src: &str) -> meadow_compiler::ast::LDecl {
    let source = Source::new(SourceKind::Interactive, src.into());
    let lex = tokenize(source);
    let (module, errs) = parser::parse("t".into(), source, &lex.tokens);
    assert!(errs.is_empty(), "{src:?} should parse: {errs:?}");
    module
        .expect("parsed")
        .value
        .decls
        .into_iter()
        .next()
        .unwrap()
}

/// What Tab would offer for `line` (cursor at the end).
fn complete_at(names: &Names, line: &str) -> Vec<String> {
    let start = complete::word_start(line, line.len());
    let ctx = complete::context(line);
    names.candidates(&ctx, &line[start..])
}

// --- the namespaces are populated -------------------------------------------

#[test]
fn values_come_from_the_prelude() {
    let n = std_names();
    assert!(n.values.contains(&"println".to_string()));
    assert!(n.values.contains(&"not".to_string()));
    // Operators are punctuation and would only be noise.
    assert!(!n.values.iter().any(|v| v.starts_with('+')));
}

#[test]
fn types_include_builtins_and_std_declarations() {
    let n = std_names();
    for t in [
        "Int", "BigInt", "Bool", "List", "Maybe", "Result", "Ordering",
    ] {
        assert!(n.types.contains(&t.to_string()), "missing type {t}");
    }
}

#[test]
fn constructors_are_separate_from_types() {
    let n = std_names();
    for c in ["Just", "None", "Ok", "Err", "Nil", "Cons", "Less"] {
        assert!(n.ctors.contains(&c.to_string()), "missing ctor {c}");
    }
    // `Maybe` is a type, not a constructor; `Just` is the reverse.
    assert!(!n.ctors.contains(&"Maybe".to_string()));
    assert!(!n.types.contains(&"Just".to_string()));
}

#[test]
fn modules_are_known_by_full_path() {
    let n = std_names();
    assert!(n.modules.contains_key("Std.Collections.List"));
    assert!(n.modules.contains_key("Std.Fs"));
    let list = &n.modules["Std.Collections.List"];
    assert!(list.contains(&"map".to_string()));
}

// --- completion picks the right namespace ------------------------------------

#[test]
fn a_lowercase_word_completes_values() {
    let n = std_names();
    let got = complete_at(&n, "1 + prin");
    assert_eq!(got, vec!["print", "println"]);
}

#[test]
fn an_uppercase_word_in_a_term_completes_constructors() {
    let n = std_names();
    let got = complete_at(&n, "def x = Jus");
    assert_eq!(got, vec!["Just"]);
    // A *type* must not be offered where a constructor belongs.
    assert!(!complete_at(&n, "def x = May").contains(&"Maybe".to_string()));
}

#[test]
fn a_type_position_completes_types() {
    let n = std_names();
    assert_eq!(
        complete_at(&n, "record R = { x : In"),
        vec!["IndentOpt", "Int", "Int16", "Int32", "Int64", "Int8"]
    );
    assert_eq!(complete_at(&n, "record R = { x : UInt8"), vec!["UInt8"]);
    assert!(complete_at(&n, "record R = { x : May").contains(&"Maybe".to_string()));
    // ...and not constructors.
    assert!(!complete_at(&n, "record R = { x : Jus").contains(&"Just".to_string()));
    // An alias's right-hand side, and a signature.
    assert!(complete_at(&n, "type Names = [May").contains(&"Maybe".to_string()));
    assert!(complete_at(&n, "@pub type T = Resu").contains(&"Result".to_string()));
    assert!(complete_at(&n, "fun f : Int -> May").contains(&"Maybe".to_string()));
    assert!(complete_at(&n, "def x : Stri").contains(&"String".to_string()));
    // Past the `=`, a term again.
    assert!(!complete_at(&n, "def x : Int = May").contains(&"Maybe".to_string()));
}

#[test]
fn use_completes_module_paths_one_segment_at_a_time() {
    let n = std_names();
    assert_eq!(complete_at(&n, "use St"), vec!["Std"]);
    let under_std = complete_at(&n, "use Std.");
    assert!(under_std.contains(&"Collections".to_string()));
    assert!(under_std.contains(&"Fs".to_string()));
    assert_eq!(complete_at(&n, "use Std.Collections.L"), vec!["List"]);
}

#[test]
fn use_name_list_completes_that_modules_exports() {
    let n = std_names();
    let got = complete_at(&n, "use Std.Collections.List (fold");
    assert!(got.contains(&"foldl".to_string()));
    assert!(got.contains(&"foldr".to_string()));
}

#[test]
fn a_bare_use_completes_its_names_unqualified() {
    // `use M` imports everything unqualified, so that is what Tab must offer.
    // `lookupAssoc` is `List`-only — the prelude's sequence API is `Vector`'s.
    let bare = std_names();
    assert!(!complete_at(&bare, "lookup").contains(&"lookupAssoc".to_string()));

    let n = names_with_uses(&["use Std.Collections.List"]);
    let got = complete_at(&n, "lookup");
    assert!(
        got.contains(&"lookupAssoc".to_string()),
        "a bare `use` should complete its names unqualified, got {got:?}"
    );
}

#[test]
fn a_bare_use_offers_no_qualifier() {
    // It introduces no qualifier, so completing one would offer names that do
    // not resolve.
    let n = names_with_uses(&["use Std.Collections.List"]);
    assert!(complete_at(&n, "List.ma").is_empty());
    assert!(!complete_at(&n, "Lis").contains(&"List".to_string()));
}

#[test]
fn selected_names_complete_unqualified() {
    let n = names_with_uses(&["use Std.Collections.List (intercalate)"]);
    assert!(complete_at(&n, "inter").contains(&"intercalate".to_string()));
    // …and only those: `lookupAssoc` is also `List`-only but was not named.
    assert!(!complete_at(&n, "lookup").contains(&"lookupAssoc".to_string()));
    assert!(complete_at(&n, "List.ma").is_empty());
}

#[test]
fn an_alias_is_what_completes() {
    let n = names_with_uses(&["use Std.Collections.List as L"]);
    assert_eq!(complete_at(&n, "L.ma"), vec!["map", "maximum"]);
    // A qualifier comes only from `as`, so the module's own name is not one.
    assert!(complete_at(&n, "List.ma").is_empty());
    // The alias is offered in term position.
    assert!(complete_at(&n, "L").contains(&"L".to_string()));
}

#[test]
fn an_alias_imports_nothing_unqualified() {
    let n = names_with_uses(&["use Std.Collections.List as L"]);
    assert!(!complete_at(&n, "lookup").contains(&"lookupAssoc".to_string()));
}

#[test]
fn an_alias_combines_with_selected_names() {
    let n = names_with_uses(&["use Std.Collections.List as L (intercalate)"]);
    assert!(complete_at(&n, "inter").contains(&"intercalate".to_string()));
    assert_eq!(complete_at(&n, "L.ma"), vec!["map", "maximum"]);
}

#[test]
fn nothing_is_offered_where_a_new_name_goes() {
    let n = std_names();
    assert!(complete_at(&n, "data D = ").is_empty());
    assert!(complete_at(&n, "record R = { na").is_empty());
    assert!(complete_at(&n, "use Std.Collections.List as ").is_empty());
}

#[test]
fn a_repl_definition_becomes_completable() {
    // A REPL line arrives as a package with `prelude_exports: None`, so
    // everything it defines is unqualified — the completer must see it.
    let (mut pkgs, _) = meadow::stdlib::std_packages(meadow::Options::debug().entry("result"));
    let (line, diags) = meadow_compiler::compile_str("repl:1", "def wobble = 1\n");
    assert!(diags.is_empty(), "{diags:?}");
    pkgs.push(line);
    let n = complete::snapshot(&pkgs, &[]);
    assert!(n.values.contains(&"wobble".to_string()));

    assert_eq!(complete_at(&n, "wob"), vec!["wobble"]);
}
