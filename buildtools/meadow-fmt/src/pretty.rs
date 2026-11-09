//! **Printing a file from its tokens** -- what `meadow fmt` does.
//!
//! The file is read into tokens, the tokens into the shapes the language
//! has -- a declaration, a `let … in`, a `match` and its arms, a function
//! and its arguments, a bracketed list -- and each shape into a [`Doc`] that
//! says where it may be cut. [`crate::doc::print`] lays that out. Where the
//! author ended a line is not read at all, so the same tokens come out the
//! same however they were typed in, and formatting what was formatted
//! changes nothing.
//!
//! Three things are kept from the source, because the tokens do not say
//! them:
//!
//! - **comments**, each with the token it is before, or after on its line;
//! - **empty lines** between declarations, the steps of a `let`, the arms of
//!   a `match` and the items of a block, one at most;
//! - **what starts a line inside a macro call.** `lang! { … }` and its kind
//!   hold languages of their own, with a rule to a line and nothing between
//!   two rules but the end of the line. So in a macro's body a line starts a
//!   new entry if it starts with `|` or is indented no further than the
//!   entry before it, and is otherwise more of that entry; an entry is then
//!   laid out as anything else is.
//!
//! A declaration is what starts in column 0, as it is to the lexer.
//!
//! No token is added, dropped or reordered, and two that touch are not
//! parted, nor two that are apart joined: `a+b` stays as it is, and so does
//! `f -1`. An operator is laid out as one -- a space each side, a line cut
//! at it -- where it was written with space on both sides.
//!
//! This reads shapes, not the grammar: it has to print a file that does not
//! parse, and the inside of a macro call, which is not Meadow. What it does
//! not recognise it prints as words with spaces between.

use crate::doc::{Doc, align, broken, cat, group, nest, small, text};

/// How wide a `match` and its arms, a `let` and its body, or a `data` and
/// its variants may be and still be one line.
const SMALL: usize = 60;

const IN: u8 = 1;
const THEN: u8 = 2;
const ELSE: u8 = 4;
const WITH: u8 = 8;
const BAR: u8 = 16;

const NONE: usize = usize::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Word,
    /// A number, a string or a character.
    Literal,
    Op,
    Open,
    Close,
    /// `,` or `;`
    Comma,
    Punct,
}

struct Tok<'a> {
    text: &'a str,
    kind: Kind,
    /// Nothing is between it and the token before.
    tight: bool,
    /// It is the first token on its line.
    first: bool,
    col: usize,
    /// How far the line it is on is indented.
    indent: usize,
    /// There is an empty line before it, after its comments.
    blank: bool,
    /// The comments on lines of their own before it, each with whether an
    /// empty line is before it.
    lead: Vec<(bool, &'a str)>,
    /// The comment after it on its line.
    trail: Option<&'a str>,
    /// The bracket that closes or opens it.
    mate: usize,
}

/// Where the string literal that opens at `i` ends.
fn string_end(src: &str, i: usize) -> Option<usize> {
    let b = src.as_bytes();
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            b'$' if b.get(j + 1) == Some(&b'{') => j = hole_end(src, j + 2)?,
            _ => j += 1,
        }
    }
    None
}

/// Where the `${…}` whose inside starts at `i` ends.
fn hole_end(src: &str, i: usize) -> Option<usize> {
    let b = src.as_bytes();
    let (mut j, mut depth) = (i, 1usize);
    while j < b.len() {
        match b[j] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            b'"' => {
                j = string_end(src, j)?;
                continue;
            }
            b'\'' => {
                if let Some(end) = char_end(src, j) {
                    j = end;
                    continue;
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Where the character literal that opens at `i` ends, if it is one.
fn char_end(src: &str, i: usize) -> Option<usize> {
    let rest = &src[i + 1..];
    let inner = if rest.starts_with("\\u{") {
        rest.find('}')? + 1
    } else if rest.starts_with("\\x") {
        2 + rest[2..]
            .bytes()
            .take(2)
            .take_while(|c| *c != b'\'')
            .count()
    } else if rest.starts_with('\\') {
        1 + rest[1..].chars().next()?.len_utf8()
    } else {
        let c = rest.chars().next()?;
        if c == '\'' || c == '\n' {
            return None;
        }
        c.len_utf8()
    };
    rest[inner..].starts_with('\'').then_some(i + 1 + inner + 1)
}

/// Where the raw string whose `r` is at `i` ends, if it is one.
fn raw_end(src: &str, i: usize) -> Option<usize> {
    let rest = &src[i + 1..];
    let hashes = rest.bytes().take_while(|c| *c == b'#').count();
    if !rest[hashes..].starts_with('"') {
        return None;
    }
    let body = i + 1 + hashes + 1;
    let close = format!("\"{}", "#".repeat(hashes));
    Some(body + src[body..].find(&close)? + close.len())
}

fn is_op(c: u8) -> bool {
    b"!%&*+./<=>?@|^~:-".contains(&c)
}

/// The tokens of `src` and the comments after the last, or nothing where a
/// string does not end or the brackets do not match.
#[allow(clippy::type_complexity)]
fn scan(src: &str) -> Option<(Vec<Tok<'_>>, Vec<(bool, &str)>)> {
    let b = src.as_bytes();
    let mut toks: Vec<Tok<'_>> = Vec::new();
    let mut pending: Vec<(bool, &str)> = Vec::new();
    let (mut i, mut newlines, mut spaced, mut line_start) = (0usize, 0usize, false, 0usize);
    let mut commented = false;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            newlines += 1;
            i += 1;
            line_start = i;
            spaced = true;
            continue;
        }
        if c == b' ' || c == b'\t' || c == b'\r' {
            spaced = true;
            i += 1;
            continue;
        }
        if src[i..].starts_with("--") {
            let end = src[i..].find('\n').map_or(src.len(), |n| i + n);
            let comment = src[i..end].trim_end();
            match toks.last_mut() {
                Some(last) if newlines == 0 && !commented => last.trail = Some(comment),
                _ => pending.push((newlines >= 2, comment)),
            }
            commented = true;
            newlines = 0;
            spaced = true;
            i = end;
            continue;
        }
        let start = i;
        let (end, kind) = match c {
            b'"' => (string_end(src, i)?, Kind::Literal),
            b'r' if raw_end(src, i).is_some() => (raw_end(src, i)?, Kind::Literal),
            b'\'' => match char_end(src, i) {
                Some(end) => (end, Kind::Literal),
                None => (i + 1, Kind::Punct),
            },
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let n = b[i..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_' || **c == b'\'')
                    .count();
                (i + n, Kind::Word)
            }
            b'0'..=b'9' => {
                let word = |from: usize| {
                    b[from..]
                        .iter()
                        .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
                        .count()
                };
                let mut end = i + word(i);
                if b.get(end) == Some(&b'.') && b.get(end + 1).is_some_and(u8::is_ascii_digit) {
                    end += 1;
                    end += word(end);
                }
                (end, Kind::Literal)
            }
            b'(' | b'[' | b'{' => (i + 1, Kind::Open),
            b')' | b']' | b'}' => (i + 1, Kind::Close),
            b',' | b';' => (i + 1, Kind::Comma),
            c if is_op(c) => (
                i + b[i..].iter().take_while(|c| is_op(**c)).count(),
                Kind::Op,
            ),
            _ => (i + src[i..].chars().next()?.len_utf8(), Kind::Punct),
        };
        let line = &src[line_start..start];
        toks.push(Tok {
            text: &src[start..end],
            kind,
            tight: !spaced && !toks.is_empty(),
            first: newlines > 0 || toks.is_empty(),
            col: line.chars().count(),
            indent: line.chars().take_while(|c| *c == ' ' || *c == '\t').count(),
            blank: newlines >= 2,
            // A comment before a comma is before what the comma is before.
            lead: if kind == Kind::Comma {
                Vec::new()
            } else {
                std::mem::take(&mut pending)
            },
            trail: None,
            mate: NONE,
        });
        if let Some(at) = src[start..end].rfind('\n') {
            line_start = start + at + 1;
        }
        (newlines, spaced, commented) = (0, false, false);
        i = end;
    }
    let mut open: Vec<usize> = Vec::new();
    for j in 0..toks.len() {
        match toks[j].kind {
            Kind::Open => open.push(j),
            Kind::Close => {
                let o = open.pop()?;
                let pair = (toks[o].text, toks[j].text);
                if !matches!(pair, ("(", ")") | ("[", "]") | ("{", "}")) {
                    return None;
                }
                toks[o].mate = j;
                toks[j].mate = o;
            }
            _ => {}
        }
    }
    open.is_empty().then_some((toks, pending))
}

/// The words that begin a declaration.
const DECLARES: &[&str] = &[
    "fun", "def", "data", "record", "effect", "type", "trait", "impl", "use", "mod", "macro",
    "infix", "infixl", "infixr",
];

enum Elem {
    Operand(Doc),
    Op(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    /// `a, b`: the line ends after it.
    Sep,
    /// `a =` and what it is on the next line, further in.
    Trailing,
    /// `a` and `+ b` on the next line, further in.
    Leading,
}

const TOP: u8 = 12;

fn tier(op: &str) -> (u8, Style) {
    match op {
        "," | ";" => (0, Style::Sep),
        // Below `=`, so that a rule's alternatives cut as a macro's body
        // reads them back: the rule and its first on one line, and a line
        // to each `|` after.
        "|" => (1, Style::Leading),
        "=" | ":=" | "<-" => (2, Style::Trailing),
        ":" => (3, Style::Leading),
        "->" | "=>" => (4, Style::Trailing),
        "!" => (5, Style::Leading),
        "|>" | "<|" | "$" | ">>=" | ">>" => (6, Style::Leading),
        "or" | "||" => (7, Style::Leading),
        "and" | "&&" => (8, Style::Leading),
        "==" | "!=" | "/=" | "<" | ">" | "<=" | ">=" => (9, Style::Leading),
        "+" | "-" => (11, Style::Leading),
        "*" | "/" | "%" => (TOP, Style::Leading),
        _ => (10, Style::Leading),
    }
}

struct Printer<'a> {
    src: &'a str,
    toks: Vec<Tok<'a>>,
    /// The declaration read next is left as it is written: `@fmt(skip)` was
    /// on a line of its own before it.
    skip: bool,
    /// The token being read, and where what is being read ends.
    i: usize,
    end: usize,
    ids: u32,
    /// How many macro calls this is inside.
    macros: usize,
    /// Whose comments are written already.
    led: Vec<bool>,
    /// What is read next is all there is in a pair of brackets, which is
    /// where a `let … in` may be one line.
    alone: bool,
    /// What is read is the head of a declaration: its name and what it
    /// takes, which are one line or a line each.
    heading: bool,
    /// Whether what is being read is in a trait's or an `impl`'s block.
    member: bool,
}

/// `src` laid out within `width` columns, or nothing where its brackets do
/// not match or a string in it does not end: a file in the middle of being
/// written, which has no shape to print.
pub fn pretty(src: &str, width: usize) -> Option<String> {
    let (toks, after) = scan(src)?;
    let mut p = Printer {
        led: vec![false; toks.len()],
        end: toks.len(),
        src,
        toks,
        skip: false,
        i: 0,
        ids: 0,
        macros: 0,
        alone: false,
        heading: false,
        member: false,
    };
    let mut doc = p.file(&after);
    Some(crate::doc::print(&mut doc, width))
}

impl<'a> Printer<'a> {
    fn is(&self, j: usize, what: &str) -> bool {
        j < self.end && self.toks[j].text == what
    }

    /// Whether token `j` is an operator between two things: one written
    /// with space on both sides, or a word that is one.
    fn binop(&self, j: usize) -> bool {
        let t = &self.toks[j];
        match t.kind {
            Kind::Op => !t.tight && self.toks.get(j + 1).is_none_or(|next| !next.tight),
            Kind::Word => t.text == "and" || t.text == "or",
            _ => false,
        }
    }

    fn is_stop(&self, j: usize, stops: u8) -> bool {
        let t = &self.toks[j];
        let flag = match (t.kind, t.text) {
            (Kind::Word, "in") => IN,
            (Kind::Word, "then") => THEN,
            (Kind::Word, "else") => ELSE,
            (Kind::Word, "with") => WITH,
            (Kind::Op, "|") => BAR,
            _ => return false,
        };
        stops & flag != 0
    }

    /// Whether token `j` begins something that runs to the end of what it
    /// is in: a `let`, an `if`, a `match`, a `handle` or a function.
    fn opens_tail(&self, j: usize) -> bool {
        let t = &self.toks[j];
        match t.kind {
            Kind::Word => matches!(t.text, "let" | "if" | "match" | "handle"),
            Kind::Punct => t.text == "\\" && self.arrow_from(j + 1).is_some(),
            _ => false,
        }
    }

    /// The first `->` from `from` that is not inside brackets.
    fn arrow_from(&self, from: usize) -> Option<usize> {
        self.find(from, "->")
    }

    /// The first token from `from` that is `what`, outside any brackets.
    fn find(&self, from: usize, what: &str) -> Option<usize> {
        let mut j = from;
        while j < self.end {
            let t = &self.toks[j];
            if t.kind != Kind::Literal && t.text == what {
                return Some(j);
            }
            j = if t.kind == Kind::Open {
                t.mate + 1
            } else {
                j + 1
            };
        }
        None
    }

    fn blank_before(&self, j: usize) -> bool {
        let t = &self.toks[j];
        t.lead.first().map_or(t.blank, |c| c.0)
    }

    /// What ends the line before token `j`: an empty line too, where the
    /// source has one.
    fn line_before(&self, j: usize) -> Doc {
        if self.blank_before(j) {
            Doc::Blank
        } else {
            Doc::Hard
        }
    }

    /// The comments before token `j`, a line each.
    fn leads(&mut self, j: usize) -> Doc {
        if std::mem::replace(&mut self.led[j], true) {
            return cat(Vec::new());
        }
        let t = &self.toks[j];
        let mut out = Vec::new();
        for (k, (_, comment)) in t.lead.iter().enumerate() {
            out.push(text(*comment));
            let blank = t.lead.get(k + 1).map_or(t.blank, |next| next.0);
            out.push(if blank { Doc::Blank } else { Doc::Hard });
        }
        cat(out)
    }

    /// Token `j`, with the comments around it.
    fn t(&mut self, j: usize) -> Doc {
        let lead = self.leads(j);
        let t = &self.toks[j];
        let mut out = vec![lead, text(t.text)];
        if let Some(comment) = t.trail {
            out.push(Doc::Suffix(comment.to_string()));
        }
        cat(out)
    }

    /// Read `lo..hi` with `f`, and come back to where this was.
    fn within<R>(&mut self, lo: usize, hi: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        let (i, end) = (self.i, self.end);
        (self.i, self.end) = (lo, hi);
        let r = f(self);
        (self.i, self.end) = (i, end);
        r
    }

    // ----- a file, and what it declares -----------------------------------

    fn file(&mut self, after: &[(bool, &str)]) -> Doc {
        let n = self.toks.len();
        let mut starts = Vec::new();
        let mut j = 0;
        while j < n {
            let t = &self.toks[j];
            let continues = t.kind == Kind::Close
                || (t.kind == Kind::Op && !t.text.starts_with('@'))
                || matches!(t.text, "in" | "then" | "else" | "with");
            // What can only begin a declaration begins one wherever its line
            // starts: a file whose declarations were pushed in from the
            // margin -- as they were after a string that ran over lines --
            // gets them back.
            let declares = (t.kind == Kind::Word
                && matches!(
                    t.text,
                    "use"
                        | "data"
                        | "record"
                        | "effect"
                        | "trait"
                        | "impl"
                        | "mod"
                        | "macro"
                        | "infix"
                        | "infixl"
                        | "infixr"
                ))
                || (self.attr_at(j) && self.toks.get(j + 1).is_some_and(|n| n.kind == Kind::Word));
            if j == 0 || (t.first && ((t.col == 0 && !continues) || declares)) {
                starts.push(j);
            }
            j = if t.kind == Kind::Open {
                t.mate + 1
            } else {
                j + 1
            };
        }
        let mut out = Vec::new();
        for (k, &lo) in starts.iter().enumerate() {
            let hi = starts.get(k + 1).copied().unwrap_or(n);
            if k > 0 {
                out.push(self.line_before(lo));
            }
            out.push(self.leads(lo));
            out.push(self.item(lo, hi));
        }
        for (k, (blank, comment)) in after.iter().enumerate() {
            if k > 0 || n > 0 {
                out.push(if *blank { Doc::Blank } else { Doc::Hard });
            }
            out.push(text(*comment));
        }
        cat(out)
    }

    /// The declaration `lo..hi`, laid out -- or as it is written, where
    /// `@fmt(skip)` is on it or on a line of its own before it, as
    /// `#[rustfmt::skip]` is on an item: for a table that reads as one, a
    /// row to a line, whatever the width. A `macro` is always as it is
    /// written: its rules are templates of what they expand to, declarations
    /// as often as an expression, whose shape the tokens do not show.
    fn item(&mut self, lo: usize, hi: usize) -> Doc {
        let (asks, alone, word) = self.skips(lo, hi);
        let skip = std::mem::take(&mut self.skip) || asks;
        if alone {
            // Attributes and nothing else: the declaration is what follows.
            self.skip = skip;
        } else if skip || self.toks[word].text == "macro" {
            return self.written(lo, hi);
        }
        self.within(lo, hi, |p| p.decl())
    }

    /// Whether `lo..hi` starts with an `@fmt(skip)` among its attributes,
    /// whether attributes are all it is, and where what they are on starts.
    fn skips(&self, lo: usize, hi: usize) -> (bool, bool, usize) {
        let (mut j, mut asks) = (lo, false);
        while j < hi && self.attr_at(j) {
            let named = |k: usize, what: &str| k < hi && self.toks[k].text == what;
            let args = j + 2 < hi && self.toks[j + 2].kind == Kind::Open && self.toks[j + 2].tight;
            if named(j, "@") && named(j + 1, "fmt") && args && named(j + 3, "skip") {
                asks = true;
            }
            j = if args {
                self.toks[j + 2].mate + 1
            } else {
                j + 2
            };
        }
        (asks, j >= hi, j)
    }

    /// Tokens `lo..hi` exactly as the source has them, comments and all.
    fn written(&mut self, lo: usize, hi: usize) -> Doc {
        let at = |t: &str| t.as_ptr() as usize - self.src.as_ptr() as usize;
        let last = &self.toks[hi - 1];
        let end = match last.trail {
            Some(comment) => at(comment) + comment.len(),
            None => at(last.text) + last.text.len(),
        };
        let start = at(self.toks[lo].text);
        for led in &mut self.led[lo..hi] {
            *led = true;
        }
        text(&self.src[start..end])
    }

    /// Whether token `j` is the `@` of an attribute.
    fn attr_at(&self, j: usize) -> bool {
        let t = &self.toks[j];
        t.kind == Kind::Op
            && t.text.starts_with('@')
            && self.toks.get(j + 1).is_some_and(|n| n.tight)
    }

    /// One declaration: everything from here to the end of what is read.
    fn decl(&mut self) -> Doc {
        let mut out = Vec::new();
        while self.i < self.end && self.attr_at(self.i) {
            out.push(self.glued().0);
            if self.i < self.end {
                out.push(text(" "));
            }
        }
        if self.i >= self.end {
            return cat(out);
        }
        let word = self.toks[self.i].text;
        let body = match word {
            "fun" | "def" | "type" => self.binding(),
            "data" => self.data(),
            // In a trait or an `impl`, what an associated effect is: a row,
            // not a block of fields.
            "effect" if self.member && self.find(self.i, "=").is_some() => self.binding(),
            "record" | "effect" => self.block(true),
            "trait" | "impl" | "mod" => {
                let was = std::mem::replace(&mut self.member, word != "mod");
                let d = self.block(false);
                self.member = was;
                d
            }
            "macro" => {
                self.macros += 1;
                let d = self.seq(2);
                self.macros -= 1;
                d
            }
            _ => self.seq(2),
        };
        out.push(body);
        cat(out)
    }

    /// `fun name params : type = body`, a `def` or a `type`. A `fun` or a
    /// `def` defined by clauses is one line where it fits, and has each
    /// clause on a line of its own, a step in, under its signature or its
    /// first clause where it does not:
    ///
    /// ```text
    /// fun gcd : Int -> Int -> Int
    ///   | gcd a 0 = a
    ///   | gcd a b = gcd b (a % b)
    /// ```
    fn binding(&mut self) -> Doc {
        let bars = match self.toks.get(self.i + 1) {
            Some(name) if matches!(self.toks[self.i].text, "fun" | "def") => {
                self.clauses(self.i + 2, name.text)
            }
            _ => Vec::new(),
        };
        let Some(&first) = bars.first() else {
            return self.plain_binding();
        };
        let end = self.end;
        let head = if self.within(self.i, first, |p| p.find(p.i, "=")).is_some() {
            self.within(self.i, first, |p| p.plain_binding())
        } else {
            self.heading = true;
            let d = self.within(self.i, first, |p| p.chain(0, 4, true));
            self.heading = false;
            d
        };
        let mut out = vec![head];
        for (k, &bar) in bars.iter().enumerate() {
            let hi = bars.get(k + 1).copied().unwrap_or(end);
            let bar_doc = self.t(bar);
            let clause = self.within(bar + 1, hi, |p| p.plain_binding());
            out.push(nest(2, cat(vec![Doc::Line, bar_doc, text(" "), clause])));
        }
        self.i = end;
        group(cat(out))
    }

    /// Where each clause of the binding named `name` starts, from `from`: a
    /// `|` outside brackets followed by the name, with an `=` before any
    /// `->`. A `match`'s arm has its arrow first.
    fn clauses(&self, from: usize, name: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut j = from;
        while let Some(bar) = self.find(j, "|") {
            if self.toks.get(bar + 1).is_some_and(|n| n.text == name) {
                let eq = self.find(bar + 1, "=");
                let arrow = self.find(bar + 1, "->");
                if eq.is_some_and(|e| arrow.is_none_or(|a| e < a)) {
                    out.push(bar);
                }
            }
            j = bar + 1;
        }
        out
    }

    /// A binding with no clauses after it: what is before its `=`, and its
    /// body.
    fn plain_binding(&mut self) -> Doc {
        let Some(eq) = self.find(self.i, "=") else {
            return self.seq(4);
        };
        self.heading = true;
        let head = self.within(self.i, eq, |p| p.chain(0, 4, true));
        self.heading = false;
        let eq_doc = self.t(eq);
        self.i = eq + 1;
        if self.i >= self.end {
            return cat(vec![head, text(" "), eq_doc]);
        }
        let first = self.leads(self.i);
        let body = self.seq(2);
        group(cat(vec![
            head,
            text(" "),
            eq_doc,
            nest(2, cat(vec![Doc::Line, first, body])),
        ]))
    }

    /// `data Name params = A | B`.
    fn data(&mut self) -> Doc {
        let Some(eq) = self.find(self.i, "=") else {
            return self.seq(4);
        };
        let head = self.within(self.i, eq, |p| p.chain(0, 4, true));
        let mut rest = vec![Doc::Line, self.t(eq)];
        let mut lo = eq + 1;
        loop {
            let hi = self.find(lo, "|").unwrap_or(self.end);
            if lo < hi {
                rest.push(text(" "));
                rest.push(self.leads(lo));
                rest.push(self.within(lo, hi, |p| p.chain(0, 4, true)));
            }
            if hi >= self.end {
                break;
            }
            rest.push(Doc::Line);
            rest.push(self.t(hi));
            lo = hi + 1;
        }
        self.i = self.end;
        small(cat(vec![head, nest(2, cat(rest))]), SMALL)
    }

    /// A declaration that ends in a block: a `record` or an `effect`, whose
    /// block is `fields`, or a `trait`, an `impl` or a `mod`, whose block
    /// is declarations. The block is a line to each thing in it, always.
    fn block(&mut self, fields: bool) -> Doc {
        let close = self.end - 1;
        let open = self.toks[close].mate;
        if self.toks[close].text != "}" || open == NONE || open < self.i {
            return self.seq(2);
        }
        let head = self.within(self.i, open, |p| p.words());
        let open_doc = self.t(open);
        let mut inside = Vec::new();
        if fields {
            let mut lo = open + 1;
            while lo < close {
                let sep = self.within(lo, close, |p| {
                    let mut j = lo;
                    while j < p.end && p.toks[j].kind != Kind::Comma {
                        j = if p.toks[j].kind == Kind::Open {
                            p.toks[j].mate + 1
                        } else {
                            j + 1
                        };
                    }
                    j
                });
                inside.push(if lo == open + 1 {
                    Doc::Hard
                } else {
                    self.line_before(lo)
                });
                inside.push(self.leads(lo));
                if lo < sep {
                    inside.push(self.within(lo, sep, |p| p.seq(2)));
                }
                if sep < close {
                    inside.push(self.t(sep));
                }
                lo = sep + 1;
            }
        } else {
            let starts = self.item_starts(open + 1, close);
            for (k, &lo) in starts.iter().enumerate() {
                let hi = starts.get(k + 1).copied().unwrap_or(close);
                inside.push(if k == 0 {
                    Doc::Hard
                } else {
                    self.line_before(lo)
                });
                inside.push(self.leads(lo));
                inside.push(self.item(lo, hi));
            }
        }
        let tail = self.closing_comments(close);
        let close_doc = self.t(close);
        self.i = self.end;
        if inside.is_empty() && tail.is_none() {
            return cat(vec![head, text(" "), open_doc, close_doc]);
        }
        inside.extend(tail);
        cat(vec![
            head,
            text(" "),
            open_doc,
            nest(2, cat(inside)),
            Doc::Hard,
            close_doc,
        ])
    }

    /// The comments before the bracket that closes at `close`, to go on
    /// lines of their own inside it.
    fn closing_comments(&mut self, close: usize) -> Option<Doc> {
        if self.led[close] || self.toks[close].lead.is_empty() {
            return None;
        }
        self.led[close] = true;
        let t = &self.toks[close];
        let mut out = Vec::new();
        for (blank, comment) in &t.lead {
            out.push(if *blank { Doc::Blank } else { Doc::Hard });
            out.push(text(*comment));
        }
        Some(cat(out))
    }

    /// Where each declaration in the block `lo..hi` starts: at a line that
    /// begins with what begins a declaration, indented no further than the
    /// first.
    fn item_starts(&self, lo: usize, hi: usize) -> Vec<usize> {
        let mut starts = Vec::new();
        if lo >= hi {
            return starts;
        }
        let base = self.toks[lo].indent;
        let mut j = lo;
        while j < hi {
            let t = &self.toks[j];
            let declares = (t.kind == Kind::Word && DECLARES.contains(&t.text))
                || self.attr_at(j)
                || (t.kind == Kind::Word
                    && self
                        .toks
                        .get(j + 1)
                        .is_some_and(|n| n.tight && n.text == "!"));
            if j == lo || (t.first && t.indent <= base && declares) {
                starts.push(j);
            }
            j = if t.kind == Kind::Open {
                t.mate + 1
            } else {
                j + 1
            };
        }
        starts
    }

    /// What is read as words with a space between, and no line cut: the
    /// head of a block.
    fn words(&mut self) -> Doc {
        let mut out = Vec::new();
        while self.i < self.end {
            let j = self.i;
            if self.toks[j].kind == Kind::Comma {
                out.push(self.t(j));
                self.i += 1;
                continue;
            }
            if !out.is_empty() {
                out.push(text(" "));
            }
            if self.binop(j) {
                out.push(self.t(j));
                self.i += 1;
            } else {
                out.push(self.glued().0);
            }
        }
        cat(out)
    }

    // ----- expressions ----------------------------------------------------

    /// Everything from here to the end of what is read.
    fn seq(&mut self, indent: usize) -> Doc {
        let mut out = Vec::new();
        while self.i < self.end {
            if !out.is_empty() {
                out.push(text(" "));
            }
            let before = self.i;
            out.push(self.expr_at(0, indent));
            if self.i == before {
                out.push(self.t(before));
                self.i += 1;
            }
        }
        cat(out)
    }

    fn expr(&mut self, stops: u8) -> Doc {
        self.expr_at(stops, 2)
    }

    fn expr_at(&mut self, stops: u8, indent: usize) -> Doc {
        let alone = std::mem::take(&mut self.alone);
        if self.i >= self.end {
            return cat(Vec::new());
        }
        let j = self.i;
        if !self.opens_tail(j) {
            return self.chain(stops, indent, false);
        }
        match self.toks[j].text {
            "let" => self.let_in(stops, alone),
            "if" => self.if_then(stops),
            "match" => self.match_with(stops),
            "handle" => self.handle(stops),
            _ => self.lambda(stops),
        }
    }

    /// Operands and the operators between them, as far as a stop.
    fn chain(&mut self, stops: u8, indent: usize, plain: bool) -> Doc {
        let mut elems = Vec::new();
        while self.i < self.end && !self.is_stop(self.i, stops) {
            let j = self.i;
            if self.toks[j].kind == Kind::Comma || self.binop(j) {
                elems.push(Elem::Op(j));
                self.i += 1;
            } else if !plain && self.opens_tail(j) {
                elems.push(Elem::Operand(self.expr(stops)));
            } else {
                elems.push(Elem::Operand(self.app(stops, plain)));
            }
        }
        self.build(elems, 0, indent)
    }

    /// `elems` cut at the operators that bind least, each part cut the same
    /// way at those that bind next.
    fn build(&mut self, mut elems: Vec<Elem>, level: u8, indent: usize) -> Doc {
        if elems.len() == 1 {
            return match elems.pop() {
                Some(Elem::Operand(d)) => d,
                Some(Elem::Op(j)) => self.t(j),
                None => cat(Vec::new()),
            };
        }
        let here = |p: &Self, e: &Elem| match e {
            Elem::Op(j) => tier(p.toks[*j].text).0 == level,
            Elem::Operand(_) => false,
        };
        if level > TOP {
            let mut out = Vec::new();
            for e in elems {
                if !out.is_empty() {
                    out.push(text(" "));
                }
                out.push(match e {
                    Elem::Operand(d) => d,
                    Elem::Op(j) => self.t(j),
                });
            }
            return cat(out);
        }
        if !elems.iter().any(|e| here(self, e)) {
            return self.build(elems, level + 1, indent);
        }
        let mut parts: Vec<Vec<Elem>> = vec![Vec::new()];
        let mut ops = Vec::new();
        for e in elems {
            if here(self, &e) {
                if let Elem::Op(j) = e {
                    ops.push(j);
                }
                parts.push(Vec::new());
            } else if let Some(last) = parts.last_mut() {
                last.push(e);
            }
        }
        let style = tier(self.toks[ops[0]].text).1;
        let mut docs: Vec<Option<Doc>> = Vec::new();
        for part in parts {
            docs.push(if part.is_empty() {
                None
            } else {
                Some(self.build(part, level + 1, indent))
            });
        }
        let mut docs = docs.into_iter();
        let mut head = Vec::new();
        let mut rest = Vec::new();
        let first = docs.next().flatten();
        let had_first = first.is_some();
        head.extend(first);
        for (k, (op, part)) in ops.into_iter().zip(docs).enumerate() {
            let op_doc = self.t(op);
            match style {
                Style::Sep => {
                    rest.push(op_doc);
                    if let Some(part) = part {
                        rest.push(Doc::Line);
                        rest.push(part);
                    }
                }
                Style::Trailing => {
                    if k > 0 || had_first {
                        rest.push(text(" "));
                    }
                    rest.push(op_doc);
                    if let Some(part) = part {
                        rest.push(Doc::Line);
                        rest.push(part);
                    }
                }
                Style::Leading => {
                    if k > 0 || had_first {
                        rest.push(Doc::Line);
                    }
                    rest.push(op_doc);
                    if let Some(part) = part {
                        rest.push(text(" "));
                        rest.push(part);
                    }
                }
            }
        }
        head.push(nest(indent, cat(rest)));
        group(cat(head))
    }

    /// A thing and what it is applied to, as far as an operator or a stop.
    fn app(&mut self, stops: u8, plain: bool) -> Doc {
        let mut args: Vec<Doc> = Vec::new();
        let mut hug = false;
        while self.i < self.end
            && !self.is_stop(self.i, stops)
            && self.toks[self.i].kind != Kind::Comma
            && !self.binop(self.i)
        {
            if !plain && !args.is_empty() && self.opens_tail(self.i) {
                args.push(self.expr(stops));
                hug = true;
                break;
            }
            let (d, h) = self.glued();
            args.push(d);
            hug = h;
        }
        let Some(last) = args.pop() else {
            return cat(Vec::new());
        };
        if args.is_empty() {
            return last;
        }
        if self.heading {
            args.push(last);
            let mut args = args.into_iter();
            // As rustfmt has a signature: on one line, or the name on the
            // first and everything it takes a line each.
            let mut out = Vec::new();
            out.extend(args.next());
            if let Some(name) = args.next() {
                out.push(text(" "));
                out.push(name);
            }
            let more: Vec<Doc> = args.flat_map(|a| [Doc::Line, a]).collect();
            out.push(nest(4, cat(more)));
            return group(cat(out));
        }
        let mut args = args.into_iter();
        let mut init = Vec::new();
        init.extend(args.next());
        let more: Vec<Doc> = args.flat_map(|a| [Doc::Line, a]).collect();
        init.push(nest(2, cat(more)));
        Doc::Hug {
            init: Box::new(cat(init)),
            last: Box::new(last),
            hug,
            indent: 2,
            split: false,
        }
    }

    /// One thing and whatever touches it: `Ffi.open`, `@pub`, `f(x)`,
    /// `lang!`. With whether it is a bracket that may be laid out from the
    /// end of the line it opens on.
    fn glued(&mut self) -> (Doc, bool) {
        let mut out = Vec::new();
        let mut hug;
        loop {
            let j = self.i;
            let t = &self.toks[j];
            if t.kind == Kind::Open && t.mate != NONE && t.mate < self.end {
                let (d, h) = self.bracket(j);
                hug = h && out.is_empty();
                out.push(d);
            } else {
                hug = false;
                out.push(self.t(j));
                self.i += 1;
            }
            if self.i >= self.end {
                break;
            }
            let (prev, next) = (&self.toks[self.i - 1], &self.toks[self.i]);
            let parted = next.text == "{" && matches!(prev.kind, Kind::Word | Kind::Close);
            if !next.tight
                || parted
                || next.kind == Kind::Comma
                || prev.trail.is_some()
                || self.is_stop(self.i, IN | THEN | ELSE | WITH)
            {
                break;
            }
        }
        (cat(out), hug)
    }

    /// The bracket that opens at `j` and what is in it.
    fn bracket(&mut self, j: usize) -> (Doc, bool) {
        let close = self.toks[j].mate;
        let brace = self.toks[j].text == "{";
        let called = j >= 2
            && self.toks[j - 1].text == "!"
            && self.toks[j - 1].tight
            && self.toks[j - 2].kind == Kind::Word;
        let open_doc = self.t(j);
        let done = |p: &mut Self, d: Doc, hug: bool| {
            p.i = close + 1;
            (d, hug)
        };
        if j + 1 == close {
            let tail = self.closing_comments(close);
            let close_doc = self.t(close);
            let mut out = vec![open_doc];
            if let Some(tail) = tail {
                out.push(nest(2, tail));
                out.push(Doc::Hard);
            }
            out.push(close_doc);
            return done(self, cat(out), false);
        }
        if called {
            self.macros += 1;
        }
        let mut seps = Vec::new();
        let mut k = j + 1;
        while k < close {
            let t = &self.toks[k];
            if t.kind == Kind::Comma {
                seps.push(k);
            }
            k = if t.kind == Kind::Open {
                t.mate + 1
            } else {
                k + 1
            };
        }
        let entries = if called || (self.macros > 0 && brace) {
            self.entry_starts(j + 1, close)
        } else {
            Vec::new()
        };
        // A macro's own body is its lines, commas in them or not: `lang! {
        // pub Core with In, set` and a rule to a line after it. Brackets
        // inside one are a list where they have commas, unless arms say
        // otherwise.
        let barred = entries.iter().any(|&e| self.toks[e].text == "|");
        let out = if entries.len() > 1 && (called || seps.is_empty() || barred) {
            (self.entries(open_doc, &entries, close), true)
        } else if seps.is_empty() {
            self.single(open_doc, j, close)
        } else {
            (self.list(open_doc, j, &seps, close), true)
        };
        if called {
            self.macros -= 1;
        }
        done(self, out.0, out.1)
    }

    /// A bracket with one thing in it.
    fn single(&mut self, open_doc: Doc, j: usize, close: usize) -> (Doc, bool) {
        let brace = self.toks[j].text == "{";
        // A function in brackets may start on the line of what it is
        // given to, and its body is then a step in from that line.
        let function = self.toks[j + 1].text == "\\" && self.opens_tail(j + 1);
        let lead = self.leads(j + 1);
        self.alone = !brace;
        let inner = self.within(j + 1, close, |p| p.seq(2));
        self.alone = false;
        let tail = self.closing_comments(close);
        let close_doc = self.t(close);
        if brace {
            let mut inside = vec![Doc::Line, lead, inner];
            inside.extend(tail);
            (
                group(cat(vec![
                    open_doc,
                    nest(2, cat(inside)),
                    Doc::Line,
                    close_doc,
                ])),
                true,
            )
        } else {
            let mut inside = vec![lead, inner];
            inside.extend(tail);
            // Brackets directly in brackets, `[(…)]`, are indented once.
            let nested = self.toks[j + 1].kind == Kind::Open && self.toks[j + 1].mate + 1 == close;
            let step = if function || nested { 0 } else { 1 };
            (
                group(cat(vec![open_doc, nest(step, cat(inside)), close_doc])),
                function,
            )
        }
    }

    /// A bracket with things between commas in it.
    fn list(&mut self, open_doc: Doc, j: usize, seps: &[usize], close: usize) -> Doc {
        let brace = self.toks[j].text == "{";
        let mut bounds = vec![j];
        bounds.extend_from_slice(seps);
        bounds.push(close);
        let ranges: Vec<(usize, usize)> = bounds.windows(2).map(|w| (w[0] + 1, w[1])).collect();
        if ranges.iter().all(|(lo, hi)| lo == hi) {
            let mut out = vec![open_doc];
            for &s in seps {
                out.push(self.t(s));
            }
            out.push(self.t(close));
            return cat(out);
        }
        // Many short things are run on, as many to a line as fit; anything
        // else is one to a line where one line does not hold them all.
        let short = ranges.len() > 6
            && ranges.iter().all(|&(lo, hi)| {
                hi - lo <= 3
                    && (lo..hi).all(|k| {
                        let t = &self.toks[k];
                        t.kind != Kind::Open && t.lead.is_empty() && t.trail.is_none()
                    })
            });
        let edge = || if brace { Doc::Line } else { Doc::Soft };
        let mut inside = vec![edge()];
        for (k, &(lo, hi)) in ranges.iter().enumerate() {
            let mut item = Vec::new();
            if lo < hi {
                item.push(self.leads(lo));
                item.push(self.within(lo, hi, |p| p.seq(2)));
            }
            if let Some(&s) = seps.get(k) {
                item.push(self.t(s));
            }
            if lo < hi && k > 0 {
                if short {
                    inside.push(Doc::Fit(Box::new(cat(item))));
                    continue;
                }
                inside.push(Doc::Line);
            }
            inside.extend(item);
        }
        inside.extend(self.closing_comments(close));
        let close_doc = self.t(close);
        group(cat(vec![open_doc, nest(2, cat(inside)), edge(), close_doc]))
    }

    /// Where each entry of a macro's body `lo..hi` starts: see the module.
    fn entry_starts(&self, lo: usize, hi: usize) -> Vec<usize> {
        let mut starts = vec![lo];
        let mut level = self.toks[lo].indent;
        let mut j = lo;
        while j < hi {
            let t = &self.toks[j];
            if j > lo && t.first {
                let bar = t.kind == Kind::Op && t.text == "|";
                let continues = t.kind == Kind::Op && !bar && !t.text.starts_with('@');
                if bar || (t.indent <= level && !continues) {
                    starts.push(j);
                    if !bar {
                        level = t.indent;
                    }
                }
            }
            j = if t.kind == Kind::Open {
                t.mate + 1
            } else {
                j + 1
            };
        }
        starts
    }

    /// A macro's body, an entry to a line.
    fn entries(&mut self, open_doc: Doc, starts: &[usize], close: usize) -> Doc {
        let mut inside = Vec::new();
        let mut headed = false;
        for (k, &lo) in starts.iter().enumerate() {
            let hi = starts.get(k + 1).copied().unwrap_or(close);
            let line = if k == 0 {
                Doc::Hard
            } else {
                self.line_before(lo)
            };
            let bar = self.toks[lo].text == "|";
            let lead = self.leads(lo);
            let entry = self.within(lo, hi, |p| if bar { p.arm(0) } else { p.seq(2) });
            let entry = cat(vec![line, lead, entry]);
            // The arms under a heading are a step in from it.
            inside.push(if bar && headed { nest(2, entry) } else { entry });
            headed |= !bar;
        }
        inside.extend(self.closing_comments(close));
        let close_doc = self.t(close);
        cat(vec![open_doc, nest(2, cat(inside)), Doc::Hard, close_doc])
    }

    /// `| pattern -> body`, from the `|`.
    fn arm(&mut self, stops: u8) -> Doc {
        let bar = self.t(self.i);
        self.i += 1;
        let arrow = self.arrow_from(self.i);
        match arrow {
            Some(a) => {
                let pattern = self.within(self.i, a, |p| p.chain(0, 4, true));
                let arrow_doc = self.t(a);
                self.i = a + 1;
                let first = if self.i < self.end {
                    self.leads(self.i)
                } else {
                    cat(Vec::new())
                };
                let body = if stops == 0 {
                    self.seq(2)
                } else {
                    self.expr(stops)
                };
                cat(vec![
                    bar,
                    text(" "),
                    pattern,
                    text(" "),
                    arrow_doc,
                    group(nest(4, cat(vec![Doc::Line, first, body]))),
                ])
            }
            None => {
                let rest = if stops == 0 {
                    self.seq(4)
                } else {
                    self.chain(stops, 4, false)
                };
                cat(vec![bar, text(" "), rest])
            }
        }
    }

    fn fresh(&mut self) -> u32 {
        self.ids += 1;
        self.ids
    }

    /// `let a = … in let b = … in body`: a step to a line, and the body
    /// after the last. One step that fits with its body is one line.
    fn let_in(&mut self, stops: u8, alone: bool) -> Doc {
        let mut out = Vec::new();
        let (mut steps, mut spaced) = (0, false);
        loop {
            let kw = self.t(self.i);
            self.i += 1;
            let eq = self
                .find(self.i, "=")
                .filter(|&e| self.find(self.i, "in").is_none_or(|i| e < i));
            let Some(eq) = eq else {
                out.push(kw);
                if self.i < self.end && !self.is_stop(self.i, stops) {
                    out.push(text(" "));
                    out.push(self.chain(stops, 2, false));
                }
                break;
            };
            let lhs = self.within(self.i, eq, |p| p.chain(0, 4, true));
            let eq_doc = self.t(eq);
            self.i = eq + 1;
            let first = if self.i < self.end {
                self.leads(self.i)
            } else {
                cat(Vec::new())
            };
            let value = self.expr(stops | IN);
            let mut step = vec![
                kw,
                text(" "),
                lhs,
                text(" "),
                eq_doc,
                nest(2, cat(vec![Doc::Line, first, value])),
            ];
            let closed =
                self.i < self.end && self.toks[self.i].kind == Kind::Word && self.is(self.i, "in");
            if closed {
                step.push(Doc::Line);
                step.push(self.t(self.i));
                self.i += 1;
            }
            let id = self.fresh();
            out.push(Doc::Group {
                id: Some(id),
                broken: false,
                doc: Box::new(cat(step)),
            });
            steps += 1;
            if !closed || self.i >= self.end || self.is_stop(self.i, stops) {
                break;
            }
            let blank = self.blank_before(self.i);
            spaced |= blank;
            // After an `in` on a line of its own, the body is on that line;
            // another step starts a line, as every step does.
            let commented = !self.toks[self.i].lead.is_empty() && !self.led[self.i];
            let step = self.toks[self.i].kind == Kind::Word && self.is(self.i, "let");
            let noted = self.toks[self.i - 1].trail.is_some();
            out.push(if commented || step || noted {
                self.line_before(self.i)
            } else {
                Doc::IfBreak {
                    id,
                    broken: Box::new(text(" ")),
                    flat: Box::new(if blank { Doc::Blank } else { Doc::Line }),
                }
            });
            if self.toks[self.i].kind == Kind::Word && self.is(self.i, "let") {
                out.push(self.leads(self.i));
                continue;
            }
            // After a lone `in`, the body starts mid-line: what it puts on
            // lines of its own goes under where it starts.
            out.push(self.leads(self.i));
            out.push(align(self.expr(stops)));
            break;
        }
        let d = cat(out);
        if steps >= 2 || spaced {
            broken(d)
        } else if alone {
            group(d)
        } else {
            small(d, SMALL)
        }
    }

    /// `if … then … else …`: the `then` on the line of its `if`, what it
    /// answers on that line or under it, and each `else` starting a line
    /// where one line does not hold it all.
    fn if_then(&mut self, stops: u8) -> Doc {
        let mut out = Vec::new();
        loop {
            out.push(self.t(self.i));
            self.i += 1;
            out.push(text(" "));
            out.push(self.expr(stops | THEN));
            if self.i < self.end && self.is(self.i, "then") && self.toks[self.i].kind == Kind::Word
            {
                out.push(text(" "));
                out.push(self.t(self.i));
                self.i += 1;
                let first = self.leads_here();
                let yes = self.expr(stops | ELSE);
                out.push(group(nest(2, cat(vec![Doc::Line, first, yes]))));
            }
            if self.i < self.end && self.is(self.i, "else") && self.toks[self.i].kind == Kind::Word
            {
                out.push(Doc::Line);
                out.push(self.t(self.i));
                self.i += 1;
                if self.i < self.end && self.is(self.i, "if") && self.toks[self.i].lead.is_empty() {
                    out.push(text(" "));
                    continue;
                }
                let first = self.leads_here();
                let no = self.expr(stops);
                out.push(group(nest(2, cat(vec![Doc::Line, first, no]))));
            }
            break;
        }
        group(cat(out))
    }

    /// The comments before the token being read, if there is one.
    fn leads_here(&mut self) -> Doc {
        if self.i < self.end {
            self.leads(self.i)
        } else {
            cat(Vec::new())
        }
    }

    /// `match … with` and its arms: an arm to a line, under the `match`.
    /// Two arms that fit on the line with it stay there.
    fn match_with(&mut self, stops: u8) -> Doc {
        let mut out = vec![self.t(self.i), text(" ")];
        self.i += 1;
        out.push(self.expr(stops | WITH));
        if self.i < self.end && self.is(self.i, "with") {
            out.push(text(" "));
            out.push(self.t(self.i));
            self.i += 1;
        }
        let (mut arms, mut spaced) = (0, false);
        while self.i < self.end && self.is(self.i, "|") && self.toks[self.i].kind == Kind::Op {
            let blank = arms > 0 && self.blank_before(self.i);
            spaced |= blank;
            out.push(if blank { Doc::Blank } else { Doc::Line });
            out.push(self.leads(self.i));
            out.push(self.arm(stops | BAR));
            arms += 1;
        }
        if arms == 0 && self.i < self.end && !self.is_stop(self.i, stops) {
            out.push(text(" "));
            out.push(self.chain(stops, 2, false));
        }
        let d = cat(out);
        if arms > 2 || spaced {
            broken(d)
        } else {
            small(d, SMALL)
        }
    }

    /// `handle … with { … } …`.
    fn handle(&mut self, stops: u8) -> Doc {
        let mut out = vec![self.t(self.i), text(" ")];
        self.i += 1;
        out.push(self.expr(stops | WITH));
        if self.i < self.end && self.is(self.i, "with") {
            out.push(text(" "));
            out.push(self.t(self.i));
            self.i += 1;
            let more = |p: &Self| {
                p.i < p.end
                    && !p.is_stop(p.i, stops)
                    && p.toks[p.i].kind != Kind::Comma
                    && !p.binop(p.i)
            };
            if more(self) {
                out.push(text(" "));
                out.push(self.glued().0);
            }
            if more(self) {
                out.push(text(" "));
                out.push(self.app(stops, false));
            }
        }
        cat(out)
    }

    /// `\params -> body`.
    fn lambda(&mut self, stops: u8) -> Doc {
        let Some(arrow) = self.arrow_from(self.i + 1) else {
            return self.chain(stops, 2, true);
        };
        let head = self.within(self.i, arrow, |p| p.words());
        let arrow_doc = self.t(arrow);
        self.i = arrow + 1;
        let first = if self.i < self.end {
            self.leads(self.i)
        } else {
            cat(Vec::new())
        };
        let body = self.expr(stops);
        group(cat(vec![
            head,
            text(" "),
            arrow_doc,
            nest(2, cat(vec![Doc::Line, first, body])),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::pretty;

    fn printed(src: &str) -> String {
        pretty(src, 100).expect("something to print")
    }

    /// `src` is printed as `want`, and `want` as itself.
    fn check(src: &str, want: &str) {
        assert_eq!(printed(src), want);
        assert_eq!(printed(want), want, "printing it again changed it");
    }

    #[test]
    fn the_same_tokens_come_out_the_same_however_they_were_typed() {
        let one_line = "fun declare (acc : Table) (o : Syntax) : Table = match find (\\x -> kindOf x == OpName) (nodes o) with | None -> acc | Just n -> let name = trim (text n) in if known name then (let u = say (errorAt (metaOf n) \"`${name}`-is-in-the-language-itself\") in acc) else insert name acc\n";
        // A token to a line, and each further in than the last.
        let mut scattered = String::new();
        for (k, word) in one_line.split(' ').enumerate() {
            scattered.push_str(&" ".repeat(if k == 0 { 0 } else { 1 + k % 7 }));
            scattered.push_str(word.trim_end());
            scattered.push('\n');
        }
        // Cut wherever forty columns ran out.
        let mut ragged = String::new();
        let mut col = 0;
        for word in one_line.split(' ') {
            if col + word.len() > 40 {
                ragged.push_str("\n    ");
                col = 4;
            } else if col > 0 {
                ragged.push(' ');
            }
            ragged.push_str(word.trim_end());
            col += word.len() + 1;
        }
        ragged.push('\n');
        let want = printed(one_line);
        assert!(want.lines().count() > 4, "{want}");
        assert_eq!(printed(&scattered), want);
        assert_eq!(printed(&ragged), want);
        assert_eq!(printed(&want), want);
    }

    #[test]
    fn nothing_is_added_dropped_or_reordered() {
        let bare = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        for src in SAMPLES {
            assert_eq!(bare(&printed(src)), bare(src));
        }
    }

    #[test]
    fn a_file_with_no_shape_is_not_printed() {
        // In the middle of being typed: the indenter has it instead.
        assert_eq!(pretty("fun f x = (a\n", 100), None);
        assert_eq!(pretty("fun f x = \"a\n", 100), None);
        assert_eq!(pretty("fun f x = (a]\n", 100), None);
        assert_eq!(
            crate::format_within("fun f x =\n(a\n", 100),
            crate::format("fun f x =\n(a\n")
        );
    }

    #[test]
    fn an_empty_file_stays_empty() {
        assert_eq!(printed(""), "");
        assert_eq!(printed("\n\n"), "");
        assert_eq!(printed("-- only this\n"), "-- only this\n");
    }

    // A definition and its body, a call and what it is given, a record and its
    // fields, an `if` and its branches: each is one line where one line holds it.
    #[test]
    fn what_fits_on_a_line_is_put_on_one() {
        check(
            r#"fun area
  (w : Int)
  (h : Int) =
  w
    * h

def origin =
  Point {
    x = 0,
    y = 0
  }

fun sign n =
  if n < 0
  then negative
  else positive
"#,
            r#"fun area (w : Int) (h : Int) = w * h

def origin = Point { x = 0, y = 0 }

fun sign n = if n < 0 then negative else positive
"#,
        );
    }

    // After the `=` before anything in the body; and in the body, a thing to
    // a line before anything inside one of them.
    #[test]
    fn a_line_too_long_is_cut_at_its_outermost_joint() {
        check(
            r#"fun total (orders : [Order]) : Int = V.foldl (\acc o -> acc + o.price * o.count) 0 (V.filter (\o -> o.count > 0) orders)

fun describe (orders : [Order]) : String = S.join ", " (V.map (\o -> "${show o.count} of ${o.name} at ${show o.price} each") (V.filter (\o -> o.count > 0) orders))
"#,
            r#"fun total (orders : [Order]) : Int =
  V.foldl (\acc o -> acc + o.price * o.count) 0 (V.filter (\o -> o.count > 0) orders)

fun describe (orders : [Order]) : String =
  S.join
    ", "
    (V.map
       (\o -> "${show o.count} of ${o.name} at ${show o.price} each")
       (V.filter (\o -> o.count > 0) orders))
"#,
        );
    }

    // As a closure does in Rust: the call stays on its line, and the function's
    // body is a step in from it.
    #[test]
    fn a_function_given_last_starts_on_the_line_of_what_it_is_given_to() {
        check(
            r#"fun bind (ar : Arena) (id : Int) (t : Type) = andThen (occursAdjust ar id t) (\() -> andThen (requireLacks ar t (lacksOf ar id)) (\() -> setSlot ar id (Solved t)))
"#,
            r#"fun bind (ar : Arena) (id : Int) (t : Type) =
  andThen (occursAdjust ar id t) (\() ->
    andThen (requireLacks ar t (lacksOf ar id)) (\() -> setSlot ar id (Solved t)))
"#,
        );
    }

    // Its brace on the line of what it belongs to, a field to a line, and the
    // closing brace back under the start of that line.
    #[test]
    fn a_record_is_set_out_as_rustfmt_sets_out_a_struct() {
        check(
            r#"fun lowered n = Decl.Record { name = someFunction n, fields = V.map (\f -> lowerField context f) (fieldsOf n), span = spanOf n }

record Point = { x : Int, y : Int }
"#,
            r#"fun lowered n =
  Decl.Record {
    name = someFunction n,
    fields = V.map (\f -> lowerField context f) (fieldsOf n),
    span = spanOf n
  }

record Point = {
  x : Int,
  y : Int
}
"#,
        );
    }

    // And the body the line after the last. A step whose value does not fit
    // has its `in` on a line of its own, and the body is then on that line.
    #[test]
    fn the_steps_of_a_let_have_a_line_each() {
        check(
            r#"fun steps () = let a = first 1 in let b = second a in a + b

fun long () = let table = V.foldl (\acc entry -> H.insert (fst entry) (snd entry) acc) H.empty (V.zip names (V.map lengthOf names)) in H.size table
"#,
            r#"fun steps () =
  let a = first 1 in
  let b = second a in
  a + b

fun long () =
  let table =
    V.foldl
      (\acc entry -> H.insert (fst entry) (snd entry) acc)
      H.empty
      (V.zip names (V.map lengthOf names))
  in H.size table
"#,
        );
    }

    #[test]
    fn a_let_in_brackets_is_one_line_where_it_fits() {
        check(
            r#"fun f x = if x then (let u = say "the one thing there is to say about it" in acc) else acc
"#,
            r#"fun f x = if x then (let u = say "the one thing there is to say about it" in acc) else acc
"#,
        );
    }

    // Two arms that are short stay on the line of their `match`, in brackets
    // or out; more than two, or any that make it long, have a line each.
    #[test]
    fn a_match_has_an_arm_to_a_line() {
        check(
            r#"fun isSome o = match o with | None -> False | Just x -> True

fun name f = match f with | Left -> "infixl" | Right -> "infixr" | Neither -> "infix"

fun parse text = match quick text with | Just v -> Ok v | None -> (match P.runParser document text with | Ok v -> Ok v | Err e -> Err (P.showError e))
"#,
            r#"fun isSome o = match o with | None -> False | Just x -> True

fun name f =
  match f with
  | Left -> "infixl"
  | Right -> "infixr"
  | Neither -> "infix"

fun parse text =
  match quick text with
  | Just v -> Ok v
  | None ->
      (match P.runParser document text with
       | Ok v -> Ok v
       | Err e -> Err (P.showError e))
"#,
        );
    }

    #[test]
    fn an_else_starts_a_line_and_a_then_stays_on_its_ifs() {
        check(
            r#"fun decode n text = if n == 0 then Closed else if n == 20 then Resized else if n == 21 then Pasted text else Key (codeOf n text)

fun skip s n i = if i < n then (let c = byteAt s i in if c == 32 or c == 10 or c == 13 or c == 9 then skipSpace s n (i + 1) else i) else i
"#,
            r#"fun decode n text =
  if n == 0 then Closed
  else if n == 20 then Resized
  else if n == 21 then Pasted text
  else Key (codeOf n text)

fun skip s n i =
  if i < n then
    (let c = byteAt s i in
     if c == 32 or c == 10 or c == 13 or c == 9 then skipSpace s n (i + 1) else i)
  else i
"#,
        );
    }

    // As rustfmt has a signature. A head that fits but for its type keeps
    // what it takes on its line, and the type starts the next.
    #[test]
    fn a_long_head_has_a_line_to_each_thing_it_takes() {
        check(
            r#"@pub fun lowered (pkg : String) (input : String) (tree : Green Grouped) (exps : HashMap String Json) (fx : HashMap String Fixity) : AstProgram = top

fun declareOp (acc : HashMap String Fixity) (f : Fixity) (o : Syntax Surface) : HashMap String Fixity ! { Report | e } = acc
"#,
            r#"@pub fun lowered
    (pkg : String)
    (input : String)
    (tree : Green Grouped)
    (exps : HashMap String Json)
    (fx : HashMap String Fixity)
    : AstProgram =
  top

fun declareOp (acc : HashMap String Fixity) (f : Fixity) (o : Syntax Surface)
    : HashMap String Fixity ! { Report | e } =
  acc
"#,
        );
    }

    #[test]
    fn clauses_have_a_line_each_under_what_they_define() {
        check(
            r#"fun gcd : Int -> Int -> Int | gcd a 0 = a | gcd a b = gcd b (a % b)

fun area (Shape.Circle r) = r * r | area (Shape.Square s) = s * s

def value : Int | value = 3

fun f x = match x with | f -> 1 | g -> 2

fun greatest : Int -> Int -> Int | greatest a 0 = a | greatest a b = greatest b (a % b) + nothing + more
"#,
            r#"fun gcd : Int -> Int -> Int | gcd a 0 = a | gcd a b = gcd b (a % b)

fun area (Shape.Circle r) = r * r | area (Shape.Square s) = s * s

def value : Int | value = 3

fun f x = match x with | f -> 1 | g -> 2

fun greatest : Int -> Int -> Int
  | greatest a 0 = a
  | greatest a b = greatest b (a % b) + nothing + more
"#,
        );
    }

    #[test]
    fn variants_have_a_line_each_when_they_are_many_or_long() {
        check(
            r#"data Ordering = Less | Equal | Greater

data Json = Null | Bool Bool | Int Int | Float Float | String String | Array [Json] | Object [(String, Json)]
"#,
            r#"data Ordering = Less | Equal | Greater

data Json
  = Null
  | Bool Bool
  | Int Int
  | Float Float
  | String String
  | Array [Json]
  | Object [(String, Json)]
"#,
        );
    }

    #[test]
    fn a_block_has_a_line_to_each_thing_in_it() {
        check(
            r#"effect State s { get : () -> s, put : s -> (), }

impl Display Bool { fun display b = if b then "True" else "False"
fun other x = x }
"#,
            r#"effect State s {
  get : () -> s,
  put : s -> (),
}

impl Display Bool {
  fun display b = if b then "True" else "False"
  fun other x = x
}
"#,
        );
    }

    // One on a line of its own before what it is before; one that ends a line
    // at the end of that line. Neither moves a token off its line that would
    // fit there, unless something follows the comment.
    #[test]
    fn comments_stay_with_what_they_are_on() {
        check(
            r#"-- The head.
fun f x = -- why
  match x with
  | A -> 1 -- one
  -- and the other
  | B -> 2

fun g () =
  let a = 1 in -- first
  [ a, -- the a
    a -- again
    -- and no more
  ]

fun h x = x -- the end
-- after everything
"#,
            r#"-- The head.
fun f x = -- why
  match x with
  | A -> 1 -- one
  -- and the other
  | B -> 2

fun g () =
  let a = 1 in -- first
  [
    a, -- the a
    a -- again
    -- and no more
  ]

fun h x = x -- the end
-- after everything
"#,
        );
    }

    #[test]
    fn one_empty_line_is_kept_where_there_were_some() {
        check(
            r#"fun a = 1



fun b =
  let x = 1 in


  let y = 2 in
  x + y
"#,
            r#"fun a = 1

fun b =
  let x = 1 in

  let y = 2 in
  x + y
"#,
        );
    }

    // What a macro reads is not Meadow, and a line there is a rule. A line is
    // a new entry if it starts with `|` or is no further in than the entry
    // before it, and each entry is laid out as anything is.
    #[test]
    fn a_macros_body_keeps_its_lines() {
        check(
            r#"lang! {
  pub Core extends Printed
  File + File { entry : Int,
     defs : [Def] }
  Term = Var | Lam
    | App
}

pass! {
  read : Printed -> Core
    | File { entry, defs } ->
        File { entry = number entry,
          defs = defs }
    | Def { id } -> Def { id = number id }
}
"#,
            r#"lang! {
  pub Core extends Printed
  File + File { entry : Int, defs : [Def] }
  Term = Var | Lam
    | App
}

pass! {
  read : Printed -> Core
    | File { entry, defs } -> File { entry = number entry, defs = defs }
    | Def { id } -> Def { id = number id }
}
"#,
        );
    }

    // An operator is laid out as one where it has a space each side; one
    // written against a neighbour is left against it.
    #[test]
    fn tokens_that_touch_stay_touching() {
        check(
            r#"fun tight xs = Ffi.open (a+b) (f -1) xs.len [;] #[1, 2] (x :: rest)

@pub(pkg) fun id x = x

use Maybe.*
"#,
            r#"fun tight xs = Ffi.open (a+b) (f -1) xs.len [;] #[1, 2] (x :: rest)

@pub(pkg) fun id x = x

use Maybe.*
"#,
        );
    }

    #[test]
    fn a_string_over_lines_is_left_as_it_is() {
        check(
            r#"fun text () = S.join "" ["one
  two   " ,  "three"]
"#,
            r#"fun text () = S.join "" ["one
  two   ", "three"]
"#,
        );
    }

    #[test]
    fn many_short_things_run_on() {
        check(
            r#"integer! { Int, BigInt, Int8, Int16, Int32, UInt8, UInt16, UInt32, UInt64, Float, Float32, Word8, Word16, Word32, Word64 }
"#,
            r#"integer! {
  Int, BigInt, Int8, Int16, Int32, UInt8, UInt16, UInt32, UInt64, Float, Float32, Word8, Word16,
  Word32, Word64
}
"#,
        );
    }

    // As `#[rustfmt::skip]` leaves an item: a table set out by hand stays so.
    #[test]
    fn a_declaration_marked_to_be_skipped_is_left_as_it_is_written() {
        let table = "@fmt(skip)\ndef table =\n  [ (\"a\",   1),   -- one\n    (\"bcd\", 22) ]\n";
        let src = format!(
            "fun  f  x =\n  x\n\n{table}\n@fmt(skip) def  g  =  [ 1,\n  2 ]\n\nfun  h  y =\n  y\n"
        );
        let want =
            format!("fun f x = x\n\n{table}\n@fmt(skip) def  g  =  [ 1,\n  2 ]\n\nfun h y = y\n");
        check(&src, &want);
    }

    #[test]
    fn a_macros_own_body_keeps_its_lines_though_one_has_a_comma() {
        check(
            "lang! {\n  pub Core extends Resolved with In, modules\n  Expr - Atom\n  Expr + Int { value : Int }\n}\n",
            "lang! {\n  pub Core extends Resolved with In, modules\n  Expr - Atom\n  Expr + Int { value : Int }\n}\n",
        );
    }

    // Left in from the margin by an earlier formatter, after a string that
    // ran over lines: what can only begin a declaration begins one.
    #[test]
    fn a_declaration_pushed_in_from_the_margin_comes_back() {
        check(
            "def text = \"one\ntwo\"\n        -- tests\n\n        use\n        Std.Test\n        (assertEq)\n        @test\n        fun\n        works\n        () =\n        assertEq 1 1 \"one\"\n",
            "def text = \"one\ntwo\"\n-- tests\n\nuse Std.Test (assertEq)\n@test fun works () = assertEq 1 1 \"one\"\n",
        );
    }

    // What an associated effect is, in an `impl`, is a row on its line; an
    // effect's alias at the top is a label a line, as it was.
    #[test]
    fn an_associated_effect_is_a_row_on_its_line() {
        check(
            "impl Rows (Writing s) {\n  effect Reading (Writing s) = {\n    St s\n  }\n  fun slot r i = get r i\n}\n\neffect Eff = { Console, Fs }\n",
            "impl Rows (Writing s) {\n  effect Reading (Writing s) = { St s }\n  fun slot r i = get r i\n}\n\neffect Eff = {\n  Console,\n  Fs\n}\n",
        );
    }

    const SAMPLES: &[&str] = &[
        r#"fun area
  (w : Int)
  (h : Int) =
  w
    * h

def origin =
  Point {
    x = 0,
    y = 0
  }

fun sign n =
  if n < 0
  then negative
  else positive
"#,
        r#"fun total (orders : [Order]) : Int = V.foldl (\acc o -> acc + o.price * o.count) 0 (V.filter (\o -> o.count > 0) orders)

fun describe (orders : [Order]) : String = S.join ", " (V.map (\o -> "${show o.count} of ${o.name} at ${show o.price} each") (V.filter (\o -> o.count > 0) orders))
"#,
        r#"fun bind (ar : Arena) (id : Int) (t : Type) = andThen (occursAdjust ar id t) (\() -> andThen (requireLacks ar t (lacksOf ar id)) (\() -> setSlot ar id (Solved t)))
"#,
        r#"fun lowered n = Decl.Record { name = someFunction n, fields = V.map (\f -> lowerField context f) (fieldsOf n), span = spanOf n }

record Point = { x : Int, y : Int }
"#,
        r#"fun steps () = let a = first 1 in let b = second a in a + b

fun long () = let table = V.foldl (\acc entry -> H.insert (fst entry) (snd entry) acc) H.empty (V.zip names (V.map lengthOf names)) in H.size table
"#,
        r#"fun f x = if x then (let u = say "the one thing there is to say about it" in acc) else acc
"#,
        r#"fun isSome o = match o with | None -> False | Just x -> True

fun name f = match f with | Left -> "infixl" | Right -> "infixr" | Neither -> "infix"

fun parse text = match quick text with | Just v -> Ok v | None -> (match P.runParser document text with | Ok v -> Ok v | Err e -> Err (P.showError e))
"#,
        r#"fun decode n text = if n == 0 then Closed else if n == 20 then Resized else if n == 21 then Pasted text else Key (codeOf n text)

fun skip s n i = if i < n then (let c = byteAt s i in if c == 32 or c == 10 or c == 13 or c == 9 then skipSpace s n (i + 1) else i) else i
"#,
        r#"@pub fun lowered (pkg : String) (input : String) (tree : Green Grouped) (exps : HashMap String Json) (fx : HashMap String Fixity) : AstProgram = top

fun declareOp (acc : HashMap String Fixity) (f : Fixity) (o : Syntax Surface) : HashMap String Fixity ! { Report | e } = acc
"#,
        r#"data Ordering = Less | Equal | Greater

data Json = Null | Bool Bool | Int Int | Float Float | String String | Array [Json] | Object [(String, Json)]
"#,
        r#"effect State s { get : () -> s, put : s -> (), }

impl Display Bool { fun display b = if b then "True" else "False"
fun other x = x }
"#,
        r#"-- The head.
fun f x = -- why
  match x with
  | A -> 1 -- one
  -- and the other
  | B -> 2

fun g () =
  let a = 1 in -- first
  [ a, -- the a
    a -- again
    -- and no more
  ]

fun h x = x -- the end
-- after everything
"#,
        r#"fun a = 1



fun b =
  let x = 1 in


  let y = 2 in
  x + y
"#,
        r#"lang! {
  pub Core extends Printed
  File + File { entry : Int,
     defs : [Def] }
  Term = Var | Lam
    | App
}

pass! {
  read : Printed -> Core
    | File { entry, defs } ->
        File { entry = number entry,
          defs = defs }
    | Def { id } -> Def { id = number id }
}
"#,
        r#"fun tight xs = Ffi.open (a+b) (f -1) xs.len [;] #[1, 2] (x :: rest)

@pub(pkg) fun id x = x

use Maybe.*
"#,
        r#"fun text () = S.join "" ["one
  two   " ,  "three"]
"#,
        r#"integer! { Int, BigInt, Int8, Int16, Int32, UInt8, UInt16, UInt32, UInt64, Float, Float32, Word8, Word16, Word32, Word64 }
"#,
    ];
}
