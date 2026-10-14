//! Cut as text: what [`crate::parse`] reads, written. A program printed and
//! read back is the program it was.
//!
//! [`listing`] is the same text with what each part of it is: which
//! declaration a range is, which variable or symbol a name in it -- for a
//! tool that shows a program beside what it was compiled from, or to.

use crate::*;
use std::fmt::Write;

impl fmt::Display for Rep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Rep::I64 => "i64",
            Rep::F64 => "f64",
            Rep::F32 => "f32",
            Rep::I8 => "i8",
            Rep::I16 => "i16",
            Rep::I32 => "i32",
            Rep::U8 => "u8",
            Rep::U16 => "u16",
            Rep::U32 => "u32",
            Rep::U64 => "u64",
            Rep::Bool => "bool",
            Rep::Char => "char",
            Rep::Unit => "unit",
            Rep::Sym => "sym",
            Rep::Str => "str",
            Rep::Ptr => "ptr",
            Rep::Desc => "desc",
            Rep::Any => "any",
            Rep::Var(a) => return write!(f, "'{a}"),
        })
    }
}

impl fmt::Display for Binder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name, self.rep)
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Effect => "Effect",
            Kind::ManyEffect => "ManyEffect",
        })
    }
}

fn commas<T: fmt::Display>(xs: &[T]) -> String {
    xs.iter().map(T::to_string).collect::<Vec<_>>().join(", ")
}

/// A string literal, escaped as the parser reads one.
pub fn string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn character(c: char) -> String {
    match c {
        '\'' => "'\\''".to_string(),
        '\\' => "'\\\\'".to_string(),
        '\n' => "'\\n'".to_string(),
        '\t' => "'\\t'".to_string(),
        '\r' => "'\\r'".to_string(),
        '\0' => "'\\0'".to_string(),
        c if c.is_control() => format!("'\\u{{{:x}}}'", c as u32),
        c => format!("'{c}'"),
    }
}

/// A float as a literal that reads back as itself: always with a point.
fn float(x: f64) -> String {
    let s = format!("{x:?}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

/// A program's text, and what its parts are.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listing {
    pub text: String,
    /// In the order written. A declaration's comes before those of what is
    /// in it.
    pub segments: Vec<Segment>,
}

/// Bytes `start..end` of a listing's text, and what is written there.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub start: usize,
    pub end: usize,
    pub part: Part,
}

/// What a range of a listing is.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    /// A whole declaration: a `data`, an `effect`, a `val` or a `def`.
    Decl(Symbol),
    /// A variable, bound or mentioned -- a value's or a continuation's -- in
    /// the declaration `of`. Its name is its own within that declaration.
    Var { of: Symbol, name: String },
    /// A symbol mentioned: a value read, a definition called, a constructor,
    /// an operation -- in the declaration `of`.
    Symbol { of: Symbol, symbol: Symbol },
}

impl Listing {
    /// The segments that are the variable `name` of declaration `of`.
    pub fn var<'a>(&'a self, of: &'a Symbol, name: &'a str) -> impl Iterator<Item = &'a Segment> {
        self.segments.iter().filter(
            move |s| matches!(&s.part, Part::Var { of: o, name: n } if o == of && n == name),
        )
    }

    /// The declaration of `symbol`, if the program has one.
    pub fn decl(&self, symbol: &Symbol) -> Option<&Segment> {
        self.segments
            .iter()
            .find(|s| matches!(&s.part, Part::Decl(d) if d == symbol))
    }
}

/// A name written, before it is known which declaration it is in.
enum Mark {
    Var(String),
    Symbol(Symbol),
}

#[derive(Default)]
struct Printer {
    out: String,
    marks: Vec<(usize, usize, Mark)>,
}

impl Printer {
    /// A variable, bound or mentioned.
    fn var(&mut self, x: &str) {
        let start = self.out.len();
        self.out.push_str(x);
        self.marks
            .push((start, self.out.len(), Mark::Var(x.to_string())));
    }

    fn symbol(&mut self, s: &Symbol) {
        let start = self.out.len();
        let _ = write!(self.out, "{s}");
        self.marks
            .push((start, self.out.len(), Mark::Symbol(s.clone())));
    }

    /// `x: rep, ..`
    fn binders(&mut self, bs: &[Binder]) {
        for (i, b) in bs.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.var(&b.name);
            let _ = write!(self.out, ": {}", b.rep);
        }
    }

    /// `(values; continuations)`, every continuation written as the `ptr`
    /// it is.
    fn params(&mut self, values: &[Binder], conts: &[String]) {
        self.out.push('(');
        self.binders(values);
        self.out.push_str("; ");
        for (i, k) in conts.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.var(k);
            self.out.push_str(": ptr");
        }
        self.out.push(')');
    }

    fn line(&mut self, indent: usize) {
        self.out.push('\n');
        self.out.push_str(&"  ".repeat(indent));
    }

    /// A statement on a line of its own, `indent` levels in.
    fn nested(&mut self, s: &Statement, indent: usize) {
        self.line(indent);
        self.statement(s, indent);
    }

    fn list(&mut self, ps: &[Producer], indent: usize) {
        for (i, p) in ps.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.producer(p, indent);
        }
    }

    /// `(p, ..; c, ..)`
    fn call(&mut self, args: &[Producer], conts: &[Consumer], indent: usize) {
        self.out.push('(');
        self.list(args, indent);
        self.out.push_str("; ");
        for (i, c) in conts.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.consumer(c, indent);
        }
        self.out.push(')');
    }

    fn producer(&mut self, p: &Producer, indent: usize) {
        match p {
            Producer::Var(x) => self.var(x),
            Producer::Val(s) => self.symbol(s),
            Producer::Int(n) => {
                let _ = write!(self.out, "{n}");
            }
            Producer::Float(x) => self.out.push_str(&float(*x)),
            Producer::Char(c) => self.out.push_str(&character(*c)),
            Producer::Str(s) => self.out.push_str(&string(s)),
            Producer::Bool(b) => {
                let _ = write!(self.out, "{b}");
            }
            Producer::Unit => self.out.push_str("unit"),
            Producer::Desc(r) => {
                let _ = write!(self.out, "desc({r})");
            }
            Producer::Con(k, args) => {
                self.symbol(k);
                if !args.is_empty() {
                    self.out.push('(');
                    self.list(args, indent);
                    self.out.push(')');
                }
            }
            Producer::Tuple(args) => {
                self.out.push_str("#tuple(");
                self.list(args, indent);
                self.out.push(')');
            }
            Producer::Array(args) => {
                self.out.push('[');
                self.list(args, indent);
                self.out.push(']');
            }
            Producer::Record(fields) => {
                self.out.push_str("record { ");
                for (i, (l, p)) in fields.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    let _ = write!(self.out, "{l} = ");
                    self.producer(p, indent);
                }
                self.out.push_str(" }");
            }
            Producer::Mu(k, s) => {
                self.out.push_str("μ ");
                self.var(k);
                self.out.push('.');
                self.nested(s, indent + 1);
            }
            Producer::Cocase(methods) => {
                self.out.push_str("cocase {");
                for (i, m) in methods.iter().enumerate() {
                    if i > 0 {
                        self.out.push(';');
                    }
                    self.line(indent + 1);
                    self.out.push_str(&m.name);
                    self.params(&m.params, &m.conts);
                    self.out.push_str(" =>");
                    self.nested(&m.body, indent + 2);
                }
                self.line(indent);
                self.out.push('}');
            }
        }
    }

    fn consumer(&mut self, c: &Consumer, indent: usize) {
        match c {
            Consumer::Var(k) => self.var(k),
            Consumer::Halt => self.out.push_str("halt"),
            Consumer::MuTilde(x, s) => {
                self.out.push_str("μ̃ ");
                self.binders(std::slice::from_ref(x));
                self.out.push('.');
                self.nested(s, indent + 1);
            }
            Consumer::Case(arms) => {
                self.out.push_str("case {");
                for (i, arm) in arms.iter().enumerate() {
                    if i > 0 {
                        self.out.push(';');
                    }
                    self.line(indent + 1);
                    match &arm.pattern {
                        Pattern::Con(k) => self.symbol(k),
                        Pattern::Tuple => self.out.push_str("#tuple"),
                        Pattern::Default => self.out.push('_'),
                    }
                    if !arm.fields.is_empty() || arm.pattern == Pattern::Tuple {
                        self.out.push('(');
                        self.binders(&arm.fields);
                        self.out.push(')');
                    }
                    self.out.push_str(" =>");
                    self.nested(&arm.body, indent + 2);
                }
                self.line(indent);
                self.out.push('}');
            }
            Consumer::Method(m, args, conts) => {
                self.out.push_str(m);
                self.call(args, conts, indent);
            }
        }
    }

    fn statement(&mut self, s: &Statement, indent: usize) {
        match s {
            Statement::Cut(p, c) => {
                self.out.push('<');
                self.producer(p, indent);
                self.out.push_str(" | ");
                self.consumer(c, indent);
                self.out.push('>');
            }
            Statement::Call(f, args, conts) => {
                self.symbol(f);
                self.call(args, conts, indent);
            }
            Statement::Prim(op, args, conts) => {
                let _ = write!(self.out, "prim {op}");
                self.call(args, conts, indent);
            }
            Statement::Let(x, p, body) => {
                self.out.push_str("let ");
                self.binders(std::slice::from_ref(x));
                self.out.push_str(" = ");
                self.producer(p, indent);
                self.out.push_str(" in");
                self.nested(body, indent);
            }
            Statement::Perform(op, args, c) => {
                self.out.push_str("perform ");
                self.symbol(op);
                self.call(args, std::slice::from_ref(c), indent);
            }
            Statement::Error(msg) => {
                let _ = write!(self.out, "error {}", string(msg));
            }
            Statement::Handle(h) => {
                self.out.push_str("handle {");
                for c in &h.clauses {
                    self.line(indent + 1);
                    let conts = [c.resumption.clone(), c.cont.clone()];
                    self.symbol(&c.op);
                    self.params(&c.params, &conts);
                    self.out.push_str(" =>");
                    self.nested(&c.body, indent + 2);
                    self.out.push(';');
                }
                let (x, k, body) = &h.ret;
                self.line(indent + 1);
                self.out.push_str("return");
                self.params(std::slice::from_ref(x), std::slice::from_ref(k));
                self.out.push_str(" =>");
                self.nested(body, indent + 2);
                self.line(indent);
                self.out.push_str("} in μ ");
                self.var(&h.body_cont);
                self.out.push('.');
                self.nested(&h.body, indent + 1);
                self.line(indent);
                self.out.push_str("; ");
                self.consumer(&h.cont, indent);
            }
        }
    }
}

/// A program as the text [`crate::parse`] reads.
pub fn program(p: &Program) -> String {
    listing(p).text
}

/// What `pr` wrote, a declaration of `symbol`'s, after a blank line of
/// `out`: the declaration's segment, then those of the names in it.
fn declared(out: &mut String, segments: &mut Vec<Segment>, symbol: &Symbol, pr: Printer) {
    out.push('\n');
    let base = out.len();
    out.push_str(&pr.out);
    segments.push(Segment {
        start: base,
        end: out.len(),
        part: Part::Decl(symbol.clone()),
    });
    out.push('\n');
    for (start, end, mark) in pr.marks {
        segments.push(Segment {
            start: base + start,
            end: base + end,
            part: match mark {
                Mark::Var(name) => Part::Var {
                    of: symbol.clone(),
                    name,
                },
                Mark::Symbol(s) => Part::Symbol {
                    of: symbol.clone(),
                    symbol: s,
                },
            },
        });
    }
}

/// [`program`]'s text, and what each part of it is.
pub fn listing(p: &Program) -> Listing {
    let mut segments = Vec::new();
    let mut out = format!("cut {}\n", p.version);
    if let Some(entry) = &p.entry {
        let _ = writeln!(out, "entry {entry}");
        let answer = match p.answer {
            Answer::None => "none",
            Answer::Str => "str",
        };
        let _ = writeln!(out, "answer {answer}");
    }
    for e in &p.exports {
        let _ = writeln!(out, "export {:?} = {} -> {}", e.name, e.symbol, e.result);
    }
    for d in &p.datas {
        let vars = if d.rep_vars.is_empty() {
            String::new()
        } else {
            let vs: Vec<String> = d.rep_vars.iter().map(|a| format!("'{a}")).collect();
            format!(" <{}>", vs.join(", "))
        };
        let ctors: Vec<String> = d
            .ctors
            .iter()
            .map(|(k, fields)| {
                if fields.is_empty() {
                    segment(k)
                } else {
                    format!("{}({})", segment(k), commas(fields))
                }
            })
            .collect();
        let mut pr = Printer::default();
        let _ = write!(pr.out, "data {}{vars} {{ {} }}", d.symbol, ctors.join("; "));
        declared(&mut out, &mut segments, &d.symbol, pr);
    }
    if !p.roles.is_empty() {
        out.push_str("\nroles {\n");
        for (role, k) in &p.roles {
            let _ = writeln!(out, "  {role} = {k}");
        }
        out.push_str("}\n");
    }
    if !p.natives.is_empty() {
        out.push_str("\nnative {\n");
        for (op, runtime) in &p.natives {
            let _ = writeln!(out, "  {op} = {runtime}");
        }
        out.push_str("}\n");
    }
    for e in &p.effects {
        let ops: Vec<String> = e
            .ops
            .iter()
            .map(|op| {
                let many = if op.many { "@many " } else { "" };
                format!(
                    "{many}{}({}) -> {}",
                    segment(&op.name),
                    commas(&op.params),
                    op.result
                )
            })
            .collect();
        let mut pr = Printer::default();
        let _ = write!(pr.out, "effect {} {{ {} }}", e.symbol, ops.join("; "));
        declared(&mut out, &mut segments, &e.symbol, pr);
    }
    for v in &p.vals {
        let mut pr = Printer::default();
        let _ = write!(pr.out, "val {} : {} =", v.symbol, v.rep);
        pr.nested(&v.body, 1);
        declared(&mut out, &mut segments, &v.symbol, pr);
    }
    for d in &p.defs {
        let mut head = Vec::new();
        if !d.rep_vars.is_empty() {
            let vs: Vec<String> = d
                .rep_vars
                .iter()
                .map(|(a, desc)| format!("'{a} = {desc}"))
                .collect();
            head.push(vs.join(", "));
        }
        if !d.effect_vars.is_empty() {
            let es: Vec<String> = d
                .effect_vars
                .iter()
                .map(|(e, k)| format!("{e}: {k}"))
                .collect();
            head.push(es.join(", "));
        }
        let generics = if head.is_empty() {
            String::new()
        } else {
            format!(" <{}>", head.join("; "))
        };
        let mut pr = Printer::default();
        let _ = write!(pr.out, "def {}{generics} ", d.symbol);
        pr.params(&d.params, &d.conts);
        pr.out.push_str(" =");
        pr.nested(&d.body, 1);
        declared(&mut out, &mut segments, &d.symbol, pr);
    }
    Listing {
        text: out,
        segments,
    }
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&program(self))
    }
}
