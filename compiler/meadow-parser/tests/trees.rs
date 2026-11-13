//! Token trees against real source, and the per-position parser entry points.
//!
//! The point of the first half is that grouping by bracket changes nothing:
//! every module in the standard library and the examples groups without an
//! error, the tokens come back out of the tree in the order they went in, and
//! the parser makes the same module of them either way. That is what lets macro
//! expansion hand its result back to the ordinary parser.

use meadow_lexer::{LToken, Token, tokenize, tt};
use meadow_parser::{parse, parse_decls, parse_expr, parse_pat};
use meadow_source::{Source, SourceKind};
use meadow_span::Span;
use std::path::{Path, PathBuf};

/// Every `.mw` file under `dir`, recursively.
fn modules(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            modules(&path, out);
        } else if path.extension().is_some_and(|x| x == "mw") {
            out.push(path);
        }
    }
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate is two levels below the repository")
        .to_path_buf()
}

fn lex(text: &str) -> Vec<LToken> {
    let src = Source::new(SourceKind::Interactive, text.into());
    let res = tokenize(src);
    assert!(res.errors.is_empty(), "lexing failed: {:?}", res.errors);
    res.tokens
}

fn eoi(text: &str) -> Span {
    Span::from(0..text.len())
}

#[test]
fn every_module_groups_by_bracket_and_comes_back_unchanged() {
    let repo = repo();
    let mut paths = Vec::new();
    modules(&repo.join("lib"), &mut paths);
    modules(&repo.join("examples"), &mut paths);
    paths.sort();
    assert!(
        paths.len() > 30,
        "expected the standard library and the examples, found {}",
        paths.len()
    );

    for path in paths {
        let text = std::fs::read_to_string(&path).expect("a module that was just listed");
        let name = path.display().to_string();
        let src = Source::new(SourceKind::File(name.as_str().into()), text.as_str().into());
        let lexed = tokenize(src);
        assert!(lexed.errors.is_empty(), "{name}: {:?}", lexed.errors);

        let (trees, errors) = tt::trees(&lexed.tokens, &name);
        assert!(errors.is_empty(), "{name}: {errors:?}");

        let flat = tt::flatten(&trees);
        assert_eq!(flat, lexed.tokens, "{name}: tokens changed shape");

        // And so the parser cannot tell the difference.
        let module = "M".into();
        let (before, errs) = parse(module, src, &lexed.tokens);
        assert!(errs.is_empty(), "{name}: {errs:?}");
        let (after, errs) = parse(module, src, &flat);
        assert!(errs.is_empty(), "{name}: {errs:?}");
        assert_eq!(before, after, "{name}: parsed differently");
    }
}

#[test]
fn an_empty_module_parses_to_no_declarations() {
    for text in ["", "\n", "-- only a comment\n", "@!feature(ffi)\n"] {
        let src = Source::new(SourceKind::Interactive, text.into());
        let tokens = lex(text);
        let (module, errs) = parse("M".into(), src, &tokens);
        assert!(errs.is_empty(), "{text:?}: {errs:?}");
        let module = module.expect("a module");
        assert!(module.value.decls.is_empty(), "{text:?}");
    }
}

#[test]
fn every_module_renders_back_to_the_tokens_it_came_from() {
    // `stringify!` writes token trees back as text. A token tree does not
    // remember its whitespace, so the text is not the file -- but lexing it must
    // give the same tokens, or a macro could change what it was handed.
    let repo = repo();
    let mut paths = Vec::new();
    modules(&repo.join("lib"), &mut paths);
    modules(&repo.join("examples"), &mut paths);
    paths.sort();

    for path in paths {
        let text = std::fs::read_to_string(&path).expect("a module that was just listed");
        let name = path.display().to_string();
        let lexed = tokenize(Source::new(
            SourceKind::File(name.as_str().into()),
            text.as_str().into(),
        ));
        let (trees, errors) = tt::trees(&lexed.tokens, &name);
        assert!(errors.is_empty(), "{name}: {errors:?}");

        let rendered = tt::render(&trees);
        let again = tokenize(Source::new(
            SourceKind::File(name.as_str().into()),
            rendered.as_str().into(),
        ));
        assert!(again.errors.is_empty(), "{name}: {:?}", again.errors);

        // Spans differ -- the text is respaced -- so compare the tokens. So is
        // the layout, and with it which calls began a line: a `DeclBang` is a
        // `!` that did, which the rendering cannot say.
        let bare = |t: &LToken| match t.value() {
            Token::DeclBang => Token::Bang,
            other => other.clone(),
        };
        let before: Vec<_> = lexed.tokens.iter().map(bare).collect();
        let after: Vec<_> = again.tokens.iter().map(bare).collect();
        assert_eq!(before, after, "{name}: rendering changed the tokens");
    }
}

#[test]
fn an_expression_parses_on_its_own() {
    let src = "match xs with | [;] -> 0 | (y :: _) -> y + 1";
    let tokens = lex(src);
    let (e, errs) = parse_expr(&tokens, eoi(src));
    assert!(errs.is_empty(), "{errs:?}");
    assert!(e.is_some());
}

/// The tree an expression parses to, with its spans taken out, for comparing
/// with another written differently.
fn shape(text: &str) -> String {
    let tokens = lex(text);
    let (e, errors) = parse_expr(&tokens, eoi(text));
    assert!(errors.is_empty(), "{text}: {errors:?}");
    let shown = format!("{:?}", e.expect("it parses"));
    let mut out = String::new();
    let mut rest = shown.as_str();
    while let Some(at) = rest.find("span: ") {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        rest = &rest[rest.find('}').map_or(rest.len(), |i| i + 1)..];
    }
    out + rest
}

#[test]
fn a_pipe_with_a_side_that_does_nothing_is_the_call() {
    assert_eq!(shape("x |> f"), shape("f x"));
    assert_eq!(shape("g y |> f"), shape("f (g y)"));
    assert_eq!(shape("x |> f a"), shape("(f a) x"));
    assert_eq!(shape("g y |> M.f"), shape("M.f (g y)"));
    assert_eq!(shape("g y |> Just"), shape("(Just) (g y)"));
    assert_eq!(shape(r"g y |> (\n -> n)"), shape(r"(\n -> n) (g y)"));
    assert_eq!(shape("g y |> (_ + 1)"), shape("(_ + 1) (g y)"));
    assert_eq!(shape("1 |> f a"), shape("(f a) 1"));
    assert_eq!(shape("-1 |> f a"), shape("(f a) (-1)"));
    assert_eq!(shape("() |> f a"), shape("(f a) ()"));
}

#[test]
fn a_pipe_passes_its_value_in_where_the_order_could_show() {
    // Only `$pipe` tells these from what a program could write itself, and no
    // program can write it.
    let spelled = |text: &str| shape(text).replace("pipeValue", "$pipe");
    assert_eq!(
        shape("g y |> f a"),
        spelled(r"(\pipeValue -> (f a) pipeValue) (g y)")
    );
    // A field can fault, so it is not nothing.
    assert_eq!(
        shape("r.x |> f a"),
        spelled(r"(\pipeValue -> (f a) pipeValue) r.x")
    );
    // The rest of a chain is the value of the pipe after it.
    assert_eq!(
        shape("g y |> f a |> h b"),
        spelled(r"(\pipeValue -> (h b) pipeValue) ((\pipeValue -> (f a) pipeValue) (g y))")
    );
}

#[test]
fn the_holes_of_a_section_are_numbered_as_written_across_a_pipe() {
    let spelled = |text: &str| shape(text).replace("pipeValue", "$pipe");
    assert_eq!(
        shape("(g _ |> f _)"),
        spelled(r"\_hole0 _hole1 -> (\pipeValue -> (f _hole1) pipeValue) (g _hole0)")
    );
    assert_eq!(
        shape("(_ |> f _)"),
        spelled(r"\_hole0 _hole1 -> (\pipeValue -> (f _hole1) pipeValue) _hole0")
    );
    assert_eq!(
        shape("(_ |> _)"),
        spelled(r"\_hole0 _hole1 -> (\pipeValue -> _hole1 pipeValue) _hole0")
    );
    assert_eq!(shape("(_ |> f)"), shape(r"\_hole0 -> f _hole0"));
}

#[test]
fn an_expression_entry_point_takes_the_whole_input_or_none_of_it() {
    // Application is juxtaposition, so `f x` is one expression -- but a `)` with
    // nothing to close is not part of any, and the entry point must say so
    // rather than parsing the `f x` and stopping.
    let src = "f x )";
    let tokens = lex(src);
    let (e, errs) = parse_expr(&tokens, eoi(src));
    assert!(!errs.is_empty(), "expected an error, got {e:?}");
}

#[test]
fn a_pattern_parses_on_its_own() {
    let src = "(Cons x rest)";
    let tokens = lex(src);
    let (p, errs) = parse_pat(&tokens, eoi(src));
    assert!(errs.is_empty(), "{errs:?}");
    assert!(p.is_some());
}

#[test]
fn declarations_parse_on_their_own() {
    let src = "fun double x = x * 2\ndef four = double 2";
    let tokens = lex(src);
    let (ds, errs) = parse_decls(&tokens, eoi(src));
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(ds.expect("two declarations").len(), 2);
}

#[test]
fn no_declarations_is_not_an_error() {
    // A macro may expand to nothing, so the declaration entry point -- unlike a
    // module, which must have something in it -- accepts an empty run.
    let (ds, errs) = parse_decls(&[], Span::from(0..0));
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(ds.expect("an empty run").len(), 0);
}
