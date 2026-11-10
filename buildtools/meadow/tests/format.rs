//! The formatter, against real source.
//!
//! The standard library is formatted with it, so it is what the formatter
//! prints: anything `meadow fmt` would change there is a change to one of the
//! two that was not carried to the other.

use meadow::stdlib::MODULES;
use meadow_fmt as fmt;

fn formatted(src: &str) -> String {
    fmt::format_within(src, fmt::WIDTH)
}

#[test]
fn the_standard_library_is_already_formatted() {
    for (name, src) in MODULES {
        let out = formatted(src);
        if out != *src {
            // Show the first line that differs rather than 400 lines of source.
            let (line, want, got) = src
                .lines()
                .zip(out.lines())
                .enumerate()
                .find(|(_, (a, b))| a != b)
                .map(|(i, (a, b))| (i + 1, a.to_string(), b.to_string()))
                .unwrap_or((0, "<length>".into(), "<length>".into()));
            panic!(
                "Std.{name} line {line} would be reformatted:\n  is:   {want:?}\n  want: {got:?}"
            );
        }
    }
}

#[test]
fn formatting_moves_no_token() {
    use meadow_compiler::lexer::tokenize;
    use meadow_compiler::source::{Source, SourceKind};
    let tokens = |text: &str| {
        tokenize(Source::new(SourceKind::Interactive, text.into()))
            .tokens
            .iter()
            .map(|t| t.value().clone())
            .collect::<Vec<_>>()
    };
    for (name, src) in MODULES {
        // Every line cut where forty columns would have cut it: nothing
        // like how it is written, and the same tokens.
        let narrow = fmt::format_within(src, 40);
        assert!(tokens(src) == tokens(&narrow), "Std.{name} at 40 columns");
    }
}

#[test]
fn how_far_in_a_line_was_typed_does_not_decide_where_it_goes() {
    use meadow_compiler::lexer::tokenize;
    use meadow_compiler::source::{Source, SourceKind};
    for (name, src) in MODULES {
        // The lines a string runs over are the string's, and stay.
        let lexed = tokenize(Source::new(SourceKind::Interactive, (*src).into()));
        let mut held = std::collections::HashSet::new();
        for t in &lexed.tokens {
            let (start, end) = (t.span.start as usize, t.span.end as usize);
            let first = src[..start].matches('\n').count();
            for extra in 1..=src[start..end].matches('\n').count() {
                held.insert(first + extra);
            }
        }
        // So are a `macro`'s, which is kept as it is written: from its line
        // to the next that starts at the margin.
        let mut in_macro = false;
        for (n, line) in src.lines().enumerate() {
            if !line.is_empty() && !line.starts_with(' ') {
                in_macro = line.starts_with("macro ");
            } else if in_macro {
                held.insert(n);
            }
        }
        // Every other line three times as far in as it was.
        let moved: String = src
            .lines()
            .enumerate()
            .map(|(n, line)| {
                let text = line.trim_start();
                let indent = line.len() - text.len();
                if held.contains(&n) {
                    format!("{line}\n")
                } else {
                    format!("{}{text}\n", " ".repeat(indent * 3))
                }
            })
            .collect();
        assert_eq!(formatted(&moved), *src, "Std.{name}");
    }
}
