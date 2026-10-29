//! An editor's focused analysis of MeadowBoot -- one module inferred, the rest
//! reused -- says what a whole one does, for every module of it.
//!
//! MeadowBoot is the package this was made for: about thirty modules, most of
//! what inference reads written by `lang!`, `syntax!` and `pass!`. Ignored by
//! default, since it compiles MeadowBoot about seventy times and fetches its
//! dependencies; run it with
//!
//!     cargo test --release -p meadow --test editor_focus -- --ignored --nocapture

use meadow_compiler::focus::Memory;
use meadow_lsp::analysis::{Analysis, Reuse, Std};
use std::path::PathBuf;
use std::time::Instant;

fn std_lib() -> Std {
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

/// Diagnostics, and the hover at every offset: what a reader sees.
fn seen(a: &Analysis, text: &str) -> (Vec<String>, Vec<Option<String>>) {
    let mut msgs: Vec<String> = a.diagnostics.iter().map(|d| d.msg.clone()).collect();
    msgs.sort();
    let hovers = (0..text.len())
        .filter(|&o| text.is_char_boundary(o))
        .map(|o| a.hover_at(o).map(|h| renumbered(&h)))
        .collect();
    (msgs, hovers)
}

/// `h` with each rigid variable's number -- the `46` of `s#46` -- replaced by
/// its order of appearance. The number is a count of the rigid variables the
/// whole inference run has made, so it says how much of the package was
/// inferred before, which a focused compile is meant to change.
fn renumbered(h: &str) -> String {
    let mut out = String::new();
    let mut seen: Vec<String> = Vec::new();
    let mut rest = h;
    while let Some(i) = rest.find('#') {
        out.push_str(&rest[..=i]);
        let digits: String = rest[i + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.is_empty() {
            rest = &rest[i + 1..];
            continue;
        }
        let k = match seen.iter().position(|d| *d == digits) {
            Some(k) => k,
            None => {
                seen.push(digits.clone());
                seen.len() - 1
            }
        };
        out.push_str(&(k + 1).to_string());
        rest = &rest[i + 1 + digits.len()..];
    }
    out.push_str(rest);
    out
}

#[test]
fn rigid_variables_are_renumbered_by_appearance() {
    assert_eq!(
        renumbered("(() -> Int ! St s#46) -> StRef s#46 Int ! St t#3"),
        "(() -> Int ! St s#1) -> StRef s#1 Int ! St t#2"
    );
}

#[test]
#[ignore = "compiles MeadowBoot about seventy times"]
fn every_module_of_meadowboot_analyses_alike_focused_and_whole() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bootstrap");
    let root = std::fs::canonicalize(&root).unwrap();
    meadow::editor::allow_fetching();
    let main = root.join("src").join("Main.mw");
    let sources = match meadow::editor::find_package(&main) {
        Some(Ok(s)) => s,
        Some(Err(why)) => panic!("{why}"),
        None => panic!("MeadowBoot is not a package"),
    };
    let std = std_lib();
    let files: Vec<PathBuf> = sources.modules.iter().map(|m| m.file.clone()).collect();

    let mut memory = Memory::default();
    let text = std::fs::read_to_string(&main).unwrap();
    let at = Instant::now();
    std.analyse_documents(&sources, &[(&main, &text)], Reuse::Remember(&mut memory));
    eprintln!("whole, remembering: {:.1?}", at.elapsed());

    // `MEADOWBOOT_MODULES=Core,Elab` for just those.
    let only: Option<Vec<String>> = std::env::var("MEADOWBOOT_MODULES")
        .ok()
        .map(|v| v.split(',').map(|m| format!("{m}.mw")).collect());
    let mut mismatched = Vec::new();
    for file in &files {
        if let Some(only) = &only
            && !only.iter().any(|m| file.ends_with(m))
        {
            continue;
        }
        // An edit inside the module: a comment at the end, which changes its
        // text and nothing it means.
        let text = std::fs::read_to_string(file).unwrap() + "\n-- edited\n";
        let at = Instant::now();
        let focused = std
            .analyse_documents(
                &sources,
                &[(file.as_path(), &text)],
                Reuse::Focus(&mut memory),
            )
            .pop()
            .flatten()
            .expect("a module of the package");
        let fast = at.elapsed();
        let tally = memory.last();
        let at = Instant::now();
        let whole = std
            .analyse_package(&sources, file, &text)
            .expect("a module of the package");
        let slow = at.elapsed();
        let name = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .display()
            .to_string();
        eprintln!(
            "{name:<28} focused {fast:>8.1?}  whole {slow:>8.1?}  \
             inferred {:>5} reused {:>5} skipped {:>5}",
            tally.inferred, tally.reused, tally.skipped
        );
        let (fd, fh) = seen(&focused, &text);
        let (wd, wh) = seen(&whole, &text);
        if (&fd, &fh) != (&wd, &wh) {
            for d in fd.iter().filter(|d| !wd.contains(d)) {
                eprintln!("  only focused: {d}");
            }
            for d in wd.iter().filter(|d| !fd.contains(d)) {
                eprintln!("  only whole:   {d}");
            }
            let offsets: Vec<usize> = (0..text.len())
                .filter(|&o| text.is_char_boundary(o))
                .collect();
            let mut shown = 0;
            let mut last: Option<(&Option<String>, &Option<String>)> = None;
            for ((o, f), w) in offsets.iter().zip(&fh).zip(&wh) {
                if f == w || last == Some((f, w)) {
                    continue;
                }
                last = Some((f, w));
                let line = text[..*o].matches('\n').count() + 1;
                let word: String = text[*o..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                eprintln!("  {line}: `{word}` focused {f:?}\n      whole   {w:?}");
                shown += 1;
                if shown == 6 {
                    break;
                }
            }
            mismatched.push(name);
        }
    }
    assert!(
        mismatched.is_empty(),
        "focused and whole differ: {mismatched:?}"
    );
}
