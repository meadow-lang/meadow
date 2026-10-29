//! A compile focused on one module infers that module, reuses what an earlier
//! compile found for the rest where it still holds, and agrees with a whole
//! compile about the focused module. See `meadow_compiler::focus`.

use meadow_compiler::{
    AstModule, CompiledPackage, Options, compile_unit_focused,
    diagnostics::Diagnostic,
    focus::{Focus, Memory, Tally},
    intern::InternedString,
    lexer::tokenize,
    parser,
    source::{Source, SourceKind},
};

/// `files` as the modules of a package `T`: the first is the root, the rest
/// are named by their module path.
fn modules(files: &[(&str, &str)]) -> Vec<AstModule> {
    files
        .iter()
        .map(|(path, src)| {
            let name = InternedString::from(if path.is_empty() { "T" } else { *path });
            let source = Source::new(
                SourceKind::File(InternedString::from(format!("{path}.mw").as_str())),
                InternedString::from(*src),
            );
            let lex = tokenize(source);
            let (ast, errs) = parser::parse(name, source, &lex.tokens);
            assert!(errs.is_empty(), "{path} does not parse");
            AstModule {
                path: if path.is_empty() {
                    vec![]
                } else {
                    vec![InternedString::from(*path)]
                },
                name,
                ast: ast.expect("parses"),
                source,
            }
        })
        .collect()
}

fn compile(
    files: &[(&str, &str)],
    focus: Option<&str>,
    memory: &mut Memory,
) -> (CompiledPackage, Vec<Diagnostic>) {
    let t = InternedString::from("T");
    compile_unit_focused(
        t,
        t,
        t,
        0,
        modules(files),
        &[],
        Options::debug(),
        0,
        None,
        Focus {
            module: focus.map(|m| vec![InternedString::from(m)]),
            memory,
        },
    )
}

/// The type of the top-level binding `name`, as a whole compile of the same
/// files would show it.
fn scheme(pkg: &CompiledPackage, name: &str) -> String {
    pkg.exports
        .iter()
        .find(|e| &*e.name == name)
        .map(|e| e.scheme.to_string())
        .unwrap_or_else(|| panic!("no `{name}`"))
}

fn messages(diags: &[Diagnostic]) -> Vec<String> {
    let mut m: Vec<String> = diags.iter().map(|d| d.msg.clone()).collect();
    m.sort();
    m
}

const ROOT: &str = "mod A\nmod B\nmod C\nmod D\n";
const A: &str = "@pub data Shape = Dot | Box Int\n\
                 @pub fun area s = match s with | Shape.Dot -> 0 | Shape.Box n -> n * n\n\
                 @pub fun double x = x + x\n";
const B: &str = "use T.A (double)\n@pub fun quad x = double (double x)\n";
const C: &str = "use T.B (quad)\nuse T.A (area, Shape)\n\
                 @pub fun sixteen u = quad 4\n\
                 @pub fun size s = area s\n";
const D: &str = "use T.C (sixteen)\n@pub fun more u = sixteen u + 1\n";

fn files<'a>(a: &'a str, c: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![("", ROOT), ("A", a), ("B", B), ("C", c), ("D", D)]
}

#[test]
fn a_whole_compile_infers_everything_and_remembers_it() {
    let mut memory = Memory::default();
    let (_, diags) = compile(&files(A, C), None, &mut memory);
    assert!(diags.is_empty(), "{:?}", messages(&diags));
    let t = memory.last();
    assert_eq!((t.reused, t.skipped), (0, 0), "{t:?}");
    assert!(t.inferred >= 6, "{t:?}");
    assert!(!memory.is_empty());
}

#[test]
fn an_edit_to_one_module_reinfers_that_module_and_reuses_what_it_calls() {
    let mut memory = Memory::default();
    compile(&files(A, C), None, &mut memory);

    let edited = C.replace("quad 4", "quad 5");
    let (focused, diags) = compile(&files(A, &edited), Some("C"), &mut memory);
    assert!(diags.is_empty(), "{:?}", messages(&diags));
    let t = memory.last();
    // `sixteen` and `size` inferred; `quad`, `double` and `area` reused --
    // and `more`, in `D`, which nothing in `C` calls, reused as it was.
    assert_eq!(
        t,
        Tally {
            inferred: 2,
            reused: 4,
            skipped: 0
        }
    );

    let (whole, _) = compile(&files(A, &edited), None, &mut Memory::default());
    for name in ["sixteen", "size"] {
        assert_eq!(scheme(&focused, name), scheme(&whole, name), "{name}");
    }
}

#[test]
fn a_changed_module_below_is_inferred_again_rather_than_trusted() {
    let mut memory = Memory::default();
    compile(&files(A, C), None, &mut memory);

    // `double` now makes a string, so `quad` and `sixteen` break -- which a
    // reused `Int -> Int` for `quad` would hide.
    let a = A.replace("fun double x = x + x", "fun double x = \"no\"");
    let (focused, diags) = compile(&files(&a, C), Some("C"), &mut memory);
    let (whole, all) = compile(&files(&a, C), None, &mut Memory::default());
    let mine = |d: &[Diagnostic]| -> Vec<String> {
        messages(
            &d.iter()
                .filter(|d| d.filename == "C.mw")
                .cloned()
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(mine(&diags), mine(&all));
    assert_eq!(scheme(&focused, "size"), scheme(&whole, "size"));
    assert!(memory.last().inferred >= 3, "{:?}", memory.last());
}

#[test]
fn a_changed_type_declaration_trusts_nothing_it_was_used_for() {
    let mut memory = Memory::default();
    compile(&files(A, C), None, &mut memory);

    // Only a declaration changed, not the body of anything `C` calls.
    let a = A.replace("Box Int", "Box String");
    let (_, diags) = compile(&files(&a, C), Some("C"), &mut memory);
    let (_, all) = compile(&files(&a, C), None, &mut Memory::default());
    let c = |d: &[Diagnostic]| -> Vec<String> {
        messages(
            &d.iter()
                .filter(|d| d.filename == "C.mw")
                .cloned()
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(c(&diags), c(&all));
    let t = memory.last();
    assert_eq!(t.reused, 1, "only `more`, which nothing needs: {t:?}");
}

#[test]
fn with_nothing_remembered_what_the_focus_does_not_need_is_left_alone() {
    let mut memory = Memory::default();
    let (focused, diags) = compile(&files(A, C), Some("B"), &mut memory);
    assert!(diags.is_empty(), "{:?}", messages(&diags));
    // `quad` and the `double` it calls; not `area`, `sixteen`, `size`, `more`.
    assert_eq!(
        memory.last(),
        Tally {
            inferred: 2,
            reused: 0,
            skipped: 4
        }
    );
    let (whole, _) = compile(&files(A, C), None, &mut Memory::default());
    assert_eq!(scheme(&focused, "quad"), scheme(&whole, "quad"));
}

#[test]
fn only_the_focused_module_is_lowered() {
    let mut memory = Memory::default();
    let (pkg, _) = compile(&files(A, C), Some("B"), &mut memory);
    let modules: std::collections::BTreeSet<String> =
        pkg.defs.iter().map(|d| d.module.to_string()).collect();
    assert_eq!(
        modules.into_iter().collect::<Vec<_>>(),
        vec!["T.B".to_string()]
    );
}
