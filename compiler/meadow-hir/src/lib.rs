//! The **high-level IR** — the resolver's output, and the input to inference and
//! lowering.
//!
//! Structurally it mirrors [`meadow_ast`], with two differences:
//!
//! * Every node is a [`Node<T>`] — value + [`Span`] + a dense [`NodeId`]. Inference
//!   keeps its results in a `Vec` indexed by `NodeId`, producing a fully annotated
//!   tree.
//! * Every identifier is a [`VarId`] (globally unique — see the module docs in
//!   `lib.rs`). Type-constructor and data-constructor names stay interned strings.
//!
//! Operators are gone (desugared to primitive [`Expr::App`]s). `data` / `record`
//! declarations arrive here already validated ([`Decl::Data`], [`Decl::Record`],
//! [`TypeExpr`]).

use meadow_intern::InternedString;
use meadow_span::Span;

/// A value's name as it is written where a name is expected: `x` as itself,
/// an operator such as `++` in parentheses, `(++)`.
pub fn spell_name(name: &str) -> std::borrow::Cow<'_, str> {
    match name.chars().next() {
        Some(c) if !(c.is_alphanumeric() || c == '_') => format!("({name})").into(),
        _ => name.into(),
    }
}

/// A type's, effect's or trait's name as a person writes it.
///
pub use meadow_rt::{ctor_spelling, spelling};

/// The package a type's canonical name says it belongs to, if any: `None` for
/// the names the compiler knows, which have none.
///
/// The package is the path's first segment, and a version's parts with it --
/// `json@1.2.0.Value` is `json@1.2.0`'s -- which no module or type is mistaken
/// for, since those begin with a capital.
pub fn type_package(name: &str) -> Option<&str> {
    let first = name.find('.')?;
    if !name[..first].contains('@') {
        return Some(&name[..first]);
    }
    let mut end = first;
    while let Some(next) = name[end + 1..].find('.').map(|at| end + 1 + at) {
        if name[end + 1..].starts_with(|c: char| c.is_uppercase()) {
            break;
        }
        end = next;
    }
    Some(&name[..end])
}

/// The operators the language itself knows: how tightly each binds, which way
/// it groups, and the primitive it means where no definition of it is in scope
/// -- a program built without `Std`, which is where `Std.Ops` defines them all
/// as methods of traits, over these same primitives.
///
/// `::` is the list constructor's and nothing else's, and `++` has no
/// primitive: it is `Std.String`'s. The primitives are the integer ones:
/// `Std.Ops` gives the float types the same operators, and without it float
/// arithmetic names its primitives (`_primAddF x y`).
pub const OPERATORS: &[(&str, Fixity, Option<&str>)] = &[
    ("^", Fixity::right(8), Some("_primPow")),
    ("*", Fixity::left(7), Some("_primMul")),
    ("/", Fixity::left(7), Some("_primDiv")),
    ("%", Fixity::left(7), Some("_primMod")),
    ("+", Fixity::left(6), Some("_primAdd")),
    ("-", Fixity::left(6), Some("_primSub")),
    ("<<", Fixity::left(6), Some("_primShl")),
    (">>", Fixity::left(6), Some("_primShr")),
    (">>>", Fixity::left(6), Some("_primUshr")),
    ("::", Fixity::right(5), None),
    ("++", Fixity::right(5), None),
    ("==", Fixity::none(4), Some("_primEq")),
    ("!=", Fixity::none(4), Some("_primNe")),
    ("<", Fixity::none(4), Some("_primLt")),
    (">", Fixity::none(4), Some("_primGt")),
    ("<=", Fixity::none(4), Some("_primLe")),
    (">=", Fixity::none(4), Some("_primGe")),
];

/// Which way an operator groups: `a - b - c` is `(a - b) - c` (left), `a ^ b ^
/// c` is `a ^ (b ^ c)` (right), and `a == b == c` is an error (neither).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Assoc {
    Left,
    Right,
    None,
}

/// How an operator binds: `infixl 6` is `Fixity { assoc: Left, level: 6 }`.
/// Levels run 0 to 9, tighter as they go up; application is tighter than all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Fixity {
    pub assoc: Assoc,
    pub level: u8,
}

impl Fixity {
    pub const fn left(level: u8) -> Fixity {
        Fixity {
            assoc: Assoc::Left,
            level,
        }
    }
    pub const fn right(level: u8) -> Fixity {
        Fixity {
            assoc: Assoc::Right,
            level,
        }
    }
    pub const fn none(level: u8) -> Fixity {
        Fixity {
            assoc: Assoc::None,
            level,
        }
    }

    /// What an operator nobody declared binds as: `infixl 9`, as in Haskell.
    pub const DEFAULT: Fixity = Fixity::left(9);

    /// The keyword a declaration of it is written with.
    pub fn keyword(self) -> &'static str {
        match self.assoc {
            Assoc::Left => "infixl",
            Assoc::Right => "infixr",
            Assoc::None => "infix",
        }
    }
}

/// The fixity and fallback primitive of an operator the language knows.
pub fn builtin_operator(op: &str) -> Option<(Fixity, Option<&'static str>)> {
    OPERATORS
        .iter()
        .find(|(o, _, _)| *o == op)
        .map(|(_, f, p)| (*f, *p))
}

/// The traits of `Std` a type nothing settles is taken to be `Int` for, as
/// Haskell defaults its numeric classes: `x < y` of two things nothing ever
/// says the type of compares them as `Int`s, as it did when `<` was built in,
/// and `[;] == [;]` compares two empty lists of them. Only `Std`'s, which are
/// named without a package.
pub const NUMERIC_TRAITS: &[&str] = &[
    "Add",
    "Sub",
    "Mul",
    "Div",
    "Rem",
    "Pow",
    "Shift",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    "Debug",
    "Display",
];

/// Is `tr` (a canonical trait name) one of [`NUMERIC_TRAITS`] -- the standard
/// library's, and no other package's of the same name?
pub fn is_numeric_trait(tr: &str) -> bool {
    type_package(tr) == Some("Std") && NUMERIC_TRAITS.contains(&spelling(tr))
}

/// Primitive operators, in the order their [`VarId`]s are handed out by
/// `rename::Resolver::with_prelude`. `infer` builds matching type schemes by
/// index, and `core`/the linker map names to `Prim`s, so the order is load-bearing.
///
/// `and` / `or` are *not* here — they are keyword operators the resolver desugars
/// to `if` (that is what makes them short-circuit); `not` is a `Std.Bool`
/// function.
pub const PRIMS: &[&str] = &[
    "display",
    "hash",
    "_primAdd",
    "_primSub",
    "_primMul",
    "_primDiv",
    "_primMod",
    "_primPow",
    "_primEq",
    "_primNe",
    "_primLt",
    "_primGt",
    "_primLe",
    "_primGe",
    "neg", //
    "_primAddF",
    "_primSubF",
    "_primMulF",
    "_primDivF",
    "_primLtF",
    "_primGtF",
    "_primLeF",
    "_primGeF",
    "toFloat",
    "toFloat64",
    "toFloat32",
    "floor", //
    // conversions between the integer types
    "toBigInt",
    "toInt",
    "toInt64",
    "toInt8",
    "toInt16",
    "toInt32",
    "toUInt8",
    "toUInt16",
    "toUInt32",
    "toUInt64",
    "bitWidth", //
    // the one builtin collection: `Array`
    "arrayLen",
    "arrayGet",
    "arrayGetOr",
    "arraySet",
    "arrayPush",
    "arrayPop",
    "arraySlice",
    "arrayConcat", //
    // bitwise ops on `Int`, and the `String` <-> byte-array bridge
    "shl",
    "shr",
    "ushr",
    "_primShl",
    "_primShr",
    "_primUshr",
    // `display` by the name `Std.Display`'s one `impl` for every type calls,
    // which a program's own `display` -- the trait's method -- cannot shadow.
    "_primDisplay",
    "bitAnd",
    "bitOr",
    "bitXor",
    "bitNot",
    "popCount", //
    "stringToBytes",
    "bytesToString",
    "bytesToHex",
    "bytesFromHex", //
    // render any value as the REPL would print it
    "show", //
    // `Char` <-> its Unicode scalar value, and `String` <-> its characters
    "charCode",
    "charFromCode",
    "stringToChars",
    "charsToString", //
    // the one mutable cell; every operation carries the `Mut` effect
    "newRef",
    "getRef",
    "setRef", //
    // local mutable state: cells and arrays that cannot outlive their `runSt`
    "runSt",
    "stNewRef",
    "stGetRef",
    "stSetRef",
    "stNewArray",
    "stGetArray",
    "stSetArray",
    "stArrayLen",
    "stFreeze",
    "stThaw", //
    // compact regions: long-lived data the collector treats as one object
    "compact",
    "getCompact",
    "compactAdd",
    "compactSize", //
    // green threads, each with a heap of its own, and the channels between them
    "threadSpawn",
    "threadAwait",
    "threadYield",
    "channelNew",
    "channelSend",
    "channelReceive", //
    // software transactional memory: the parts `Std.Stm` cannot write itself
    "stmNew",
    "stmNewIO",
    "stmRead",
    "stmWrite",
    "stmBegin",
    "stmCommit",
    "stmWait",
    "stmNest",
    "stmMerge",
    "stmRollback", //
    // the strings of an array, one after another: what `"a ${b} c"` becomes
    "concatStrings",
    // a string's bytes, read in place
    "stringByteLength",
    "stringByteAt",
    "stringSlice",
    "stringIndexOf",
    "stringCompare",
];
use std::ops::Deref;

/// A stable identifier for every node/subnode in the HIR tree.
///
/// Ids are dense and allocated per program by [`NodeIdGen`], so downstream passes
/// (type inference in particular) can keep their results in cheap `Vec`-backed
/// side tables indexed by `NodeId.0` and hand back a fully annotated tree.
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone)]
pub struct NodeIdGen {
    next: u32,
}

impl NodeIdGen {
    pub fn new() -> Self {
        Self { next: 0 }
    }

    /// Start handing out ids from `n` — used when several modules of one package
    /// share a single dense id space (and one [`crate`]-wide side table).
    pub fn starting_at(n: usize) -> Self {
        Self { next: n as u32 }
    }

    pub fn fresh(&mut self) -> NodeId {
        let id = NodeId(self.next);
        self.next += 1;
        id
    }

    /// Number of ids handed out so far — i.e. the length a dense side table needs.
    pub fn count(&self) -> usize {
        self.next as usize
    }
}

impl Default for NodeIdGen {
    fn default() -> Self {
        Self::new()
    }
}

/// HIR spine wrapper: like `span::Located` but additionally carries a [`NodeId`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Node<T> {
    pub id: NodeId,
    pub span: Span,
    pub value: Box<T>,
}

impl<T> Node<T> {
    pub fn new(id: NodeId, value: T, span: Span) -> Self {
        Self {
            id,
            span,
            value: Box::new(value),
        }
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn id(&self) -> NodeId {
        self.id
    }
}

impl<T> Deref for Node<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

pub type LModule = Node<Module>;

/// Label for a record field. Carries a [`NodeId`]/span like any other node, but is
/// a plain name rather than a resolved variable.
pub type Label = Node<InternedString>;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Module {
    pub name: InternedString,
    pub decls: Vec<LDecl>,
    /// Top-level binding groups, in dependency order — see [`BindGroup`].
    ///
    /// Filled by `meadow-scc`, which also reorders `decls` to match. Empty
    /// before that pass runs, which a consumer should read as "no grouping
    /// information", not "no bindings".
    pub groups: Vec<BindGroup>,
    /// Each constructor given its fields by name in another order than its
    /// type declares them -- `Point { y = …, x = … }` -- as the [`Expr::Cons`]
    /// whose arguments are in the declared order, and the position there of
    /// each field in the order it was written. The fields are evaluated in
    /// that order, as everything else is evaluated in the order it is written.
    pub written_orders: Vec<(NodeId, Vec<u32>)>,
}

/// One strongly connected component of the top-level binding dependency graph.
///
/// Bindings that refer to one another have to be typed together: each is
/// monomorphic while the group is being solved, and all of them generalize once
/// it is. Ordering the groups by dependency means every binding a group refers
/// to is already generalized by the time the group is inferred, which is what
/// makes a helper usable at two different types.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BindGroup {
    /// Indices into [`Module::decls`]. Every one names a [`Decl::Bind`].
    pub members: Vec<usize>,
    /// Whether any member refers to the group — a self- or mutual recursion.
    /// A lone non-recursive binding needs no monomorphic seeding.
    pub recursive: bool,
}

pub type LDecl = Node<Decl>;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Decl {
    Bind(Bind),
    /// `mod Foo` — declares a child module. Consumed by the driver; inert here.
    Mod(InternedString),
    /// `use a.b.c` — recorded for the (package-wide) resolver; carries no runtime weight.
    Use(Vec<InternedString>),
    /// `data Node a = …`
    Data(DataDecl),
    /// `record Person = { … }`
    Record(RecordDecl),
    /// `effect State s { … }`
    Effect(EffectDecl),
    /// `type Span = (Int, Int)` -- a name for a type. Inference expands it
    /// wherever it is written; nothing after that knows it was there.
    Alias(AliasDecl),
    /// `effect Eff = { Console, Fs }` -- a name for effects. Inference expands
    /// it wherever a row names it, as it does a type alias.
    EffectAlias(EffectAliasDecl),
    /// `fun f : T` -- the declared type of the top-level binding `f`, which it
    /// must be at least as general as.
    ///
    /// With the traits its `where` asks of its type variables.
    Sig(Ident, LTypeExpr, Vec<Bound>),
    /// `trait Show a { … }`
    Trait(TraitDecl),
    /// `impl Show Int { … }`
    Impl(ImplDecl),
    Error,
}

/// `Show a` in a `where`: the types have to implement the trait `tr` (its
/// canonical name, where it was written) -- one type, or one per parameter of
/// a trait of several.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bound {
    pub tr: Label,
    pub tys: Vec<LTypeExpr>,
}

/// A trait: what a type has to provide to implement it.
///
/// At run time a trait is a *dictionary* -- one constructor, [`TraitDecl::dict`],
/// holding the dictionaries of the traits it requires and then its methods, in
/// the order they are declared here. A method is a top-level value that takes
/// the dictionary and answers the field.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TraitDecl {
    /// Canonical, as a type's name is: it is also the dictionary's type.
    pub name: InternedString,
    pub name_span: Span,
    /// The type variables the trait is about: one, usually.
    pub params: Vec<Ident>,
    /// The traits the implementing types must implement too, canonical, each
    /// with the parameters of this trait it is asked of, by position.
    pub supers: Vec<(Label, Vec<usize>)>,
    /// Associated types, canonical: each a type the implementation chooses.
    pub assocs: Vec<Label>,
    /// Associated effects, canonical: each an effect row the implementation
    /// chooses, of the trait's parameters as an associated type is.
    pub effects: Vec<Label>,
    pub methods: Vec<TraitMethod>,
    /// The canonical name of the dictionary's constructor.
    pub dict: InternedString,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TraitMethod {
    pub name: InternedString,
    /// The top-level value a program calls.
    pub var: Ident,
    /// Its type, over the trait's parameter and associated types.
    pub ty: LTypeExpr,
    /// What an implementation that leaves it out gets: a function of the
    /// trait's dictionary, bound to [`DefaultMethod::var`].
    pub default: Option<DefaultMethod>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DefaultMethod {
    pub var: VarId,
    /// `None` in a dependency's declaration, which has been compiled already.
    pub body: Option<Bind>,
}

/// One type's implementation of a trait.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImplDecl {
    pub tr: Label,
    /// The implementing types, one per parameter of the trait: each a
    /// constructor applied to distinct variables.
    pub tys: Vec<LTypeExpr>,
    /// What those variables must implement.
    pub context: Vec<Bound>,
    /// `(associated type, what it is here)`.
    pub assocs: Vec<(Label, LTypeExpr)>,
    /// `(associated effect, the effects it is here)`.
    pub effects: Vec<(Label, EffectRow)>,
    /// `(method name, its definition)`. Empty in a dependency's declaration.
    pub methods: Vec<(InternedString, Bind)>,
    /// The top-level value that is this implementation's dictionary -- or the
    /// function from its context's dictionaries to it.
    pub dict: Ident,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AliasDecl {
    pub name: InternedString,
    /// Where the name was written — see [`DataDecl::name_span`].
    pub name_span: Span,
    pub params: Vec<Ident>,
    pub ty: LTypeExpr,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EffectAliasDecl {
    pub name: InternedString,
    /// Where the name was written — see [`DataDecl::name_span`].
    pub name_span: Span,
    pub params: Vec<Ident>,
    /// What it stands for: labels only, as no row it is spliced into could
    /// say what a tail of its own should join.
    pub row: EffectRow,
}

pub type LTypeExpr = Node<TypeExpr>;

/// A resolved type expression from a `data` / `record` declaration. Type-variable
/// names are resolved to [`VarId`]s (fresh per declaration); type-constructor names
/// stay interned strings and are validated against the tycon environment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TypeExpr {
    Var(Ident),
    Con(Label, Vec<LTypeExpr>),
    /// `arg -> ret ! effect` (curried; effect on the last arrow).
    ///
    /// With no parameters it is a declared result and its effect, `: R ! e`
    /// after a function's inline parameters -- only ever there.
    Fun(Vec<LTypeExpr>, LTypeExpr, Option<EffectRow>),
    Tuple(Vec<LTypeExpr>),
    /// `[a]` — the default sequence, an RRB `Vector`.
    Vector(LTypeExpr),
    /// `[a;]` — a linked `List`.
    List(LTypeExpr),
    /// `{ name : String | r }` — a structural record, closed when `tail` is
    /// `None`. The tail is a row variable.
    Record(Vec<(Label, LTypeExpr)>, Option<Ident>),
    /// An effect row where a trait is given one for a parameter that is an
    /// effect: `impl Rows (Writing s) { St s | e }`.
    Row(EffectRow),
    /// A type the resolver could not make sense of: unknown, given the wrong
    /// number of arguments, or an unbound variable. It has already been
    /// reported, and inference reads it as the error type, which agrees with
    /// everything, so the one mistake is not reported again further on.
    Error,
}

/// A resolved effect annotation. `labels` names are effect constructors; `tail`
/// (if any) is a resolved type variable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EffectRow {
    pub labels: Vec<(InternedString, Vec<LTypeExpr>)>,
    pub tail: Option<Ident>,
    /// A trait's associated effect, at the types it is of: whatever its
    /// implementation there says, as the rest of the row. A row has this or
    /// a `tail`, not both: there is one "rest".
    pub assoc: Option<(InternedString, Vec<LTypeExpr>)>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EffectDecl {
    pub name: InternedString,
    /// Where the name was written — see [`DataDecl::name_span`].
    pub name_span: Span,
    pub params: Vec<Ident>,
    /// Each operation: `(name, its top-level VarId, declared type)`. The op is
    /// callable as a value, so it gets a `VarId` like a `def`.
    pub ops: Vec<(InternedString, Ident, LTypeExpr)>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HandlerArm {
    /// The effect this operation belongs to (resolved from `op`).
    pub effect: InternedString,
    pub op: InternedString,
    pub param: LPat,
    pub resume: Ident,
    pub body: LExpr,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DataDecl {
    pub name: InternedString,
    /// Where the name was written.
    ///
    /// A type constructor is not resolved to an id — see [`TypeExpr::Con`] — so
    /// a reference to one is matched by name, and a name cannot say *where* the
    /// declaration is. An editor asked to go there needs to know, so the
    /// position is carried rather than recovered.
    pub name_span: Span,
    pub params: Vec<Ident>,
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Variant {
    pub name: InternedString,
    /// Where the name was written — see [`DataDecl::name_span`].
    pub name_span: Span,
    pub fields: VariantFields,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VariantFields {
    Positional(Vec<LTypeExpr>),
    Named(Vec<(InternedString, LTypeExpr)>),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordDecl {
    pub name: InternedString,
    /// The canonical name of this record's one constructor -- `Person.Person`.
    ///
    /// Spelled out rather than derived, so that inference, lowering and the
    /// exhaustiveness checker cannot each reinvent the convention and disagree
    /// about it. A `data` variant carries its canonical name the same way.
    pub ctor: InternedString,
    /// Where the name was written — see [`DataDecl::name_span`].
    pub name_span: Span,
    pub params: Vec<Ident>,
    pub fields: Vec<(InternedString, LTypeExpr)>,
}

pub type LExpr = Node<Expr>;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Expr {
    Var(Ident),
    Lit(Lit),
    Lam(Vec<LPat>, LExpr),
    App(LExpr, Vec<LExpr>),
    Let(Vec<Bind>, LExpr),
    If(LExpr, LExpr, LExpr),
    /// Each arm a pattern, the guard it is taken on if any, and its body.
    Match(LExpr, Vec<(LPat, Option<LExpr>, LExpr)>),
    Tuple(Vec<LExpr>),
    /// `#[e, ...]` -- a builtin `Array` literal.
    Array(Vec<LExpr>),
    List(Vec<LExpr>),
    /// Data-constructor application. Constructors are not resolved to [`VarId`]s yet
    /// (no `data` decls), so the name is kept as an opaque [`Label`].
    Cons(Label, Vec<LExpr>),
    /// `{ x = e, y = e | base }` — the optional trailing expr is a record to extend.
    Record(Vec<(Label, LExpr)>, Option<LExpr>),
    /// `e.label`
    Field(LExpr, Label),
    /// `{ e | x = v }` -- `e` with the fields named replaced, each keeping its
    /// type.
    Update(LExpr, Vec<(Label, LExpr)>),
    /// `handle e with { … }`
    Handle(LExpr, Vec<HandlerArm>, Option<(LPat, LExpr)>),
    Unit,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Bind {
    Pat(LPat, LExpr),
    /// `fun f a (x, y) = e` — parameters are irrefutable patterns.
    /// `fun f p q : T = e` -- the optional type is the declared *result*.
    Fun(Ident, Vec<LPat>, Option<LTypeExpr>, LExpr),
    Error,
}

impl Bind {
    /// Every name this binding introduces, in source order.
    pub fn bound_vars(&self) -> Vec<VarId> {
        let mut out = Vec::new();
        match self {
            Bind::Fun(name, ..) => out.push(*name.value()),
            Bind::Pat(pat, _) => pat_vars(pat, &mut out),
            Bind::Error => {}
        }
        out
    }
}

/// Every name a pattern introduces, in source order, appended to `out`.
pub fn pat_vars(pat: &LPat, out: &mut Vec<VarId>) {
    match pat.value() {
        Pat::Var(id) => out.push(*id.value()),
        // The annotation binds nothing; the pattern under it does.
        Pat::Ann(inner, _) => pat_vars(inner, out),
        Pat::As(id, sub) => {
            out.push(*id.value());
            pat_vars(sub, out);
        }
        Pat::Tuple(ps) | Pat::List(ps) | Pat::Array(ps) | Pat::Cons(_, ps) => {
            ps.iter().for_each(|p| pat_vars(p, out))
        }
        Pat::Record(fields, _) => fields.iter().for_each(|(_, p)| pat_vars(p, out)),
        // The view's expression binds nothing outside itself.
        Pat::View(_, p) => pat_vars(p, out),
        Pat::Wildcard | Pat::Lit(_) | Pat::Unit | Pat::Error => {}
    }
}

pub type LPat = Node<Pat>;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Pat {
    Wildcard,
    Var(Ident),
    /// `(p : T)` -- a pattern with a declared type. The annotation is kept
    /// rather than erased: inference unifies against it, and it is the only
    /// type an editor can point at outside a declaration.
    Ann(Box<LPat>, LTypeExpr),
    Lit(Lit),
    As(Ident, LPat),
    Cons(Label, Vec<LPat>),
    Tuple(Vec<LPat>),
    /// `#[p, ...]` -- matches a builtin `Array` of exactly this length.
    Array(Vec<LPat>),
    List(Vec<LPat>),
    /// `{ x, y = p | _ }` — `open` is true when the pattern ends in `| _`.
    Record(Vec<(Label, LPat)>, bool),
    Unit,
    /// `(f -> p)` -- `f` applied to what is matched here, and its answer
    /// matched against `p`.
    View(LExpr, LPat),
    Error,
}

pub type Ident = Node<VarId>;

pub use meadow_axcut::{SYNTHETIC_BASE, VarId};

/// Hands out the `VarId`s of one compilation unit.
///
/// Deliberately not a global counter, which is what this used to be. A
/// `VarId`'s number then depended on how many had been minted *anywhere in the
/// process*, so the same package compiled twice — in a different order, or in
/// another process — came out with different ids. That is invisible while
/// everything is rebuilt from scratch every time, and it is precisely what
/// would stop a compiled package from being written to disk and read back: the
/// ids would mean something different on the way in, silently, rather than
/// failing.
///
/// A unit is seeded from a base that clears its dependencies (see
/// `CompiledPackage::vars`), so its ids depend only on the unit itself and on
/// how much space the packages under it needed.
#[derive(Debug, Clone, Copy)]
pub struct VarIdGen {
    next: u32,
}

impl VarIdGen {
    /// A generator whose first `VarId` is `base`.
    pub const fn starting_at(base: u32) -> VarIdGen {
        VarIdGen { next: base }
    }

    pub fn fresh(&mut self) -> VarId {
        let v = VarId(self.next);
        self.next += 1;
        v
    }

    /// One past the last id handed out — the base a unit stacked on top of this
    /// one must start at.
    pub const fn end(self) -> u32 {
        self.next
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Lit {
    /// Fixed-width integer (`Int`, i.e. i64). No `BigInt` literal — see [`PRIMS`].
    Int(i64),
    /// Float literal as its IEEE-754 bit pattern (see `ast::Lit::Float`).
    Float(u64),
    String(InternedString),
    Char(char),
}

// ===========================================================================
// Overloaded names
// ===========================================================================

/// One thing a bare name could mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Alt {
    /// A top-level value, by id.
    Value(VarId),
    /// A data constructor, by its canonical `Type.Ctor` name.
    Ctor(InternedString),
}

/// A candidate for an overloaded name, with where it came from -- which a
/// person needs in order to pick one when inference cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub alt: Alt,
    /// The spelling that was written, which every candidate shares.
    pub name: InternedString,
    /// The module it was declared in, or `None` for the module being compiled.
    pub origin: Option<InternedString>,
}

/// A bare name the resolver found more than one meaning for, keyed by the node
/// the name was written at: the `Ident` of an `Expr::Var`, or the label of an
/// `Expr::Cons` / `Pat::Cons`.
///
/// The HIR still holds *a* meaning at that node -- the first candidate -- so a
/// pass that never looks at this table sees a well-formed tree. Inference
/// decides which candidate the types allow, and [`apply_resolutions`] writes
/// that one back.
pub type Overloads = std::collections::HashMap<NodeId, Vec<Candidate>>;

/// Replace each overloaded name with the candidate inference chose for it.
pub fn apply_resolutions(module: &mut LModule, chosen: &std::collections::HashMap<NodeId, Alt>) {
    if chosen.is_empty() {
        return;
    }
    for decl in &mut module.value.decls {
        if let Decl::Bind(b) = &mut *decl.value {
            rewrite_bind(b, chosen);
        }
    }
}

fn rewrite_bind(b: &mut Bind, chosen: &std::collections::HashMap<NodeId, Alt>) {
    match b {
        Bind::Pat(p, e) => {
            rewrite_pat(p, chosen);
            rewrite_expr(e, chosen);
        }
        Bind::Fun(_, ps, _, e) => {
            ps.iter_mut().for_each(|p| rewrite_pat(p, chosen));
            rewrite_expr(e, chosen);
        }
        Bind::Error => {}
    }
}

fn rewrite_expr(e: &mut LExpr, chosen: &std::collections::HashMap<NodeId, Alt>) {
    match &mut *e.value {
        Expr::Var(ident) => {
            if let Some(Alt::Value(v)) = chosen.get(&ident.id) {
                *ident.value = *v;
            }
        }
        Expr::Cons(label, args) => {
            if let Some(Alt::Ctor(c)) = chosen.get(&label.id) {
                *label.value = *c;
            }
            args.iter_mut().for_each(|a| rewrite_expr(a, chosen));
        }
        Expr::Lam(ps, body) => {
            ps.iter_mut().for_each(|p| rewrite_pat(p, chosen));
            rewrite_expr(body, chosen);
        }
        Expr::App(f, args) => {
            rewrite_expr(f, chosen);
            args.iter_mut().for_each(|a| rewrite_expr(a, chosen));
        }
        Expr::Let(binds, body) => {
            binds.iter_mut().for_each(|b| rewrite_bind(b, chosen));
            rewrite_expr(body, chosen);
        }
        Expr::If(c, t, f) => {
            rewrite_expr(c, chosen);
            rewrite_expr(t, chosen);
            rewrite_expr(f, chosen);
        }
        Expr::Match(s, arms) => {
            rewrite_expr(s, chosen);
            for (p, g, b) in arms {
                rewrite_pat(p, chosen);
                if let Some(g) = g {
                    rewrite_expr(g, chosen);
                }
                rewrite_expr(b, chosen);
            }
        }
        Expr::Tuple(xs) | Expr::Array(xs) | Expr::List(xs) => {
            xs.iter_mut().for_each(|x| rewrite_expr(x, chosen))
        }
        Expr::Record(fields, base) => {
            fields.iter_mut().for_each(|(_, x)| rewrite_expr(x, chosen));
            if let Some(b) = base {
                rewrite_expr(b, chosen);
            }
        }
        Expr::Field(o, _) => rewrite_expr(o, chosen),
        Expr::Update(base, fields) => {
            rewrite_expr(base, chosen);
            fields.iter_mut().for_each(|(_, x)| rewrite_expr(x, chosen));
        }
        Expr::Handle(body, arms, ret) => {
            rewrite_expr(body, chosen);
            for arm in arms {
                rewrite_pat(&mut arm.param, chosen);
                rewrite_expr(&mut arm.body, chosen);
            }
            if let Some((p, b)) = ret {
                rewrite_pat(p, chosen);
                rewrite_expr(b, chosen);
            }
        }
        Expr::Lit(_) | Expr::Unit | Expr::Error => {}
    }
}

fn rewrite_pat(p: &mut LPat, chosen: &std::collections::HashMap<NodeId, Alt>) {
    match &mut *p.value {
        Pat::Cons(label, args) => {
            if let Some(Alt::Ctor(c)) = chosen.get(&label.id) {
                *label.value = *c;
            }
            args.iter_mut().for_each(|a| rewrite_pat(a, chosen));
        }
        Pat::Ann(inner, _) => rewrite_pat(inner, chosen),
        Pat::As(_, sub) => rewrite_pat(sub, chosen),
        Pat::Tuple(ps) | Pat::Array(ps) | Pat::List(ps) => {
            ps.iter_mut().for_each(|x| rewrite_pat(x, chosen))
        }
        Pat::Record(fields, _) => fields.iter_mut().for_each(|(_, x)| rewrite_pat(x, chosen)),
        Pat::View(f, p) => {
            rewrite_expr(f, chosen);
            rewrite_pat(p, chosen);
        }
        Pat::Wildcard | Pat::Unit | Pat::Var(_) | Pat::Lit(_) | Pat::Error => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_canonical_name_is_spelled_without_its_path() {
        assert_eq!(spelling("app.Syntax.Tree.Expr"), "Expr");
        assert_eq!(spelling("anstyle.Color"), "Color");
        assert_eq!(spelling("Maybe"), "Maybe");
        assert_eq!(ctor_spelling("app.Syntax.Tree.Expr.Lit"), "Expr.Lit");
        assert_eq!(ctor_spelling("Maybe.Just"), "Maybe.Just");
    }

    #[test]
    fn a_canonical_name_says_its_package() {
        assert_eq!(type_package("Std.Ops.Add"), Some("Std"));
        assert_eq!(type_package("anstyle.Color"), Some("anstyle"));
        assert_eq!(type_package("json@1.2.0.Value"), Some("json@1.2.0"));
        assert_eq!(
            type_package("json@1.2.0-rc.1.Data.Value"),
            Some("json@1.2.0-rc.1")
        );
        assert_eq!(type_package("Int"), None);
    }
}
