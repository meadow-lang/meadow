//! Cut text, read: the syntax `docs/CUT.md` gives, settled here.
//!
//! A program is its version line, `cut 0`, then its declarations and
//! definitions in any order. Whitespace separates nothing that punctuation
//! does not already, and `--` starts a comment that runs to the end of the
//! line.
//!
//! Two things are told apart by their spelling alone. A symbol is a name, a
//! `:`, a package and a `/` with no space between -- `idyll:App/double` --
//! where a binder is a name, a `:` and a representation: `n: i64`. And a
//! character literal closes its quote -- `'c'` -- where a representation
//! variable does not: `'a`.
//!
//! A symbol written as a producer is a constructor if the program declares
//! one by that name, and a top-level value otherwise: which is settled once
//! the whole program has been read, since declarations come in any order.

use crate::*;

/// Read `text` as a Cut program, or say where it stops making sense.
pub fn parse(text: &str) -> Result<Program, String> {
    let toks = lex(text)?;
    let mut p = Parser { toks, at: 0 };
    let mut program = p.program()?;
    resolve(&mut program)?;
    Ok(program)
}

// --- tokens ------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Sym(Symbol),
    Str(String),
    Char(char),
    Int(i64),
    Float(f64),
    /// `'a`: a representation variable, without its quote.
    RepVar(String),
    /// `#tuple`: a name of the IR's own, without its `#`.
    Hash(String),
    Mu,
    MuTilde,
    Punct(&'static str),
    Eof,
}

impl std::fmt::Display for Tok {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tok::Ident(x) => write!(f, "`{x}`"),
            Tok::Sym(s) => write!(f, "`{s}`"),
            Tok::Str(s) => write!(f, "{s:?}"),
            Tok::Char(c) => write!(f, "{c:?}"),
            Tok::Int(n) => write!(f, "`{n}`"),
            Tok::Float(x) => write!(f, "`{x}`"),
            Tok::RepVar(a) => write!(f, "`'{a}`"),
            Tok::Hash(x) => write!(f, "`#{x}`"),
            Tok::Mu => f.write_str("`μ`"),
            Tok::MuTilde => f.write_str("`μ̃`"),
            Tok::Punct(p) => write!(f, "`{p}`"),
            Tok::Eof => f.write_str("the end of the text"),
        }
    }
}

/// A token, and the line and column it starts at.
#[derive(Clone, Debug)]
struct Spanned {
    tok: Tok,
    line: usize,
    col: usize,
}

const PUNCTS: &[&str] = &[
    "=>", "->", "<", ">", "|", ";", ",", "(", ")", "{", "}", "[", "]", "=", ":", ".", "@", "!",
    "-", "_",
];

fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn lex(text: &str) -> Result<Vec<Spanned>, String> {
    let cs: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line, mut col) = (0, 1, 1);
    let advance = |i: &mut usize, line: &mut usize, col: &mut usize, n: usize, cs: &[char]| {
        for _ in 0..n {
            if cs[*i] == '\n' {
                *line += 1;
                *col = 1;
            } else {
                *col += 1;
            }
            *i += 1;
        }
    };
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            advance(&mut i, &mut line, &mut col, 1, &cs);
            continue;
        }
        if c == '-' && cs.get(i + 1) == Some(&'-') {
            while i < cs.len() && cs[i] != '\n' {
                advance(&mut i, &mut line, &mut col, 1, &cs);
            }
            continue;
        }
        let (l0, c0) = (line, col);
        let err = |msg: String| format!("{l0}:{c0}: {msg}");
        let (tok, len) = if c == 'μ' {
            if cs.get(i + 1) == Some(&'\u{303}') {
                (Tok::MuTilde, 2)
            } else {
                (Tok::Mu, 1)
            }
        } else if c == '"' {
            let (s, len) = string_at(&cs, i).map_err(&err)?;
            (Tok::Str(s), len)
        } else if c == '\'' {
            // `'c'` closes its quote; `'a` does not.
            match char_at(&cs, i) {
                Some((ch, len)) => (Tok::Char(ch), len),
                None => {
                    let mut j = i + 1;
                    while j < cs.len() && ident_char(cs[j]) {
                        j += 1;
                    }
                    if j == i + 1 {
                        return Err(err("a `'` with no name after it".into()));
                    }
                    (Tok::RepVar(cs[i + 1..j].iter().collect()), j - i)
                }
            }
        } else if c == '#' && cs.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
            let mut j = i + 1;
            while j < cs.len() && ident_char(cs[j]) {
                j += 1;
            }
            (Tok::Hash(cs[i + 1..j].iter().collect()), j - i)
        } else if c.is_ascii_digit() {
            number_at(&cs, i).map_err(&err)?
        } else if c.is_ascii_alphabetic()
            || c == '_' && cs.get(i + 1).is_some_and(|c| ident_char(*c))
        {
            let mut j = i;
            while j < cs.len() && ident_char(cs[j]) {
                j += 1;
            }
            let name: String = cs[i..j].iter().collect();
            match symbol_at(&cs, j, &name).map_err(&err)? {
                Some((sym, end)) => (Tok::Sym(sym), end - i),
                None => (Tok::Ident(name), j - i),
            }
        } else {
            let rest: String = cs[i..(i + 2).min(cs.len())].iter().collect();
            match PUNCTS.iter().find(|p| rest.starts_with(*p)) {
                Some(p) => (Tok::Punct(p), p.chars().count()),
                None => return Err(err(format!("unexpected character {c:?}"))),
            }
        };
        out.push(Spanned {
            tok,
            line: l0,
            col: c0,
        });
        advance(&mut i, &mut line, &mut col, len, &cs);
    }
    out.push(Spanned {
        tok: Tok::Eof,
        line,
        col,
    });
    Ok(out)
}

/// A string literal at `i`, unescaped, and how many characters it took.
fn string_at(cs: &[char], i: usize) -> Result<(String, usize), String> {
    let mut s = String::new();
    let mut j = i + 1;
    loop {
        match cs.get(j) {
            None => return Err("this string is never closed".into()),
            Some('"') => return Ok((s, j + 1 - i)),
            Some('\\') => {
                let (c, len) = escape_at(cs, j)?;
                s.push(c);
                j += len;
            }
            Some(c) => {
                s.push(*c);
                j += 1;
            }
        }
    }
}

/// The escape whose backslash is at `j`, and how many characters it took.
fn escape_at(cs: &[char], j: usize) -> Result<(char, usize), String> {
    Ok(match cs.get(j + 1) {
        Some('n') => ('\n', 2),
        Some('t') => ('\t', 2),
        Some('r') => ('\r', 2),
        Some('0') => ('\0', 2),
        Some('"') => ('"', 2),
        Some('\'') => ('\'', 2),
        Some('\\') => ('\\', 2),
        Some('u') if cs.get(j + 2) == Some(&'{') => {
            let close = (j + 3..cs.len())
                .find(|k| cs[*k] == '}')
                .ok_or("this `\\u{` is never closed")?;
            let hex: String = cs[j + 3..close].iter().collect();
            let code = u32::from_str_radix(&hex, 16)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| format!("`\\u{{{hex}}}` is not a character"))?;
            (code, close + 1 - j)
        }
        other => return Err(format!("`\\{}` is not an escape", other.unwrap_or(&' '))),
    })
}

/// A character literal at `i` -- `'c'`, `'\n'` -- if one closes there.
fn char_at(cs: &[char], i: usize) -> Option<(char, usize)> {
    match cs.get(i + 1)? {
        '\\' => {
            let (c, len) = escape_at(cs, i + 1).ok()?;
            (cs.get(i + 1 + len) == Some(&'\'')).then_some((c, len + 2))
        }
        c => (cs.get(i + 2) == Some(&'\'')).then_some((*c, 3)),
    }
}

/// An integer or a float at `i`.
fn number_at(cs: &[char], i: usize) -> Result<(Tok, usize), String> {
    let mut j = i;
    while j < cs.len() && cs[j].is_ascii_digit() {
        j += 1;
    }
    let float = cs.get(j) == Some(&'.') && cs.get(j + 1).is_some_and(|c| c.is_ascii_digit());
    if float {
        j += 1;
        while j < cs.len() && cs[j].is_ascii_digit() {
            j += 1;
        }
        if matches!(cs.get(j), Some('e' | 'E')) {
            let mut k = j + 1;
            if matches!(cs.get(k), Some('+' | '-')) {
                k += 1;
            }
            if cs.get(k).is_some_and(|c| c.is_ascii_digit()) {
                j = k;
                while j < cs.len() && cs[j].is_ascii_digit() {
                    j += 1;
                }
            }
        }
        let text: String = cs[i..j].iter().collect();
        let x = text
            .parse::<f64>()
            .map_err(|_| format!("`{text}` is not a number"))?;
        return Ok((Tok::Float(x), j - i));
    }
    let text: String = cs[i..j].iter().collect();
    // One past `i64::MAX`, for `-9223372036854775808`: the parser negates it.
    let n = text
        .parse::<i128>()
        .ok()
        .filter(|n| *n <= i64::MAX as i128 + 1)
        .ok_or_else(|| format!("`{text}` does not fit 64 bits"))?;
    Ok((Tok::Int(n as i64), j - i))
}

/// The symbol whose language is `lang`, ending just before `j`, if one
/// starts there: `:`, a package, `/`, and its path. Answers where it ends.
fn symbol_at(cs: &[char], j: usize, lang: &str) -> Result<Option<(Symbol, usize)>, String> {
    if cs.get(j) != Some(&':') {
        return Ok(None);
    }
    let pkg_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '.' | '-' | '+');
    let mut k = j + 1;
    while k < cs.len() && pkg_char(cs[k]) {
        k += 1;
    }
    if k == j + 1 || cs.get(k) != Some(&'/') {
        return Ok(None);
    }
    let package: String = cs[j + 1..k].iter().collect();
    let mut path = Vec::new();
    let mut at = k + 1;
    loop {
        let (seg, end) = match cs.get(at) {
            Some('"') => string_at(cs, at).map(|(s, len)| (s, at + len))?,
            Some(c) if ident_char(*c) => {
                let mut e = at;
                while e < cs.len() && ident_char(cs[e]) {
                    e += 1;
                }
                (cs[at..e].iter().collect(), e)
            }
            _ => return Err(format!("`{lang}:{package}/` needs a name after it")),
        };
        path.push(seg);
        at = end;
        // A `.` continues the path only when a segment follows it.
        let more =
            cs.get(at) == Some(&'.') && cs.get(at + 1).is_some_and(|c| *c == '"' || ident_char(*c));
        if !more {
            break;
        }
        at += 1;
    }
    Ok(Some((
        Symbol {
            lang: lang.to_string(),
            package,
            path,
        },
        at,
    )))
}

// --- the parser --------------------------------------------------------------------

struct Parser {
    toks: Vec<Spanned>,
    at: usize,
}

type R<T> = Result<T, String>;

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.at].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.at + n).min(self.toks.len() - 1)].tok
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.at].tok.clone();
        if self.at + 1 < self.toks.len() {
            self.at += 1;
        }
        t
    }

    /// Put back `t`, which [`Parser::next`] took -- unless it was the end,
    /// which `next` never moves past.
    fn back(&mut self, t: &Tok) {
        if *t != Tok::Eof {
            self.at -= 1;
        }
    }

    fn fail<T>(&self, what: &str) -> R<T> {
        let s = &self.toks[self.at];
        Err(format!(
            "{}:{}: expected {what}, found {}",
            s.line, s.col, s.tok
        ))
    }

    fn is_punct(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(q) if *q == p)
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Ident(x) if x == w)
    }

    fn punct(&mut self, p: &str) -> R<()> {
        if self.is_punct(p) {
            self.next();
            Ok(())
        } else {
            self.fail(&format!("`{p}`"))
        }
    }

    fn word(&mut self, w: &str) -> R<()> {
        if self.is_word(w) {
            self.next();
            Ok(())
        } else {
            self.fail(&format!("`{w}`"))
        }
    }

    fn eat(&mut self, p: &str) -> bool {
        let yes = self.is_punct(p);
        if yes {
            self.next();
        }
        yes
    }

    fn ident(&mut self) -> R<String> {
        match self.peek().clone() {
            Tok::Ident(x) => {
                self.next();
                Ok(x)
            }
            _ => self.fail("a name"),
        }
    }

    fn symbol(&mut self) -> R<Symbol> {
        match self.peek().clone() {
            Tok::Sym(s) => {
                self.next();
                Ok(s)
            }
            _ => self.fail("a symbol, such as `idyll:App/main`"),
        }
    }

    /// `x`, or a quoted name: a constructor's or an operation's own name.
    fn own_name(&mut self) -> R<String> {
        match self.peek().clone() {
            Tok::Ident(x) => {
                self.next();
                Ok(x)
            }
            Tok::Str(s) => {
                self.next();
                Ok(s)
            }
            _ => self.fail("a name"),
        }
    }

    // --- a program ---------------------------------------------------------------

    fn program(&mut self) -> R<Program> {
        self.word("cut")?;
        let version = match self.next() {
            Tok::Int(n) if n >= 0 => n as u32,
            _ => return self.fail("Cut's version, a number"),
        };
        if version != VERSION {
            return Err(format!(
                "this is Cut version {version}; this reader reads version {VERSION}"
            ));
        }
        let mut p = Program {
            version,
            entry: None,
            answer: Answer::None,
            exports: Vec::new(),
            datas: Vec::new(),
            roles: Vec::new(),
            natives: Vec::new(),
            effects: Vec::new(),
            vals: Vec::new(),
            defs: Vec::new(),
        };
        loop {
            match self.peek().clone() {
                Tok::Eof => break,
                Tok::Ident(w) => match w.as_str() {
                    "entry" => {
                        self.next();
                        p.entry = Some(self.symbol()?);
                    }
                    "answer" => {
                        self.next();
                        p.answer = match self.ident()?.as_str() {
                            "none" => Answer::None,
                            "str" => Answer::Str,
                            _ => return self.fail("`none` or `str`"),
                        };
                    }
                    "export" => p.exports.push(self.export()?),
                    "data" => p.datas.push(self.data()?),
                    "roles" => self.roles(&mut p.roles)?,
                    "native" => self.natives(&mut p.natives)?,
                    "effect" => p.effects.push(self.effect()?),
                    "val" => p.vals.push(self.val()?),
                    "def" => p.defs.push(self.def()?),
                    _ => return self.fail("a declaration or a definition"),
                },
                _ => return self.fail("a declaration or a definition"),
            }
        }
        Ok(p)
    }

    /// `export "c_name" = S -> rep`.
    fn export(&mut self) -> R<Export> {
        self.word("export")?;
        let name = match self.next() {
            Tok::Str(s) => s,
            _ => return self.fail("the name C calls it by, in quotes"),
        };
        self.punct("=")?;
        let symbol = self.symbol()?;
        self.punct("->")?;
        let result = self.rep()?;
        Ok(Export {
            name,
            symbol,
            result,
        })
    }

    fn data(&mut self) -> R<DataDecl> {
        self.word("data")?;
        let symbol = self.symbol()?;
        let mut rep_vars = Vec::new();
        if self.eat("<") {
            loop {
                match self.next() {
                    Tok::RepVar(a) => rep_vars.push(a),
                    _ => return self.fail("a representation variable, such as `'a`"),
                }
                if !self.eat(",") {
                    break;
                }
            }
            self.punct(">")?;
        }
        self.punct("{")?;
        let mut ctors = Vec::new();
        while !self.is_punct("}") {
            let name = self.own_name()?;
            let mut fields = Vec::new();
            if self.eat("(") {
                if !self.is_punct(")") {
                    loop {
                        fields.push(self.rep()?);
                        if !self.eat(",") {
                            break;
                        }
                    }
                }
                self.punct(")")?;
            }
            ctors.push((name, fields));
            if !self.eat(";") {
                break;
            }
        }
        self.punct("}")?;
        Ok(DataDecl {
            symbol,
            rep_vars,
            ctors,
        })
    }

    /// `none`, `vector-empty`: a role's name, which may have hyphens in it.
    fn role_name(&mut self) -> R<String> {
        let mut name = self.ident()?;
        while self.is_punct("-") && matches!(self.peek_at(1), Tok::Ident(_)) {
            self.next();
            name.push('-');
            name.push_str(&self.ident()?);
        }
        Ok(name)
    }

    fn roles(&mut self, out: &mut Vec<(String, Symbol)>) -> R<()> {
        self.word("roles")?;
        self.punct("{")?;
        while !self.is_punct("}") {
            let role = self.role_name()?;
            self.punct("=")?;
            let ctor = self.symbol()?;
            out.push((role, ctor));
            self.eat(",");
        }
        self.punct("}")
    }

    fn natives(&mut self, out: &mut Vec<(Symbol, String)>) -> R<()> {
        self.word("native")?;
        self.punct("{")?;
        while !self.is_punct("}") {
            let op = self.symbol()?;
            self.punct("=")?;
            let effect = self.ident()?;
            self.punct(".")?;
            let name = self.ident()?;
            out.push((op, format!("{effect}.{name}")));
            self.eat(",");
        }
        self.punct("}")
    }

    fn effect(&mut self) -> R<EffectDecl> {
        self.word("effect")?;
        let symbol = self.symbol()?;
        self.punct("{")?;
        let mut ops = Vec::new();
        while !self.is_punct("}") {
            let many = if self.eat("@") {
                self.word("many")?;
                true
            } else {
                false
            };
            let name = self.own_name()?;
            self.punct("(")?;
            let mut params = Vec::new();
            if !self.is_punct(")") {
                loop {
                    params.push(self.rep()?);
                    if !self.eat(",") {
                        break;
                    }
                }
            }
            self.punct(")")?;
            self.punct("->")?;
            let result = self.rep()?;
            ops.push(OpDecl {
                name,
                many,
                params,
                result,
            });
            if !self.eat(";") {
                break;
            }
        }
        self.punct("}")?;
        Ok(EffectDecl { symbol, ops })
    }

    fn val(&mut self) -> R<Val> {
        self.word("val")?;
        let symbol = self.symbol()?;
        self.punct(":")?;
        let rep = self.rep()?;
        self.punct("=")?;
        let body = self.statement()?;
        Ok(Val { symbol, rep, body })
    }

    fn def(&mut self) -> R<Def> {
        self.word("def")?;
        let symbol = self.symbol()?;
        let mut rep_vars = Vec::new();
        let mut effect_vars = Vec::new();
        if self.eat("<") {
            while !self.is_punct(">") {
                match self.peek().clone() {
                    Tok::RepVar(a) => {
                        self.next();
                        self.punct("=")?;
                        rep_vars.push((a, self.ident()?));
                    }
                    Tok::Ident(e) => {
                        self.next();
                        self.punct(":")?;
                        let kind = match self.ident()?.as_str() {
                            "Effect" => Kind::Effect,
                            "ManyEffect" => Kind::ManyEffect,
                            _ => return self.fail("`Effect` or `ManyEffect`"),
                        };
                        effect_vars.push((e, kind));
                    }
                    _ => return self.fail("a representation or an effect variable"),
                }
                if !self.eat(",") && !self.eat(";") {
                    break;
                }
            }
            self.punct(">")?;
        }
        let (params, conts) = self.params()?;
        self.punct("=")?;
        let body = self.statement()?;
        Ok(Def {
            symbol,
            rep_vars,
            effect_vars,
            params,
            conts,
            body,
        })
    }

    /// `(x: rep, ..; k: ptr, ..)`: values, then continuations. A
    /// continuation's representation may be left out, since it is `ptr`.
    fn params(&mut self) -> R<(Vec<Binder>, Vec<String>)> {
        self.punct("(")?;
        let mut values = Vec::new();
        while !self.is_punct(";") && !self.is_punct(")") {
            values.push(self.binder()?);
            if !self.eat(",") {
                break;
            }
        }
        let mut conts = Vec::new();
        if self.eat(";") {
            while !self.is_punct(")") {
                let k = self.ident()?;
                if self.eat(":") {
                    self.rep()?;
                }
                conts.push(k);
                if !self.eat(",") {
                    break;
                }
            }
        }
        self.punct(")")?;
        Ok((values, conts))
    }

    fn binder(&mut self) -> R<Binder> {
        let name = self.ident()?;
        self.punct(":")?;
        let rep = self.rep()?;
        self.hints();
        Ok(Binder { name, rep })
    }

    /// Hints after a binder's representation -- `@once`, `@0..1` -- which v0
    /// reads and ignores.
    fn hints(&mut self) {
        while self.is_punct("@") && !matches!(self.peek_at(1), Tok::Ident(w) if w == "many") {
            self.next();
            while matches!(self.peek(), Tok::Ident(_) | Tok::Int(_) | Tok::Punct(".")) {
                self.next();
            }
        }
    }

    fn rep(&mut self) -> R<Rep> {
        match self.peek().clone() {
            Tok::RepVar(a) => {
                self.next();
                Ok(Rep::Var(a))
            }
            Tok::Ident(x) => match Rep::named(&x) {
                Some(r) => {
                    self.next();
                    Ok(r)
                }
                None => self.fail("a representation, such as `i64` or `ptr`"),
            },
            _ => self.fail("a representation, such as `i64` or `ptr`"),
        }
    }

    // --- statements, producers, consumers ----------------------------------------------

    fn statement(&mut self) -> R<Statement> {
        match self.peek().clone() {
            Tok::Punct("<") => {
                self.next();
                let p = self.producer()?;
                self.punct("|")?;
                let c = self.consumer()?;
                self.punct(">")?;
                Ok(Statement::Cut(p, c))
            }
            Tok::Sym(f) => {
                self.next();
                let (args, conts) = self.call()?;
                Ok(Statement::Call(f, args, conts))
            }
            Tok::Ident(w) => match w.as_str() {
                "prim" => {
                    self.next();
                    let op = self.ident()?;
                    let (args, conts) = self.call()?;
                    Ok(Statement::Prim(op, args, conts))
                }
                "let" => {
                    self.next();
                    let x = self.binder()?;
                    self.punct("=")?;
                    let p = self.producer()?;
                    self.word("in")?;
                    let body = self.statement()?;
                    Ok(Statement::Let(x, p, Box::new(body)))
                }
                "perform" => {
                    self.next();
                    let op = self.symbol()?;
                    let (args, mut conts) = self.call()?;
                    if conts.len() != 1 {
                        return self.fail("one continuation for `perform`");
                    }
                    Ok(Statement::Perform(op, args, conts.remove(0)))
                }
                "error" => {
                    self.next();
                    match self.next() {
                        Tok::Str(s) => Ok(Statement::Error(s)),
                        _ => self.fail("the error's message, a string"),
                    }
                }
                "handle" => self.handle(),
                _ => self.fail("a statement"),
            },
            _ => self.fail("a statement"),
        }
    }

    /// `(p, ..; c, ..)`
    fn call(&mut self) -> R<(Vec<Producer>, Vec<Consumer>)> {
        self.punct("(")?;
        let mut args = Vec::new();
        while !self.is_punct(";") && !self.is_punct(")") {
            args.push(self.producer()?);
            if !self.eat(",") {
                break;
            }
        }
        let mut conts = Vec::new();
        if self.eat(";") {
            while !self.is_punct(")") {
                conts.push(self.consumer()?);
                if !self.eat(",") {
                    break;
                }
            }
        }
        self.punct(")")?;
        Ok((args, conts))
    }

    fn handle(&mut self) -> R<Statement> {
        self.word("handle")?;
        self.punct("{")?;
        let mut clauses = Vec::new();
        let ret = loop {
            if self.is_word("return") {
                self.next();
                let (mut params, mut conts) = self.params()?;
                if params.len() != 1 || conts.len() != 1 {
                    return self.fail("`return(x: rep; k: ptr)`");
                }
                self.punct("=>")?;
                let body = self.statement()?;
                self.eat(";");
                break (params.remove(0), conts.remove(0), body);
            }
            let op = self.symbol()?;
            let (params, conts) = self.params()?;
            let [resumption, cont]: [String; 2] = conts.try_into().map_err(|_| {
                format!(
                    "`{op}`'s clause binds a resumption and a continuation: `(..; r: ptr, k: ptr)`"
                )
            })?;
            self.punct("=>")?;
            let body = self.statement()?;
            clauses.push(Clause {
                op,
                params,
                resumption,
                cont,
                body,
            });
            self.eat(";");
        };
        self.punct("}")?;
        self.word("in")?;
        match self.next() {
            Tok::Mu => {}
            _ => return self.fail("`μ b.`, the body's own continuation"),
        }
        let body_cont = self.ident()?;
        self.punct(".")?;
        let body = self.statement()?;
        self.punct(";")?;
        let cont = self.consumer()?;
        Ok(Statement::Handle(Box::new(Handle {
            clauses,
            ret,
            body_cont,
            body,
            cont,
        })))
    }

    fn producers(&mut self, close: &str) -> R<Vec<Producer>> {
        let mut out = Vec::new();
        while !self.is_punct(close) {
            out.push(self.producer()?);
            if !self.eat(",") {
                break;
            }
        }
        self.punct(close)?;
        Ok(out)
    }

    fn producer(&mut self) -> R<Producer> {
        match self.next() {
            Tok::Int(n) => Ok(Producer::Int(n)),
            Tok::Float(x) => Ok(Producer::Float(x)),
            Tok::Str(s) => Ok(Producer::Str(s)),
            Tok::Char(c) => Ok(Producer::Char(c)),
            Tok::Punct("-") => match self.next() {
                Tok::Int(n) => Ok(Producer::Int(n.wrapping_neg())),
                Tok::Float(x) => Ok(Producer::Float(-x)),
                _ => self.fail("a number after `-`"),
            },
            Tok::Sym(s) => {
                let args = if self.eat("(") {
                    self.producers(")")?
                } else {
                    Vec::new()
                };
                // A constructor or a val: settled by `resolve`.
                Ok(Producer::Con(s, args))
            }
            Tok::Hash(h) if h == "tuple" => {
                self.punct("(")?;
                Ok(Producer::Tuple(self.producers(")")?))
            }
            Tok::Punct("[") => Ok(Producer::Array(self.producers("]")?)),
            Tok::Mu => {
                let k = self.ident()?;
                self.punct(".")?;
                Ok(Producer::Mu(k, Box::new(self.statement()?)))
            }
            Tok::Ident(w) => match w.as_str() {
                "true" => Ok(Producer::Bool(true)),
                "false" => Ok(Producer::Bool(false)),
                "unit" => Ok(Producer::Unit),
                "desc" => {
                    self.punct("(")?;
                    let r = self.rep()?;
                    self.punct(")")?;
                    Ok(Producer::Desc(r))
                }
                "cocase" => {
                    self.punct("{")?;
                    let mut methods = Vec::new();
                    while !self.is_punct("}") {
                        let name = self.ident()?;
                        let (params, conts) = self.params()?;
                        self.punct("=>")?;
                        let body = self.statement()?;
                        methods.push(Method {
                            name,
                            params,
                            conts,
                            body,
                        });
                        if !self.eat(";") {
                            break;
                        }
                    }
                    self.punct("}")?;
                    Ok(Producer::Cocase(methods))
                }
                "record" => {
                    self.punct("{")?;
                    let mut fields = Vec::new();
                    while !self.is_punct("}") {
                        let l = self.ident()?;
                        self.punct("=")?;
                        fields.push((l, self.producer()?));
                        if !self.eat(",") {
                            break;
                        }
                    }
                    self.punct("}")?;
                    Ok(Producer::Record(fields))
                }
                _ => Ok(Producer::Var(w)),
            },
            t => {
                self.back(&t);
                self.fail("a value")
            }
        }
    }

    fn consumer(&mut self) -> R<Consumer> {
        match self.next() {
            Tok::MuTilde => {
                let x = self.binder()?;
                self.punct(".")?;
                Ok(Consumer::MuTilde(x, Box::new(self.statement()?)))
            }
            Tok::Ident(w) => match w.as_str() {
                "halt" => Ok(Consumer::Halt),
                "case" => {
                    self.punct("{")?;
                    let mut arms = Vec::new();
                    while !self.is_punct("}") {
                        arms.push(self.arm()?);
                        if !self.eat(";") {
                            break;
                        }
                    }
                    self.punct("}")?;
                    Ok(Consumer::Case(arms))
                }
                _ if self.is_punct("(") => {
                    let (args, conts) = self.call()?;
                    Ok(Consumer::Method(w, args, conts))
                }
                _ => Ok(Consumer::Var(w)),
            },
            t => {
                self.back(&t);
                self.fail("a continuation")
            }
        }
    }

    fn arm(&mut self) -> R<Arm> {
        let pattern = match self.next() {
            Tok::Sym(s) => Pattern::Con(s),
            Tok::Hash(h) if h == "tuple" => Pattern::Tuple,
            Tok::Punct("_") => Pattern::Default,
            t => {
                self.back(&t);
                return self.fail("a constructor, `#tuple` or `_`");
            }
        };
        let mut fields = Vec::new();
        if self.eat("(") {
            while !self.is_punct(")") {
                fields.push(self.binder()?);
                if !self.eat(",") {
                    break;
                }
            }
            self.punct(")")?;
        }
        self.punct("=>")?;
        let body = self.statement()?;
        Ok(Arm {
            pattern,
            fields,
            body,
        })
    }
}

// --- constructors and vals -----------------------------------------------------------

/// Settle each symbol written as a producer: a declared constructor, or a
/// val. A symbol applied to arguments must be a constructor.
fn resolve(p: &mut Program) -> Result<(), String> {
    let ctors: std::collections::HashSet<Symbol> = p
        .datas
        .iter()
        .flat_map(|d| d.ctors.iter().map(|(k, _)| d.symbol.child(k)))
        .collect();
    let mut errors = Vec::new();
    let mut fix = |s: &mut Statement| statement(s, &ctors, &mut errors);
    for v in &mut p.vals {
        fix(&mut v.body);
    }
    for d in &mut p.defs {
        fix(&mut d.body);
    }
    match errors.first() {
        Some(e) => Err(e.clone()),
        None => Ok(()),
    }
}

fn statement(
    s: &mut Statement,
    ctors: &std::collections::HashSet<Symbol>,
    errors: &mut Vec<String>,
) {
    match s {
        Statement::Cut(p, c) => {
            producer(p, ctors, errors);
            consumer(c, ctors, errors);
        }
        Statement::Call(_, ps, cs) | Statement::Prim(_, ps, cs) => {
            ps.iter_mut().for_each(|p| producer(p, ctors, errors));
            cs.iter_mut().for_each(|c| consumer(c, ctors, errors));
        }
        Statement::Let(_, p, body) => {
            producer(p, ctors, errors);
            statement(body, ctors, errors);
        }
        Statement::Handle(h) => {
            for c in &mut h.clauses {
                statement(&mut c.body, ctors, errors);
            }
            statement(&mut h.ret.2, ctors, errors);
            statement(&mut h.body, ctors, errors);
            consumer(&mut h.cont, ctors, errors);
        }
        Statement::Perform(_, ps, c) => {
            ps.iter_mut().for_each(|p| producer(p, ctors, errors));
            consumer(c, ctors, errors);
        }
        Statement::Error(_) => {}
    }
}

fn producer(p: &mut Producer, ctors: &std::collections::HashSet<Symbol>, errors: &mut Vec<String>) {
    match p {
        Producer::Con(s, args) => {
            args.iter_mut().for_each(|a| producer(a, ctors, errors));
            if !ctors.contains(s) {
                if args.is_empty() {
                    *p = Producer::Val(s.clone());
                } else {
                    errors.push(format!("`{s}` is applied, but no data declares it"));
                }
            }
        }
        Producer::Tuple(ps) | Producer::Array(ps) => {
            ps.iter_mut().for_each(|p| producer(p, ctors, errors));
        }
        Producer::Record(fs) => fs.iter_mut().for_each(|(_, p)| producer(p, ctors, errors)),
        Producer::Mu(_, s) => statement(s, ctors, errors),
        Producer::Cocase(ms) => ms
            .iter_mut()
            .for_each(|m| statement(&mut m.body, ctors, errors)),
        _ => {}
    }
}

fn consumer(c: &mut Consumer, ctors: &std::collections::HashSet<Symbol>, errors: &mut Vec<String>) {
    match c {
        Consumer::MuTilde(_, s) => statement(s, ctors, errors),
        Consumer::Case(arms) => arms
            .iter_mut()
            .for_each(|a| statement(&mut a.body, ctors, errors)),
        Consumer::Method(_, ps, cs) => {
            ps.iter_mut().for_each(|p| producer(p, ctors, errors));
            cs.iter_mut().for_each(|c| consumer(c, ctors, errors));
        }
        Consumer::Var(_) | Consumer::Halt => {}
    }
}
