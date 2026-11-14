//! **Cut**: the upper IR every front end hands Meadow's back end, as data.
//!
//! The contract is `docs/CUT.md`. A front end -- Meadow's, Idyll's -- writes a
//! program as Cut text ([`parse`] reads it back); the reference interpreter
//! ([`interp`]) runs it, and what it answers and prints is what the program
//! means; and the lowering takes it to AxCut for Glade and Silo. Nothing here
//! knows any front end.
//!
//! Cut is a sequent calculus typed by representations: producers make values,
//! consumers receive them, and a statement is one meeting the other. A
//! function is codata with an `apply` method, a continuation is a consumer,
//! and a program is the declarations a runtime needs and the definitions that
//! run.

pub mod any;
pub mod grade;
pub mod inline;
pub mod interp;
pub mod lower;
pub mod parse;
pub mod print;
pub mod pure;
pub mod specialize;
pub mod strip;
pub mod trmc;

pub use parse::parse;

use std::fmt;

/// How a value is represented at run time: the only types Cut has.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Rep {
    I64,
    F64,
    F32,
    I8,
    I16,
    I32,
    U8,
    U16,
    U32,
    U64,
    Bool,
    Char,
    Unit,
    Sym,
    Str,
    Ptr,
    /// A descriptor: what a representation variable is, at run time.
    Desc,
    /// A representation variable, `'a`.
    Var(String),
    /// Boxed, and self-describing at run time.
    Any,
}

impl Rep {
    /// The representation `name` writes, without the `'` of a variable.
    pub fn named(name: &str) -> Option<Rep> {
        Some(match name {
            "i64" => Rep::I64,
            "f64" => Rep::F64,
            "f32" => Rep::F32,
            "i8" => Rep::I8,
            "i16" => Rep::I16,
            "i32" => Rep::I32,
            "u8" => Rep::U8,
            "u16" => Rep::U16,
            "u32" => Rep::U32,
            "u64" => Rep::U64,
            "bool" => Rep::Bool,
            "char" => Rep::Char,
            "unit" => Rep::Unit,
            "sym" => Rep::Sym,
            "str" => Rep::Str,
            "ptr" => Rep::Ptr,
            "desc" => Rep::Desc,
            "any" => Rep::Any,
            _ => return None,
        })
    }
}

/// A flat, globally unique name: `idyll:Json@1.2.0/Value.parse`. Kept as its
/// parts; [`fmt::Display`] writes it as the spec spells it, quoting a segment
/// that is not a plain identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol {
    /// The front end: `meadow`, `idyll`.
    pub lang: String,
    /// The package, with its version if it has one: `Json@1.2.0`.
    pub package: String,
    /// The module path, then the name.
    pub path: Vec<String>,
}

impl Symbol {
    /// The symbol of something declared under this one: a constructor under
    /// its type, an operation under its effect.
    pub fn child(&self, name: &str) -> Symbol {
        let mut path = self.path.clone();
        path.push(name.to_string());
        Symbol {
            path,
            ..self.clone()
        }
    }

    /// The last segment: a constructor's or an operation's own name.
    pub fn last(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }
}

/// Is `s` a plain identifier, which a symbol writes without quotes?
pub fn is_ident(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A segment of a symbol as it is written: as it is if it is a plain
/// identifier, quoted if not.
pub fn segment(seg: &str) -> String {
    if is_ident(seg) {
        seg.to_string()
    } else {
        format!("\"{}\"", seg.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}/", self.lang, self.package)?;
        for (i, seg) in self.path.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(&segment(seg))?;
        }
        Ok(())
    }
}

/// A name a program binds, and how its value is represented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binder {
    pub name: String,
    pub rep: Rep,
    /// What the source calls it, for a debugger: see [`Shown`]. Nothing
    /// but a debugger reads it.
    pub shown: Option<Shown>,
}

impl Binder {
    /// `name: rep`, with no name of the source's.
    pub fn new(name: impl Into<String>, rep: Rep) -> Binder {
        Binder {
            name: name.into(),
            rep,
            shown: None,
        }
    }
}

/// A variable as the source wrote it: its name, and its type as the front
/// end writes types -- `count`, `Int`. Written after a binder's
/// representation, `v12: i64 @"count" "Int"`, by a front end building a
/// program to be debugged; what a debugger shows a value under.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Shown {
    pub name: String,
    pub ty: String,
}

/// A source file the positions of a program are in: the symbol positions
/// name it by, and its path. `source meadow:App/"src/Main.mw" = "/home/me/App/src/Main.mw"`.
///
/// A symbol, so that programs of several packages put together text by text
/// -- as MeadowBoot links its units -- name their files apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub symbol: Symbol,
    pub path: String,
}

/// Where a statement was written: bytes `start..end` of the source file at
/// [`Program::sources`]`[source]`. `@meadow:App/"src/Main.mw":120-134 s`.
///
/// A position says what a debugger stops at and shows: it does nothing,
/// and a program runs as it would without it ([`strip`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pos {
    pub source: u32,
    pub start: u32,
    pub end: u32,
}

/// What makes a value.
#[derive(Clone, Debug, PartialEq)]
pub enum Producer {
    Var(String),
    /// A top-level value, by its symbol: what the val computed.
    Val(Symbol),
    Int(i64),
    Float(f64),
    Char(char),
    Str(String),
    Bool(bool),
    Unit,
    /// A descriptor for a representation known when compiled.
    Desc(Rep),
    /// A declared constructor, applied.
    Con(Symbol, Vec<Producer>),
    /// The IR's own tuple.
    Tuple(Vec<Producer>),
    /// `μ k. s`: the value `s` gives to `k`.
    Mu(String, Box<Statement>),
    /// Codata: an object with methods.
    Cocase(Vec<Method>),
    Record(Vec<(String, Producer)>),
    Array(Vec<Producer>),
}

/// A method of an object: `apply(x: i64; k: ptr) ! {E} => s`.
#[derive(Clone, Debug, PartialEq)]
pub struct Method {
    pub name: String,
    /// What the source calls the function it is part of, for a debugger's
    /// stack: `apply @"mainLocal1"(..)`, as `Def::shown` for a definition.
    pub shown: Option<String>,
    pub params: Vec<Binder>,
    pub conts: Vec<String>,
    /// What it may perform, if the front end says: see [`Row`].
    pub row: Option<Row>,
    pub body: Statement,
}

/// What receives a value.
#[derive(Clone, Debug, PartialEq)]
pub enum Consumer {
    Var(String),
    /// The end: a val's value, or the entry's answer.
    Halt,
    /// `μ̃ x: rep. s`
    MuTilde(Binder, Box<Statement>),
    /// Take data apart: an arm for each constructor, and perhaps a default.
    Case(Vec<Arm>),
    /// Call a method of the object received, with arguments and continuations.
    Method(String, Vec<Producer>, Vec<Consumer>),
}

/// What an arm of a `case` matches.
#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    Con(Symbol),
    Tuple,
    /// `_`: anything the arms before it did not.
    Default,
}

/// An arm of a `case`: what it matches, the fields bound, and what runs.
#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    pub pattern: Pattern,
    pub fields: Vec<Binder>,
    pub body: Statement,
}

/// A producer meeting a consumer, or something that ends the same way.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    Cut(Producer, Consumer),
    /// A top-level definition, called with values and continuations.
    Call(Symbol, Vec<Producer>, Vec<Consumer>),
    /// A primitive, by its Cut name: `prim add(x, y; k)`.
    Prim(String, Vec<Producer>, Vec<Consumer>),
    Let(Binder, Producer, Box<Statement>),
    Handle(Box<Handle>),
    /// `perform E.op(p, ..; c)`
    Perform(Symbol, Vec<Producer>, Consumer),
    Error(String),
    /// `@S:a-b s`: `s`, written at that position. It does nothing; see
    /// [`Pos`].
    At(Pos, Box<Statement>),
}

impl Statement {
    /// The statement under any positions around it.
    pub fn bare(&self) -> &Statement {
        let mut s = self;
        while let Statement::At(_, inner) = s {
            s = inner;
        }
        s
    }
}

/// `handle { clauses; return(..) } in μ b. body ; cont`
#[derive(Clone, Debug, PartialEq)]
pub struct Handle {
    pub clauses: Vec<Clause>,
    /// `return(x: rep; k: ptr) => s`
    pub ret: (Binder, String, Statement),
    /// The body's own continuation, which gives what it answers to `return`.
    pub body_cont: String,
    pub body: Statement,
    /// Where the handle's value goes.
    pub cont: Consumer,
}

/// `E.op(x: rep, ..; r: ptr, k: ptr) => s`
#[derive(Clone, Debug, PartialEq)]
pub struct Clause {
    pub op: Symbol,
    pub params: Vec<Binder>,
    pub resumption: String,
    pub cont: String,
    pub body: Statement,
}

/// What becomes of the entry's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Nothing is printed.
    None,
    /// A string, printed.
    Str,
}

/// `data S <'a> { K(rep, ..); .. }`
#[derive(Clone, Debug, PartialEq)]
pub struct DataDecl {
    pub symbol: Symbol,
    pub rep_vars: Vec<String>,
    /// Each constructor's own name -- its symbol is the type's and this --
    /// and its fields' representations, in the order declared.
    pub ctors: Vec<(String, Vec<Rep>)>,
}

/// `effect S { @many op(rep, ..) -> rep; .. }`
#[derive(Clone, Debug, PartialEq)]
pub struct EffectDecl {
    pub symbol: Symbol,
    pub ops: Vec<OpDecl>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpDecl {
    pub name: String,
    /// Resumed more than once, perhaps.
    pub many: bool,
    pub params: Vec<Rep>,
    pub result: Rep,
}

/// An effect variable's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Effect,
    ManyEffect,
}

/// `def S <'a = d; e: Effect> (params; conts) ! {E | e} = body`
#[derive(Clone, Debug, PartialEq)]
pub struct Def {
    pub symbol: Symbol,
    /// What the source calls it, for a debugger's stack: `def S @"fact" ..`.
    /// A copy of it, or a twin, is shown by the same name.
    pub shown: Option<String>,
    /// Each representation variable, with the descriptor parameter that
    /// carries it.
    pub rep_vars: Vec<(String, String)>,
    pub effect_vars: Vec<(String, Kind)>,
    pub params: Vec<Binder>,
    pub conts: Vec<String>,
    /// What it may perform, if the front end says.
    pub row: Option<Row>,
    pub body: Statement,
}

/// `! {E, .. | e, ..}`: what a definition or a method may perform, once
/// given its parameters, and does not handle itself -- the effects, each
/// declared by an `effect`, and the effect variables of the definition
/// it is in, whatever they stand for. `! {}` is nothing.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Row {
    pub effects: Vec<Symbol>,
    pub vars: Vec<String>,
}

/// `val S : rep = s`
#[derive(Clone, Debug, PartialEq)]
pub struct Val {
    pub symbol: Symbol,
    /// What the source calls it, as [`Def::shown`]: `val S @"answer" : ..`.
    pub shown: Option<String>,
    pub rep: Rep,
    pub body: Statement,
}

/// `export "c_name" = S -> rep`: definition `S` is a function C can call,
/// as `c_name`: its parameters are the C arguments, each as its
/// representation says -- `i32` an `int32_t`, `f32` a `float`, `i64` an
/// integer or a pointer -- and what it answers is `result`, `unit` for
/// `void`. A program of exports is built as a library, not an executable.
#[derive(Clone, Debug, PartialEq)]
pub struct Export {
    pub name: String,
    pub symbol: Symbol,
    pub result: Rep,
}

/// A whole program, as one text.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    /// The version its first line names.
    pub version: u32,
    pub entry: Option<Symbol>,
    pub answer: Answer,
    /// What C may call, by the names it calls them.
    pub exports: Vec<Export>,
    /// The files its positions are in: see [`Pos`].
    pub sources: Vec<Source>,
    pub datas: Vec<DataDecl>,
    /// Each role a runtime knows (`meadow_rt::roles`), by its name, and the
    /// constructor that plays it.
    pub roles: Vec<(String, Symbol)>,
    /// Each operation the program may leave unhandled, and the runtime's
    /// operation that performs it: `Console.writeOutput`.
    pub natives: Vec<(Symbol, String)>,
    pub effects: Vec<EffectDecl>,
    /// In the order they are computed.
    pub vals: Vec<Val>,
    pub defs: Vec<Def>,
}

/// The version of Cut this crate reads and writes.
pub const VERSION: u32 = 0;
