//! Semantic tokens, derived from the lexer.
//!
//! Highlighting from the compiler's own lexer cannot drift from the language the
//! way a hand-written grammar can. It also settles what a grammar can only guess:
//! `Maybe` and `Just` are both capitalised, and only name resolution knows that
//! one is a type and the other a constructor.
//!
//! The extension still ships a TextMate grammar — it colours a file before the
//! server has started, and comments never reach the lexer at all.

use crate::analysis::{Analysis, Namespace};
use crate::pos::LineIndex;
use meadow_compiler::lexer::{Token, tokenize};
use meadow_compiler::source::{Source, SourceKind};
use std::collections::HashMap;

/// The legend, in the order the protocol indexes it.
pub const LEGEND: &[&str] = &[
    "keyword",
    "type",
    "enumMember",
    "function",
    "variable",
    "number",
    "string",
    "operator",
    "namespace",
];

fn index_of(name: &str) -> u32 {
    LEGEND.iter().position(|l| *l == name).unwrap_or(4) as u32
}

/// What decides a capitalised word's class, beyond its spelling: who is
/// asking knows what is in scope, and where it can, what each occurrence
/// resolved to.
pub struct Scope<'a> {
    /// What the word starting at each offset resolved to, where resolution
    /// got that far: an answer about that occurrence.
    pub resolved: &'a HashMap<u32, Namespace>,
    /// Whether a constructor of that spelling is in scope: a guess, for a
    /// word resolution did not reach.
    pub is_ctor: &'a dyn Fn(&str) -> bool,
    /// The same for a type.
    pub is_type: &'a dyn Fn(&str) -> bool,
    /// Whether the name written from one offset to the other is a function:
    /// by its type where the document was typed, or by what is known of the
    /// spelling where it was not.
    pub is_function: &'a dyn Fn(usize, usize) -> bool,
}

/// Whether a type, as it is rendered, is a function's: an arrow that is not
/// inside brackets of any kind. `(a -> b) -> [a] -> [b]` is; `[a -> b]`, a
/// list of functions, is not.
pub fn is_function_type(rendered: &str) -> bool {
    let mut depth = 0usize;
    let bytes = rendered.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'-' if depth == 0 && bytes.get(i + 1) == Some(&b'>') => return true,
            _ => {}
        }
    }
    false
}

/// The names in `analysis` that are functions, by where each is written.
pub fn functions(analysis: &Analysis) -> std::collections::HashSet<(usize, usize)> {
    analysis
        .typed
        .iter()
        .filter(|(_, ty)| is_function_type(ty))
        .map(|(span, _)| (span.start as usize, span.end as usize))
        .collect()
}

/// `(line, start character, length, token type)`, in source order.
pub fn tokens(text: &str, analysis: &Analysis) -> Vec<(u32, u32, u32, u32)> {
    let idx = LineIndex::new(text);

    // What each capitalised word resolved to, by where it starts. The walk
    // records a type or constructor reference with the span of that one word —
    // in `Expr.Int`, `Expr` is a type and `Int` a constructor — so this is an
    // answer about *this* occurrence, not about the spelling.
    let resolved: HashMap<u32, Namespace> = analysis
        .name_refs
        .iter()
        .map(|(span, _, ns)| (span.start, *ns))
        .collect();
    let functions = functions(analysis);
    let scope = Scope {
        resolved: &resolved,
        is_ctor: &|n| analysis.ctors_in_scope.contains(n),
        is_type: &|n| analysis.types_in_scope.contains(n),
        is_function: &|from, to| functions.contains(&(from, to)),
    };

    let mut out = Vec::new();
    for (start, end, kind) in classified(text, &scope) {
        let (line, from) = idx.position(start);
        let (end_line, to) = idx.position(end);
        // A token that wraps a line would need splitting; none of ours do, and
        // emitting a bad length would corrupt every token after it.
        if line != end_line {
            continue;
        }
        out.push((line, from, to - from, kind));
    }
    out
}

/// Each token of `text` that has a class: where it starts and ends, in bytes,
/// and its class as an index into [`LEGEND`], in source order.
///
/// The one place that decides what a token is coloured as. The server
/// answers an editor's `textDocument/semanticTokens/full` with it, and the
/// REPL colours what is typed at its prompt with it.
pub fn classified(text: &str, scope: &Scope<'_>) -> Vec<(usize, usize, u32)> {
    let source = Source::new(SourceKind::Interactive, text.into());
    let lex = tokenize(source);
    let mut out = Vec::new();
    let resolved = scope.resolved;

    // Inside `use a.b.c`, up to the import list: an unresolved capital there is
    // a module. The path is over at its list, or at anything a path cannot
    // contain -- `use M.Ty.*` has no list, and the next line is not a path.
    let mut in_use_path = false;

    for (i, t) in lex.tokens.iter().enumerate() {
        match t.value() {
            Token::Use => in_use_path = true,
            Token::UpperIdent(_) | Token::LowerIdent(_) | Token::Period | Token::As => {}
            _ => in_use_path = false,
        }
        let next_is_period = matches!(
            lex.tokens.get(i + 1).map(|n| n.value()),
            Some(Token::Period)
        );
        // `mod Int` declares a module, whatever else that spelling names -- in
        // `Std`, `Int` is also a type and one of `Json`'s constructors.
        let declares_module = i > 0 && matches!(lex.tokens[i - 1].value(), Token::Mod);
        let kind = match t.value() {
            Token::Mod
            | Token::Use
            | Token::Def
            | Token::Fun
            | Token::Let
            | Token::In
            | Token::Match
            | Token::With
            | Token::If
            | Token::Then
            | Token::Else
            | Token::Data
            | Token::Record
            | Token::Effect
            | Token::Handle
            | Token::Type
            | Token::Trait
            | Token::Impl
            | Token::Where
            | Token::Infix
            | Token::Infixl
            | Token::Infixr
            | Token::End
            | Token::As
            | Token::And
            | Token::Or => "keyword",
            Token::Int(_) | Token::Real(_) => "number",
            Token::String(_)
            | Token::InterpStart(_)
            | Token::InterpMid(_)
            | Token::InterpEnd(_)
            | Token::Char(_) => "string",
            Token::UpperIdent(name) => {
                let n = name.to_string();
                if declares_module {
                    "namespace"
                } else if let Some(ns) = resolved.get(&t.span.start) {
                    // Resolution first, and it is the whole answer when there
                    // is one. Matching the *spelling* against known names is
                    // what coloured mini-ml's `Expr.Int` and `Expr.Bool` as
                    // constructors -- of `Std.Json`, which also has an `Int`
                    // and a `Bool` -- while its `Lam` and `Var`, matching
                    // nothing, fell through to `namespace`.
                    match ns {
                        Namespace::Ctor => "enumMember",
                        Namespace::Type => "type",
                        Namespace::Module => "namespace",
                        Namespace::Field => "variable",
                    }
                } else if in_use_path {
                    // `Ty` in `use M.Ty (C)` resolved above; the rest of a
                    // path is modules.
                    "namespace"
                } else if next_is_period {
                    // Unresolved and qualifying something: a module or an
                    // alias (`S.concat`), whatever else that spelling means.
                    "namespace"
                } else if (scope.is_ctor)(&n) {
                    // Only a guess from here on, for a document whose analysis
                    // did not get far enough to resolve it -- a parse error
                    // mid-edit -- where a guess beats losing colour entirely.
                    "enumMember"
                } else if (scope.is_type)(&n) {
                    "type"
                } else {
                    // An unresolved capital is a module qualifier or a name the
                    // file has not defined yet; `namespace` reads better than
                    // guessing `type`.
                    "namespace"
                }
            }
            Token::LowerIdent(_) => {
                if (scope.is_function)(t.span.start as usize, t.span.end as usize) {
                    "function"
                } else {
                    "variable"
                }
            }
            Token::OpIdent(_)
            | Token::ConOpIdent(_)
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Caret
            | Token::Eq
            | Token::EqEq
            | Token::Neq
            | Token::Lt
            | Token::Gt
            | Token::Leq
            | Token::Geq
            | Token::Bang
            | Token::DeclBang
            | Token::RArrow
            | Token::LArrow
            | Token::ColonColon
            | Token::LPipe
            | Token::RPipe
            | Token::Backslash
            | Token::At
            | Token::Dollar => "operator",
            _ => continue,
        };

        out.push((t.span.start as usize, t.span.end as usize, index_of(kind)));
    }
    out
}

/// The protocol wants each token relative to the one before it.
pub fn encode(tokens: &[(u32, u32, u32, u32)]) -> Vec<u32> {
    let mut data = Vec::with_capacity(tokens.len() * 5);
    let (mut prev_line, mut prev_start) = (0u32, 0u32);
    for &(line, start, len, kind) in tokens {
        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 {
            start - prev_start
        } else {
            start
        };
        data.extend_from_slice(&[delta_line, delta_start, len, kind, 0]);
        prev_line = line;
        prev_start = start;
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_function_is_told_by_an_arrow_outside_every_bracket() {
        assert!(is_function_type("Int -> Int"));
        assert!(is_function_type("(a -> b) -> [a] -> [b]"));
        assert!(is_function_type("() -> () ! Console"));
        assert!(!is_function_type("Int"));
        assert!(!is_function_type("[a -> b]"));
        assert!(!is_function_type("(Int -> Int, String)"));
        assert!(!is_function_type("{ f : a -> b }"));
    }

    #[test]
    fn deltas_are_relative_to_the_previous_token() {
        let toks = vec![(0, 0, 3, 0), (0, 4, 2, 1), (2, 5, 1, 2)];
        assert_eq!(
            encode(&toks),
            vec![0, 0, 3, 0, 0, 0, 4, 2, 1, 0, 2, 5, 1, 2, 0]
        );
    }
}
