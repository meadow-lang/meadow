//! What the server answers, without the protocol in the way.

use meadow_lsp::analysis::Std;
use meadow_lsp::pos::LineIndex;

/// Analyse `src` and resolve `⟨marker⟩` to a byte offset. The marker is removed
/// before compiling, so it never affects the result.
fn at(src: &str, marker: &str) -> (meadow_lsp::analysis::Analysis, usize) {
    let offset = src
        .find(marker)
        .unwrap_or_else(|| panic!("no {marker:?} in source"));
    let clean = src.replacen(marker, "", 1);
    // The marker sits just before the token of interest.
    (STD.with(|s| s.analyse(&clean)), offset)
}

/// The standard library, in both the shapes the server wants: the bundle a
/// package depends on, and the modules it was bundled from.
fn std() -> Std {
    let opts = meadow::Options::debug();
    let modules = meadow::stdlib::std_modules(opts)
        .0
        .into_iter()
        .map(|(dotted, pkg)| (dotted.to_string(), pkg))
        .collect();
    Std::new(
        meadow::stdlib::std_packages(opts).0,
        modules,
        None,
        meadow::stdlib::MODULES,
    )
}

thread_local! {
    static STD: Std = std();
}

#[test]
fn hover_reports_the_type_at_a_position() {
    let (a, off) = at("fun double n = n * 2\ndef demo = do@uble 21\n", "@");
    let hover = a.hover_at(off).expect("hover");
    assert!(
        hover.contains("double : forall n. Mul n => n -> n"),
        "got: {hover}"
    );
}

#[test]
fn hover_shows_the_generalised_scheme_not_the_use_site() {
    let (a, off) = at("fun ident x = x\ndef demo = ide@nt 1\n", "@");
    let hover = a.hover_at(off).expect("hover");
    assert!(
        hover.contains("forall"),
        "a top-level binding should hover as its scheme: {hover}"
    );
}

#[test]
fn hover_includes_the_doc_comment_above_the_definition() {
    let src = "\
-- Doubles its argument.
-- Twice as much, in other words.
fun double n = n * 2

def demo = do@uble 21
";
    let (a, off) = at(src, "@");
    let hover = a.hover_at(off).expect("hover");
    assert!(hover.contains("Doubles its argument."), "got: {hover}");
    assert!(hover.contains("Twice as much"), "got: {hover}");
}

#[test]
fn a_doc_comment_stops_at_a_blank_line() {
    let src = "\
-- Not part of the docs.

-- The real docs.
fun double n = n * 2

def demo = do@uble 21
";
    let (a, off) = at(src, "@");
    let hover = a.hover_at(off).unwrap();
    assert!(hover.contains("The real docs."));
    assert!(!hover.contains("Not part of the docs."), "got: {hover}");
}

#[test]
fn hovering_a_constructor_shows_its_path_rather_than_its_type() {
    let (a, off) = at(
        "data Shape = Circle Int | Square Int\nuse Shape.*\ndef demo = Ci@rcle 2\n",
        "@",
    );
    let hover = a.hover_at(off).expect("hover");
    assert_eq!(hover, "```meadow\nShape.Circle\n```");
}

#[test]
fn hovering_a_constructor_from_std_names_its_package_and_module() {
    let (a, off) = at("def demo = J@ust 1\n", "@");
    assert_eq!(
        a.hover_at(off).expect("hover"),
        "```meadow\nStd.Maybe.Maybe.Just\n```"
    );
    let (a, off) = at(
        "def demo = match Just 1 with | N@one -> 0 | Just n -> n\n",
        "@",
    );
    assert_eq!(
        a.hover_at(off).expect("hover"),
        "```meadow\nStd.Maybe.Maybe.None\n```"
    );
}

#[test]
fn hovering_a_qualified_constructor_shows_its_path() {
    let (a, off) = at("data Ty = Int | Bool\ndef demo = Ty.I@nt\n", "@");
    assert_eq!(a.hover_at(off).expect("hover"), "```meadow\nTy.Int\n```");
}

#[test]
fn hovering_a_siblings_constructor_names_the_package_and_module() {
    let root = package(
        "ctorhover",
        Some("[package]\nname = \"Demo\"\n"),
        &[
            ("Syntax.mw", "@pub(pkg) data Tv = Unbound Int | Link Int\n"),
            (
                "Main.mw",
                "use Demo.Syntax (Tv)\ndef demo = match Tv.Link 1 with | Tv.Link n -> n | Tv.Unbound n -> n\n",
            ),
        ],
    );
    let file = root.join("src").join("Main.mw");
    let text = std::fs::read_to_string(&file).unwrap();
    let sources = meadow::editor::load_package(&file).expect("a package");
    let a = STD
        .with(|s| s.analyse_package(&sources, &file, &text))
        .expect("analysis");
    let off = text.find("Unbound n").unwrap() + 2;
    assert_eq!(
        a.hover_at(off).expect("hover"),
        "```meadow\nDemo.Syntax.Tv.Unbound\n```"
    );
}

#[test]
fn go_to_definition_finds_the_binder() {
    let src = "fun double n = n * 2\ndef demo = do@uble 21\n";
    let (a, off) = at(src, "@");
    let def = a
        .definition_at(off, &Default::default(), &Default::default())
        .expect("definition");
    let clean = src.replacen("@", "", 1);
    assert_eq!(
        &clean[def.span.start as usize..def.span.end as usize],
        "double"
    );
}

#[test]
fn go_to_definition_works_for_a_local_binding() {
    let src = "fun f u =\n  let answer = 42 in\n  ans@wer + 1\ndef demo = f ()\n";
    let (a, off) = at(src, "@");
    let def = a
        .definition_at(off, &Default::default(), &Default::default())
        .expect("definition");
    let clean = src.replacen("@", "", 1);
    assert_eq!(
        &clean[def.span.start as usize..def.span.end as usize],
        "answer"
    );
    // …and it is the *local* one, not something at the top level.
    assert!(def.span.start as usize > clean.find("let").unwrap());
}

#[test]
fn the_innermost_node_wins() {
    // At `xs` the type is the vector's, not the enclosing `len xs`'s `Int`.
    let (a, off) = at("fun size xs = len @xs\ndef demo = size [1, 2]\n", "@");
    assert_eq!(a.type_at(off), Some("[a]"));
}

/// Render the hints back into the source, the way an editor overlays them.
fn hinted(src: &str) -> String {
    use meadow_lsp::analysis::HintPart;
    let a = STD.with(|s| s.analyse(src));
    let mut out = src.to_string();
    let mut hints: Vec<_> = a.binders.iter().collect();
    // Right to left, so an earlier insertion does not move a later offset.
    hints.sort_by_key(|h| std::cmp::Reverse(h.offset));
    for h in hints {
        let text: String = h
            .parts
            .iter()
            .map(|p| match p {
                HintPart::Text(t) => t.clone(),
                HintPart::Name(n) => n.to_string(),
            })
            .collect();
        out.insert_str(h.offset as usize, &text);
    }
    out
}

/// Hints read as the annotation you could have typed in their place.
///
/// The parentheses are the point: `(x : Int)` is real syntax, so the hint is
/// written as a replacement for the name rather than as a notation of its own.
#[test]
fn inlay_hints_read_as_a_writable_annotation() {
    assert_eq!(
        hinted("fun add a b = a + b\n"),
        "fun add (a : a) (b : a) : a = a + b\n"
    );
    assert_eq!(
        hinted("fun f u = let k = toInt 1 in k\n"),
        "fun f (u : a) : Int = let (k : Int) = toInt 1 in k\n"
    );
}

#[test]
fn a_parameter_that_is_already_annotated_is_not_hinted_again() {
    assert_eq!(
        hinted("fun add (a : Int) b = a + b\n"),
        "fun add (a : Int) (b : Int) : Int = a + b\n"
    );
}

/// A parameter that brings its own parentheses closes before its span ends, so
/// the result type cannot be appended to it and gets a hint of its own.
#[test]
fn a_parenthesised_parameter_still_gets_a_result_type() {
    assert_eq!(
        hinted("fun snd (a, b) = b\n"),
        "fun snd ((a : a), (b : b)) : b = b\n"
    );
}

#[test]
fn a_lambda_gets_its_parameters_but_no_result_type() {
    // `\(n : Int) -> …` is writable; a result type there is not. A `def`
    // does not generalize a number class, so the literal defaults to `Int`.
    assert_eq!(
        hinted("def f = \\n -> n + 1\n"),
        "def f = \\(n : Int) -> n + 1\n"
    );
}

/// A type name in a hint is a part of its own, so the server can link it.
#[test]
fn a_hint_names_the_types_inside_it_separately() {
    use meadow_lsp::analysis::HintPart;
    let a = STD.with(|s| s.analyse("fun pick m = match m with | Just v -> v | None -> toInt 0\n"));
    let names: Vec<String> = a
        .binders
        .iter()
        .flat_map(|h| &h.parts)
        .filter_map(|p| match p {
            HintPart::Name(n) => Some(n.to_string()),
            _ => None,
        })
        .collect();
    assert!(names.contains(&"Maybe".to_string()), "got {names:?}");
    assert!(names.contains(&"Int".to_string()), "got {names:?}");
    // Whether a name *links* is decided when the request is answered, by
    // whether the index has it: `Maybe` is declared somewhere, `Int` is not.
    STD.with(|s| {
        let ty = |n: &str| meadow_compiler::intern::InternedString::from(n);
        assert!(s.declared_names().types.contains_key(&ty("Maybe")));
        assert!(!s.declared_names().types.contains_key(&ty("Int")));
    });
}

#[test]
fn a_top_level_name_gets_no_inlay_hint() {
    // Its signature is already on the line; repeating it is noise.
    let a = STD.with(|s| s.analyse("def answer = 42\n"));
    assert!(a.binders.is_empty(), "got: {:?}", a.binders);
}

#[test]
fn diagnostics_come_through() {
    let a = STD.with(|s| s.analyse("def demo = 1 + \"nope\"\n"));
    assert!(
        a.diagnostics
            .iter()
            .any(|d| d.msg.contains("type mismatch")),
        "got: {:?}",
        a.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
}

#[test]
fn a_clean_file_has_no_diagnostics() {
    let a = STD.with(|s| s.analyse("def demo = 1 + 2\n"));
    assert!(a.diagnostics.is_empty(), "got: {:?}", a.diagnostics);
}

#[test]
fn a_parse_error_does_not_panic() {
    let a = STD.with(|s| s.analyse("def demo = let in\n"));
    assert!(!a.diagnostics.is_empty());
    assert!(a.hover_at(0).is_none() || a.hover_at(0).is_some());
}

#[test]
fn positions_map_through_to_spans() {
    let src = "fun double n = n * 2\ndef demo = double 21\n";
    let a = STD.with(|s| s.analyse(src));
    let idx = LineIndex::new(src);
    // Line 1, character 11 is inside `double` in `def demo = double 21`.
    let off = idx.offset(1, 11);
    let def = a
        .definition_at(off, &Default::default(), &Default::default())
        .expect("definition");
    let (line, _) = idx.position(def.span.start as usize);
    assert_eq!(line, 0, "the definition is on the first line");
}

// --- editing the standard library itself -----------------------------------

#[test]
fn a_std_module_recognises_itself_by_path() {
    let s = std();
    for (uri, want) in [
        (
            "file:///c%3A/repos/meadow/lib/Std/src/Collections/Vector.mw",
            Some("Collections.Vector"),
        ),
        ("file:///home/u/meadow/lib/Std/src/Maybe.mw", Some("Maybe")),
        (
            "file:///home/u/meadow/lib/Std/src/Prelude.mw",
            Some("Prelude"),
        ),
        // Backslashes, as a Windows path reaches us.
        ("c:\\repos\\meadow\\lib\\Std\\src\\Json.mw", Some("Json")),
        // Not one of ours, however much it looks like one.
        ("file:///home/u/other/lib/Std/src/NotAModule.mw", None),
        ("file:///home/u/meadow/examples/Euler/src/Main.mw", None),
    ] {
        let got = s.module_at(uri).map(|i| s.module_name(i));
        assert_eq!(got, want, "{uri}");
    }
}

#[test]
fn every_std_module_analyses_clean_as_itself() {
    // The bug this exists for: a `Std` source opened in the editor was analysed
    // as a package *depending* on `Std`, so every type and constructor it
    // declares was declared twice — once here, once in its own dependency — and
    // the file filled with `already defined`.
    //
    // It also keeps `module_path` here honest against the one in
    // `meadow::stdlib`, which this crate cannot call: if the two ever disagree
    // about where a module sits, its exports land in the wrong place and
    // something below stops resolving.
    let s = std();
    let opts = meadow::Options::debug();
    for (dotted, _) in meadow::stdlib::std_modules(opts).0 {
        let source = meadow::stdlib::MODULES
            .iter()
            .find(|(name, _)| *name == dotted)
            .expect("every compiled module comes from MODULES")
            .1;
        let i = s
            .module_at(&format!(
                "file:///w/lib/Std/src/{}.mw",
                dotted.replace('.', "/")
            ))
            .unwrap_or_else(|| panic!("{dotted} should be recognised by its path"));
        let a = s.analyse_module(i, source);
        assert!(
            a.diagnostics.is_empty(),
            "{dotted} does not analyse clean:\n{}",
            a.diagnostics
                .iter()
                .map(|d| format!("  {}", d.msg))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

#[test]
fn a_std_module_analysed_the_ordinary_way_collides_with_itself() {
    // Which is why the special case exists. `Maybe` declares `Option`, and the
    // `Std` a normal document depends on already has one.
    let s = std();
    let source = meadow::stdlib::MODULES
        .iter()
        .find(|(name, _)| *name == "Maybe")
        .unwrap()
        .1;
    let a = s.analyse(source);
    assert!(
        a.diagnostics
            .iter()
            .any(|d| d.msg.contains("already defined")),
        "expected the collision this feature avoids, got {:?}",
        a.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
}

// --- across modules -----------------------------------------------------------

/// A name from another module resolves to *that module's* definition.
///
/// The point is not that a span comes back — it is that the span is an offset
/// into a different file, and that the file is the one that defines the name.
/// Checking the text at the span against the source the `Loc` names is what
/// makes this test able to fail: a span from the wrong file would land on
/// whatever happens to sit at that offset.
#[test]
fn go_to_definition_reaches_into_the_standard_library() {
    let src = "use Std.String (concatAll)\ndef demo = concat@All\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("a definition for an imported name");

    assert_ne!(
        loc.source.id, a.source_id,
        "the definition should be in another source, not the open document"
    );
    let text = &loc.source.content[loc.span.start as usize..loc.span.end as usize];
    assert_eq!(text, "concatAll");
    assert!(
        loc.source.name().to_string().ends_with("String.mw"),
        "expected Std/String.mw, got {}",
        loc.source.name()
    );
}

/// A name the prelude re-exports, reached with no `use` at all.
///
/// `map` is deliberate: `Std.Collections.List` defines one too, and the prelude
/// says bare `map` is the *`Vector`* one. Landing in `Vector.mw` is the test --
/// an index that merely found *a* binding called `map` would be wrong here.
#[test]
fn a_prelude_name_is_followed_to_the_module_that_defines_it() {
    let src = "fun incr x = x + 1\ndef demo = ma@p incr [1; 2; 3]\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("a definition for a prelude name");
    let text = &loc.source.content[loc.span.start as usize..loc.span.end as usize];
    assert_eq!(text, "map");
    assert!(
        loc.source
            .name()
            .to_string()
            .ends_with("Collections/Vector.mw"),
        "bare `map` is Vector's, got {}",
        loc.source.name()
    );
}

/// A local binding still wins over an imported one of the same name, and still
/// reports the open document.
#[test]
fn a_local_definition_is_preferred_to_an_imported_one() {
    let src = "fun map f xs = xs\nfun id x = x\ndef demo = ma@p id [1; 2]\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("definition");
    assert_eq!(
        loc.source.id, a.source_id,
        "the shadowing definition is the one in this file"
    );
}

/// Editing one `Std` module and following a name into an earlier one — the case
/// that only works because a module analysed in its own place is compiled
/// against the *same* units the index was built from.
#[test]
fn one_std_module_can_reach_another() {
    STD.with(|s| {
        let i = s
            .module_at("/anywhere/lib/Std/src/Collections/Vector.mw")
            .expect("Vector is a Std module");
        let text = meadow::stdlib::MODULES
            .iter()
            .find(|(d, _)| *d == "Collections.Vector")
            .expect("its source")
            .1;
        let a = s.analyse_module(i, text);

        // Some name it uses that this module does not define.
        //
        // Not simply "whose index entry names another source": a `VarId` is
        // stable per unit, so re-analysing `Vector.mw` mints the very ids the
        // cached copy has, and a *local* binding is therefore in the index too
        // — pointing at the cached file rather than at this throwaway one.
        // That is the ids working; it just is not a cross-module reference.
        let (off, _) = a
            .refs
            .iter()
            .filter(|(_, v)| !a.defs.contains_key(v))
            .filter_map(|(span, v)| s.definitions().get(v).map(|l| (span.start as usize, *l)))
            .find(|(_, l)| l.source.id != a.source_id)
            .expect("Vector refers to something from another module");

        let loc = a
            .definition_at(off, s.definitions(), s.declared_names())
            .expect("and it can be followed");
        assert_ne!(loc.source.id, a.source_id);
        assert!(loc.source.name().to_string().ends_with(".mw"));
    });
}

// --- types and data constructors ----------------------------------------------

/// Follow a type name, in the same file.
#[test]
fn go_to_definition_finds_a_type() {
    let src = "data Colour = Red | Green\ndata Box = Box Col@our\ndef demo = 0\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("a definition for a type");
    let clean = src.replacen("@", "", 1);
    assert_eq!(
        &clean[loc.span.start as usize..loc.span.end as usize],
        "Colour"
    );
    assert_eq!(loc.span.start as usize, clean.find("Colour").unwrap());
}

/// Follow a data constructor from an expression, and from a pattern.
#[test]
fn go_to_definition_finds_a_constructor() {
    for src in [
        "data Colour = Red | Green\nuse Colour.*\ndef demo = Gre@en\n",
        "data Colour = Red | Green\nuse Colour.*\nfun f c = match c with | Gre@en -> 1 | Red -> 0\ndef demo = f Red\n",
    ] {
        let (a, off) = at(src, "@");
        let loc = STD
            .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
            .expect("a definition for a constructor");
        let clean = src.replacen("@", "", 1);
        assert_eq!(
            &clean[loc.span.start as usize..loc.span.end as usize],
            "Green"
        );
        // The *declaration*, not the other use site.
        assert_eq!(
            loc.span.start as usize,
            clean.find("Green").unwrap(),
            "{src}"
        );
    }
}

/// An overloaded constructor goes to the candidate inference chose -- not to
/// whichever the resolver happened to list first.
#[test]
fn go_to_definition_follows_the_overload_that_was_chosen() {
    let head = "use Std.Maybe.Maybe (Just, None)\ndata Box = Just Int | Empty\nuse Box.*\n";

    let src = format!("{head}fun unbox (b : Box) = match b with | Ju@st n -> n | Empty -> 0\n");
    let (a, off) = at(&src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("a definition for the local `Just`");
    assert_eq!(loc.source.id, a.source_id, "this file's `Box.Just`");
    let clean = src.replacen("@", "", 1);
    assert_eq!(loc.span.start as usize, clean.find("Just Int").unwrap());

    let src =
        format!("{head}fun orZero (m : Maybe Int) = match m with | Ju@st n -> n | None -> 0\n");
    let (a, off) = at(&src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("a definition for `Maybe.Just`");
    assert_ne!(loc.source.id, a.source_id, "`Std.Maybe`'s `Just`");
}

/// A name that is both a type and a constructor resolves by where it is written.
///
/// `data Pair = Pair Int Int` puts `Pair` in both namespaces at different
/// positions. An index that kept one map would answer one of them for both, and
/// be right half the time.
#[test]
fn a_type_and_a_constructor_may_share_a_name() {
    let src = "data Pair = Pair Int Int\nfun fst p = match p with | Pai@r a b -> a\ndef demo = 0\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("definition");
    let clean = src.replacen("@", "", 1);
    // The *constructor* `Pair`, which is the second occurrence on line 1.
    let ctor = clean.find("= Pair").unwrap() + 2;
    assert_eq!(
        loc.span.start as usize, ctor,
        "should be the constructor, not the type"
    );

    // And in a type position, the type.
    let src = "data Pair = Pair Int Int\ndata Box = Box Pai@r\ndef demo = 0\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("definition");
    let clean = src.replacen("@", "", 1);
    assert_eq!(loc.span.start as usize, clean.find("Pair").unwrap());
}

/// The innermost name wins: in `Outer Inner` the cursor decides which.
#[test]
fn the_innermost_type_wins() {
    let src = "\
data Inner = I
data Outer a = O a
data Holder = Holder (Outer Inn@er)
def demo = 0
";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("definition");
    let clean = src.replacen("@", "", 1);
    assert_eq!(
        &clean[loc.span.start as usize..loc.span.end as usize],
        "Inner"
    );
}

/// A builtin type has no declaration, and the answer is *nothing* rather than
/// the enclosing type.
///
/// `Int` in `Outer Int` is nested inside a name that does have a definition, so
/// a lookup that widened on a miss would jump to `Outer` — a confident wrong
/// answer, which is worse than none.
#[test]
fn a_builtin_type_has_nowhere_to_go() {
    let src = "data Outer a = O a\ndata Holder = Holder (Outer In@t)\ndef demo = 0\n";
    let (a, off) = at(src, "@");
    let loc = STD.with(|s| a.definition_at(off, s.definitions(), s.declared_names()));
    assert!(
        loc.is_none(),
        "expected no definition for a builtin, got {:?}",
        loc.map(|l| l.source.name().to_string())
    );
}

/// A type from another module, across the package boundary.
#[test]
fn go_to_definition_reaches_a_type_in_the_standard_library() {
    let src = "data Holder = Holder (May@be Int)\ndef demo = 0\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("a definition for an imported type");
    assert_ne!(loc.source.id, a.source_id, "it is in another file");
    let text = &loc.source.content[loc.span.start as usize..loc.span.end as usize];
    assert_eq!(text, "Maybe");
    assert!(
        loc.source.name().to_string().ends_with("Maybe.mw"),
        "got {}",
        loc.source.name()
    );
}

/// And a constructor from another module.
#[test]
fn go_to_definition_reaches_a_constructor_in_the_standard_library() {
    let src = "def demo = Jus@t 5\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("a definition for an imported constructor");
    assert_ne!(loc.source.id, a.source_id);
    let text = &loc.source.content[loc.span.start as usize..loc.span.end as usize];
    assert_eq!(text, "Just");
    assert!(
        loc.source.name().to_string().ends_with("Maybe.mw"),
        "got {}",
        loc.source.name()
    );
}

/// An effect is a type too, and its operations are values.
#[test]
fn go_to_definition_covers_effects_and_their_operations() {
    let src = "effect Counter { bump : Int -> Int }\ndef demo = bum@p 1\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("an effect operation is a value");
    let clean = src.replacen("@", "", 1);
    assert_eq!(
        &clean[loc.span.start as usize..loc.span.end as usize],
        "bump"
    );
}

/// A result type that is written is not also hinted.
///
/// The four spellings below differ only in how much the author typed (each
/// pins the number type somewhere); the line
/// an editor shows is the same for all of them, which is the property that
/// makes a hint and an annotation interchangeable.
#[test]
fn a_written_result_type_is_not_hinted_again() {
    let want = "fun f (x : Int) (y : Int) : Int = x + y\n";
    assert_eq!(hinted("fun f (x : Int) y = x + y\n"), want);
    assert_eq!(hinted("fun f x (y : Int) = x + y\n"), want);
    assert_eq!(hinted("fun f x y : Int = x + y\n"), want);
    assert_eq!(hinted("fun f (x : Int) (y : Int) : Int = x + y\n"), want);
}

/// A type named in a result annotation can be followed, like any other.
#[test]
fn go_to_definition_reaches_a_type_from_a_result_annotation() {
    let src = "fun f (n : Int) : May@be Int = None\ndef demo = 0\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("a definition for the result type");
    assert_ne!(loc.source.id, a.source_id);
    assert!(
        loc.source.name().to_string().ends_with("Maybe.mw"),
        "got {}",
        loc.source.name()
    );
}

/// A result hint carries the function's **effect**, not just the type it
/// returns.
///
/// An effect is a property of the arrow, so the body's type does not have it:
/// `fun logIt x = let _ = println "hi" in x` has a body of type `a` and a type
/// of `a -> a ! { Console | e }`. Hinting the body reads as a claim that the
/// function is pure, which is worse than not annotating it at all.
#[test]
fn a_result_hint_shows_the_effect() {
    assert_eq!(
        hinted("fun logIt x = let _ = println \"hi\" in x\n"),
        "fun logIt (x : a) : a ! { Console | e } = let _ = println \"hi\" in x\n"
    );
    assert_eq!(
        hinted("fun bump r = setRef r 1\n"),
        "fun bump (r : Ref n) : () ! { Mut | e } = setRef r 1\n"
    );
    // Shared with a parameter: the result carries whatever `f` does.
    assert_eq!(
        hinted("fun apply2 f x = f (f x)\n"),
        "fun apply2 (f : a -> a ! e) (x : a) : a ! e = f (f x)\n"
    );
}

/// A pure function shows no effect at all.
///
/// It still *has* a latent effect variable — every arrow does — but one that
/// is mentioned nowhere else says nothing, and the scheme printer hides it for
/// the same reason. `n ! a` on `a + b` would be noise on every line.
#[test]
fn a_pure_function_is_not_decorated_with_an_empty_effect() {
    assert_eq!(
        hinted("fun add a b = a + b\n"),
        "fun add (a : a) (b : a) : a = a + b\n"
    );
}

// --- a package, not a file ---------------------------------------------------

/// Write a package under a fresh temporary directory and hand back its root.
///
/// `files` are `(relative path under src, contents)`. No manifest is written on
/// purpose in one of the tests below; where there is one, it names the package,
/// which is what a `use <pkg>.<module>` path starts with.
fn package(dir: &str, manifest: Option<&str>, files: &[(&str, &str)]) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("meadow-lsp-{dir}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    if let Some(m) = manifest {
        std::fs::write(root.join("Meadow.toml"), m).expect("manifest");
    }
    for (name, text) in files {
        let path = root.join("src").join(name);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).expect("mkdir");
        }
        std::fs::write(path, text).expect("write");
    }
    std::fs::canonicalize(&root).unwrap_or(root)
}

#[test]
fn a_module_sees_its_siblings_through_a_use() {
    let root = package(
        "siblings",
        Some("[package]\nname = \"Demo\"\n"),
        &[
            (
                "Syntax.mw",
                "@pub(pkg) data Expr = Int Int | Add Expr Expr\n@pub(pkg) fun zero u = Expr.Int 0\n",
            ),
            (
                "Eval.mw",
                "use Demo.Syntax.Expr.*\n\n@pub(pkg) fun eval e = match e with\n  | Int n -> n\n  | Add a b -> eval a + eval b\n",
            ),
            (
                "Main.mw",
                "use Demo.Eval (eval)\nuse Demo.Syntax (Expr)\ndef demo = eval (Expr.Int 1)\n",
            ),
        ],
    );
    let file = root.join("src").join("Eval.mw");
    let text = std::fs::read_to_string(&file).unwrap();
    let sources = meadow::editor::load_package(&file).expect("a package");
    let a = STD
        .with(|s| s.analyse_package(&sources, &file, &text))
        .expect("analysis");
    let msgs: Vec<&str> = a.diagnostics.iter().map(|d| d.msg.as_str()).collect();
    assert!(
        msgs.is_empty(),
        "a `use` of a sibling was not resolved: {msgs:?}"
    );
}

#[test]
fn only_this_documents_diagnostics_are_reported() {
    let root = package(
        "diags",
        Some("[package]\nname = \"Demo\"\n"),
        &[
            ("Broken.mw", "@pub(pkg) fun oops x = nosuchthing x\n"),
            ("Main.mw", "def demo = 1\n"),
        ],
    );
    let file = root.join("src").join("Main.mw");
    let text = std::fs::read_to_string(&file).unwrap();
    let sources = meadow::editor::load_package(&file).expect("a package");
    let a = STD
        .with(|s| s.analyse_package(&sources, &file, &text))
        .expect("analysis");
    let msgs: Vec<&str> = a.diagnostics.iter().map(|d| d.msg.as_str()).collect();
    assert!(
        msgs.is_empty(),
        "another file's error landed here: {msgs:?}"
    );

    // ...and the file that *is* broken still says so.
    let broken = root.join("src").join("Broken.mw");
    let text = std::fs::read_to_string(&broken).unwrap();
    let sources = meadow::editor::load_package(&broken).expect("a package");
    let a = STD
        .with(|s| s.analyse_package(&sources, &broken, &text))
        .expect("analysis");
    let msgs: Vec<&str> = a.diagnostics.iter().map(|d| d.msg.as_str()).collect();
    assert!(
        msgs.iter().any(|m| m.contains("nosuchthing")),
        "the broken file reported nothing: {msgs:?}"
    );
}

#[test]
fn go_to_definition_crosses_to_another_module_of_the_package() {
    let root = package(
        "goto",
        Some("[package]\nname = \"Demo\"\n"),
        &[
            (
                "Syntax.mw",
                "@pub(pkg) data Expr = Int Int\n@pub(pkg) fun zero u = Expr.Int 0\n",
            ),
            (
                "Main.mw",
                "use Demo.Syntax (Expr, zero)\ndef demo = zero ()\n",
            ),
        ],
    );
    let file = root.join("src").join("Main.mw");
    let text = std::fs::read_to_string(&file).unwrap();
    let sources = meadow::editor::load_package(&file).expect("a package");
    let a = STD
        .with(|s| s.analyse_package(&sources, &file, &text))
        .expect("analysis");
    let offset = text.find("zero ()").expect("the call") + 1;
    let loc = a
        .definition_at(offset, &Default::default(), &Default::default())
        .expect("a definition");
    assert_ne!(
        loc.source.id, a.source_id,
        "the definition should be in the other file"
    );
    let name = loc.source.name().to_string();
    assert!(name.ends_with("Syntax.mw"), "landed in {name}");
}

#[test]
fn a_file_outside_a_package_is_still_analysed_alone() {
    let stray = std::env::temp_dir().join(format!("meadow-stray-{}.mw", std::process::id()));
    std::fs::write(&stray, "def demo = 1\n").expect("write");
    assert!(
        meadow::editor::load_package(&stray).is_none(),
        "a file with no package around it should not find one"
    );
}

#[test]
fn the_mini_ml_example_is_clean_in_an_editor() {
    // The example that prompted all of this: five modules, each importing the
    // others' types. Analysed one file at a time it was a screen of red.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/MiniML/src");
    for name in ["Syntax.mw", "Parser.mw", "Infer.mw", "Eval.mw", "Main.mw"] {
        let file = std::fs::canonicalize(root.join(name)).expect("the example");
        let text = std::fs::read_to_string(&file).unwrap();
        let sources = meadow::editor::load_package(&file).expect("a package");
        let a = STD
            .with(|s| s.analyse_package(&sources, &file, &text))
            .expect("analysis");
        let msgs: Vec<&str> = a.diagnostics.iter().map(|d| d.msg.as_str()).collect();
        assert!(msgs.is_empty(), "{name}: {msgs:?}");
    }
}

#[test]
fn a_bad_use_is_reported_in_the_file_that_wrote_it() {
    let root = package(
        "baduse",
        Some("[package]\nname = \"Demo\"\n"),
        &[
            (
                "Other.mw",
                "use Demo.Nowhere (thing)\n@pub(pkg) def n = 1\n",
            ),
            ("Main.mw", "def demo = 1\n"),
        ],
    );
    for (name, wanted) in [("Main.mw", false), ("Other.mw", true)] {
        let file = root.join("src").join(name);
        let text = std::fs::read_to_string(&file).unwrap();
        let sources = meadow::editor::load_package(&file).expect("a package");
        let a = STD
            .with(|s| s.analyse_package(&sources, &file, &text))
            .expect("analysis");
        let said = a.diagnostics.iter().any(|d| d.msg.contains("no module"));
        assert_eq!(
            said,
            wanted,
            "{name}: {:?}",
            a.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
        );
    }
}

// --- rename ------------------------------------------------------------------

/// Apply a rename at the marker and hand back the edited text of every file.
///
/// `files` are the package's sources; the first is the one being edited, and
/// `$` in it marks the cursor (removed before analysis) -- not `@`, which
/// starts an attribute.
fn renamed(
    dir: &str,
    files: &[(&str, &str)],
    new_name: &str,
) -> Result<Vec<(String, String)>, String> {
    let cleaned: Vec<(String, String)> = files
        .iter()
        .map(|(n, t)| (n.to_string(), t.replacen('$', "", 1)))
        .collect();
    let refs: Vec<(&str, &str)> = cleaned
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    let root = package(dir, Some("[package]\nname = \"Demo\"\n"), &refs);
    let (open_name, open_text) = &cleaned[0];
    let offset = files[0].1.find('$').expect("a cursor marker");
    let file = root.join("src").join(open_name);
    let sources = meadow::editor::load_package(&file).expect("a package");
    let edits = STD.with(|s| {
        let a = s
            .analyse_package(&sources, &file, open_text)
            .expect("analysis");
        a.rename_at(offset, new_name, s)
    })?;

    // Apply each file's edits back to front, so earlier spans stay valid.
    let mut out: Vec<(String, String)> = cleaned.clone();
    let mut by_file: std::collections::HashMap<String, Vec<(u32, u32)>> = Default::default();
    for (source, span) in edits {
        let name = match source {
            None => open_name.clone(),
            Some(src) => std::path::Path::new(&src.name().to_string())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        };
        // Spans from another file are offsets into what was analysed, which is
        // the text on disk -- the same text this test wrote.
        by_file
            .entry(name)
            .or_default()
            .push((span.start, span.end));
    }
    for (name, mut spans) in by_file {
        spans.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        let slot = out.iter_mut().find(|(n, _)| *n == name).expect("a file");
        for (start, end) in spans {
            slot.1.replace_range(start as usize..end as usize, new_name);
        }
    }
    Ok(out)
}

#[test]
fn renaming_reaches_every_module_of_the_package() {
    let out = renamed(
        "rename-cross",
        &[
            (
                "Math.mw",
                "@pub(pkg) fun dou$ble n = n * 2\n@pub(pkg) fun quad n = double (double n)\n",
            ),
            ("Main.mw", "use Demo.Math (double)\ndef demo = double 21\n"),
        ],
        "twice",
    )
    .expect("a rename");
    let math = &out.iter().find(|(n, _)| n == "Math.mw").unwrap().1;
    let main = &out.iter().find(|(n, _)| n == "Main.mw").unwrap().1;
    assert_eq!(
        *math,
        "@pub(pkg) fun twice n = n * 2\n@pub(pkg) fun quad n = twice (twice n)\n"
    );
    assert_eq!(*main, "use Demo.Math (twice)\ndef demo = twice 21\n");
}

#[test]
fn renaming_leaves_a_different_binding_of_the_same_name_alone() {
    // Three `x`es: a parameter, a `let`, and a top-level one in another
    // module. Renaming the parameter is renaming *that* binding.
    let out = renamed(
        "rename-shadow",
        &[
            (
                "Main.mw",
                "@pub(pkg) def x = 1\nfun f $x = let x = 2 in x + 1\nfun g y = x + y\n",
            ),
            ("Other.mw", "@pub(pkg) def x = 99\n"),
        ],
        "p",
    )
    .expect("a rename");
    let main = &out.iter().find(|(n, _)| n == "Main.mw").unwrap().1;
    assert_eq!(
        *main,
        "@pub(pkg) def x = 1\nfun f p = let x = 2 in x + 1\nfun g y = x + y\n"
    );
    assert_eq!(
        out.iter().find(|(n, _)| n == "Other.mw").unwrap().1,
        "@pub(pkg) def x = 99\n"
    );
}

#[test]
fn renaming_a_type_takes_its_mentions_with_it() {
    let out = renamed(
        "rename-type",
        &[
            ("Syntax.mw", "@pub(pkg) data Ex$pr = Lit Int\n@pub(pkg) fun lit n = Expr.Lit n\n"),
            (
                "Main.mw",
                "use Demo.Syntax (Expr, lit)\nfun size e = match e with | Lit n -> n\ndef demo = size (lit 1)\n",
            ),
        ],
        "Term",
    )
    .expect("a rename");
    assert_eq!(
        out.iter().find(|(n, _)| n == "Syntax.mw").unwrap().1,
        "@pub(pkg) data Term = Lit Int\n@pub(pkg) fun lit n = Term.Lit n\n"
    );
    assert_eq!(
        out.iter().find(|(n, _)| n == "Main.mw").unwrap().1,
        "use Demo.Syntax (Term, lit)\nfun size e = match e with | Lit n -> n\ndef demo = size (lit 1)\n"
    );
}

#[test]
fn two_constructors_with_one_name_are_told_apart() {
    // `Expr.Int` and `Ty.Int` are both written `Int`. Renaming one leaves the
    // other where it is -- the resolver already knows which is which.
    let out = renamed(
        "rename-ctor",
        &[(
            "Main.mw",
            "@pub(pkg) data Expr = I$nt Int\n@pub(pkg) data Ty = Int\nfun a e = match e with | Expr.Int n -> n\nfun b t = match t with | Ty.Int -> 0\n",
        )],
        "Lit",
    )
    .expect("a rename");
    assert_eq!(
        out[0].1,
        "@pub(pkg) data Expr = Lit Int\n@pub(pkg) data Ty = Int\nfun a e = match e with | Expr.Lit n -> n\nfun b t = match t with | Ty.Int -> 0\n"
    );
}

#[test]
fn a_standard_library_name_is_not_renamed() {
    let err = renamed(
        "rename-std",
        &[("Main.mw", "def demo = ma$p (\\x -> x) [1, 2]\n")],
        "mapped",
    )
    .expect_err("should refuse");
    assert!(err.contains("outside the package"), "got: {err}");
}

#[test]
fn a_new_name_has_to_be_one() {
    let err = renamed(
        "rename-bad",
        &[("Main.mw", "fun dou$ble n = n * 2\ndef demo = double 1\n")],
        "Double",
    )
    .expect_err("should refuse");
    assert!(err.contains("read as a constructor"), "got: {err}");

    let err = renamed(
        "rename-bad2",
        &[("Main.mw", "fun dou$ble n = n * 2\ndef demo = double 1\n")],
        "two words",
    )
    .expect_err("should refuse");
    assert!(err.contains("is not a name"), "got: {err}");
}

/// `main` is `() -> () ! Eff` wherever an editor shows it, whatever less its
/// body performs: the type its node has, which is what is listed and hinted,
/// as well as its scheme.
#[test]
fn main_is_shown_at_the_effects_it_is_held_to() {
    let src = "fun game () = println \"hi\"\n\nfun main () = game ()\n";
    let a = STD.with(|s| s.analyse(src));
    let shown: Vec<(&str, &str)> = a
        .functions
        .iter()
        .map(|f| (f.name.as_str(), f.signature.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            ("game", "() -> () ! { Console | e }"),
            ("main", "() -> () ! Eff")
        ]
    );
}

/// What the editor offers to debug: each top-level definition, with how many
/// arguments it takes and its type -- private ones included, since those are
/// most of what a person wants to try on its own.
#[test]
fn top_level_definitions_are_listed_for_debugging() {
    let src =
        "fun double n = n * 2\n\nfun pair a b = (a, b)\n\ndef answer = 42\n\ndef (x, y) = (1, 2)\n";
    let a = STD.with(|s| s.analyse(src));
    let got: Vec<(&str, usize, &str)> = a
        .functions
        .iter()
        .map(|f| (f.name.as_str(), f.params, f.signature.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("double", 1, "n -> n"),
            ("pair", 2, "a -> b -> (a, b)"),
            ("answer", 0, "Int")
        ]
    );
    let double = &a.functions[0];
    assert_eq!(
        &src[double.span.start as usize..double.span.end as usize],
        "double"
    );
}

// --- the Test lens ------------------------------------------------------------

/// A `@test` carries the name `meadow test --exact` knows it by, and the name
/// is the one the runner actually uses — checked against the linker rather than
/// against a spelling of our own, since the whole point is that the lens and the
/// runner agree about which test is which.
#[test]
fn a_test_is_marked_with_the_name_the_runner_knows_it_by() {
    let root = package(
        "test-lens",
        Some("[package]\nname = \"Lens\"\n"),
        &[
            (
                "A.mw",
                "use Std.Test (assertEq)\n\nfun helper x = x\n\n@test fun works u = assertEq (helper 1) 1 \"a\"\n",
            ),
            // The same bare name in a sibling, which is why the name is qualified.
            (
                "B.mw",
                "use Std.Test (assertEq)\n\n@test fun works u = assertEq 1 1 \"b\"\n",
            ),
            ("Main.mw", "def demo = 0\n"),
        ],
    );
    let file = root.join("src").join("A.mw");
    let text = std::fs::read_to_string(&file).unwrap();
    let sources = meadow::editor::load_package(&file).expect("a package");
    let a = STD
        .with(|s| s.analyse_package(&sources, &file, &text))
        .expect("analysis");

    let marked: Vec<(&str, Option<&str>)> = a
        .functions
        .iter()
        .map(|f| (f.name.as_str(), f.test.as_deref()))
        .collect();
    assert_eq!(marked, [("helper", None), ("works", Some("A.works"))]);

    // And that is the name the runner will be asked for.
    let out = meadow::pipeline::build(&root, meadow::Options::debug());
    assert!(
        out.diagnostics.is_empty(),
        "{:?}",
        out.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let runner: Vec<String> = out
        .linked
        .expect("linked")
        .tests
        .iter()
        .filter(|t| &*t.package == "Lens")
        .map(|t| t.name.to_string())
        .collect();
    assert!(
        runner.contains(&"A.works".to_string()),
        "the runner knows: {runner:?}"
    );
    assert!(
        runner.contains(&"B.works".to_string()),
        "the runner knows: {runner:?}"
    );
}

/// A file on its own is a package of one root module, so its tests have no
/// qualifier — and `meadow test <file> --exact name` finds them by the bare name.
#[test]
fn a_test_in_a_lone_file_is_named_bare() {
    let a = STD.with(|s| {
        s.analyse("use Std.Test (assertEq)\n\n@test fun checks u = assertEq 1 1 \"x\"\n")
    });
    let f = a
        .functions
        .iter()
        .find(|f| f.name == "checks")
        .expect("listed");
    assert_eq!(f.test.as_deref(), Some("checks"));
}

/// No Test lens in a `Std` source file. The library is compiled into the
/// binary, so `meadow test --std` runs that copy and not the file on screen —
/// after an edit, a lens here could call a broken fix a pass.
#[test]
fn a_std_module_offers_no_test_to_run() {
    STD.with(|s| {
        let i = s
            .module_at("/anywhere/lib/Std/src/Char.mw")
            .expect("Char is a Std module");
        let text = meadow::stdlib::MODULES
            .iter()
            .find(|(d, _)| *d == "Char")
            .expect("its source")
            .1;
        assert!(
            text.contains("@test"),
            "the premise: Char has tests of its own"
        );
        let a = s.analyse_module(i, text);
        assert!(
            !a.functions.is_empty(),
            "its functions are still listed for debugging"
        );
        assert!(
            a.functions.iter().all(|f| f.test.is_none()),
            "but none is offered as a test to run"
        );
    });
}

// --- highlighting ---------------------------------------------------------------

/// Each capitalised word, with the colour the server gives it.
fn colours(src: &str) -> Vec<(String, &'static str)> {
    let a = STD.with(|s| s.analyse(src));
    let idx = LineIndex::new(src);
    meadow_lsp::tokens::tokens(src, &a)
        .into_iter()
        .filter_map(|(line, ch, len, kind)| {
            let at = idx.offset(line, ch);
            let word = &src[at..at + len as usize];
            word.starts_with(|c: char| c.is_ascii_uppercase())
                .then(|| (word.to_string(), meadow_lsp::tokens::LEGEND[kind as usize]))
        })
        .collect()
}

/// A constructor is coloured as one by what it resolved to, not by its spelling.
///
/// mini-ml's `Expr` has constructors called `Int` and `Bool`, which are also
/// types -- and `Std.Json` has constructors of those names too. Matching the word
/// against known names coloured those two as `Json`'s constructors and left
/// every other constructor of `Expr` (`Lam`, which matched nothing) as a
/// namespace. The same word can be both in one line: `Int Int`.
#[test]
fn a_constructor_that_shares_a_types_name_is_still_a_constructor() {
    let src = "data Expr = Int Int | Lam Expr\n\
               data Ty = Int | Var Ty\n\
               fun f e = match e with\n  | Expr.Int n -> Ty.Var Ty.Int\n  | Lam b -> Ty.Int\n";
    assert_eq!(
        colours(src),
        [
            ("Expr", "type"),
            ("Int", "enumMember"),
            ("Int", "type"),
            ("Lam", "enumMember"),
            ("Expr", "type"),
            ("Ty", "type"),
            ("Int", "enumMember"),
            ("Var", "enumMember"),
            ("Ty", "type"),
            ("Expr", "type"),
            ("Int", "enumMember"),
            ("Ty", "type"),
            ("Var", "enumMember"),
            ("Ty", "type"),
            ("Int", "enumMember"),
            ("Lam", "enumMember"),
            ("Ty", "type"),
            ("Int", "enumMember"),
        ]
        .map(|(w, k)| (w.to_string(), k))
    );
}

/// A module is a namespace wherever it is written, even when it is spelled like
/// something else in scope: `Bool` in `use Std.Bool` is the module, not a type
/// or a constructor, and an alias qualifying a call is too.
#[test]
fn a_module_path_is_a_namespace_whatever_it_is_called() {
    let src = "use Std.Bool (not)\nuse Std.String as S\n\ndef demo = S.concat \"a\" \"b\"\n";
    assert_eq!(
        colours(src),
        [
            ("Std", "namespace"),
            ("Bool", "namespace"),
            ("Std", "namespace"),
            ("String", "namespace"),
            ("S", "namespace"),
            ("S", "namespace")
        ]
        .map(|(w, k)| (w.to_string(), k))
    );
}

/// In `use Std.Either.Either (Left)` the modules are modules, the type is a type
/// and the constructor is a constructor -- and a `use` ending in `.*` does not
/// leave the next line reading as a path.
#[test]
fn a_use_of_a_types_constructors_colours_each_part() {
    let src = "use Std.Either.Either (Left)\nuse Std.Maybe.Maybe.*\ndata Box = Full Int\n";
    assert_eq!(
        colours(src),
        vec![
            ("Std".to_string(), "namespace"),
            ("Either".to_string(), "namespace"),
            ("Either".to_string(), "type"),
            ("Left".to_string(), "enumMember"),
            ("Std".to_string(), "namespace"),
            ("Maybe".to_string(), "namespace"),
            ("Maybe".to_string(), "type"),
            ("Box".to_string(), "type"),
            ("Full".to_string(), "enumMember"),
            ("Int".to_string(), "type"),
        ]
    );
}

/// A module is a namespace wherever it is written, even when its name is also a
/// type and a constructor -- `Std`'s `Int` and `String` are all three. The
/// declaration in `Lib.mw` was the one place that still coloured them by
/// spelling.
#[test]
fn a_module_named_like_a_type_is_a_namespace() {
    let src = "@pub mod Int\n@pub mod String\nuse Std.Num.Int as I\nuse Std.String as String\ndef x = (I.max 1 2, String.concat \"a\" \"b\")\n";
    let wrong: Vec<(String, &str)> = colours(src)
        .into_iter()
        .filter(|(_, kind)| *kind != "namespace")
        .collect();
    assert!(
        wrong.is_empty(),
        "coloured as something other than a module: {wrong:?}"
    );
}

/// `++` is a name, so the editor treats the operator like one: hovering it
/// shows what it applies, and a binding of its own is spelled `(++)`, the way
/// it is defined.
#[test]
fn hovering_a_user_operator_shows_its_binding() {
    let (a, off) = at("def demo = \"a\" @++ \"b\"\n", "@");
    let hover = a.hover_at(off).expect("hover");
    assert!(hover.contains("String -> String -> String"), "got: {hover}");

    let (a, off) = at("fun (++) a b = (a, b)\ndef demo = 1 @++ 2\n", "@");
    let hover = a.hover_at(off).expect("hover");
    assert!(
        hover.contains("(++) : forall a b. a -> b -> (a, b)"),
        "got: {hover}"
    );
}

#[test]
fn a_user_operator_goes_to_its_definition() {
    let src = "fun (++) a b = (a, b)\ndef demo = 1 @++ 2\n";
    let (a, off) = at(src, "@");
    let loc = STD
        .with(|s| a.definition_at(off, s.definitions(), s.declared_names()))
        .expect("a definition for `++`");
    assert_eq!(loc.source.id, a.source_id);
    assert_eq!(
        &src.replacen("@", "", 1)[loc.span.start as usize..loc.span.end as usize],
        "++"
    );
}

/// An effect variable in a hint is named like one, `e`, as the scheme on hover
/// names it -- not with the next free type-variable letter.
#[test]
fn a_hint_names_an_effect_variable_e() {
    assert_eq!(
        hinted("fun each f xs = forEach f xs\n"),
        "fun each (f : a -> () ! e) (xs : [a]) : () ! e = forEach f xs\n"
    );
    assert_eq!(
        hinted("fun twice f g x = f (g x)\n"),
        "fun twice (f : a -> b ! e) (g : c -> a ! e) (x : c) : b ! e = f (g x)\n"
    );
}

/// A local `let rec` hovers as its scheme, the way a top-level `fun` does.
///
/// It used to hover as the type its binder node was given, which still has its
/// variables free -- and a free effect variable prints as `! c` even when
/// nothing constrains it, so a pure loop looked effectful.
#[test]
fn a_local_function_hovers_as_its_scheme() {
    let src = "fun fib (n : Int) : Int =\n  let rec @loop a b i =\n    if i == 0 then a else loop b (a + b) (i - 1)\n  in loop 0 1 n\n";
    let (a, off) = at(src, "@");
    let hover = a.hover_at(off).expect("hover");
    // A local function takes no dictionaries, so the operators it uses pin
    // its numbers to what `fib` has: `Int`.
    assert!(hover.contains("loop : Int -> Int -> Int -> Int"), "{hover}");
    assert!(!hover.contains('!'), "a pure loop has no effect: {hover}");

    let top = "fun @loop a b i = if i == 0 then a else loop b (a + b) (i - 1)\n";
    let (t, off) = at(top, "@");
    let top_hover = t.hover_at(off).expect("hover");
    assert!(top_hover.contains("loop : forall"), "{top_hover}");
    assert!(
        !top_hover.contains('!'),
        "a pure loop has no effect: {top_hover}"
    );
}

/// ...but an effect a local function shares with its surroundings is shown.
///
/// This is why the fix is the scheme and not "hide an effect variable that
/// appears once": in `g`'s type, `h`'s effect appears once, and hiding it would
/// say `g` is pure when it does whatever `h` does. A scheme hides only what it
/// quantified, and `g` cannot quantify over a variable that came from outside.
#[test]
fn an_effect_from_outside_a_local_function_still_shows() {
    let (a, off) = at("fun app h x =\n  let @g y = h y in g x\n", "@");
    let hover = a.hover_at(off).expect("hover");
    assert!(hover.contains('!'), "g performs whatever h does: {hover}");

    let (a, off) = at(
        "fun f (u : Unit) =\n  let @g s = println s in g \"hi\"\n",
        "@",
    );
    let hover = a.hover_at(off).expect("hover");
    assert!(hover.contains("Console"), "{hover}");
}

// --- completion --------------------------------------------------------------

/// What the pipe at the end of `src` offers, best first.
fn piped(src: &str) -> Vec<meadow_lsp::complete::Offer> {
    let offset = src.len();
    let meadow_lsp::complete::Ask::Piped { pipe_at, word } = meadow_lsp::complete::ask(src, offset)
    else {
        panic!("not a pipe: {src:?}");
    };
    let word_start = meadow_lsp::complete::word_start(src, offset);
    let repaired = meadow_lsp::complete::repaired(src, word_start, offset);
    let a = STD.with(|s| s.analyse(&repaired));
    let subject = meadow_lsp::complete::subject(&a, pipe_at)
        .unwrap_or_else(|| panic!("no type for the value piped in {src:?}"))
        .clone();
    meadow_lsp::complete::piped(&a, &subject, &word)
}

fn names_of(offers: &[meadow_lsp::complete::Offer]) -> Vec<&str> {
    offers.iter().map(|o| o.name.as_str()).collect()
}

#[test]
fn a_pipe_offers_what_the_value_can_be_piped_into() {
    let offers = piped("def xs = [1, 2, 3]\n\ndef demo = xs |> ");
    let names = names_of(&offers);
    // Functions of a vector, and nothing that wants something else.
    assert!(names.contains(&"len"), "{names:?}");
    assert!(names.contains(&"head"), "{names:?}");
    assert!(names.contains(&"reverse"), "{names:?}");
    assert!(
        !names.contains(&"even"),
        "an `Int -> Bool` does not take a vector: {names:?}"
    );
}

#[test]
fn what_is_typed_after_the_pipe_narrows_it() {
    let offers = piped("def xs = [1, 2, 3]\n\ndef demo = xs |> le");
    let names = names_of(&offers);
    assert!(names.contains(&"len"), "{names:?}");
    assert!(names.iter().all(|n| n.starts_with("le")), "{names:?}");
}

#[test]
fn a_name_that_would_need_a_use_first_is_not_offered() {
    // `toUpper` is `Std.String`'s, which this document has not brought in:
    // offering it would write something that does not compile.
    let offers = piped("def greeting = \"hello\"\n\ndef demo = greeting |> ");
    let names = names_of(&offers);
    assert!(!names.contains(&"toUpper"), "{names:?}");
    // What the prelude does have for a `String` is still there.
    assert!(names.contains(&"++"), "{names:?}");
}

#[test]
fn a_function_that_takes_the_value_directly_comes_before_one_that_fits_anything() {
    let offers = piped("def xs = [1, 2, 3]\n\ndef demo = xs |> ");
    let names = names_of(&offers);
    let at = |want: &str| names.iter().position(|n| *n == want);
    // `id : a -> a` fits every value, so it is not what the menu leads with.
    if let (Some(len), Some(any)) = (at("len"), at("id")) {
        assert!(len < any, "{names:?}");
    }
}

#[test]
fn what_performs_no_effect_comes_first() {
    let offers = piped("def path = \"notes.txt\"\n\ndef demo = path |> ");
    let names = names_of(&offers);
    let at = |want: &str| names.iter().position(|n| *n == want);
    // Reading a file in the middle of a pipeline is deliberate; joining two
    // strings is routine.
    if let (Some(joined), Some(read)) = (at("++"), at("readToString")) {
        assert!(joined < read, "{names:?}");
    }
}

#[test]
fn what_the_file_already_uses_comes_first() {
    // Two functions fit equally; the one this file already reaches for wins.
    let src = "def xs = [1, 2, 3]\n\ndef n = len xs\n\ndef demo = xs |> ";
    let offers = piped(src);
    let names = names_of(&offers);
    let at = |want: &str| names.iter().position(|n| *n == want);
    if let (Some(len), Some(other)) = (at("len"), at("last")) {
        assert!(len < other, "`len` is used above: {names:?}");
    }
}

#[test]
fn a_function_written_to_chain_is_offered_partly_applied() {
    let offers = piped("def xs = [1, 2, 3]\n\ndef demo = xs |> ");
    let snippets: Vec<&str> = offers
        .iter()
        .filter(|o| o.snippet)
        .map(|o| o.insert.as_str())
        .collect();
    assert!(
        snippets.iter().all(|s| s.contains("${1}")),
        "a partly applied offer leaves a hole: {snippets:?}"
    );
}

#[test]
fn an_offer_says_what_it_is_and_where_it_came_from() {
    let offers = piped("def xs = [1, 2, 3]\n\ndef demo = xs |> ");
    let len = offers
        .iter()
        .find(|o| o.name == "len")
        .expect("len is offered");
    assert!(len.detail.contains("Int"), "{}", len.detail);
    assert!(!len.from.is_empty());
}

#[test]
fn the_editors_own_ordering_cannot_undo_this_one() {
    let offers = piped("def xs = [1, 2, 3]\n\ndef demo = xs |> ");
    let sorts: Vec<&str> = offers.iter().map(|o| o.sort.as_str()).collect();
    let mut sorted = sorts.clone();
    sorted.sort_unstable();
    assert_eq!(sorts, sorted, "sortText is in the order offered");
}

#[test]
fn typing_the_name_keeps_it_in_the_list() {
    // What the euler example reaches for: fold a range into a number. Typing
    // more of the name narrows the list rather than emptying it.
    for src in [
        "def xs = [11..20]\n\ndef demo = xs |> ",
        "def xs = [11..20]\n\ndef demo = xs |> f",
        "def xs = [11..20]\n\ndef demo = xs |> fol",
        "def xs = [11..20]\n\ndef demo = xs |> foldl",
    ] {
        let offers = piped(src);
        let names = names_of(&offers);
        assert!(
            names.contains(&"foldl"),
            "{:?} lost it: {names:?}",
            src.lines().last().unwrap()
        );
    }
}

#[test]
fn the_ways_of_taking_a_collection_are_all_near_the_top() {
    // `len v` takes it first and `map f v` takes it last; both are ordinary,
    // so neither convention is buried under the other.
    let offers = piped("def xs = [1, 2, 3]\n\ndef demo = xs |> ");
    let names = names_of(&offers);
    for want in ["map", "filter", "foldl"] {
        assert!(names.contains(&want), "{want} is missing: {names:?}");
    }
}

#[test]
fn what_idiomatic_meadow_reaches_for_comes_first() {
    // With nothing in this file to go on, the order is what the standard
    // library's own source does: `map` and `foldl` constantly, `splitAt`
    // hardly ever.
    let offers = piped("def xs = [11..20]\n\ndef demo = xs |> ");
    let names = names_of(&offers);
    let at = |want: &str| {
        names
            .iter()
            .position(|n| *n == want)
            .unwrap_or_else(|| panic!("{want} is missing: {names:?}"))
    };
    for want in ["map", "len", "foldl"] {
        assert!(at(want) < 8, "{want} is at {}: {names:?}", at(want));
    }
    assert!(at("splitAt") > at("foldl"), "{names:?}");
}

#[test]
fn a_literal_is_piped_as_what_it_is_not_as_what_it_desugars_to() {
    // `[11..20]` is written once but becomes a call, the function called, and
    // its arguments -- all spanning the same text. The value piped is the
    // outermost of them, so this offers what can be done with a vector rather
    // than what can be done with an `Int`.
    for src in [
        "def e5 = [11..20] |> ",
        "def e5 = [1, 2, 3] |> ",
        "def xs = [1, 2, 3]\n\ndef e5 = xs |> ",
    ] {
        let offers = piped(src);
        let names = names_of(&offers);
        assert!(
            names.contains(&"foldl") && names.contains(&"len"),
            "{:?} offers {names:?}",
            src.lines().last().unwrap()
        );
        assert!(
            !names.contains(&"fromArray"),
            "`#[a] -> [a]` takes an array, not a vector: {names:?}"
        );
    }
}

// --- paths -------------------------------------------------------------------

/// What the cursor at the end of `src` offers, as the server would answer it.
fn path_offers(src: &str) -> Vec<meadow_lsp::complete::Offer> {
    use meadow_lsp::complete::{
        Ask, ask, member, repaired, use_names, use_path, without_line, word_start,
    };
    let offset = src.len();
    let start = word_start(src, offset);
    match ask(src, offset) {
        Ask::Member { qualifier, word } => {
            let a = STD.with(|s| s.analyse(&repaired(src, start, offset)));
            member(&a, &qualifier, &word)
        }
        Ask::UsePath { segments, word } => {
            let a = STD.with(|s| s.analyse(&without_line(src, start)));
            use_path(&a, &segments, &word)
        }
        Ask::UseNames { segments, word } => {
            let a = STD.with(|s| s.analyse(&without_line(src, start)));
            use_names(&a, &segments, &word)
        }
        other => panic!("not a path: {other:?}"),
    }
}

#[test]
fn a_type_offers_its_constructors() {
    let offers = path_offers("data Shape = Circle Int | Square Int\n\ndef demo = Shape.");
    assert_eq!(names_of(&offers), vec!["Circle", "Square"]);
}

#[test]
fn a_type_from_the_library_offers_its_constructors() {
    // `Maybe` is a module *and* the type inside it. Only the type is something
    // a constructor can follow, so that is the one this means.
    let offers = path_offers("def demo = Maybe.");
    assert_eq!(names_of(&offers), vec!["Just", "None"]);
}

#[test]
fn a_half_written_constructor_narrows_the_list() {
    let offers = path_offers("data Shape = Circle Int | Square Int\n\ndef demo = Shape.Ci");
    assert_eq!(names_of(&offers), vec!["Circle"]);
}

#[test]
fn a_qualifier_that_names_nothing_offers_nothing() {
    assert!(path_offers("def demo = Nowhere.").is_empty());
}

#[test]
fn an_alias_reaches_the_module_it_names() {
    let offers = path_offers("use Std.Maybe as M\n\ndef demo = M.");
    let names = names_of(&offers);
    // Its values, and the constructors of the type it declares -- both of which
    // `M.` reaches, as the compiler resolves it.
    assert!(names.contains(&"unwrapOr"), "{names:?}");
    assert!(names.contains(&"andThen"), "{names:?}");
    assert!(names.contains(&"Just"), "{names:?}");
    // Nothing from anywhere else.
    assert!(!names.contains(&"println"), "{names:?}");
}

#[test]
fn a_use_offers_the_packages_and_then_what_is_under_them() {
    let offers = path_offers("use ");
    let names = names_of(&offers);
    assert!(names.contains(&"Std"), "{names:?}");

    let offers = path_offers("use Std.");
    let names = names_of(&offers);
    assert!(names.contains(&"Maybe"), "{names:?}");
    assert!(names.contains(&"Collections"), "{names:?}");

    let offers = path_offers("use Std.Collections.");
    let names = names_of(&offers);
    assert!(names.contains(&"Vector"), "{names:?}");
    assert!(names.contains(&"HashMap"), "{names:?}");
}

#[test]
fn a_use_path_narrows_as_a_segment_is_typed() {
    let offers = path_offers("use Std.Coll");
    assert_eq!(names_of(&offers), vec!["Collections"]);
}

#[test]
fn a_type_is_offered_at_the_end_of_a_use_path() {
    // `use Std.Maybe.Maybe.*` is how a type's constructors are brought in, so a
    // type belongs in a path as much as a module does.
    let offers = path_offers("use Std.Maybe.");
    let names = names_of(&offers);
    assert!(names.contains(&"Maybe"), "{names:?}");
}

#[test]
fn the_list_of_a_use_offers_what_the_module_exports() {
    let offers = path_offers("use Std.Maybe (");
    let names = names_of(&offers);
    assert!(names.contains(&"unwrapOr"), "{names:?}");
    assert!(names.contains(&"Maybe"), "{names:?}");
    assert!(!names.contains(&"println"), "{names:?}");

    // And narrows as it is typed.
    let offers = path_offers("use Std.Maybe (unwrap");
    assert_eq!(names_of(&offers), vec!["unwrapOr"]);
}

#[test]
fn an_offer_says_what_kind_of_name_it_is() {
    use meadow_lsp::analysis::PathKind;
    let offers = path_offers("use Std.");
    let module = offers
        .iter()
        .find(|o| o.name == "Collections")
        .expect("a module");
    assert_eq!(module.kind, PathKind::Module);
    let offers = path_offers("def demo = Maybe.");
    assert_eq!(offers[0].kind, PathKind::Ctor);
}

#[test]
fn a_signature_carries_the_doc_for_the_clauses_under_it() {
    // A signature and its clauses are one declaration, and the doc is above
    // the first line of it. Looking above the clause would find the signature
    // and nothing else.
    let src = "\
-- Doubles its argument.
fun double : Int -> Int
  | double n = n * 2

def demo = do@uble 21
";
    let (a, off) = at(src, "@");
    let hover = a.hover_at(off).expect("hover");
    assert!(hover.contains("Doubles its argument."), "got: {hover}");
}

// --- a macro call is not the code it stands for -------------------------------------

/// `adder!(inc)` makes `fun inc x = x + 1`, whose spans are the call's.
const ADDER: &str = "\
macro adder
  | ($name) -> { fun $name x = x + 1 }

adder!(inc)

def demo = in@c 1
";

#[test]
fn what_a_macro_generates_gets_no_lenses_or_hints() {
    let (a, _) = at(ADDER, "@");
    let clean = ADDER.replacen("@", "", 1);
    let call = clean.find("adder!(inc)").unwrap() as u32;
    let end = call + "adder!(inc)".len() as u32;
    let names: Vec<&str> = a.functions.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["demo"], "no Debug lens on the call");
    assert!(
        a.binders
            .iter()
            .all(|h| !(call <= h.offset && h.offset <= end)),
        "no `(x : Int)` inside the call, nor its `)` at the end"
    );
}

#[test]
fn a_name_a_macro_made_still_hovers_and_goes_to_its_definition() {
    let (a, off) = at(ADDER, "@");
    let hover = a.hover_at(off).expect("hover");
    assert!(hover.contains("inc"), "got: {hover}");
    let def = a
        .definition_at(off, &Default::default(), &Default::default())
        .expect("definition");
    let clean = ADDER.replacen("@", "", 1);
    let call = clean.find("adder!(inc)").unwrap() as u32;
    assert!(
        def.span.start >= call && def.span.start < call + "adder!(inc)".len() as u32,
        "into the call that made it: {:?}",
        def.span
    );
}

// --- a definition in the compiler's IRs ---------------------------------------

/// The definition around `⟨marker⟩` in the IR called `which`, and the text
/// of each place the name at the marker is in it.
fn ir(src: &str, which: &str) -> (meadow_lsp::ir::View, Vec<String>) {
    let (a, off) = at(src, "@");
    let view = STD
        .with(|s| a.ir_at(off, which, s.packages()))
        .expect("a definition there");
    let focused = view
        .focus
        .iter()
        .map(|(s, e)| view.text[*s..*e].to_string())
        .collect();
    (view, focused)
}

const SUM_TO: &str = "fun sumTo (m : Int) : Int =
  let rec go n acc = if n == 0 then acc else go (n - 1) (a@cc + n) in
  go m 0

def other = 1
";

#[test]
fn a_definition_is_shown_as_core() {
    let (view, focused) = ir(SUM_TO, "core");
    assert_eq!(view.name, "sumTo");
    assert!(view.text.starts_with("v0 : (Int -> Int)"), "{}", view.text);
    assert!(
        focused.is_empty(),
        "core's variables are renumbered to be read"
    );
}

#[test]
fn a_local_variable_is_found_in_the_cut_of_its_definition() {
    let (view, focused) = ir(SUM_TO, "cut");
    assert_eq!(view.name, "sumTo");
    // The definition's value, and the definition its local function is.
    assert!(view.text.starts_with("val meadow:"), "{}", view.text);
    assert!(view.text.contains("\n\ndef meadow:"), "{}", view.text);
    assert!(
        !view.text.contains("other"),
        "one definition: {}",
        view.text
    );
    // `acc`: where the function takes it, and each place it is read.
    assert_eq!(focused.len(), 3, "{focused:?}\n{}", view.text);
    assert!(focused.iter().all(|n| *n == focused[0]), "{focused:?}");
    // Every segment is what the text there says.
    for g in &view.segments {
        if g.kind == "variable" {
            assert_eq!(&view.text[g.start..g.end], g.name);
        }
    }
    assert!(view.segments.iter().any(|g| g.kind == "declaration"));
}

#[test]
fn a_local_variable_is_found_in_the_axcut_of_its_definition() {
    let (view, focused) = ir(SUM_TO, "axcut");
    assert_eq!(view.name, "sumTo");
    assert!(view.text.contains("def #"), "{}", view.text);
    // A variable of the source keeps its number in the AxCut.
    assert!(focused.len() >= 2, "{focused:?}\n{}", view.text);
    assert!(focused.iter().all(|n| *n == focused[0]), "{focused:?}");
    assert!(focused[0].starts_with('v'));
}

#[test]
fn the_name_of_a_definition_called_is_found_by_its_symbol() {
    let (view, focused) = ir(
        "fun double (n : Int) : Int = n * 2\ndef four = dou@ble 2\n",
        "cut",
    );
    assert_eq!(view.name, "four");
    assert_eq!(focused.len(), 1, "{focused:?}\n{}", view.text);
    assert!(focused[0].ends_with("double"), "{focused:?}");
}
