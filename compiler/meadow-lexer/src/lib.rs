//! Lexing, via `logos`.
//!
//! [`tokenize`] turns a [`Source`] into a `Vec<LToken>` (each a `Token` + its
//! [`Span`]). Whitespace and `--` line comments are skipped by the lexer itself.
//! An unrecognized byte becomes a [`Token::Error`] *and* a [`Diagnostic`], so the
//! parser can keep going.
//!
//! Note the operator handling: `==`, `->`, `<=` … are their own `#[token]`s and
//! win over the catch-all `OpIdent` regex (which covers user-defined operator
//! names); this is why the regex must *not* include letters.

pub mod tt;

use logos::Logos;
use meadow_diagnostics::Diagnostic;
use meadow_intern::InternedString;
use meadow_source::Source;
use meadow_span::{Located, Span};
use std::fmt::Display;

pub type LToken = Located<Token>;

/// Parse an integer literal slice — optional `-`, then decimal or a `0b` / `0o` /
/// `0x` prefixed literal — into a fixed-width [`i64`]. A literal that does not fit
/// in `i64` fails to lex (there is no `BigInt` literal syntax — use `toBigInt`).
fn parse_int(s: &str) -> Option<i64> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (radix, digits) = if let Some(d) = body.strip_prefix("0x") {
        (16, d)
    } else if let Some(d) = body.strip_prefix("0b") {
        (2, d)
    } else if let Some(d) = body.strip_prefix("0o") {
        (8, d)
    } else {
        (10, body)
    };
    let n = i64::from_str_radix(digits, radix).ok()?;
    Some(if neg { -n } else { n })
}

/// The character inside a `'…'` literal, resolving an escape: the same escapes
/// a string has (see [`escape`]). `None` for a bad one, which makes the literal
/// an invalid token.
fn unescape_char(raw: &str) -> Option<char> {
    let inner = &raw[1..raw.len() - 1];
    match inner.strip_prefix('\\') {
        Some(rest) => match escape(rest) {
            Ok((Some(c), n)) if n == rest.len() => Some(c),
            _ => None,
        },
        None => inner.chars().next(),
    }
}

/// The escape sequence `rest` starts with -- `rest` being what follows its
/// backslash: the character it stands for, and how many bytes of `rest` it
/// takes. Or, for a bad one, what is wrong and how many bytes to skip.
///
/// * `\n \r \t \0`, `\\ \" \'`, and `\$` -- a dollar sign that does not start a
///   `${…}`;
/// * `\a \b \f \v`, and `\e` -- the escape character, as terminal codes start;
/// * `\x41`, two hex digits, up to `\x7F`: a string is characters, so what an
///   ASCII code cannot say, `\u` does;
/// * `\u{1F600}`, one to six hex digits: any Unicode scalar value;
/// * a backslash ending a line joins it to the next: the line break and the
///   next line's leading whitespace are not in the string. `None` is that.
fn escape(rest: &str) -> Result<(Option<char>, usize), (String, usize)> {
    let Some(c) = rest.chars().next() else {
        return Err(("a `\\` with nothing after it".to_string(), 0));
    };
    let simple = |ch: char| Ok((Some(ch), 1));
    match c {
        'n' => simple('\n'),
        'r' => simple('\r'),
        't' => simple('\t'),
        '0' => simple('\0'),
        'a' => simple('\x07'),
        'b' => simple('\x08'),
        'f' => simple('\x0C'),
        'v' => simple('\x0B'),
        'e' => simple('\x1B'),
        '\\' | '"' | '\'' | '$' => simple(c),
        'x' => {
            let Some(hex) = rest
                .get(1..3)
                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
            else {
                return Err(("`\\x` takes two hex digits, as `\\x41`".to_string(), 1));
            };
            let code = u8::from_str_radix(hex, 16).expect("two hex digits");
            if code > 0x7F {
                return Err((
                    format!("`\\x{hex}` is past ASCII; write `\\u{{{hex}}}` for that character"),
                    3,
                ));
            }
            Ok((Some(code as char), 3))
        }
        'u' => {
            let Some(body) = rest[1..].strip_prefix('{') else {
                return Err((
                    "`\\u` takes hex digits in braces, as `\\u{1F600}`".to_string(),
                    1,
                ));
            };
            let Some(close) = body
                .find(['}', '"', '\n'])
                .filter(|&k| body[k..].starts_with('}'))
            else {
                return Err(("this `\\u{` is never closed".to_string(), 2));
            };
            let hex = &body[..close];
            let len = 3 + close;
            if hex.is_empty() || hex.len() > 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err((format!("`\\u{{{hex}}}` needs one to six hex digits"), len));
            }
            match char::from_u32(u32::from_str_radix(hex, 16).expect("hex digits")) {
                Some(ch) => Ok((Some(ch), len)),
                None => Err((format!("`\\u{{{hex}}}` is not a Unicode character"), len)),
            }
        }
        '\n' | '\r' => {
            let joined = rest.trim_start_matches([' ', '\t', '\n', '\r']);
            Ok((None, rest.len() - joined.len()))
        }
        other => Err((
            format!("`\\{other}` is not an escape; a backslash itself is `\\\\`"),
            other.len_utf8(),
        )),
    }
}

#[derive(Logos, Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[logos(subpattern alpha = r"[a-zA-Z]+")]
pub enum Token {
    Eof,
    #[regex(r"--[^\n]*", logos::skip, allow_greedy = true)]
    Comment,
    #[regex(r"[ \t\n\r]+", logos::skip)]
    Whitespace,
    // Literals and identifiers
    // No leading `-`: a minus is always its own token, so `-2` / `-2.5` are the
    // prefix operator applied to a literal (the parser folds the sign in).
    #[regex(
        r"(0b[0-1]+)|(0o[0-7]+)|(0x[0-9a-fA-F]+)|([1-9]\d*|0)",
        |lex| parse_int(lex.slice()),
        priority = 3
    )]
    Int(i64),
    /// A floating-point literal, as its IEEE-754 bit pattern -- so that a token,
    /// and the token trees a macro carries, are `Eq` like the rest of the AST.
    /// Decode with [`f64::from_bits`].
    #[regex(
        r"([0-9]*[.])?[0-9]+",
        |lex| lex.slice().parse::<f64>().ok().map(f64::to_bits),
        priority = 2
    )]
    Real(u64),
    // #[regex(
    //     r"-?((0b[0-1]+)|(0o[0-7]+)|(0x[0-9a-fA-F]+)|([1-9]\d*|0))(/-?((0b[0-1]+)|(0o[0-7]+)|(0x[0-9a-fA-F]+)|([1-9]\d*|0)))",
    //     |lex| lex.slice().parse().ok())]
    // Rational(Rational64),
    /// A string literal with no `${…}` in it, unquoted and unescaped. Lexed by
    /// [`tokenize`] rather than a pattern -- see [`Token::Quote`].
    String(InternedString),
    /// The text of an interpolated string literal up to its first `${`:
    /// `"a ${` of `"a ${x} b"`. The tokens of the hole follow, then an
    /// [`Token::InterpMid`] or [`Token::InterpEnd`].
    InterpStart(InternedString),
    /// The text between two holes: `} and ${` of `"${x} and ${y}"`.
    InterpMid(InternedString),
    /// The text after the last hole: `} b"` of `"a ${x} b"`.
    InterpEnd(InternedString),
    /// Where a string literal starts. Never in a token stream: [`tokenize`]
    /// reads the literal from here itself, since what is inside a `${…}` is
    /// tokens again -- strings, braces and all -- which no pattern can match.
    #[token("\"")]
    Quote,
    #[regex(r"'(\\u\{[^}'\n]*\}|\\x[^'\n]{0,2}|\\.|[^'\\])'", |lex| unescape_char(lex.slice()))]
    Char(char),
    // A leading `_` is a name too, when something follows it: `_primAdd`, the
    // primitives the operators are defined with. `_` alone is `Wildcard`.
    #[regex(r"[a-z][a-zA-Z0-9'_]*", |lex| InternedString::from(lex.slice()), priority = 2)]
    #[regex(r"_[a-zA-Z0-9'_]+", |lex| InternedString::from(lex.slice()), priority = 2)]
    LowerIdent(InternedString),
    #[regex(r"[A-Z][a-zA-Z0-9']*", |lex| InternedString::from(lex.slice()))]
    UpperIdent(InternedString),
    // `$` is not in either operator charset: it is the macro system's splice
    // (see `docs/MACROS.md`), so it must lex on its own even when it abuts an
    // operator -- `$x`, `$$`, `$( … ),*`.
    #[regex(r"[!%&*+./<=>?@|^~:\-]+", |lex| InternedString::from(lex.slice()), priority = 1)]
    OpIdent(InternedString),
    #[regex(r":[!%&*+./<=>?@|^~:\-]+", |lex| InternedString::from(lex.slice()))]
    ConOpIdent(InternedString),

    // Punctuation
    #[token("_")]
    Wildcard,
    #[token("\\")]
    Backslash,
    #[token("<-")]
    LArrow,
    #[token("->")]
    RArrow,
    /// `Show a => a -> String`: what a signature's type variables must implement.
    #[token("=>")]
    FatArrow,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("^")]
    Caret,
    #[token("and")]
    And,
    #[token("or")]
    Or,
    #[token("=")]
    Eq,
    #[token("==")]
    EqEq,
    #[token("!=")]
    Neq,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("<=")]
    Leq,
    #[token(">=")]
    Geq,
    #[token("!")]
    Bang,
    /// The `!` of a macro call written at the start of a line, `name!(…)` in
    /// column 0 -- never lexed as such, but made from a [`Token::Bang`] by
    /// [`tokenize`]. A call there begins a declaration, and the parser takes
    /// no argument that is one: `def result = f x` followed by `derive! { … }` on
    /// the next line is two declarations, not `f x derive! { … }`.
    DeclBang,
    #[token(",")]
    Comma,
    #[token(".")]
    Period,
    #[token("..")]
    DoublePeriod,
    #[token("..=")]
    DoublePeriodEq,
    #[token("::", priority = 10)]
    ColonColon,
    #[token(":")]
    Colon,
    #[token(";")]
    SemiColon,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("[")]
    LBrack,
    #[token("]")]
    RBrack,
    #[token("#")]
    Hash,
    #[token("|")]
    Bar,
    #[token("<|")]
    LPipe,
    #[token("|>")]
    RPipe,
    #[token("@")]
    At,
    #[token("`")]
    Backtick,
    /// The macro splice: `$x`, `$( … ),*`, `$$`, `$pkg`. Outside a macro it is
    /// not part of any form, so a stray one is the parser's error to report.
    #[token("$")]
    Dollar,

    // Keywords
    #[token("mod")]
    Mod,
    #[token("use")]
    Use,
    #[token("def")]
    Def,
    #[token("fun")]
    Fun,
    #[token("let")]
    Let,
    #[token("in")]
    In,
    #[token("match")]
    Match,
    #[token("with")]
    With,
    #[token("if")]
    If,
    #[token("then")]
    Then,
    #[token("else")]
    Else,
    #[token("data")]
    Data,
    #[token("record")]
    Record,
    #[token("effect")]
    Effect,
    #[token("handle")]
    Handle,
    #[token("type")]
    Type,
    #[token("trait")]
    Trait,
    #[token("impl")]
    Impl,
    #[token("where")]
    Where,
    #[token("as")]
    As,
    #[token("macro")]
    Macro,
    /// `infixl 6 +, -` -- how tightly an operator binds, and which way.
    #[token("infix")]
    Infix,
    #[token("infixl")]
    Infixl,
    #[token("infixr")]
    Infixr,
    Error,
}

/// `text` in a string or character literal, with the escapes that make it one
/// again: the inverse of [`escape`], for the ones that have to be written back.
fn quote(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            // Only before a `{`, where it would otherwise start a hole.
            '$' => out.push_str("\\$"),
            c => out.push(c),
        }
    }
}

impl Token {
    /// How this token is written in source.
    ///
    /// What a macro's `stringify!` answers, and how a token appears in a
    /// message. Lexing the result gives the token back, with one exception:
    /// [`Token::Error`] stands for text that did not lex at all, and there is
    /// nothing to write for it.
    pub fn text(&self) -> String {
        // `Token::String` shadows the type inside this match, so the few places
        // that build one name it through the alias.
        use Token::*;
        use std::string::String as Str;
        let owned = |s: &str| s.to_string();
        match self {
            Eof | Whitespace | Comment | Error => Str::new(),
            Int(i) => i.to_string(),
            Real(bits) => {
                let n = f64::from_bits(*bits);
                // `1.0` must not come back as `1`, which would lex as an `Int`.
                if n.fract() == 0.0 && n.is_finite() {
                    format!("{n:.1}")
                } else {
                    n.to_string()
                }
            }
            String(s) => {
                let mut out = Str::from('"');
                quote(s, &mut out);
                out.push('"');
                out
            }
            // The three pieces of an interpolated literal carry the `${` and `}`
            // around them, so that the pieces and the holes' own tokens between
            // them write the literal back exactly.
            InterpStart(s) => {
                let mut out = Str::from('"');
                quote(s, &mut out);
                out.push_str("${");
                out
            }
            InterpMid(s) => {
                let mut out = Str::from('}');
                quote(s, &mut out);
                out.push_str("${");
                out
            }
            InterpEnd(s) => {
                let mut out = Str::from('}');
                quote(s, &mut out);
                out.push('"');
                out
            }
            Quote => owned("\""),
            Char(c) => {
                let mut out = Str::from('\'');
                if *c == '\'' {
                    out.push_str("\\'");
                } else {
                    quote(&c.to_string(), &mut out);
                }
                out.push('\'');
                out
            }
            LowerIdent(s) | UpperIdent(s) | OpIdent(s) | ConOpIdent(s) => s.to_string(),
            Wildcard => owned("_"),
            Backslash => owned("\\"),
            LArrow => owned("<-"),
            RArrow => owned("->"),
            FatArrow => owned("=>"),
            Plus => owned("+"),
            Minus => owned("-"),
            Star => owned("*"),
            Slash => owned("/"),
            Percent => owned("%"),
            Caret => owned("^"),
            And => owned("and"),
            Or => owned("or"),
            Eq => owned("="),
            EqEq => owned("=="),
            Neq => owned("!="),
            Lt => owned("<"),
            Gt => owned(">"),
            Leq => owned("<="),
            Geq => owned(">="),
            Bang | DeclBang => owned("!"),
            Comma => owned(","),
            Period => owned("."),
            DoublePeriod => owned(".."),
            DoublePeriodEq => owned("..="),
            ColonColon => owned("::"),
            Colon => owned(":"),
            SemiColon => owned(";"),
            LParen => owned("("),
            RParen => owned(")"),
            LBrace => owned("{"),
            RBrace => owned("}"),
            LBrack => owned("["),
            RBrack => owned("]"),
            Hash => owned("#"),
            Bar => owned("|"),
            LPipe => owned("<|"),
            RPipe => owned("|>"),
            At => owned("@"),
            Backtick => owned("`"),
            Dollar => owned("$"),
            Mod => owned("mod"),
            Use => owned("use"),
            Def => owned("def"),
            Fun => owned("fun"),
            Let => owned("let"),
            In => owned("in"),
            Match => owned("match"),
            With => owned("with"),
            If => owned("if"),
            Then => owned("then"),
            Else => owned("else"),
            Data => owned("data"),
            Record => owned("record"),
            Effect => owned("effect"),
            Handle => owned("handle"),
            Type => owned("type"),
            Trait => owned("trait"),
            Impl => owned("impl"),
            Where => owned("where"),
            Infix => owned("infix"),
            Infixl => owned("infixl"),
            Infixr => owned("infixr"),
            As => owned("as"),
            Macro => owned("macro"),
        }
    }
}

impl<'a> Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use Token::*;
        match self {
            Eof => write!(f, "Eof"),
            Comment => write!(f, "Comment"),
            Whitespace => write!(f, "Whitespace"),
            Int(i) => write!(f, "Int({})", i),
            Real(r) => write!(f, "Real({})", f64::from_bits(*r)),
            String(s) => write!(f, "InternedString({})", s),
            InterpStart(s) => write!(f, "InterpStart({})", s),
            InterpMid(s) => write!(f, "InterpMid({})", s),
            InterpEnd(s) => write!(f, "InterpEnd({})", s),
            Quote => write!(f, "Quote"),
            Char(c) => write!(f, "Char({})", c),
            LowerIdent(s) => write!(f, "LowerIdent({})", s),
            UpperIdent(s) => write!(f, "UpperIdent({})", s),
            OpIdent(s) => write!(f, "OpIdent({})", s),
            ConOpIdent(s) => write!(f, "ConOpIdent({})", s),

            Wildcard => write!(f, "Wildcard"),
            Backslash => write!(f, "Backslash"),
            LArrow => write!(f, "LArrow"),
            RArrow => write!(f, "RArrow"),
            FatArrow => write!(f, "FatArrow"),
            Plus => write!(f, "Plus"),
            Minus => write!(f, "Minus"),
            Star => write!(f, "Star"),
            Slash => write!(f, "Slash"),
            Percent => write!(f, "Percent"),
            Caret => write!(f, "Caret"),
            And => write!(f, "And"),
            Or => write!(f, "Or"),
            Eq => write!(f, "Eq"),
            EqEq => write!(f, "EqEq"),
            Neq => write!(f, "Neq"),
            Lt => write!(f, "Lt"),
            Gt => write!(f, "Gt"),
            Leq => write!(f, "Leq"),
            Geq => write!(f, "Geq"),
            Bang | DeclBang => write!(f, "Bang"),
            Comma => write!(f, "Comma"),
            Period => write!(f, "Period"),
            DoublePeriod => write!(f, "DoublePeriod"),
            DoublePeriodEq => write!(f, "DoublePeriodEq"),
            ColonColon => write!(f, "ColonColon"),
            Colon => write!(f, "Colon"),
            SemiColon => write!(f, "SemiColon"),
            LParen => write!(f, "LParen"),
            RParen => write!(f, "RParen"),
            LBrace => write!(f, "LBrace"),
            RBrace => write!(f, "RBrace"),
            LBrack => write!(f, "LBrack"),
            RBrack => write!(f, "RBrack"),
            Hash => write!(f, "HashLBrack"),
            Bar => write!(f, "Bar"),
            LPipe => write!(f, "LPipe"),
            RPipe => write!(f, "RPipe"),
            At => write!(f, "At"),
            Backtick => write!(f, "Backtick"),
            Dollar => write!(f, "Dollar"),

            Mod => write!(f, "Mod"),
            Use => write!(f, "Use"),
            Def => write!(f, "Def"),
            Fun => write!(f, "Fun"),
            Let => write!(f, "Let"),
            In => write!(f, "In"),
            If => write!(f, "If"),
            Match => write!(f, "Match"),
            With => write!(f, "With"),
            Then => write!(f, "Then"),
            Else => write!(f, "Else"),
            Data => write!(f, "Data"),
            Record => write!(f, "Record"),
            Effect => write!(f, "Effect"),
            Handle => write!(f, "Handle"),
            Type => write!(f, "Type"),
            Trait => write!(f, "Trait"),
            Impl => write!(f, "Impl"),
            Where => write!(f, "Where"),
            Infix => write!(f, "Infix"),
            Infixl => write!(f, "Infixl"),
            Infixr => write!(f, "Infixr"),
            As => write!(f, "As"),
            Macro => write!(f, "Macro"),
            Error => write!(f, "Error"),
        }
    }
}

pub struct LexResult {
    pub tokens: Vec<LToken>,
    pub errors: Vec<Diagnostic>,
}

pub fn tokenize(src: Source) -> LexResult {
    let mut out = LexResult {
        tokens: Vec::new(),
        errors: Vec::new(),
    };
    lex_into(&src.content, 0, &src.name().to_string(), &mut out);
    mark_line_calls(&src.content, &mut out.tokens);
    out
}

/// Turn the `!` of every macro call written at the start of a line into a
/// [`Token::DeclBang`]: a name path in column 0 with the `!` right after it.
///
/// The one place layout decides anything, and only because a declaration can
/// begin with a name here -- nothing else at the top level does, since every
/// other declaration opens with a keyword or an `@`.
fn mark_line_calls(text: &str, tokens: &mut [LToken]) {
    let at_line_start = |at: usize| at == 0 || text.as_bytes().get(at - 1) == Some(&b'\n');
    for i in 1..tokens.len() {
        if *tokens[i].value() != Token::Bang {
            continue;
        }
        // Back over `A.B.name`, each piece touching the next.
        let mut first = i;
        let mut want_name = true;
        while first > 0 {
            let prev = &tokens[first - 1];
            let touching = prev.span.end == tokens[first].span.start;
            let fits = if want_name {
                matches!(prev.value(), Token::LowerIdent(_) | Token::UpperIdent(_))
            } else {
                *prev.value() == Token::Period
            };
            if !touching || !fits {
                break;
            }
            first -= 1;
            want_name = !want_name;
        }
        // A path ends on a name, and the `!` has to follow one.
        let names_something = first < i && !want_name;
        if names_something && at_line_start(tokens[first].span.start as usize) {
            let span = tokens[i].span;
            tokens[i] = LToken::new(Token::DeclBang, span);
        }
    }
}

/// Lex `text`, whose first byte is at `base` in the source named `name`.
///
/// Called on the whole source, and again on what is inside each `${…}` of a
/// string literal, so a hole holds any expression the language has.
fn lex_into(text: &str, base: usize, name: &str, out: &mut LexResult) {
    let mut lexer = Token::lexer(text);
    while let Some(res) = lexer.next() {
        let local = lexer.span();
        let span = Span::from(base + local.start..base + local.end);
        match res {
            Ok(Token::Quote) => {
                let end = lex_string(text, local.start, base, name, out);
                lexer.bump(end - local.end);
            }
            // `r"…"` or `r#"…"#`: the `r` has to be a name of its own, which the
            // identifier pattern already makes it.
            Ok(Token::LowerIdent(id))
                if &*id == "r" && raw_hashes(&text[local.end..]).is_some() =>
            {
                let end = lex_raw(text, local.start, base, name, out);
                lexer.bump(end - local.end);
            }
            Ok(token) => out.tokens.push(LToken::new(token, span)),
            Err(_) => {
                let err = Diagnostic::new(
                    format!("Invalid token: {}", &text[local]),
                    name.to_string(),
                    ("Invalid token".to_string(), span),
                    vec![],
                );
                out.errors.push(err);
                out.tokens.push(LToken::new(Token::Error, span));
            }
        }
    }
}

/// Read the string literal whose opening quote is at `start` in `text`, push
/// its tokens, and answer where it ends: just past the closing quote.
///
/// Without a `${` that is one [`Token::String`]. With some, it is the text
/// before the first hole ([`Token::InterpStart`]), the hole's own tokens, the
/// text between holes ([`Token::InterpMid`]) and after the last
/// ([`Token::InterpEnd`]) -- each piece of text spanning the `${`, `}` and
/// quotes around it, so that the pieces and the holes' tokens between them
/// cover the literal exactly.
///
/// Escapes are [`escape`]'s; `\$` is how a literal says `${` without starting a
/// hole. A bad escape is reported, and the rest of the literal still read.
fn lex_string(text: &str, start: usize, base: usize, name: &str, out: &mut LexResult) -> usize {
    let bytes = text.as_bytes();
    let at = |from: usize, to: usize| Span::from(base + from..base + to);
    let mut i = start + 1;
    // Where the piece of text being read starts: its quote, or the `}` of the
    // hole before it.
    let mut piece = start;
    let mut buf = String::new();
    let mut holes = 0;
    loop {
        let Some(&b) = bytes.get(i) else {
            let span = at(start, text.len());
            out.errors.push(Diagnostic::new(
                "this string is never closed".to_string(),
                name.to_string(),
                ("no closing `\"`".to_string(), span),
                vec![],
            ));
            out.tokens.push(LToken::new(Token::Error, span));
            return text.len();
        };
        match b {
            b'"' => {
                let token = if holes == 0 {
                    Token::String(InternedString::from(buf))
                } else {
                    Token::InterpEnd(InternedString::from(buf))
                };
                out.tokens.push(LToken::new(token, at(piece, i + 1)));
                return i + 1;
            }
            b'\\' => match escape(&text[i + 1..]) {
                Ok((c, n)) => {
                    buf.extend(c);
                    i += 1 + n;
                }
                Err((msg, n)) => {
                    let span = at(i, i + 1 + n);
                    out.errors.push(Diagnostic::new(
                        msg,
                        name.to_string(),
                        ("this escape".to_string(), span),
                        vec![],
                    ));
                    i += 1 + n;
                }
            },
            b'$' if bytes.get(i + 1) == Some(&b'{') => {
                let text_so_far = InternedString::from(std::mem::take(&mut buf));
                let token = if holes == 0 {
                    Token::InterpStart(text_so_far)
                } else {
                    Token::InterpMid(text_so_far)
                };
                out.tokens.push(LToken::new(token, at(piece, i + 2)));
                let open = i + 2;
                let Some(close) = hole_end(text, open) else {
                    let span = at(i, text.len());
                    out.errors.push(Diagnostic::new(
                        "this `${` is never closed".to_string(),
                        name.to_string(),
                        ("no closing `}`".to_string(), span),
                        vec![],
                    ));
                    out.tokens.push(LToken::new(Token::Error, span));
                    return text.len();
                };
                if text[open..close].trim().is_empty() {
                    let span = at(i, close + 1);
                    out.errors.push(Diagnostic::new(
                        "`${}` needs an expression inside".to_string(),
                        name.to_string(),
                        ("nothing to put in the string".to_string(), span),
                        vec![(
                            "write `\\${` for the characters themselves".to_string(),
                            span,
                        )],
                    ));
                    out.tokens.push(LToken::new(Token::Error, span));
                } else {
                    lex_into(&text[open..close], base + open, name, out);
                }
                holes += 1;
                piece = close;
                i = close + 1;
            }
            _ => {
                let c = text[i..]
                    .chars()
                    .next()
                    .expect("a character at a char boundary");
                buf.push(c);
                i += c.len_utf8();
            }
        }
    }
}

/// How many `#`s a raw string's opening has, if `after_r` -- what follows an
/// `r` -- is the rest of one: `#`s, then a quote.
fn raw_hashes(after_r: &str) -> Option<usize> {
    let hashes = after_r.bytes().take_while(|&b| b == b'#').count();
    (after_r.as_bytes().get(hashes) == Some(&b'"')).then_some(hashes)
}

/// Where the raw string whose `r` is at `start` ends -- just past its closing
/// quote and `#`s -- and where its text is, if it is closed.
fn raw_end(text: &str, start: usize) -> Option<(usize, std::ops::Range<usize>)> {
    let hashes = raw_hashes(&text[start + 1..])?;
    let open = start + 1 + hashes + 1;
    let close = format!("\"{}", "#".repeat(hashes));
    let k = text[open..].find(&close)?;
    Some((open + k + close.len(), open..open + k))
}

/// Read the raw string whose `r` is at `start` -- `r"…"`, or `r#"…"#` with
/// as many `#`s as it takes for the text to hold `"` followed by them -- and
/// answer where it ends. Its text is exactly what is between the quotes:
/// no escapes, no `${…}`, and line breaks as they are.
fn lex_raw(text: &str, start: usize, base: usize, name: &str, out: &mut LexResult) -> usize {
    match raw_end(text, start) {
        Some((end, body)) => {
            let token = Token::String(InternedString::from(&text[body]));
            out.tokens
                .push(LToken::new(token, Span::from(base + start..base + end)));
            end
        }
        None => {
            let hashes = raw_hashes(&text[start + 1..]).unwrap_or(0);
            let span = Span::from(base + start..base + text.len());
            out.errors.push(Diagnostic::new(
                "this raw string is never closed".to_string(),
                name.to_string(),
                (format!("no closing `\"{}`", "#".repeat(hashes)), span),
                vec![],
            ));
            out.tokens.push(LToken::new(Token::Error, span));
            text.len()
        }
    }
}

/// Where the `}` that closes a `${` whose contents start at `open` is, if it is
/// closed: past nested braces, strings (with holes of their own), raw strings,
/// character literals and comments, none of whose braces count.
fn hole_end(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'{' => depth += 1,
            b'}' if depth == 0 => return Some(i),
            b'}' => depth -= 1,
            b'"' => {
                i = string_end(text, i)?;
                continue;
            }
            b'r' if !(i > open && is_word_byte(bytes[i - 1]))
                && !bytes.get(i + 1).is_some_and(|&c| is_word_byte(c))
                && raw_hashes(&text[i + 1..]).is_some() =>
            {
                i = raw_end(text, i)?.0;
                continue;
            }
            // An apostrophe after a word character is part of a name (`xs'`).
            b'\'' if !(i > open && is_word_byte(bytes[i - 1])) => {
                if let Some(n) = char_literal_len(&text[i..]) {
                    i += n;
                    continue;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while bytes.get(i).is_some_and(|&c| c != b'\n') {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Just past the closing quote of the string literal opening at `start`.
fn string_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = start + 1;
    while let Some(&b) = bytes.get(i) {
        match b {
            // A continuation byte of what is escaped is never a quote or a `$`,
            // so stepping over one byte of it is enough.
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            b'$' if bytes.get(i + 1) == Some(&b'{') => i = hole_end(text, i + 2)? + 1,
            _ => i += 1,
        }
    }
    None
}

/// The length of the character literal `text` starts with, if it does.
fn char_literal_len(text: &str) -> Option<usize> {
    let rest = text.get(1..)?;
    let n = match rest.strip_prefix('\\') {
        Some(escaped) => 1 + escape(escaped).ok()?.1,
        None => rest.chars().next()?.len_utf8(),
    };
    rest.get(n..)?.starts_with('\'').then_some(1 + n + 1)
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'\''
}

#[cfg(test)]
mod tests {
    use super::*;
    use meadow_source::SourceKind;

    /// Lex `src` and return just the token kinds (spans and errors dropped).
    fn kinds(src: &str) -> Vec<Token> {
        let source = Source::new(SourceKind::Interactive, src.into());
        tokenize(source)
            .tokens
            .into_iter()
            .map(|t| *t.value)
            .collect()
    }

    #[test]
    fn keywords_and_identifiers() {
        use Token::*;
        assert_eq!(
            kinds("fun map xs"),
            vec![Fun, LowerIdent("map".into()), LowerIdent("xs".into())]
        );
    }

    #[test]
    fn end_is_an_identifier() {
        use Token::*;
        assert_eq!(
            kinds("let end = 3 in end"),
            vec![
                Let,
                LowerIdent("end".into()),
                Eq,
                Int(3),
                In,
                LowerIdent("end".into())
            ]
        );
    }

    #[test]
    fn upper_vs_lower_identifiers() {
        use Token::*;
        assert_eq!(
            kinds("Cons x"),
            vec![UpperIdent("Cons".into()), LowerIdent("x".into())]
        );
    }

    #[test]
    fn operators_are_distinct_tokens_not_op_idents() {
        // `==` and `->` must win over the generic `OpIdent` regex.
        use Token::*;
        assert_eq!(
            kinds("a == b"),
            vec![LowerIdent("a".into()), EqEq, LowerIdent("b".into())]
        );
        assert_eq!(
            kinds("\\x -> x"),
            vec![
                Backslash,
                LowerIdent("x".into()),
                RArrow,
                LowerIdent("x".into())
            ]
        );
    }

    #[test]
    fn comments_and_whitespace_are_skipped() {
        use Token::*;
        assert_eq!(kinds("1 -- a comment\n+ 2"), vec![Int(1), Plus, Int(2)]);
    }

    #[test]
    fn numbers_and_strings() {
        use Token::*;
        // string literals are unquoted and unescaped by the lexer
        assert_eq!(kinds("42 \"hi\""), vec![Int(42), String("hi".into())]);
        assert_eq!(kinds(r#""a\tb\n""#), vec![String("a\tb\n".into())]);
    }

    #[test]
    fn an_interpolated_string_is_its_pieces_and_its_holes() {
        use Token::*;
        assert_eq!(
            kinds(r#""a ${x + 1} b ${f "c"} d""#),
            vec![
                InterpStart("a ".into()),
                LowerIdent("x".into()),
                Plus,
                Int(1),
                InterpMid(" b ".into()),
                LowerIdent("f".into()),
                String("c".into()),
                InterpEnd(" d".into()),
            ]
        );
        // Braces inside a hole are the hole's, and a string inside one may
        // have holes of its own.
        assert_eq!(
            kinds(r#""${ {x = 1}.x } ${"${y}"}""#),
            vec![
                InterpStart("".into()),
                LBrace,
                LowerIdent("x".into()),
                Eq,
                Int(1),
                RBrace,
                Period,
                LowerIdent("x".into()),
                InterpMid(" ".into()),
                InterpStart("".into()),
                LowerIdent("y".into()),
                InterpEnd("".into()),
                InterpEnd("".into()),
            ]
        );
    }

    #[test]
    fn a_dollar_is_a_hole_only_before_a_brace_and_unescaped() {
        use Token::*;
        assert_eq!(kinds(r#""costs $5""#), vec![String("costs $5".into())]);
        assert_eq!(kinds(r#""\${x}""#), vec![String("${x}".into())]);
        assert_eq!(kinds(r#"'$'"#), vec![Char('$')]);
        // A hole's closing brace is not fooled by one in a character literal,
        // nor a name's apostrophe by one.
        assert_eq!(
            kinds(r#""${f '}' xs'}""#),
            vec![
                InterpStart("".into()),
                LowerIdent("f".into()),
                Char('}'),
                LowerIdent("xs'".into()),
                InterpEnd("".into()),
            ]
        );
    }

    #[test]
    fn the_pieces_and_holes_cover_the_literal() {
        let src = r#"f "a ${x} b" y"#;
        let res = tokenize(Source::new(SourceKind::Interactive, src.into()));
        let spans: Vec<&str> = res
            .tokens
            .iter()
            .map(|t| &src[t.span.start as usize..t.span.end as usize])
            .collect();
        assert_eq!(spans, vec!["f", "\"a ${", "x", "} b\"", "y"]);
    }

    #[test]
    fn the_usual_escapes() {
        use Token::*;
        assert_eq!(
            kinds(r#""\n\r\t\0\\\"\'\$ \a\b\f\v\e \x41\x7f \u{e9}\u{1F600}""#),
            vec![String(
                "\n\r\t\0\\\"'$ \x07\x08\x0C\x0B\x1B A\x7F \u{e9}\u{1F600}".into()
            )]
        );
        assert_eq!(
            kinds(r#"'\u{1F600}' '\x41' '\e' '\''"#),
            vec![Char('\u{1F600}'), Char('A'), Char('\x1B'), Char('\'')]
        );
    }

    #[test]
    fn a_backslash_ending_a_line_joins_it_to_the_next() {
        assert_eq!(
            kinds("\"one \\\n      two\""),
            vec![Token::String("one two".into())]
        );
    }

    #[test]
    fn a_bad_escape_is_an_error_not_a_backslash() {
        for src in [
            r#""\q""#,
            r#""\x""#,
            r#""\xFF""#,
            r#""\u41""#,
            r#""\u{}""#,
            r#""\u{110000}""#,
            r#""\u{D800}""#,
            r#""\u{41""#,
            r#"'\q'"#,
        ] {
            let res = tokenize(Source::new(SourceKind::Interactive, src.into()));
            assert!(!res.errors.is_empty(), "{src} should not lex cleanly");
        }
    }

    #[test]
    fn raw_strings_take_their_text_as_it_is() {
        use Token::*;
        assert_eq!(
            kinds(r##"r"C:\path\${x}" r#"say "hi""# r"""##),
            vec![
                String(r"C:\path\${x}".into()),
                String(r#"say "hi""#.into()),
                String("".into()),
            ]
        );
        assert_eq!(
            kinds("r##\"a \"# still\nin\"##"),
            vec![String("a \"# still\nin".into())]
        );
        // `r` on its own, or in a name, is a name.
        assert_eq!(
            kinds(r#"r x bar"a" r #[1]"#),
            vec![
                LowerIdent("r".into()),
                LowerIdent("x".into()),
                LowerIdent("bar".into()),
                String("a".into()),
                LowerIdent("r".into()),
                Hash,
                LBrack,
                Int(1),
                RBrack,
            ]
        );
        // A raw string inside a hole does not end the hole early.
        assert_eq!(
            kinds(r#""<${r"}"}>""#),
            vec![
                InterpStart("<".into()),
                String("}".into()),
                InterpEnd(">".into()),
            ]
        );
        let res = tokenize(Source::new(SourceKind::Interactive, r##"r#"open"##.into()));
        assert!(!res.errors.is_empty());
    }

    #[test]
    fn broken_strings_are_reported() {
        for src in [r#""never closed"#, r#""${x""#, r#""${}""#] {
            let res = tokenize(Source::new(SourceKind::Interactive, src.into()));
            assert!(!res.errors.is_empty(), "{src}");
        }
    }

    #[test]
    fn record_is_a_keyword() {
        assert_eq!(kinds("record"), vec![Token::Record]);
    }

    #[test]
    fn invalid_character_reports_an_error() {
        let res = tokenize(Source::new(SourceKind::Interactive, "a \0 b".into()));
        assert!(!res.errors.is_empty());
    }
}
