//! Name resolution: `ast` -> `hir`.
//!
//! Turns surface identifiers into [`VarId`]s and stamps every HIR node with a
//! [`hir::NodeId`] (via [`Resolver::node`]) so later passes can hang side tables
//! off the tree.
//!
//! The resolver knows nothing about packages or interactivity. A caller feeds it
//! the modules of one compilation unit; if the unit has several modules that may
//! refer to each other, the caller first [`Resolver::declare_toplevel`]s every
//! module so the bodies resolve against the union of their top-level names.

use itertools::Itertools;
use meadow_ast as ast;
use meadow_diagnostics::Diagnostic;
use meadow_hir::{self as hir, NodeIdGen, PRIMS, VarId, VarIdGen};
use meadow_intern::InternedString;
use meadow_span::Span;
use std::collections::HashMap;

/// What the resolver remembers about a data / record constructor, enough to
/// desugar named-field syntax and reject unknown constructors.
#[derive(Debug, Clone)]
pub struct CtorInfo {
    pub arity: usize,
    /// `Some` for record constructors and named `data` variants.
    pub field_names: Option<Vec<InternedString>>,
}

#[derive(Debug, Clone)]
pub struct Resolver {
    filename: String,
    /// Files embedded with `includeStr`, and what each hashed to when it was
    /// read. A build cannot know these from a module's own text -- what it
    /// embeds is only clear once it has been resolved -- so they are reported
    /// for the cache to check next time.
    embedded: Vec<(String, u64)>,
    /// Lexical scope stack: `(name, id)`, innermost last.
    scope: Vec<(InternedString, VarId)>,
    /// Every id we've ever bound, for debugging / pretty-printing.
    names: HashMap<VarId, InternedString>,
    /// Top-level names bound ahead of time by [`declare_toplevel`], so mutually
    /// recursive definitions resolve to a single id.
    predeclared: HashMap<(Vec<InternedString>, InternedString), VarId>,
    /// Type constructors in scope: the name as written -> what it means.
    /// Seeded with the builtins.
    tycons: HashMap<InternedString, Named>,
    /// The package whose types this unit declares, which qualifies their
    /// canonical names: `None` for the standard library. See [`Resolver::qualify`].
    package: Option<InternedString>,
    /// Built with no dependencies, so without the standard library: the unit
    /// declares for itself the language names the standard library would --
    /// `Maybe` for a primitive's type, `Stm` for the handlers of its effect.
    standalone: bool,
    /// Canonical names of the types this unit declares, for the duplicate check.
    declared_types: std::collections::HashSet<InternedString>,
    /// Every type known to the unit, in scope or not: canonical name -> arity.
    all_types: HashMap<InternedString, usize>,
    /// Every effect known to the unit: canonical name -> arity.
    all_effects: HashMap<InternedString, usize>,
    /// Every trait known to the unit, by canonical name. A trait's name is a
    /// type's name too -- its dictionary's -- so it is scoped, imported and
    /// made ambiguous exactly as a type is, through `tycons`; this says which
    /// of those names are traits, and what each declares.
    all_traits: HashMap<InternedString, TraitInfo>,
    /// Associated type -> the trait it belongs to, both canonical.
    all_assocs: HashMap<InternedString, InternedString>,
    /// Every associated effect, canonical, and the trait it is of.
    all_assoc_effects: HashMap<InternedString, InternedString>,
    /// The dictionaries of this unit's `impl`s, and the default methods of its
    /// traits: values no program names, which every dependent still needs.
    hidden_exports: Vec<VarId>,
    /// Every constructor known here, keyed by its **canonical** name --
    /// `Type.Ctor`, the one thing about it that is unique across a program.
    ///
    /// Keyed this way rather than by the bare name so that two types may each
    /// have a `Leaf`. Everything downstream -- `ctor_fields`, the back end's
    /// tag table, exhaustiveness -- inherits that uniqueness for free, because
    /// what they receive is this name.
    ctors: HashMap<InternedString, CtorInfo>,
    /// Bare name -> canonical, for the constructors this module may write
    /// unqualified: the ones its own types declare, plus whatever a
    /// `use M.Type (C)` or `use M.Type.*` brought in. A name absent here must be
    /// written `Type.Ctor`.
    ///
    /// Only the *base* layer lives here once the scope is sealed -- the
    /// builtins and what the dependencies flattened -- and a module's own and
    /// `use`d constructors go in [`Resolver::module_ctors`] instead, where one
    /// spelling may name several.
    visible_ctors: HashMap<InternedString, InternedString>,
    /// Bare name -> every constructor of that spelling the current module
    /// declares or explicitly imports. More than one is an overload, which
    /// inference settles by type.
    module_ctors: HashMap<InternedString, Vec<InternedString>>,
    /// Whether [`Resolver::seal_base_scope`] has run: from then on, what comes
    /// into scope is a module's own, and may overload rather than shadow.
    sealed: bool,
    /// Scope length once a module's own items and its `use`s are in: what is
    /// below is the module layer (down to `base_scope`), what is above is local.
    module_scope: usize,
    /// Names written bare that have more than one meaning here -- see
    /// [`hir::Overloads`].
    overloads: hir::Overloads,
    /// The module a dependency's value was imported from, for naming the
    /// candidates of an overload.
    origins: HashMap<VarId, InternedString>,
    /// Where each top-level name of each module was first declared, so that a
    /// second declaration can point at the first.
    decl_spans: HashMap<(Vec<InternedString>, InternedString), Span>,
    /// Where a second declaration of a name was written. Such a declaration is
    /// dropped once reported -- see [`Resolver::resolve_bind`].
    duplicates: std::collections::HashSet<Span>,
    /// Type -> its constructors, bare names, in declaration order. What `use`
    /// of a type consults, and what an exhaustiveness message would list.
    ctors_of: HashMap<InternedString, Vec<InternedString>>,
    /// Effects in scope: the name as written -> what it means.
    effects: HashMap<InternedString, Named>,
    /// Operation name -> the effect it belongs to, by canonical name.
    effect_ops: HashMap<InternedString, InternedString>,
    /// Operation `VarId` -> `(effect, operation)`. Keyed by id, not name: an
    /// ordinary value can share an operation's name and shadow it in scope —
    /// `Std.State.get` against the prelude's `Vector.get`, say — and the lowerer
    /// has to tell a `perform` from a variable by identity, not spelling.
    effect_op_ids: HashMap<VarId, (InternedString, InternedString)>,
    /// What each module of this unit declares, by dotted path.
    ///
    /// A package is one compilation unit — its modules may be mutually
    /// recursive and share a `NodeId` space — but each module is its own
    /// *namespace*. It sees the prims, whatever the prelude flattened, its own
    /// items, and whatever it `use`s; never a sibling's names for free. Rust
    /// draws the line in the same place, and it is what gives `use Pack.Mod`
    /// something to name.
    frames: HashMap<Vec<InternedString>, ModuleFrame>,
    /// What every module of the unit starts from: the builtins plus whatever
    /// the dependencies brought in, snapshotted by [`Resolver::seal_base_scope`].
    /// A module's own declarations are laid on top of this and nothing else.
    base_tycons: HashMap<InternedString, Named>,
    base_ctors: HashMap<InternedString, InternedString>,
    base_effects: HashMap<InternedString, Named>,
    base_effect_ops: HashMap<InternedString, InternedString>,
    /// The module being declared into, or resolved.
    current: Vec<InternedString>,
    /// Scope length before any module's own items: the prims and the prelude,
    /// which every module in the unit sees. `enter_module` truncates to this.
    base_scope: usize,
    /// Type variables of the declaration currently being resolved: a `data` /
    /// `record` / `effect` parameter list, or the ones a pattern annotation
    /// introduced.
    tyvars: Vec<(InternedString, VarId)>,
    /// While resolving a pattern annotation, an unknown type variable is bound
    /// rather than reported.
    ///
    /// `(x : a)` has no parameter list to have declared `a` in, so the
    /// alternative to binding it here is refusing to let anyone write it. It is
    /// scoped to the enclosing declaration, which is what makes the two `a`s in
    /// `fun twice (f : a -> a) (x : a)` the same variable.
    open_tyvars: bool,
    /// The bindings a standalone signature has been given, so a second one is
    /// reported.
    sigs: std::collections::HashSet<VarId>,
    ids: NodeIdGen,
    /// This unit's `VarId`s. Seeded by the driver from a base that clears the
    /// unit's dependencies -- see [`meadow_hir::VarIdGen`].
    vars: VarIdGen,
    /// True only while resolving a module-level `Decl` (not nested in an expr).
    toplevel: bool,
    /// The visibility of the declaration being declared, so that the places
    /// that record a name in its module's frame do not each have to be handed
    /// one. Set by [`Resolver::declare_types`] / [`Resolver::declare_toplevel`].
    vis: Vis,
    /// Export bookkeeping. If `any_vis` stays false — no visibility attribute
    /// anywhere in the unit — everything is exported, which is what a script,
    /// a REPL line and an unannotated package all want. Otherwise only
    /// `@pub` declarations are, plus `@pub use` re-exports.
    any_vis: bool,
    pub_vars: std::collections::HashSet<VarId>,
    pub_types: std::collections::HashSet<InternedString>,
    /// References the HIR will not carry — see [`RefSite`]. Collected per
    /// module and taken by the driver after each one is resolved.
    extra_refs: Vec<RefSite>,
    /// `@test` functions, in declaration order — `meadow test` runs these.
    test_vars: Vec<(InternedString, VarId)>,
    /// The `@macro` declarations: what a call may name with a `!`, and where
    /// each was written, so that a function whose type could never be a
    /// macro's is reported there rather than where someone tries to use it.
    macro_vars: Vec<(VarId, Span)>,
    /// Active module qualifiers: `Foo` -> its exported value names. Populated by the
    /// driver from this module's `mod` children and `use`d modules; consulted when
    /// resolving `Foo.name`.
    qualifiers: HashMap<InternedString, HashMap<InternedString, VarId>>,
    /// The qualifiers that are a module of this unit under the current one,
    /// and which: `Core.Expr` -> its path. What lets `Core.Expr.Int` be a
    /// constructor of the type `Expr` that module `Core` declares.
    qualifier_paths: HashMap<InternedString, Vec<InternedString>>,
    /// The types each qualifier reaches, by how they are spelled: `M.Ty` in a
    /// type is the type `Ty` of the module `M` qualifies, with its arity.
    qualifier_types: HashMap<InternedString, HashMap<InternedString, (InternedString, usize)>>,
    /// How every operator binds that something declared: this unit's own
    /// `infixl 6 +` and its dependencies'. One table for the whole unit,
    /// because an operator's fixity goes by its spelling, not by which
    /// definition of it is in scope -- the grouping of `a + b * c` has to be
    /// settled before anything is looked up. The language's own operators are
    /// in [`hir::OPERATORS`], which a declaration may repeat but not contradict.
    fixities: HashMap<InternedString, (hir::Fixity, Option<Span>)>,
    /// The prelude: what the standard library brings every module unqualified,
    /// by name. What a name a built-in derive wrote means (see
    /// [`ast::hygiene::PRELUDE`]).
    prelude: HashMap<InternedString, VarId>,
    errors: Vec<Diagnostic>,
}

/// Constructors that are always available (see `infer::ctor_type` / `core`). The
/// `Std` prelude also declares `data List` / `data Bool` with these variants, so a
/// re-declaration of one of these names is tolerated rather than an error.
/// Constructors the *language* depends on, always in scope unqualified.
///
/// `if` needs `True` / `False`, and `[a; b]` and `::` desugar to `Cons` / `Nil`
/// — so requiring `use Std.Bool (Bool)` before an `if` would be absurd. Every
/// other constructor follows the ordinary rule: qualified, or brought in by a
/// `use` of its type.
const BUILTIN_CTORS: &[&str] = &["Nil", "Cons", "True", "False"];

/// The same, paired with the type that owns them, to seed the scope.
const BUILTIN_CTOR_OWNERS: &[(&str, &str)] = &[
    ("List", "Nil"),
    ("List", "Cons"),
    ("Bool", "True"),
    ("Bool", "False"),
];

/// Type constructors seeded into every resolver. Like [`BUILTIN_CTORS`], the
/// prelude is allowed to (re-)declare `List` / `Bool` without it counting as a
/// duplicate-definition error.
const BUILTIN_TYCONS: &[&str] = &[
    "Int", "BigInt", "Float", "String", "Char", "Bool", "Unit", "List", "Array", "Ref", "StRef",
    "StArray", "Int64", "Int32", "Int16", "Int8", "UInt64", "UInt32", "UInt16", "UInt8", "Float64",
    "Float32", "Compact", "Task", "Channel", "TVar",
];

/// Split a declaration into its attributes and the bare declaration underneath.
/// The parser only ever nests one `Attributed` layer.
fn peel(d: &ast::LDecl) -> (&[ast::Attr], &ast::LDecl) {
    match d.value() {
        ast::Decl::Attributed(attrs, inner) => (attrs, inner),
        _ => (&[], d),
    }
}

/// How far out of its own module a declaration can be seen.
///
/// Rust's arrangement, with the package in the place of the crate — spellings
/// included, so `@pub(pkg)` is `pub(crate)` and a plain `@pub` is `pub`:
///
/// | written            | seen by                                   |
/// |--------------------|-------------------------------------------|
/// | nothing            | its own module and the modules inside it  |
/// | `@pub(super)`      | ...and its parent's subtree               |
/// | `@pub(pkg)`        | ...and every module of the package        |
/// | `@pub`             | ...and whoever depends on the package     |
///
/// A parenthesised argument always *narrows* `@pub`, as it does in Rust. It
/// used to be the other way round — plain `@pub` stopped at the package and
/// `@pub(pack)` was the public one — which read backwards to anyone coming from
/// Rust, and made `(super)` narrow while `(pack)` widened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Vis {
    Private,
    Super,
    Package,
    Exported,
}

impl Vis {
    /// Can a declaration in `owner` with this visibility be named from `from`?
    ///
    /// "Inside" is by path prefix: `a.b.c` is inside `a.b`, so a private
    /// declaration is visible to its own module's descendants, and
    /// `@pub(super)` reaches the parent and everything under it.
    fn reaches(self, owner: &[InternedString], from: &[InternedString]) -> bool {
        match self {
            Vis::Exported | Vis::Package => true,
            Vis::Super => {
                let parent = owner.split_last().map(|(_, p)| p).unwrap_or(&[]);
                from.starts_with(parent)
            }
            Vis::Private => from.starts_with(owner),
        }
    }

    /// How to say, in a sentence about a module, what this keeps a name to.
    fn describe_in(self) -> &'static str {
        match self {
            Vis::Private => "private to module",
            Vis::Super => "visible only to the parent of module",
            Vis::Package => "visible only within the package of module",
            Vis::Exported => "public in module",
        }
    }

    /// The attribute that would let one more layer of the program see it.
    fn wider(self) -> &'static str {
        match self {
            Vis::Private => "@pub(pkg)",
            Vis::Super => "@pub(pkg)",
            Vis::Package | Vis::Exported => "@pub",
        }
    }
}

/// A module path as it is written: `Collections.List`, or `the root module`
/// when there is nothing to write.
fn dotted_path(path: &[InternedString]) -> String {
    if path.is_empty() {
        "the root module".to_string()
    } else {
        path.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(".")
    }
}

/// The visibility a declaration's attributes ask for, and the span of the
/// `@pub(…)` argument when it is one this does not understand.
fn vis_of(attrs: &[ast::Attr]) -> (Vis, Option<(InternedString, Span)>) {
    let Some(a) = attrs.iter().find(|a| &**a.name.value() == "pub") else {
        return (Vis::Private, None);
    };
    match a.args.first() {
        None => (Vis::Exported, None),
        Some(arg) => match &**arg.value() {
            "pkg" => (Vis::Package, None),
            "super" => (Vis::Super, None),
            // Reported by the caller, which knows how to say what it became.
            // Treated as what it used to mean, so the one error is the only
            // one: resolving it as anything narrower would add an "undefined"
            // in every dependent that names it.
            "pack" => (Vis::Exported, Some((*arg.value(), arg.span))),
            // Package-wide for anything else unknown: narrower than a guess
            // that it was meant to be public.
            _ => (Vis::Package, Some((*arg.value(), arg.span))),
        },
    }
}

fn has_test(attrs: &[ast::Attr]) -> bool {
    attrs.iter().any(|a| &**a.name.value() == "test")
}

fn has_macro(attrs: &[ast::Attr]) -> bool {
    attrs.iter().any(|a| &**a.name.value() == "macro")
}

/// A name is a constructor iff it starts with an uppercase letter.
fn is_ctor_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_uppercase())
}

/// The type constructors every module sees without asking.
fn builtin_tycons() -> HashMap<InternedString, usize> {
    [
        ("Int", 0),
        ("BigInt", 0),
        ("Float", 0),
        ("String", 0),
        ("Char", 0),
        ("Bool", 0),
        ("Int64", 0),
        ("Int32", 0),
        ("Int16", 0),
        ("Int8", 0),
        ("UInt64", 0),
        ("UInt32", 0),
        ("UInt16", 0),
        ("UInt8", 0),
        ("Float64", 0),
        ("Float32", 0),
        ("Unit", 0),
        ("List", 1),
        ("Array", 1),
        ("Ref", 1),
        ("StRef", 2),
        ("StArray", 2),
        ("Compact", 1),
        ("Task", 1),
        ("Channel", 1),
        ("TVar", 1),
    ]
    .into_iter()
    .map(|(n, a)| (InternedString::from(n), a))
    .collect()
}

/// The data constructors every module sees without asking — see
/// [`BUILTIN_CTOR_OWNERS`].
fn builtin_ctors() -> HashMap<InternedString, InternedString> {
    BUILTIN_CTOR_OWNERS
        .iter()
        .map(|(ty, c)| {
            let c = InternedString::from(*c);
            (c, canonical_ctor(InternedString::from(*ty), c))
        })
        .collect()
}

/// Types and effects the compiler and the runtimes know by name, which are
/// never qualified by a package: see [`Resolver::qualify`].
const LANGUAGE_NAMES: &[&str] = &[
    // Types a primitive's scheme or the back end names.
    "Int", "BigInt", "Float", "String", "Char", "Bool", "Int64", "Int32", "Int16", "Int8", "UInt64",
    "UInt32", "UInt16", "UInt8", "Float64", "Float32", "Unit", "List", "Array", "Ref", "StRef",
    "StArray", "Compact", "Task", "Channel", "TVar", "Vector", "VNode", "Maybe", "Result",
    "FileMeta", // Effects the runtimes answer, or a primitive performs.
    "Thread", "Console", "Fs", "Process", "Random", "Time", "Test", "Mut", "Stm", "St",
    // Every one the runtime answers, by one name: what `main` may perform.
    "Eff",
];

/// What the resolver knows of a trait: enough to check an `impl` against.
#[derive(Debug, Clone, Default)]
struct TraitInfo {
    /// How many types it is a trait of.
    params: usize,
    /// Associated types, canonical, in declaration order.
    assocs: Vec<InternedString>,
    /// Associated effects, canonical, in declaration order.
    effects: Vec<InternedString>,
    /// Method names, in declaration order, and whether each has a default.
    methods: Vec<(InternedString, bool)>,
}

/// What a type's (or an effect's) name, as written, means in a scope.
#[derive(Debug, Clone, PartialEq)]
enum Named {
    /// One type: its canonical name, and how many parameters it takes.
    One(InternedString, usize),
    /// Types of several packages, seen without a `use`, share this spelling:
    /// naming it bare has to wait for a `use` that says which. Canonical names.
    Ambiguous(Vec<InternedString>),
}

/// Offer `canonical` under its spelling in `table`, as the types a unit sees
/// without asking are offered: a second, different type of the same spelling
/// makes the spelling ambiguous rather than replacing the first.
fn offer(table: &mut HashMap<InternedString, Named>, canonical: InternedString, arity: usize) {
    let spelled = InternedString::from(hir::spelling(&canonical));
    let next = match table.remove(&spelled) {
        None => Named::One(canonical, arity),
        Some(Named::One(c, a)) if c == canonical => Named::One(c, a),
        Some(Named::One(c, _)) => Named::Ambiguous(vec![c, canonical]),
        Some(Named::Ambiguous(mut cs)) => {
            if !cs.contains(&canonical) {
                cs.push(canonical);
            }
            Named::Ambiguous(cs)
        }
    };
    table.insert(spelled, next);
}

/// How a person names the package a canonical type belongs to.
fn package_of(canonical: InternedString) -> String {
    hir::type_package(&canonical).unwrap_or("Std").to_string()
}

/// A constructor's canonical name: `Type.Ctor`.
///
/// Type names are unique across a program, so this is too -- which is what
/// lets `Vector.Leaf` and `Tree.Leaf` both exist. Every pass after the
/// resolver sees only this form; the bare name is a *scoping* convenience that
/// stops here.
fn canonical_ctor(ty: InternedString, ctor: InternedString) -> InternedString {
    InternedString::from(format!("{ty}.{ctor}"))
}

/// The type a canonical constructor belongs to: `Expr.Int` -> `Expr`.
fn owner_of(canonical: InternedString) -> InternedString {
    match canonical.rsplit_once('.') {
        Some((ty, _)) => InternedString::from(ty),
        None => canonical,
    }
}

/// The bare spelling of a canonical name: `Expr.Int` -> `Int`.
/// FNV-1a of a file's text: what says whether an embedded file has changed.
/// Stable across runs, which `DefaultHasher` does not promise.
fn digest(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn bare_ctor(canonical: InternedString) -> InternedString {
    match canonical.rsplit_once('.') {
        Some((_, c)) => InternedString::from(c),
        None => canonical,
    }
}

/// A name written in the source that the HIR does not keep.
///
/// Most references survive resolution: a variable becomes a `VarId` in a
/// `hir::Expr::Var`, a type becomes a `hir::TypeExpr::Con`. Two do not — the
/// names listed in a `use`, and the qualifier of a `Expr.Ctor` — because
/// neither means anything after resolution. An editor still has to know they
/// are references to the same thing, or a rename would leave them behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NameRef {
    Value(VarId),
    Type(InternedString),
    /// A data constructor, by its canonical `Type.Ctor` name.
    Ctor(InternedString),
}

/// Where a [`NameRef`] was written.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct RefSite {
    pub span: Span,
    pub what: NameRef,
}

/// One module's own declarations, kept apart from every other module's.
#[derive(Debug, Default, Clone)]
struct ModuleFrame {
    /// Top-level bindings, in declaration order.
    values: Vec<(InternedString, VarId, Vis)>,
    /// Type constructors this module declares: name -> arity.
    tycons: Vec<(InternedString, usize, Vis)>,
    /// Data constructors this module declares: bare spelling -> canonical.
    /// A constructor is as visible as the type that owns it.
    ctors: Vec<(InternedString, InternedString, Vis)>,
    /// Effects declared here: name -> parameter count.
    effects: Vec<(InternedString, usize, Vis)>,
    /// Effect operations declared here: operation -> its effect.
    effect_ops: Vec<(InternedString, InternedString, Vis)>,
}

impl Resolver {
    pub fn new(filename: impl Into<String>, var_base: u32) -> Self {
        let tycons = builtin_tycons()
            .into_iter()
            .map(|(n, a)| (n, Named::One(n, a)))
            .collect();
        Resolver {
            filename: filename.into(),
            embedded: Vec::new(),
            scope: Vec::new(),
            names: HashMap::new(),
            predeclared: HashMap::new(),
            tycons,
            package: None,
            standalone: false,
            declared_types: std::collections::HashSet::new(),
            all_types: HashMap::new(),
            all_effects: HashMap::new(),
            all_traits: HashMap::new(),
            all_assocs: HashMap::new(),
            all_assoc_effects: HashMap::new(),
            hidden_exports: Vec::new(),
            ctors: HashMap::new(),
            visible_ctors: builtin_ctors(),
            module_ctors: HashMap::new(),
            sealed: false,
            module_scope: 0,
            overloads: HashMap::new(),
            origins: HashMap::new(),
            decl_spans: HashMap::new(),
            duplicates: std::collections::HashSet::new(),
            ctors_of: HashMap::new(),
            // The effects the language has without a declaration: `Mut` for a
            // `Ref`, `St s` for a `runSt`'s state, `Thread` for green threads.
            // Primitives perform them, and naming one in an annotation is fine.
            effects: [("Mut", 0), ("St", 1), ("Thread", 0)]
                .into_iter()
                .map(|(n, a)| {
                    let n = InternedString::from(n);
                    (n, Named::One(n, a))
                })
                .collect(),
            effect_ops: HashMap::new(),
            effect_op_ids: HashMap::new(),
            frames: HashMap::new(),
            base_tycons: HashMap::new(),
            base_ctors: HashMap::new(),
            base_effects: HashMap::new(),
            base_effect_ops: HashMap::new(),
            current: Vec::new(),
            base_scope: 0,
            tyvars: Vec::new(),
            open_tyvars: false,
            sigs: std::collections::HashSet::new(),
            ids: NodeIdGen::new(),
            vars: VarIdGen::starting_at(var_base),
            toplevel: false,
            vis: Vis::Private,
            any_vis: false,
            pub_vars: std::collections::HashSet::new(),
            pub_types: std::collections::HashSet::new(),
            extra_refs: Vec::new(),
            test_vars: Vec::new(),
            macro_vars: Vec::new(),
            qualifiers: HashMap::new(),
            qualifier_paths: HashMap::new(),
            qualifier_types: HashMap::new(),
            fixities: HashMap::new(),
            prelude: HashMap::new(),
            errors: Vec::new(),
        }
    }

    // --- fixity -----------------------------------------------------------

    /// Take in a module's fixity declarations, before any module is resolved:
    /// an operator can be used in a module before the one that declares it.
    pub fn declare_fixities(&mut self, decls: &[ast::LDecl]) {
        for decl in decls {
            let (_, base) = peel(decl);
            let ast::Decl::Fixity(assoc, level, ops) = base.value() else {
                continue;
            };
            let fixity = hir::Fixity {
                assoc: match assoc {
                    ast::Assoc::Left => hir::Assoc::Left,
                    ast::Assoc::Right => hir::Assoc::Right,
                    ast::Assoc::None => hir::Assoc::None,
                },
                level: *level,
            };
            if *level > 9 {
                self.error(
                    format!("fixity level {level} is out of range"),
                    "levels run from 0 to 9".to_string(),
                    base.span,
                );
                continue;
            }
            for op in ops {
                let name = *op.value();
                if let Some((builtin, _)) = hir::builtin_operator(&name) {
                    if builtin != fixity {
                        self.error(
                            format!(
                                "`{name}` is `{} {}` in the language itself",
                                builtin.keyword(),
                                builtin.level
                            ),
                            "a declaration may repeat that, but not change it".to_string(),
                            op.span,
                        );
                    }
                    continue;
                }
                match self.fixities.get(&name) {
                    Some((before, _)) if *before != fixity => {
                        let before = *before;
                        self.error(
                            format!(
                                "`{name}` was already declared `{} {}`",
                                before.keyword(),
                                before.level
                            ),
                            "an operator binds one way everywhere".to_string(),
                            op.span,
                        );
                    }
                    Some(_) => {}
                    None => {
                        self.fixities.insert(name, (fixity, Some(op.span)));
                    }
                }
            }
        }
    }

    /// A dependency's fixity declaration. Its package already checked it.
    pub fn import_fixity(&mut self, name: InternedString, fixity: hir::Fixity) {
        self.fixities.entry(name).or_insert((fixity, None));
    }

    /// Every fixity this unit knows beyond the language's, its own and its
    /// dependencies', for its dependents: a fixity is not scoped, so one a
    /// package's dependency declared binds in whatever uses that package.
    pub fn fixities(&self) -> Vec<(InternedString, hir::Fixity)> {
        let mut own = self
            .fixities
            .iter()
            .map(|(name, (f, _))| (*name, *f))
            .collect_vec();
        own.sort_by(|a, b| a.0.cmp(&b.0));
        own
    }

    fn fixity(&self, op: InternedString) -> hir::Fixity {
        let op = ast::hygiene::strip(op);
        hir::builtin_operator(&op)
            .map(|(f, _)| f)
            .or_else(|| self.fixities.get(&op).map(|(f, _)| *f))
            .unwrap_or(hir::Fixity::DEFAULT)
    }

    /// The primitive an operator means where nothing else of its name is in
    /// scope: `_primAdd` for `+`, in a program built without `Std`.
    fn operator_prim(&self, op: InternedString) -> Option<VarId> {
        let op = ast::hygiene::strip(op);
        let (_, prim) = hir::builtin_operator(&op)?;
        self.prim(prim?)
    }

    /// Group `a + b * c == d` by the fixities of its operators, into the calls
    /// it means: `(==) ((+) a ((*) b c)) d`. `x :: xs` is the list constructor.
    ///
    /// The usual operator-precedence parse. An operator on the stack is
    /// applied before the next one is pushed when it binds tighter, or as
    /// tightly and both group left; two of one level that do not group the same
    /// way -- or that group neither way, as `a == b == c` -- are an error, as
    /// they are in Haskell, since no reading of them is the obvious one.
    fn reassociate(&mut self, first: &ast::LExpr, rest: &[(ast::Ident, ast::LExpr)]) -> ast::LExpr {
        let mut errors = Vec::new();
        let grouped = reassociate(
            first.clone(),
            rest.iter().map(|(op, e)| (op.clone(), e.clone())),
            |op| self.fixity(op),
            |op, l: ast::LExpr, r: ast::LExpr| {
                let span = l.span.extend(r.span);
                let node = if &*ast::hygiene::strip(*op.value()) == "::" {
                    ast::Expr::Cons(
                        ast::Ident::new(InternedString::from("Cons"), op.span),
                        vec![l, r],
                    )
                } else {
                    let f = ast::LExpr::new(ast::Expr::Var(op.clone()), op.span);
                    ast::Expr::App(f, vec![l, r])
                };
                ast::LExpr::new(node, span)
            },
            |msg, span| errors.push((msg, span)),
        );
        for (msg, span) in errors {
            self.error(msg, "add parentheses".to_string(), span);
        }
        grouped
    }

    // --- modules ----------------------------------------------------------

    /// Name the file diagnostics from here on point into.
    ///
    /// A unit is resolved as a whole but its modules are separate files, and an
    /// error has to say which one it is in — so the driver moves this along as
    /// it goes.
    /// The files this unit embedded, with their hashes.
    pub fn embedded(&self) -> &[(String, u64)] {
        &self.embedded
    }

    /// `includeStr "path"` as the text of that file.
    ///
    /// The path is taken beside the file that wrote it, as an editor would
    /// read it, and the file is read now: what ends up in the program is a
    /// string literal, with no trace at run time that it came from anywhere.
    fn include_str(&mut self, args: &[ast::LExpr], span: Span) -> hir::LExpr {
        let [only] = args else {
            self.error(
                "`includeStr` takes one argument: the file to embed".to_string(),
                format!("{} given", args.len()),
                span,
            );
            return self.node(hir::Expr::Error, span);
        };
        let ast::Expr::Lit(ast::Lit::String(path)) = only.value() else {
            self.error(
                "`includeStr` takes a path written out in full".to_string(),
                "not a string literal".to_string(),
                only.span,
            );
            return self.node(hir::Expr::Error, span);
        };
        let at = self.beside(&path.to_string());
        match std::fs::read_to_string(&at) {
            Ok(text) => {
                self.embedded
                    .push((at.display().to_string(), digest(&text)));
                let lit = hir::Lit::String(InternedString::from(text));
                self.node(hir::Expr::Lit(lit), span)
            }
            Err(e) => {
                self.error(
                    format!("cannot embed `{}`: {e}", at.display()),
                    "no such file".to_string(),
                    only.span,
                );
                self.node(hir::Expr::Error, span)
            }
        }
    }

    /// `path` as written, taken beside the file that wrote it.
    fn beside(&self, path: &str) -> std::path::PathBuf {
        let given = std::path::Path::new(path);
        if given.is_absolute() {
            return given.to_path_buf();
        }
        match std::path::Path::new(&self.filename).parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.join(given),
            // A document with no directory of its own -- the REPL, an editor's
            // scratch buffer -- reads relative to wherever it is running.
            _ => given.to_path_buf(),
        }
    }

    pub fn set_filename(&mut self, filename: impl Into<String>) {
        self.filename = filename.into();
    }

    /// The package this unit belongs to: its types are known by it and their
    /// module as well as by their names, `package::Mod::Name` -- the standard
    /// library's too, but for the ones the compiler knows by name.
    pub fn set_package(&mut self, package: InternedString) {
        self.package = Some(package);
    }

    /// Mark this unit as built without dependencies: see [`Self::standalone`].
    pub fn set_standalone(&mut self, standalone: bool) {
        self.standalone = standalone;
    }

    /// The canonical name of a type or effect the module being declared
    /// declares as `name` -- see [`Resolver::qualify_in`].
    fn qualify(&self, name: InternedString) -> InternedString {
        self.qualify_in(&self.current.clone(), name)
    }

    /// The canonical name of a type or effect module `module` of this unit
    /// declares as `name`: its fully qualified path, `pkg.Mod.Sub.Name` --
    /// `pkg.Name` at the package's root -- so that two modules may each
    /// declare a type of one name, as they may a value.
    ///
    /// A name the compiler itself knows -- one a primitive's type mentions, or
    /// one the runtime answers -- stays as it is, wherever it is declared: the
    /// standard library declares those, and a program built without it
    /// declares its own.
    fn qualify_in(&self, module: &[InternedString], name: InternedString) -> InternedString {
        match self.package {
            Some(p) if !LANGUAGE_NAMES.contains(&&*name) => {
                if module.is_empty() {
                    InternedString::from(format!("{p}.{name}"))
                } else {
                    InternedString::from(format!("{p}.{}.{name}", dotted_path(module)))
                }
            }
            _ => name,
        }
    }

    /// Whether `canonical` is a type this module can name.
    fn type_in_scope(&self, canonical: InternedString) -> bool {
        self.tycons
            .values()
            .any(|n| matches!(n, Named::One(c, _) if *c == canonical))
    }

    /// Declarations from here on belong to `path`.
    pub fn set_module(&mut self, path: &[InternedString]) {
        self.current = path.to_vec();
        self.frames.entry(self.current.clone()).or_default();
    }

    /// Everything visible to every module — the prims and the prelude — has
    /// been imported; whatever follows is one module's own.
    pub fn seal_base_scope(&mut self) {
        self.base_scope = self.scope.len();
        self.base_tycons = self.tycons.clone();
        self.base_ctors = self.visible_ctors.clone();
        self.base_effects = self.effects.clone();
        self.base_effect_ops = self.effect_ops.clone();
        self.sealed = true;
    }

    /// Begin resolving `path`: reset to the shared base, then lay this
    /// module's own declarations on top. A sibling's names are *not* here;
    /// `use` puts them there.
    pub fn enter_module(&mut self, path: &[InternedString]) {
        self.current = path.to_vec();
        self.scope.truncate(self.base_scope);
        self.qualifiers.clear();
        self.qualifier_paths.clear();
        self.qualifier_types.clear();
        self.tycons = self.base_tycons.clone();
        self.visible_ctors = self.base_ctors.clone();
        self.module_ctors.clear();
        self.effects = self.base_effects.clone();
        self.effect_ops = self.base_effect_ops.clone();
        let frame = self.frames.get(path).cloned().unwrap_or_default();
        let _ = self.admit(&frame, None);
    }

    /// Make a frame's declarations visible in the current module.
    ///
    /// `owner` is the module the frame belongs to: `None` for the current
    /// module's own frame, where visibility does not apply — a declaration is
    /// always visible where it was written.
    fn admit(
        &mut self,
        frame: &ModuleFrame,
        owner: Option<&[InternedString]>,
    ) -> Vec<InternedString> {
        // Copied out first: what follows mutates `self`, and the test only
        // needs where we are and whether the unit talks about visibility.
        let here = self.current.clone();
        let all = !self.any_vis;
        let ok = |vis: Vis| match owner {
            None => true,
            Some(o) => all || vis.reaches(o, &here),
        };
        // Whose names these are: `owner`'s, or this module's own.
        let module: Vec<InternedString> = owner.map_or_else(|| here.clone(), <[_]>::to_vec);
        let mut brought = Vec::new();
        for (n, a, v) in &frame.tycons {
            if ok(*v) {
                let canonical = self.qualify_in(&module, *n);
                self.tycons.insert(*n, Named::One(canonical, *a));
                self.bring_struct_ctor(canonical);
                brought.push(canonical);
            }
        }
        // Constructors live under their type, as a Rust enum's variants do --
        // even in the module that declares it. `Ty.C` names one anywhere the
        // type is visible; written bare it has to have been brought by
        // `use Ty (C)` or `use Ty.*` (or `use M.Ty ...` from elsewhere). Neither
        // declaring the type, naming it in a `use`, nor a module glob does.
        for (n, a, v) in &frame.effects {
            if ok(*v) {
                let canonical = self.qualify_in(&module, *n);
                self.effects.insert(*n, Named::One(canonical, *a));
                brought.push(canonical);
            }
        }
        for (op, eff, v) in &frame.effect_ops {
            if ok(*v) {
                self.effect_ops.insert(*op, self.qualify_in(&module, *eff));
            }
        }
        for (n, id, v) in &frame.values {
            if ok(*v) {
                self.scope.push((*n, *id));
            }
        }
        brought
    }

    /// Can this module name a declaration of `owner`'s with visibility `vis`?
    ///
    /// A unit that never mentions visibility has none: a package with no
    /// visibility attribute anywhere is a script, a REPL line or a two-file program, and
    /// making it annotate itself to see across its own files buys nothing.
    /// It is the same rule the export surface uses -- say nothing and
    /// everything is public, say anything and only what you marked is.
    fn sees(&self, vis: Vis, owner: &[InternedString]) -> bool {
        !self.any_vis || vis.reaches(owner, &self.current)
    }

    /// Record a reference the HIR will not keep.
    pub fn note_ref(&mut self, span: Span, what: NameRef) {
        self.extra_refs.push(RefSite { span, what });
    }

    /// Take the references collected since the last call — the driver does
    /// this after each module, which is what pairs them with a file.
    pub fn take_extra_refs(&mut self) -> Vec<RefSite> {
        std::mem::take(&mut self.extra_refs)
    }

    /// Does this unit contain a module at `path`?
    pub fn has_module(&self, path: &[InternedString]) -> bool {
        self.frames.contains_key(path)
    }

    /// A sibling module's top-level values, for `use Pack.Mod` — the ones this
    /// module is allowed to see.
    pub fn module_values(&self, path: &[InternedString]) -> HashMap<InternedString, VarId> {
        self.frames
            .get(path)
            .map(|f| {
                f.values
                    .iter()
                    .filter(|(_, _, v)| self.sees(*v, path))
                    .map(|(n, id, _)| (*n, *id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Bring a sibling module's declarations into the current scope.
    ///
    /// `names` empty means all of them, which is what a bare `use Pack.Mod`
    /// asks for. A selected name may be a value, a type or an effect operation.
    /// Neither form brings constructors, which live under their type: see
    /// [`Resolver::use_type`].
    ///
    /// A declaration the asking module is not allowed to see is skipped when
    /// the whole module was asked for, and an error when it was named: saying
    /// `use M (secret)` and silently getting nothing would be reported later
    /// as `undefined variable`, which is true and unhelpful.
    /// Bring a sibling module's names into scope: every one it lets this
    /// module see, or those `names` asks for. Returns the types and effects
    /// brought, by canonical name, which a `@pub use` re-exports.
    pub fn use_module(
        &mut self,
        path: &[InternedString],
        names: &[ast::Ident],
    ) -> Vec<InternedString> {
        let Some(frame) = self.frames.get(path).cloned() else {
            return Vec::new();
        };
        if names.is_empty() {
            return self.admit(&frame, Some(path));
        }
        let mut brought = Vec::new();
        let here = self.current.clone();
        let all = !self.any_vis;
        for want in names {
            let name = *want.value();
            let mut found = false;
            // What the name *is* here, for the error: a name invisible in one
            // namespace may be perfectly visible in another.
            let mut hidden: Option<Vis> = None;
            let note = |vis: Vis, found: &mut bool, hidden: &mut Option<Vis>| {
                if all || vis.reaches(path, &here) {
                    *found = true;
                    true
                } else {
                    *hidden = Some(hidden.map_or(vis, |h: Vis| h.max(vis)));
                    false
                }
            };
            let mut sites: Vec<RefSite> = Vec::new();
            for (n, id, v) in &frame.values {
                if *n == name && note(*v, &mut found, &mut hidden) {
                    self.scope.push((*n, *id));
                    sites.push(RefSite {
                        span: want.span,
                        what: NameRef::Value(*id),
                    });
                }
                // `P`, a pattern synonym: its matcher and its builder.
                if part_of_synonym(n, &name) && note(*v, &mut found, &mut hidden) {
                    self.scope.push((*n, *id));
                }
            }
            for (n, a, v) in &frame.tycons {
                if *n == name && note(*v, &mut found, &mut hidden) {
                    let canonical = self.qualify_in(path, *n);
                    self.tycons.insert(*n, Named::One(canonical, *a));
                    self.bring_struct_ctor(canonical);
                    brought.push(canonical);
                    // A trait brings its methods: naming `Show` is asking to
                    // call `show`.
                    if let Some(info) = self.all_traits.get(&canonical) {
                        let methods: Vec<InternedString> =
                            info.methods.iter().map(|(m, _)| *m).collect();
                        for (m, id, _) in &frame.values {
                            if methods.contains(m) {
                                self.scope.push((*m, *id));
                            }
                        }
                    }
                    sites.push(RefSite {
                        span: want.span,
                        what: NameRef::Type(canonical),
                    });
                }
            }
            for (n, a, v) in &frame.effects {
                if *n == name && note(*v, &mut found, &mut hidden) {
                    let canonical = self.qualify_in(path, *n);
                    self.effects.insert(*n, Named::One(canonical, *a));
                    brought.push(canonical);
                }
            }
            for (op, eff, v) in &frame.effect_ops {
                if *op == name && note(*v, &mut found, &mut hidden) {
                    self.effect_ops.insert(*op, self.qualify_in(path, *eff));
                }
            }
            self.extra_refs.extend(sites);
            // A constructor is not a module's to hand out by itself; say where
            // it lives rather than calling it undefined.
            let owner = (!found)
                .then(|| frame.ctors.iter().find(|(bare, _, _)| *bare == name))
                .flatten()
                .map(|(_, canonical, _)| owner_of(*canonical));
            if let Some(ty) = owner {
                let ty = InternedString::from(hir::spelling(&ty));
                let module = dotted_path(path);
                self.error(
                    format!("`{name}` is a constructor of `{ty}`, not an item of `{module}`"),
                    format!("write `use {module}.{ty} ({name})`, or `{ty}.{name}`"),
                    want.span,
                );
                continue;
            }
            if !found {
                if let Some(vis) = hidden {
                    let module = dotted_path(path);
                    self.error(
                        format!("`{name}` is {} `{module}`", vis.describe_in()),
                        format!("mark it `{}` to use it here", vis.wider()),
                        want.span,
                    );
                }
            }
        }
        brought
    }

    pub fn with_prelude(filename: impl Into<String>, var_base: u32) -> Self {
        let mut r = Resolver::new(filename, var_base);
        for name in PRIMS {
            r.bind(InternedString::from(*name));
        }
        r
    }

    // --- scope management ---------------------------------------------------

    fn mark(&self) -> usize {
        self.scope.len()
    }

    fn reset(&mut self, mark: usize) {
        self.scope.truncate(mark);
    }

    fn bind(&mut self, name: InternedString) -> VarId {
        let id = self.vars.fresh();
        self.names.insert(id, name);
        self.scope.push((name, id));
        id
    }

    /// The innermost binding of `name`, whatever layer it is in. Right for an
    /// operator, which only the prelude defines, and for a `use` re-export.
    /// Primitive `name` itself, whatever else is called that in scope: the
    /// prims are bound first (see [`Resolver::with_prelude`]), in [`PRIMS`]
    /// order.
    fn prim(&self, name: &str) -> Option<VarId> {
        let i = PRIMS.iter().position(|p| *p == name)?;
        self.scope
            .get(i)
            .filter(|(n, _)| &**n == name)
            .map(|(_, id)| *id)
    }

    /// `name` as the dependencies bring it in, below anything the module
    /// itself binds: what an interpolated string renders with.
    fn base_lookup(&self, name: &str) -> Option<VarId> {
        self.scope[..self.base_scope.min(self.scope.len())]
            .iter()
            .rev()
            .find(|(n, _)| &**n == name)
            .map(|(_, id)| *id)
    }

    fn lookup(&self, name: InternedString) -> Option<VarId> {
        self.scope
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .map(|(_, id)| *id)
    }

    /// Every meaning a bare `name` has here, in three layers.
    ///
    /// A local binding shadows everything, as lexical scope does. Below that is
    /// the module layer -- its own top-level names and whatever its `use`s
    /// brought in -- where two meanings of one spelling are not a conflict to
    /// settle by import order but an overload for inference to settle by type.
    /// Below that, the base: the prims and the prelude, which anything above
    /// shadows. Overloading against the prelude would make every module that
    /// defines its own `map` an ambiguity waiting for an untyped use.
    /// Every binding `name` could mean, nearest first.
    ///
    /// A name a macro template wrote carries a hygiene mark (see
    /// [`ast::hygiene`]): `tmp#3` rather than `tmp`. A marked name that nothing
    /// bound is the template naming something outside itself -- an ordinary
    /// function, say -- so the mark comes off and it is looked up again. That
    /// is what makes local bindings hygienic and items not: a local the
    /// template introduced *is* bound under its mark and is found on the first
    /// look, while a name it only mentions falls through to the second.
    ///
    /// A name a built-in derive wrote carries the prelude's mark: nothing it
    /// did not bind itself is found under it, and it means the prelude's --
    /// `get` is `Std`'s, however a dependency's `get` came into scope -- or,
    /// where the prelude has none, whatever the name means here.
    fn lookup_all(&self, name: InternedString) -> Vec<VarId> {
        let found = self.lookup_marked(name);
        if found.is_empty() && ast::hygiene::is_marked(name) {
            let bare = ast::hygiene::strip(name);
            if ast::hygiene::is_prelude(name)
                && let Some(&id) = self.prelude.get(&bare)
            {
                return vec![id];
            }
            return self.lookup_all(bare);
        }
        found
    }

    fn lookup_marked(&self, name: InternedString) -> Vec<VarId> {
        let (base, module) = if self.sealed {
            (self.base_scope, self.module_scope.max(self.base_scope))
        } else {
            (0, 0)
        };
        let locals = &self.scope[module.min(self.scope.len())..];
        if let Some((_, id)) = locals.iter().rev().find(|(n, _)| *n == name) {
            return vec![*id];
        }
        let mut found: Vec<VarId> = Vec::new();
        for (n, id) in &self.scope[base.min(module)..module.min(self.scope.len())] {
            if *n == name && !found.contains(id) {
                found.push(*id);
            }
        }
        if !found.is_empty() {
            return found;
        }
        self.scope[..base.min(self.scope.len())]
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .map(|(_, id)| vec![*id])
            .unwrap_or_default()
    }

    /// At the top level, reuse a [`declare_toplevel`] id if there is one; otherwise
    /// bind fresh. Nested definitions always bind fresh (so they can shadow).
    fn bind_defn(&mut self, name: InternedString) -> VarId {
        if self.toplevel {
            if let Some(&id) = self.predeclared.get(&(self.current.clone(), name)) {
                return id;
            }
        }
        self.bind(name)
    }

    /// Bring a dependency's exported binding into scope under its own id.
    pub fn import(&mut self, name: InternedString, id: VarId) {
        self.names.insert(id, name);
        self.scope.push((name, id));
    }

    /// The same, for one of the prelude's: what a derive's names mean.
    pub fn import_prelude(&mut self, name: InternedString, id: VarId) {
        self.import(name, id);
        self.prelude.insert(name, id);
    }

    /// [`Resolver::import`], remembering the module it came from so an
    /// ambiguity involving it can say so.
    pub fn import_from(&mut self, name: InternedString, id: VarId, module: InternedString) {
        self.origins.insert(id, module);
        self.import(name, id);
    }

    /// One past the last `VarId` this unit has handed out.
    pub fn var_end(&self) -> u32 {
        self.vars.end()
    }

    /// The generator itself, to carry on with after resolution — lowering to
    /// `core` invents variables too, and they belong to the same unit.
    pub fn var_gen(&self) -> VarIdGen {
        self.vars
    }

    /// Continue node-id numbering from `base`, so every module of a package shares
    /// one dense id space (and one package-wide type table).
    pub fn set_id_base(&mut self, base: usize) {
        self.ids = NodeIdGen::starting_at(base);
    }

    /// Make module `qualifier` reachable as `qualifier.name` in the module about to
    /// be resolved. `values` are its exported bindings. (Types / constructors are
    /// registered globally elsewhere — their names are unique across a program.)
    pub fn activate_module(
        &mut self,
        qualifier: InternedString,
        values: HashMap<InternedString, VarId>,
    ) {
        for (&n, &id) in &values {
            self.names.insert(id, n);
        }
        self.qualifiers.insert(qualifier, values);
    }

    /// [`Resolver::activate_module`] for the module at `path`, one under the
    /// module about to be resolved: `qualifier` is the path from here to it.
    pub fn activate_child(&mut self, qualifier: InternedString, path: &[InternedString]) {
        let values = self.module_values(path);
        self.activate_module(qualifier, values);
        self.activate_local_types(qualifier, path);
        self.qualifier_paths.insert(qualifier, path.to_vec());
    }

    /// Make the types `types` -- a used module's, by their canonical names --
    /// reachable as `qualifier.Ty` in a type.
    pub fn activate_module_types(&mut self, qualifier: InternedString, types: &[InternedString]) {
        let reached = types
            .iter()
            .filter_map(|t| {
                self.imported_arity(*t)
                    .map(|a| (InternedString::from(hir::spelling(t)), (*t, a)))
            })
            .collect();
        self.qualifier_types.insert(qualifier, reached);
    }

    /// [`Resolver::activate_module_types`] for the module of this unit at
    /// `path`: the types it declares that this module may see.
    pub fn activate_local_types(&mut self, qualifier: InternedString, path: &[InternedString]) {
        let Some(frame) = self.frames.get(path) else {
            return;
        };
        let here = self.current.clone();
        let all = !self.any_vis;
        let reached = frame
            .tycons
            .iter()
            .filter(|(_, _, v)| all || v.reaches(path, &here))
            .map(|(n, a, _)| (*n, (self.qualify_in(path, *n), *a)))
            .collect();
        self.qualifier_types.insert(qualifier, reached);
    }

    /// `Core.Expr.Int`: the constructor `name` of the type that `q` names
    /// through a module under this one -- `Core.Expr`, the type `Expr` of
    /// module `Core`.
    fn module_type_ctor(&self, q: InternedString, name: InternedString) -> Option<InternedString> {
        let (module, ty) = q.rsplit_once('.')?;
        let path = self.qualifier_paths.get(&InternedString::from(module))?;
        let frame = self.frames.get(path)?;
        frame
            .ctors
            .iter()
            .find(|(bare, canonical, vis)| {
                *bare == name && hir::spelling(&owner_of(*canonical)) == ty && self.sees(*vis, path)
            })
            .map(|(_, canonical, _)| *canonical)
    }

    /// The function `name` of the module `q` qualifies, as a use of it.
    fn qualified_value(
        &mut self,
        q: InternedString,
        name: InternedString,
        span: Span,
    ) -> Option<hir::LExpr> {
        let id = self.qualifiers.get(&q)?.get(&name).copied()?;
        let v = self.node(id, span);
        Some(self.node(hir::Expr::Var(v), span))
    }

    fn node<T>(&mut self, value: T, span: Span) -> hir::Node<T> {
        hir::Node::new(self.ids.fresh(), value, span)
    }

    // --- public accessors ------------------------------------------------------

    pub fn id_count(&self) -> usize {
        self.ids.count()
    }

    /// The `(name, id)` pairs bound for [`PRIMS`], in declaration order. `infer`
    /// and `core` use these to attach primitive schemes / lower operator calls.
    pub fn prelude_bindings(&self) -> Vec<(InternedString, VarId)> {
        self.scope.iter().take(PRIMS.len()).copied().collect()
    }

    /// Declared arity of every data / record constructor in scope. `Cons` / `Nil`
    /// come from the prelude's `data List`; the two `Bool` constructors lower to
    /// literals and never need an arity.
    pub fn ctor_arities(&self) -> HashMap<InternedString, usize> {
        self.ctors.iter().map(|(n, c)| (*n, c.arity)).collect()
    }

    pub fn names(&self) -> &HashMap<VarId, InternedString> {
        &self.names
    }

    pub fn errors(&self) -> &[Diagnostic] {
        &self.errors
    }

    pub fn take_errors(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.errors)
    }

    // --- declaration pre-pass ------------------------------------------------

    /// Pre-bind the top-level names of a module so definitions (in this module or a
    /// sibling module of the same package) can refer to each other regardless of
    /// order. Call once per module before resolving any bodies.
    /// Read a declaration's visibility off its attributes and make it the one
    /// the recording machinery uses.
    ///
    /// `report` says whether to complain about an argument that means nothing.
    /// A declaration is read three times -- the two declaring pre-passes and the
    /// resolve pass -- and reporting from each printed every bad `@pub(…)`
    /// three times; only the resolve pass reports.
    fn set_decl_vis(&mut self, attrs: &[ast::Attr], report: bool) -> Vis {
        let (vis, bad) = vis_of(attrs);
        if let Some((arg, span)) = bad.filter(|_| report) {
            if &*arg == "pack" {
                // The old spelling of "a dependent can see this", which is now
                // plain `@pub`, as in Rust. Not `@pub(pkg)`: that is the old
                // `@pub`, and suggesting it by resemblance would quietly hide a
                // package's whole surface.
                self.error(
                    "`@pub(pack)` is now written `@pub`".to_string(),
                    "`@pub` is visible to dependents; `@pub(pkg)` keeps a name inside this package"
                        .to_string(),
                    span,
                );
            } else {
                self.error(
                    format!("unknown visibility `@pub({arg})`"),
                    "write `@pub`, `@pub(pkg)` or `@pub(super)`".to_string(),
                    span,
                );
            }
        }
        if vis != Vis::Private {
            self.any_vis = true;
        }
        self.vis = vis;
        vis
    }

    pub fn declare_toplevel(&mut self, decls: &[ast::LDecl]) {
        for d in decls {
            let (attrs, base) = peel(d);
            self.set_decl_vis(attrs, false);
            if let ast::Decl::Bind(b) = base.value() {
                match b {
                    ast::Bind::Fun(name, ..) => {
                        self.predeclare_unique(name);
                    }
                    ast::Bind::Pat(pat, _) => self.predeclare_pat(pat),
                }
            }
        }
    }

    /// Register the type constructors and data/record constructors of a module so
    /// bodies (and other modules of the same unit) can refer to them, and so type
    /// expressions can be checked. Call before [`resolve_module`].
    pub fn declare_types(&mut self, decls: &[ast::LDecl]) {
        for d in decls {
            let (attrs, base) = peel(d);
            self.set_decl_vis(attrs, false);
            match base.value() {
                ast::Decl::Data(dd) => {
                    self.declare_tycon(*dd.name.value(), dd.params.len(), dd.name.span);
                    for v in &dd.variants {
                        let (arity, field_names) = match &v.fields {
                            ast::VariantFields::Positional(ts) => (ts.len(), None),
                            ast::VariantFields::Named(fs) => {
                                self.check_dup_fields(fs);
                                (fs.len(), Some(fs.iter().map(|f| *f.name.value()).collect()))
                            }
                        };
                        self.declare_ctor(
                            *dd.name.value(),
                            *v.name.value(),
                            arity,
                            field_names,
                            v.name.span,
                        );
                    }
                }
                ast::Decl::Record(rd) => {
                    self.declare_tycon(*rd.name.value(), rd.params.len(), rd.name.span);
                    self.check_dup_fields(&rd.fields);
                    let fields = rd.fields.iter().map(|f| *f.name.value()).collect();
                    // A record's one constructor shares the type's name: `Person.Person`.
                    self.declare_ctor(
                        *rd.name.value(),
                        *rd.name.value(),
                        rd.fields.len(),
                        Some(fields),
                        rd.name.span,
                    );
                }
                ast::Decl::Effect(ed) => {
                    let name = *ed.name.value();
                    let canonical = self.qualify(name);
                    // an effect name is also a type constructor of its parameters
                    self.declare_tycon(name, ed.params.len(), ed.name.span);
                    self.effects
                        .insert(name, Named::One(canonical, ed.params.len()));
                    self.all_effects.insert(canonical, ed.params.len());
                    let vis = self.vis;
                    self.frame().effects.push((name, ed.params.len(), vis));
                    for op_field in &ed.ops {
                        let op = *op_field.name.value();
                        // A duplicate is one this module already declares. An
                        // operation of the same name from elsewhere -- the
                        // standard library's `State` has a `get` -- is only
                        // shadowed here, as any imported name is by one of the
                        // module's own.
                        let already = self.predeclared.contains_key(&(self.current.clone(), op));
                        self.effect_ops.insert(op, canonical);
                        if already {
                            self.error(
                                format!("operation `{op}` is already defined"),
                                "duplicate operation".to_string(),
                                ed.name.span,
                            );
                        }
                        // ops are top-level values (functions) — predeclare them
                        let id = self.predeclare(op);
                        self.decl_spans
                            .entry((self.current.clone(), op))
                            .or_insert(op_field.name.span);
                        self.frame().effect_ops.push((op, name, vis));
                        self.effect_op_ids.insert(id, (canonical, op));
                    }
                }
                ast::Decl::TypeAlias(ad) => {
                    self.declare_tycon(*ad.name.value(), ad.params.len(), ad.name.span);
                }
                // Named as an effect is, so a row can name it where it could
                // name one.
                ast::Decl::EffectAlias(ad) => {
                    let name = *ad.name.value();
                    let canonical = self.qualify(name);
                    self.declare_tycon(name, ad.params.len(), ad.name.span);
                    self.effects
                        .insert(name, Named::One(canonical, ad.params.len()));
                    self.all_effects.insert(canonical, ad.params.len());
                    let vis = self.vis;
                    self.frame().effects.push((name, ad.params.len(), vis));
                }
                ast::Decl::Trait(td) => {
                    let name = *td.name.value();
                    let canonical = self.qualify(name);
                    // The dictionary's type: the trait's parameters, then each
                    // associated type.
                    let n = td.params.len();
                    self.declare_tycon(name, n + td.assocs.len() + td.effects.len(), td.name.span);
                    let mut info = TraitInfo {
                        params: n,
                        ..TraitInfo::default()
                    };
                    for (assoc, _) in &td.assocs {
                        self.declare_tycon(*assoc.value(), n, assoc.span);
                        let assoc = self.qualify(*assoc.value());
                        self.all_assocs.insert(assoc, canonical);
                        info.assocs.push(assoc);
                    }
                    // An associated effect is an effect's name, which a row
                    // may say, of as many types as the trait is.
                    for (effect, _) in &td.effects {
                        let e = *effect.value();
                        let c = self.qualify(e);
                        self.effects.insert(e, Named::One(c, n));
                        self.all_effects.insert(c, n);
                        let vis = self.vis;
                        self.frame().effects.push((e, n, vis));
                        self.all_assoc_effects.insert(c, canonical);
                        info.effects.push(c);
                    }
                    for (method, _) in &td.sigs {
                        let m = *method.value();
                        if self.predeclared.contains_key(&(self.current.clone(), m)) {
                            self.error(
                                format!("`{m}` is already defined in this module"),
                                "a trait's method is a top-level name".to_string(),
                                method.span,
                            );
                        }
                        self.predeclare(m);
                        self.decl_spans
                            .entry((self.current.clone(), m))
                            .or_insert(method.span);
                        let default = td
                            .defaults
                            .iter()
                            .any(|b| matches!(method_name(b), Some(n) if *n.value() == m));
                        info.methods.push((m, default));
                    }
                    self.all_traits.insert(canonical, info);
                }
                _ => {}
            }
        }
    }

    /// Re-register the type + data constructors of an already-resolved dependency
    /// (or an earlier REPL line). Trusted, so no duplicate diagnostics.
    ///
    /// Known is not in scope. A dependency's types, effects and operations are
    /// known here -- what `use` can bring, what a canonical name means -- but a
    /// module names one only once it has asked for it: `use M (Ty)`, `use M`,
    /// or the prelude's re-export (`flat_types`). Only `in_scope` -- an earlier
    /// REPL line, an ad-hoc dependency -- puts them all in every module's scope.
    pub fn import_types(&mut self, decls: &[hir::LDecl], in_scope: bool) {
        for d in decls {
            match d.value() {
                hir::Decl::Data(dd) => {
                    if in_scope {
                        offer(&mut self.tycons, dd.name, dd.params.len());
                    }
                    self.all_types.insert(dd.name, dd.params.len());
                    for v in &dd.variants {
                        let (arity, field_names) = match &v.fields {
                            hir::VariantFields::Positional(ts) => (ts.len(), None),
                            hir::VariantFields::Named(fs) => {
                                (fs.len(), Some(fs.iter().map(|(n, _)| *n).collect()))
                            }
                        };
                        // `v.name` arrives canonical (the resolver that built
                        // this HIR made it so), and `ctors_of` wants the bare
                        // spelling a `use` would let someone write.
                        self.ctors.insert(v.name, CtorInfo { arity, field_names });
                        self.ctors_of
                            .entry(dd.name)
                            .or_default()
                            .push(bare_ctor(v.name));
                    }
                }
                hir::Decl::Record(rd) => {
                    if in_scope {
                        offer(&mut self.tycons, rd.name, rd.params.len());
                    }
                    self.all_types.insert(rd.name, rd.params.len());
                    self.ctors.insert(
                        rd.ctor,
                        CtorInfo {
                            arity: rd.fields.len(),
                            field_names: Some(rd.fields.iter().map(|(n, _)| *n).collect()),
                        },
                    );
                    self.ctors_of
                        .entry(rd.name)
                        .or_default()
                        .push(InternedString::from(hir::spelling(&rd.name)));
                }
                hir::Decl::Effect(ed) => {
                    if in_scope {
                        offer(&mut self.tycons, ed.name, ed.params.len());
                        offer(&mut self.effects, ed.name, ed.params.len());
                    }
                    self.all_types.insert(ed.name, ed.params.len());
                    self.all_effects.insert(ed.name, ed.params.len());
                    for (opname, op, _) in &ed.ops {
                        // An operation a handler names: which effect it is of.
                        self.effect_ops.insert(*opname, ed.name);
                        self.effect_op_ids.insert(*op.value(), (ed.name, *opname));
                        // The operation as a value, which a module otherwise
                        // brings with `use` as it does any other.
                        if in_scope {
                            self.import(*opname, *op.value());
                        }
                    }
                }
                hir::Decl::Alias(ad) => {
                    if in_scope {
                        offer(&mut self.tycons, ad.name, ad.params.len());
                    }
                    self.all_types.insert(ad.name, ad.params.len());
                }
                hir::Decl::EffectAlias(ad) => {
                    if in_scope {
                        offer(&mut self.tycons, ad.name, ad.params.len());
                        offer(&mut self.effects, ad.name, ad.params.len());
                    }
                    self.all_types.insert(ad.name, ad.params.len());
                    self.all_effects.insert(ad.name, ad.params.len());
                }
                hir::Decl::Trait(td) => {
                    let n = td.params.len();
                    if in_scope {
                        offer(
                            &mut self.tycons,
                            td.name,
                            n + td.assocs.len() + td.effects.len(),
                        );
                    }
                    self.all_types
                        .insert(td.name, n + td.assocs.len() + td.effects.len());
                    for e in &td.effects {
                        if in_scope {
                            offer(&mut self.effects, *e.value(), n);
                        }
                        self.all_effects.insert(*e.value(), n);
                        self.all_assoc_effects.insert(*e.value(), td.name);
                    }
                    for a in &td.assocs {
                        if in_scope {
                            offer(&mut self.tycons, *a.value(), n);
                        }
                        self.all_types.insert(*a.value(), n);
                        self.all_assocs.insert(*a.value(), td.name);
                    }
                    self.all_traits.insert(
                        td.name,
                        TraitInfo {
                            params: n,
                            assocs: td.assocs.iter().map(|a| *a.value()).collect(),
                            effects: td.effects.iter().map(|e| *e.value()).collect(),
                            methods: td
                                .methods
                                .iter()
                                .map(|m| (m.name, m.default.is_some()))
                                .collect(),
                        },
                    );
                }
                _ => {}
            }
        }
    }

    /// `(op VarId, effect name, op name)` for every declared operation — the
    /// lowerer turns a reference to one into `\x -> perform Effect.op x`.
    pub fn effect_op_vars(&self) -> Vec<(VarId, InternedString, InternedString)> {
        self.effect_op_ids
            .iter()
            .map(|(&id, &(eff, op))| (id, eff, op))
            .collect()
    }

    fn frame(&mut self) -> &mut ModuleFrame {
        self.frames.entry(self.current.clone()).or_default()
    }

    fn declare_tycon(&mut self, name: InternedString, arity: usize, span: Span) {
        // Recorded on the module, and also in the working set: type names stay
        // unique across a package (a constructor's canonical name is built
        // from one, so two `Foo`s would collide downstream), and the duplicate
        // check needs to see every module's.
        let vis = self.vis;
        self.frame().tycons.push((name, arity, vis));
        let canonical = self.qualify(name);
        // A type is known by its fully qualified path, so only another
        // declaration at that path can clash with it -- except a name the
        // compiler knows, which is not qualified, and so must also stay clear
        // of the standard library's other units'.
        let clash = canonical == name && self.type_in_scope(canonical);
        let again = !self.declared_types.insert(canonical);
        self.all_types.insert(canonical, arity);
        self.tycons.insert(name, Named::One(canonical, arity));
        // The built-in and language names -- `Bool`, `List`, `Int`, the effect
        // labels like `Mut` -- are never package-qualified (see `qualify`) and
        // the back ends give a value of one the built-in's representation. So a
        // user declaration under one of these names does not make a new type;
        // it aliases the built-in, and its values are then read as the wrong
        // thing (a segfault on the native runtimes). Only the standard library
        // may declare them; anyone else is refused here. A unit built with no dependencies
        // at all has no standard library to alias and stands in for it, so it
        // may declare the language names; never the built-in representations.
        let reserved = BUILTIN_TYCONS.contains(&&*name)
            || (LANGUAGE_NAMES.contains(&&*name) && !self.standalone);
        if reserved && self.package.is_some_and(|p| &*p != "Std") {
            self.error(
                format!("`{name}` is a built-in name and cannot be declared"),
                "built in".to_string(),
                span,
            );
        } else if (again || clash) && !BUILTIN_TYCONS.contains(&&*name) {
            self.error(
                format!("type `{name}` is already defined"),
                "duplicate type".to_string(),
                span,
            );
        }
    }

    /// Register a constructor of `owner`.
    ///
    /// Three tables, because a constructor has three separate facts about it:
    /// what it *is* (keyed canonically), what its type's constructors are (for
    /// `use`), and whether this module may write it bare. The last is the only
    /// one that is scoped -- a type's own module always may.
    fn declare_ctor(
        &mut self,
        owner: InternedString,
        name: InternedString,
        arity: usize,
        field_names: Option<Vec<InternedString>>,
        span: Span,
    ) {
        let owner = self.qualify(owner);
        let canonical = canonical_ctor(owner, name);
        if self
            .ctors
            .insert(canonical, CtorInfo { arity, field_names })
            .is_some()
            && !BUILTIN_CTORS.contains(&&*name)
        {
            self.error(
                format!(
                    "constructor `{}.{name}` is already defined",
                    hir::spelling(&owner)
                ),
                "duplicate constructor".to_string(),
                span,
            );
        }
        self.ctors_of.entry(owner).or_default().push(name);
        let vis = self.vis;
        self.frame().ctors.push((name, canonical, vis));
    }

    fn check_dup_fields(&mut self, fields: &[ast::Field]) {
        let mut seen = std::collections::HashSet::new();
        for f in fields {
            if !seen.insert(*f.name.value()) {
                self.error(
                    format!("duplicate field `{}`", f.name.value()),
                    "already declared".to_string(),
                    f.name.span,
                );
            }
        }
    }

    /// The canonical name a bare constructor refers to here, if any.
    /// Every constructor a bare `name` could mean here, best layer first: the
    /// module's own and imported ones if there are any (possibly several),
    /// otherwise the base layer's one.
    fn ctor_candidates(&self, name: InternedString) -> Vec<InternedString> {
        match self.module_ctors.get(&name) {
            Some(cs) if !cs.is_empty() => cs.clone(),
            _ => self.visible_ctors.get(&name).copied().into_iter().collect(),
        }
    }

    /// Make `bare` mean `canonical` in the current scope. Before the scope is
    /// sealed that replaces whatever it meant; after, it adds a meaning.
    fn add_ctor(&mut self, bare: InternedString, canonical: InternedString) {
        if !self.sealed {
            self.visible_ctors.insert(bare, canonical);
            return;
        }
        let cs = self.module_ctors.entry(bare).or_default();
        if !cs.contains(&canonical) {
            cs.push(canonical);
        }
    }

    /// Record that the name at `node` has several meanings.
    fn overload(&mut self, node: hir::NodeId, alts: Vec<hir::Alt>) {
        let candidates = alts
            .into_iter()
            .map(|alt| hir::Candidate {
                alt,
                name: match alt {
                    hir::Alt::Value(id) => self.names.get(&id).copied().unwrap_or_default(),
                    hir::Alt::Ctor(c) => bare_ctor(c),
                },
                origin: self.origin_of(alt),
            })
            .collect();
        self.overloads.insert(node, candidates);
    }

    /// Where a candidate came from, for a person choosing between them.
    fn origin_of(&self, alt: hir::Alt) -> Option<InternedString> {
        let hir::Alt::Value(id) = alt else {
            // A constructor's canonical name already says which type it is.
            return None;
        };
        let home = self
            .frames
            .iter()
            .find(|(_, f)| f.values.iter().any(|(_, v, _)| *v == id))
            .map(|(path, _)| path.clone());
        match home {
            Some(path) if path == self.current => None,
            Some(path) => Some(InternedString::from(dotted_path(&path))),
            None => self.origins.get(&id).copied(),
        }
    }

    /// Take the overloaded names found so far.
    pub fn take_overloads(&mut self) -> hir::Overloads {
        std::mem::take(&mut self.overloads)
    }

    /// The canonical name for an explicitly qualified `Ty.Ctor`.
    fn resolve_qualified_ctor(
        &self,
        ty: InternedString,
        name: InternedString,
    ) -> Option<InternedString> {
        // `Datum.Tag`, as a built-in derive writes it: the standard library's
        // `Datum`, whether or not this module named it (see
        // [`ast::hygiene::PRELUDE`]).
        if ast::hygiene::is_prelude(ty) {
            let bare = ast::hygiene::strip(ty);
            let std = self
                .all_types
                .keys()
                .copied()
                .find(|c| hir::type_package(c) == Some("Std") && hir::spelling(c) == &*bare);
            if let Some(ty) = std {
                let canonical = canonical_ctor(ty, name);
                return self.ctors.contains_key(&canonical).then_some(canonical);
            }
            return self.resolve_qualified_ctor(bare, name);
        }
        let owner = match self.tycons.get(&ty) {
            Some(Named::One(c, _)) => *c,
            _ => return None,
        };
        let canonical = canonical_ctor(owner, name);
        self.ctors.contains_key(&canonical).then_some(canonical)
    }

    /// Report a bare type name that several packages' types share.
    fn ambiguous_type(&mut self, name: InternedString, cs: &[InternedString], span: Span) {
        let mut owners: Vec<String> = cs.iter().map(|c| format!("`{}`", package_of(*c))).collect();
        owners.sort();
        self.error(
            format!(
                "`{name}` could be the type of any of {}; `use` the package's `{name}` you mean",
                owners.join(", ")
            ),
            "ambiguous type".to_string(),
            span,
        );
    }

    fn is_known_ctor(&self, name: InternedString) -> bool {
        self.module_ctors
            .get(&name)
            .is_some_and(|cs| !cs.is_empty())
            || self.visible_ctors.contains_key(&name)
            || BUILTIN_CTORS.contains(&&*name)
    }

    fn ctor_field_order(&self, name: InternedString) -> Option<Vec<InternedString>> {
        self.ctors.get(&name).and_then(|c| c.field_names.clone())
    }

    /// Every type name known here, including imported ones.
    pub fn imported_type_names(&self) -> Vec<InternedString> {
        self.ctors_of.keys().copied().collect()
    }

    /// A constructor with its type's own name -- a `record`'s, or a one-case
    /// `data Parser = Parser ...` -- comes with the type, as a Rust struct's does:
    /// `Point { x = 1.0, y = 2.0 }` wherever `Point` is in scope. It is not
    /// "namespaced under the type" in any useful sense, since `Point.Point` would
    /// be the only other spelling.
    fn bring_struct_ctor(&mut self, ty: InternedString) {
        let spelled = InternedString::from(hir::spelling(&ty));
        let canonical = canonical_ctor(ty, spelled);
        if self.ctors.contains_key(&canonical) {
            self.add_ctor(spelled, canonical);
        }
    }

    /// Report a bare constructor that is not in scope -- and, when a type in
    /// scope has one by that name, say that it lives there and how to write it.
    /// That is nearly always what happened: the constructor exists, and only its
    /// `use` is missing.
    fn unknown_ctor(&mut self, bare: InternedString, span: Span) {
        let here = self.current.clone();
        let mut owners: Vec<InternedString> = self
            .ctors_of
            .iter()
            .filter(|(ty, cs)| self.type_in_scope(**ty) && cs.contains(&bare))
            .map(|(ty, _)| *ty)
            .collect();
        owners.sort_by_key(|t| t.to_string());
        let spell = |t: &InternedString| InternedString::from(hir::spelling(t));
        let (msg, label) = match owners.as_slice() {
            [] => (
                format!("unknown constructor `{bare}`"),
                "not a known constructor".to_string(),
            ),
            [ty] => {
                // `use Ty.*` works for this module's own types and for a
                // dependency's, which are in scope by name; a sibling module's
                // needs its path.
                let own = hir::type_package(ty) == self.package.as_deref();
                let ty = &spell(ty);
                let sibling = own
                    && !self.module_has_type(&here, *ty)
                    && self
                        .frames
                        .values()
                        .any(|f| f.tycons.iter().any(|(n, _, _)| n == ty));
                let bring = if !sibling {
                    format!("`use {ty}.*`")
                } else {
                    format!("a `use` of `{ty}`'s constructors")
                };
                (
                    format!(
                        "unknown constructor `{bare}`: it belongs to `{ty}`, so write `{ty}.{bare}`, \
                         or bring it in with {bring}"
                    ),
                    format!("write `{ty}.{bare}`"),
                )
            }
            many => {
                let list = many
                    .iter()
                    .map(|t| format!("`{}.{bare}`", spell(t)))
                    .join(", ");
                (
                    format!(
                        "unknown constructor `{bare}`: it could be {list}; say which, or `use` one"
                    ),
                    "qualify it with its type".to_string(),
                )
            }
        };
        self.error(msg, label, span);
    }

    /// The module being resolved.
    pub fn current_module(&self) -> &[InternedString] {
        &self.current
    }

    /// Does sibling module `path` declare a type called `ty`? Visible or not:
    /// the question is what a `use` path names, and [`Resolver::use_type`]
    /// reports a type it may not see.
    pub fn module_has_type(&self, path: &[InternedString], ty: InternedString) -> bool {
        self.frames
            .get(path)
            .is_some_and(|f| f.tycons.iter().any(|(n, _, _)| *n == ty))
    }

    /// `use M.Ty`, `use M.Ty (A, B)` and `use M.Ty.*`, for a type declared in
    /// sibling module `path`: the type alone, the named constructors, or all of
    /// them. Returns the canonical constructors now in scope unqualified.
    ///
    /// This is Rust's `use m::Ty::{A, B}`. Naming the type anywhere else --
    /// `use M (Ty)` -- brings only `Ty`, and its constructors are written
    /// `Ty.A`.
    pub fn use_type(
        &mut self,
        path: &[InternedString],
        ty: &ast::Ident,
        names: &[ast::Ident],
        glob: bool,
    ) -> Vec<InternedString> {
        let Some(frame) = self.frames.get(path).cloned() else {
            return Vec::new();
        };
        let tyname = *ty.value();
        let Some(&(_, arity, vis)) = frame.tycons.iter().find(|(n, _, _)| *n == tyname) else {
            return Vec::new();
        };
        if !self.sees(vis, path) {
            let module = dotted_path(path);
            self.error(
                format!("`{tyname}` is {} `{module}`", vis.describe_in()),
                format!("mark it `{}` to use it here", vis.wider()),
                ty.span,
            );
            return Vec::new();
        }
        let canonical_ty = self.qualify_in(path, tyname);
        self.note_ref(ty.span, NameRef::Type(canonical_ty));
        let ctors: Vec<(InternedString, InternedString)> = frame
            .ctors
            .iter()
            .filter(|(_, canonical, _)| owner_of(*canonical) == canonical_ty)
            .map(|(bare, canonical, _)| (*bare, *canonical))
            .collect();
        // `use M.Ty` on its own brings the type, as `use M (Ty)` would.
        self.bring_ctors(canonical_ty, &ctors, names, glob, Some(arity))
    }

    /// `use Pkg.M.Ty (A, B)` / `use Pkg.M.Ty.*` for a dependency's type, which
    /// is already known (see [`Resolver::import_types`]); only its constructors'
    /// scope changes.
    ///
    /// `canonical` is the type the `use` names; it comes into scope under its
    /// spelling, as naming it would bring it.
    pub fn use_dep_type(
        &mut self,
        ty: &ast::Ident,
        canonical: InternedString,
        names: &[ast::Ident],
        glob: bool,
    ) -> Vec<InternedString> {
        let tyname = canonical;
        self.use_named_type(canonical);
        self.note_ref(ty.span, NameRef::Type(tyname));
        let ctors: Vec<(InternedString, InternedString)> = self
            .ctors_of
            .get(&tyname)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|c| (c, canonical_ctor(tyname, c)))
            .collect();
        self.bring_ctors(tyname, &ctors, names, glob, None)
    }

    fn bring_ctors(
        &mut self,
        tyname: InternedString,
        ctors: &[(InternedString, InternedString)],
        names: &[ast::Ident],
        glob: bool,
        alone: Option<usize>,
    ) -> Vec<InternedString> {
        if glob {
            for (bare, canonical) in ctors {
                self.add_ctor(*bare, *canonical);
            }
            return ctors.iter().map(|(_, c)| *c).collect();
        }
        if names.is_empty() {
            if let Some(arity) = alone {
                let spelled = InternedString::from(hir::spelling(&tyname));
                self.tycons.insert(spelled, Named::One(tyname, arity));
            }
            self.bring_struct_ctor(tyname);
            return Vec::new();
        }
        let mut brought = Vec::new();
        for want in names {
            let name = *want.value();
            match ctors.iter().find(|(bare, _)| *bare == name) {
                Some((bare, canonical)) => {
                    self.add_ctor(*bare, *canonical);
                    self.note_ref(want.span, NameRef::Ctor(*canonical));
                    brought.push(*canonical);
                }
                None => self.error(
                    format!("`{}` has no constructor `{name}`", hir::spelling(&tyname)),
                    "not a constructor of this type".to_string(),
                    want.span,
                ),
            }
        }
        brought
    }

    /// The bare constructors of a known type, in declaration order.
    pub fn type_ctor_names(&self, ty: InternedString) -> Vec<InternedString> {
        self.ctors_of.get(&ty).cloned().unwrap_or_default()
    }

    /// Make a dependency's constructor writable bare: the ones its package
    /// re-exported with `@pub use M.Ty.*`, which is how `Just` needs no `use`.
    pub fn use_flat_ctor(&mut self, canonical: InternedString) {
        self.add_ctor(bare_ctor(canonical), canonical);
    }

    /// Bring a known type's struct-like constructor -- one named like the type
    /// -- into scope, as naming the type would. For a unit whose dependency's
    /// types are all in scope: a REPL line's view of the ones before it.
    pub fn use_struct_ctor(&mut self, ty: InternedString) {
        self.bring_struct_ctor(ty);
    }

    /// Name a dependency's type in this module, as `use pkg (Ty)` does: its
    /// spelling now means it, whatever else of that spelling is about.
    pub fn use_named_type(&mut self, canonical: InternedString) {
        let spelled = InternedString::from(hir::spelling(&canonical));
        if let Some(arity) = self.imported_arity(canonical) {
            self.tycons.insert(spelled, Named::One(canonical, arity));
            self.bring_struct_ctor(canonical);
        }
        // An effect, named: what a row may now say. A trait's associated
        // types come with the trait.
        if let Some(arity) = self.imported_effect_arity(canonical) {
            self.effects.insert(spelled, Named::One(canonical, arity));
        }
        let effects: Vec<InternedString> = self
            .all_traits
            .get(&canonical)
            .map(|t| t.effects.clone())
            .unwrap_or_default();
        for e in effects {
            if let Some(arity) = self.imported_effect_arity(e) {
                self.effects.insert(
                    InternedString::from(hir::spelling(&e)),
                    Named::One(e, arity),
                );
            }
        }
        let assocs: Vec<InternedString> = self
            .all_traits
            .get(&canonical)
            .map(|t| t.assocs.clone())
            .unwrap_or_default();
        for a in assocs {
            if let Some(arity) = self.imported_arity(a) {
                self.tycons.insert(
                    InternedString::from(hir::spelling(&a)),
                    Named::One(a, arity),
                );
            }
        }
    }

    /// The canonical names of the known types spelled `spelled`.
    pub fn types_spelled(&self, spelled: InternedString) -> Vec<InternedString> {
        let mut out: Vec<InternedString> = self
            .all_types
            .keys()
            .copied()
            .filter(|c| hir::spelling(c) == &*spelled)
            .collect();
        out.sort_by_key(|c| c.to_string());
        out
    }

    fn imported_arity(&self, canonical: InternedString) -> Option<usize> {
        self.all_types.get(&canonical).copied()
    }

    fn imported_effect_arity(&self, canonical: InternedString) -> Option<usize> {
        self.all_effects.get(&canonical).copied()
    }

    /// Mint the `VarId` for a top-level name of the module being declared.
    ///
    /// The id is minted up front so that mutual recursion works — across
    /// modules too, since the package is one unit — but the *name* goes into
    /// this module's frame rather than into the shared scope. A sibling sees
    /// it only through a `use`.
    fn predeclare(&mut self, name: InternedString) -> VarId {
        let vis = self.vis;
        let key = (self.current.clone(), name);
        if let Some(&id) = self.predeclared.get(&key) {
            return id;
        }
        let id = self.vars.fresh();
        self.names.insert(id, name);
        self.predeclared.insert(key, id);
        self.frames
            .entry(self.current.clone())
            .or_default()
            .values
            .push((name, id, vis));
        id
    }

    /// [`Resolver::predeclare`] a name written in a declaration, reporting it if
    /// this module already declares one -- a second `fun f` would otherwise
    /// silently share the first one's id, and only one body would survive.
    ///
    /// Two *modules* may each have an `f`; that is what overloading is for.
    fn predeclare_unique(&mut self, name: &ast::Ident) -> VarId {
        let key = (self.current.clone(), *name.value());
        if self.predeclared.contains_key(&key) {
            let first = self.decl_spans.get(&key).copied();
            self.errors.push(Diagnostic {
                msg: format!("`{}` is already defined in this module", name.value()),
                filename: self.filename.clone(),
                label: ("defined again here".to_string(), name.span),
                extra_labels: first
                    .map(|s| vec![("first defined here".to_string(), s)])
                    .unwrap_or_default(),
            });
            self.duplicates.insert(name.span);
        } else {
            self.decl_spans.insert(key, name.span);
        }
        self.predeclare(*name.value())
    }

    /// Whether a top-level pattern binds a name [`Resolver::predeclare_unique`]
    /// reported as a duplicate.
    fn binds_duplicate(&self, pat: &ast::LPat) -> bool {
        match pat.value() {
            ast::Pat::Var(n) => self.duplicates.contains(&n.span),
            ast::Pat::As(n, p) => self.duplicates.contains(&n.span) || self.binds_duplicate(p),
            ast::Pat::View(_, p) => self.binds_duplicate(p),
            ast::Pat::Tuple(ps)
            | ast::Pat::List(ps)
            | ast::Pat::Vector(ps)
            | ast::Pat::Cons(_, ps) => ps.iter().any(|p| self.binds_duplicate(p)),
            ast::Pat::Record(fs, _) => fs.iter().any(|(_, p)| self.binds_duplicate(p)),
            _ => false,
        }
    }

    fn predeclare_pat(&mut self, pat: &ast::LPat) {
        match pat.value() {
            ast::Pat::Var(n) => {
                self.predeclare_unique(n);
            }
            ast::Pat::As(n, p) => {
                self.predeclare_unique(n);
                self.predeclare_pat(p);
            }
            ast::Pat::Ann(p, _) => self.predeclare_pat(p),
            ast::Pat::View(_, p) => self.predeclare_pat(p),
            ast::Pat::Tuple(ps)
            | ast::Pat::List(ps)
            | ast::Pat::Vector(ps)
            | ast::Pat::Cons(_, ps) => ps.iter().for_each(|p| self.predeclare_pat(p)),
            ast::Pat::Record(fs, _) => fs.iter().for_each(|(_, p)| self.predeclare_pat(p)),
            _ => {}
        }
    }

    // --- module ------------------------------------------------------------

    /// Resolve a module's bodies. The caller must already have run
    /// [`declare_toplevel`] for this module (and any siblings).
    pub fn resolve_module(&mut self, module: &ast::LModule) -> hir::LModule {
        // Everything in scope by now is the module's own or `use`d; what is
        // bound from here on is local.
        self.module_scope = self.scope.len();
        let decls = module
            .value()
            .decls
            .iter()
            .map(|decl| self.resolve_decl(decl))
            .collect_vec();
        self.node(
            hir::Module {
                name: module.value().name,
                decls,
                // Filled in by `meadow-scc`, which runs over the resolved module.
                groups: Vec::new(),
            },
            module.span,
        )
    }

    fn resolve_decl(&mut self, decl: &ast::LDecl) -> hir::LDecl {
        let (attrs, base) = peel(decl);
        let vis = self.set_decl_vis(attrs, true);
        // `@pub use M (a, b, c)` — re-export names this module can see.
        // Only the package's own surface is re-exportable: a `use` brings a
        // name *here*, and a sibling asks this module for it by name anyway.
        if let ast::Decl::Use(u) = base.value() {
            if vis == Vis::Exported {
                for n in &u.names {
                    let name = *n.value();
                    if let Some(id) = self.lookup(name) {
                        self.pub_vars.insert(id);
                    } else if let Some(Named::One(canonical, _)) = self.tycons.get(&name) {
                        let canonical = *canonical;
                        self.pub_types.insert(canonical);
                    } else {
                        self.error(
                            format!("cannot re-export `{name}`: not found in this scope"),
                            "not defined".to_string(),
                            n.span,
                        );
                    }
                }
            }
        }

        // A signature never carries `@pub`, `@test` or `@macro` -- not because
        // it is rejected here, but because the parser puts an attribute on the
        // definition of the declaration it belongs to, and a signature is only
        // ever the other half of one (see `meadow_parser`). So `@pub` written
        // in front of `fun f : T` reaches the clauses under it, which is where
        // it means something.
        let hir = self.resolve_bare_decl(base);
        if vis == Vis::Exported {
            self.mark_pub(&hir);
        }
        if has_test(attrs) {
            self.mark_test(&hir, base.span);
        }
        if has_macro(attrs) {
            self.mark_macro(&hir, base.span);
        }
        hir
    }

    /// Record a `@macro` declaration: a function that may be called as a macro
    /// by whoever imports it with a `!`.
    ///
    /// Only the *shape* is checked here -- that it is a function at all. What
    /// its type has to be is checked once inference has one, which is where
    /// the interesting mistake gets caught.
    fn mark_macro(&mut self, decl: &hir::LDecl, span: Span) {
        match decl.value() {
            hir::Decl::Bind(hir::Bind::Fun(name, params, _, _)) if params.len() == 1 => {
                self.macro_vars.push((*name.value(), span));
            }
            _ => self.error(
                "`@macro` must be a function of one argument".to_string(),
                "write `@macro fun name tokens = …`, which the compiler calls with the call's tokens"
                    .to_string(),
                span,
            ),
        }
    }

    /// Record a `@test` declaration. A test is a function of no interest to
    /// anything but the runner, which calls it with `()` — so it has to *be*
    /// callable, and `@test def x = …` is a mistake worth naming.
    fn mark_test(&mut self, decl: &hir::LDecl, span: Span) {
        match decl.value() {
            hir::Decl::Bind(hir::Bind::Fun(name, params, _, _)) if params.len() == 1 => {
                self.test_vars.push((
                    self.names.get(name.value()).copied().unwrap_or_default(),
                    *name.value(),
                ));
            }
            _ => self.error(
                "`@test` must be a function of one argument".to_string(),
                "write `@test fun name u = …`, which the runner calls with `()`".to_string(),
                span,
            ),
        }
    }

    /// Record a `@pub` declaration's names in the export sets.
    fn mark_pub(&mut self, decl: &hir::LDecl) {
        match decl.value() {
            hir::Decl::Bind(hir::Bind::Fun(name, ..)) => {
                self.pub_vars.insert(*name.value());
            }
            hir::Decl::Bind(hir::Bind::Pat(pat, _)) => {
                let mut ids = Vec::new();
                collect_hir_pat_vars(pat, &mut ids);
                self.pub_vars.extend(ids);
            }
            hir::Decl::Data(dd) => {
                self.pub_types.insert(dd.name);
            }
            hir::Decl::Record(rd) => {
                self.pub_types.insert(rd.name);
            }
            hir::Decl::Effect(ed) => {
                self.pub_types.insert(ed.name);
                for (_, op, _) in &ed.ops {
                    self.pub_vars.insert(*op.value());
                }
            }
            hir::Decl::Alias(ad) => {
                self.pub_types.insert(ad.name);
            }
            hir::Decl::EffectAlias(ad) => {
                self.pub_types.insert(ad.name);
            }
            hir::Decl::Trait(td) => {
                self.pub_types.insert(td.name);
                for a in &td.assocs {
                    self.pub_types.insert(*a.value());
                }
                for m in &td.methods {
                    self.pub_vars.insert(*m.var.value());
                }
            }
            _ => {}
        }
    }

    /// Values a dependent needs whatever is marked: an `impl`'s dictionary,
    /// which is found by type and never by name, and a trait's default
    /// methods, which an `impl` elsewhere is built from.
    pub fn hidden_exports(&self) -> &[VarId] {
        &self.hidden_exports
    }

    /// `true` if the unit said anything about visibility at all — it then
    /// exports only what is marked `@pub` instead of everything.
    pub fn has_pub_markers(&self) -> bool {
        self.any_vis
    }

    pub fn is_pub_var(&self, id: VarId) -> bool {
        self.pub_vars.contains(&id)
    }

    /// Every `VarId` marked `@pub` — including `@pub use` re-exports, whose
    /// schemes live in a dependency rather than this unit.
    pub fn pub_var_ids(&self) -> impl Iterator<Item = VarId> + '_ {
        self.pub_vars.iter().copied()
    }

    /// Every `@test` function of this unit, in declaration order.
    /// The `@macro` declarations, with where each was written.
    pub fn macro_vars(&self) -> &[(VarId, Span)] {
        &self.macro_vars
    }

    pub fn test_vars(&self) -> &[(InternedString, VarId)] {
        &self.test_vars
    }

    pub fn is_pub_type(&self, name: InternedString) -> bool {
        self.pub_types.contains(&name)
    }

    fn resolve_bare_decl(&mut self, decl: &ast::LDecl) -> hir::LDecl {
        match decl.value() {
            ast::Decl::MacCall(m) => {
                self.unexpanded(m, decl.span);
                self.node(hir::Decl::Error, decl.span)
            }
            // Expansion reads macro definitions and drops them: a macro is not
            // a value, and there is nowhere past here to put one.
            ast::Decl::Macro(m) => {
                self.error(
                    format!("the macro `{}!` was never expanded away", m.name.value()),
                    "this definition reached name resolution".to_string(),
                    decl.span,
                );
                self.node(hir::Decl::Error, decl.span)
            }
            ast::Decl::Attributed(_, inner) => self.resolve_bare_decl(inner),
            // Taken in before any module was resolved (`declare_fixities`);
            // nothing is left of it here.
            ast::Decl::Fixity(..) => self.node(hir::Decl::Use(Vec::new()), decl.span),
            ast::Decl::Bind(bind) => {
                self.toplevel = true;
                // A type variable an annotation introduces belongs to *this*
                // declaration. Without the clear, `fun f (x : a) = x` and a
                // later `fun g (y : a) = y` would share one variable and be
                // forced to the same type.
                self.tyvars.clear();
                let b = self.resolve_bind(bind);
                self.toplevel = false;
                self.node(hir::Decl::Bind(b), decl.span)
            }
            ast::Decl::Use(u) => {
                let segs = u.path.iter().map(|s| *s.value()).collect();
                self.node(hir::Decl::Use(segs), decl.span)
            }
            // What `mod Foo { … }` holds is a module of its own by now: the
            // compiler lifted it out before anything was resolved.
            ast::Decl::Mod(name) | ast::Decl::Module(name, _) => {
                self.node(hir::Decl::Mod(*name.value()), decl.span)
            }
            ast::Decl::Data(dd) => {
                let params = self.bind_tyvars(&dd.params);
                let variants = dd
                    .variants
                    .iter()
                    .map(|v| {
                        let fields = match &v.fields {
                            ast::VariantFields::Positional(ts) => hir::VariantFields::Positional(
                                ts.iter().map(|t| self.resolve_ty(t)).collect(),
                            ),
                            ast::VariantFields::Named(fs) => hir::VariantFields::Named(
                                fs.iter()
                                    .map(|f| (*f.name.value(), self.resolve_ty(&f.ty)))
                                    .collect(),
                            ),
                        };
                        hir::Variant {
                            // Canonical from here down: inference, the
                            // exhaustiveness checker and the back end's tag
                            // table all key on this, and all three need two
                            // types to be able to own a `Leaf`.
                            name: canonical_ctor(self.qualify(*dd.name.value()), *v.name.value()),
                            name_span: v.name.span,
                            fields,
                        }
                    })
                    .collect();
                self.tyvars.clear();
                self.node(
                    hir::Decl::Data(hir::DataDecl {
                        name: self.qualify(*dd.name.value()),
                        name_span: dd.name.span,
                        params,
                        variants,
                    }),
                    decl.span,
                )
            }
            ast::Decl::Record(rd) => {
                let params = self.bind_tyvars(&rd.params);
                let fields = rd
                    .fields
                    .iter()
                    .map(|f| (*f.name.value(), self.resolve_ty(&f.ty)))
                    .collect();
                self.tyvars.clear();
                self.node(
                    hir::Decl::Record(hir::RecordDecl {
                        name: self.qualify(*rd.name.value()),
                        ctor: canonical_ctor(self.qualify(*rd.name.value()), *rd.name.value()),
                        name_span: rd.name.span,
                        params,
                        fields,
                    }),
                    decl.span,
                )
            }
            ast::Decl::Effect(ed) => {
                let params = self.bind_tyvars(&ed.params);
                let ops = ed
                    .ops
                    .iter()
                    .map(|f| {
                        // `declare_types` predeclared the op name as a top-level value
                        let opname = *f.name.value();
                        let id = self
                            .predeclared
                            .get(&(self.current.clone(), opname))
                            .copied()
                            .unwrap_or_else(|| self.bind(opname));
                        let rty = self.resolve_ty(&f.ty);
                        (opname, self.node(id, f.name.span), rty)
                    })
                    .collect();
                self.tyvars.clear();
                self.node(
                    hir::Decl::Effect(hir::EffectDecl {
                        name: self.qualify(*ed.name.value()),
                        name_span: ed.name.span,
                        params,
                        ops,
                    }),
                    decl.span,
                )
            }
            ast::Decl::TypeAlias(ad) => {
                self.tyvars.clear();
                let params = self.bind_tyvars(&ad.params);
                let ty = self.resolve_ty(&ad.ty);
                self.tyvars.clear();
                self.node(
                    hir::Decl::Alias(hir::AliasDecl {
                        name: self.qualify(*ad.name.value()),
                        name_span: ad.name.span,
                        params,
                        ty,
                    }),
                    decl.span,
                )
            }
            ast::Decl::EffectAlias(ad) => {
                self.tyvars.clear();
                let params = self.bind_tyvars(&ad.params);
                if let Some(tail) = &ad.row.tail {
                    self.error(
                        format!(
                            "effect alias `{}` has a tail `{}`",
                            ad.name.value(),
                            tail.value()
                        ),
                        "an alias names effects; the row that names it says what else".to_string(),
                        tail.span,
                    );
                }
                // Resolved without it, which would only be reported again as
                // a variable nothing binds.
                let row = self.resolve_effect_row(&ast::EffectRow {
                    labels: ad.row.labels.clone(),
                    tail: None,
                });
                self.tyvars.clear();
                self.node(
                    hir::Decl::EffectAlias(hir::EffectAliasDecl {
                        name: self.qualify(*ad.name.value()),
                        name_span: ad.name.span,
                        params,
                        row,
                    }),
                    decl.span,
                )
            }
            ast::Decl::Trait(td) => self.resolve_trait(td, decl.span),
            ast::Decl::Impl(id) => self.resolve_impl(id, decl.span),
            ast::Decl::Sig(name, ty, bounds) => {
                let n = *name.value();
                let id = self.predeclared.get(&(self.current.clone(), n)).copied();
                // Its variables are its own: `a` here is not an `a` of the
                // definition's annotations, and is general where that is not.
                self.tyvars.clear();
                let was = std::mem::replace(&mut self.open_tyvars, true);
                let rty = self.resolve_ty(ty);
                let rbounds = self.resolve_bounds(bounds);
                self.open_tyvars = was;
                self.tyvars.clear();
                let Some(id) = id else {
                    self.error(
                        format!("a signature for `{n}`, which is not defined in this module"),
                        format!("define `{n}` here, after or before its signature"),
                        name.span,
                    );
                    return self.node(hir::Decl::Error, decl.span);
                };
                if !self.sigs.insert(id) {
                    self.error(
                        format!("`{n}` already has a signature"),
                        "a second signature".to_string(),
                        name.span,
                    );
                    return self.node(hir::Decl::Error, decl.span);
                }
                let ident = self.node(id, name.span);
                self.node(hir::Decl::Sig(ident, rty, rbounds), decl.span)
            }
        }
    }

    // --- traits -------------------------------------------------------------

    /// The trait a name written in a bound or an `impl` means, canonical.
    fn resolve_trait_name(&mut self, name: &ast::Ident) -> Option<InternedString> {
        let n = *name.value();
        match self.tycons.get(&n).cloned() {
            Some(Named::One(c, _)) if self.all_traits.contains_key(&c) => {
                self.note_ref(name.span, NameRef::Type(c));
                Some(c)
            }
            Some(Named::Ambiguous(cs)) => {
                self.ambiguous_type(n, &cs, name.span);
                None
            }
            Some(Named::One(..)) => {
                self.error(
                    format!("`{n}` is a type, not a trait"),
                    "only a trait can be implemented or asked for".to_string(),
                    name.span,
                );
                None
            }
            None => {
                self.error(
                    format!("unknown trait `{n}`"),
                    "not defined".to_string(),
                    name.span,
                );
                None
            }
        }
    }

    /// A trait is asked of, and implemented for, as many types as it has
    /// parameters.
    fn check_trait_arity(&mut self, tr: InternedString, given: usize, span: Span) -> Option<()> {
        let want = self.all_traits.get(&tr).map_or(1, |t| t.params);
        if want == given {
            return Some(());
        }
        self.error(
            format!(
                "`{}` is a trait of {want} type{}, given {given}",
                hir::spelling(&tr),
                if want == 1 { "" } else { "s" }
            ),
            "the wrong number of types".to_string(),
            span,
        );
        None
    }

    fn resolve_bounds(&mut self, bounds: &[ast::Bound]) -> Vec<hir::Bound> {
        bounds
            .iter()
            .filter_map(|b| {
                let tys = b.tys.iter().map(|t| self.resolve_ty(t)).collect_vec();
                let tr = self.resolve_trait_name(&b.tr)?;
                self.check_trait_arity(tr, tys.len(), b.tr.span)?;
                Some(hir::Bound {
                    tr: self.node(tr, b.tr.span),
                    tys,
                })
            })
            .collect()
    }

    /// A method or default written inside a `trait` or an `impl`: a function
    /// under a name of its own, which the program never sees. Its own name is
    /// **not** in scope in its body -- `show` inside `impl Show [a;]` is the
    /// trait's `show`, at whatever type it is applied to, not this one again.
    fn resolve_method(
        &mut self,
        bind: &ast::Bind,
        hidden: &str,
    ) -> Option<(ast::Ident, hir::Bind)> {
        let name = method_name(bind)?.clone();
        let id = self.vars.fresh();
        self.names.insert(
            id,
            InternedString::from(format!("{hidden}.{}", name.value())),
        );
        let name_node = self.node(id, name.span);
        let mark = self.mark();
        let bound = match bind {
            ast::Bind::Fun(_, params, ret, body) => {
                let rparams = params.iter().map(|p| self.resolve_pat(p)).collect_vec();
                let rret = ret.as_ref().map(|t| {
                    let was = std::mem::replace(&mut self.open_tyvars, true);
                    let r = self.resolve_ty(t);
                    self.open_tyvars = was;
                    r
                });
                let rbody = self.resolve_expr(body);
                hir::Bind::Fun(name_node, rparams, rret, rbody)
            }
            ast::Bind::Pat(_, body) => {
                let rbody = self.resolve_expr(body);
                let span = name.span;
                let pat = self.node(hir::Pat::Var(name_node), span);
                hir::Bind::Pat(pat, rbody)
            }
        };
        self.reset(mark);
        Some((name, bound))
    }

    fn resolve_trait(&mut self, td: &ast::TraitDecl, span: Span) -> hir::LDecl {
        let name = *td.name.value();
        let canonical = self.qualify(name);
        self.tyvars.clear();
        let params = self.bind_tyvars(&td.params);
        let listed = td.params.iter().map(|p| p.value().to_string()).join(" ");
        let mut supers = Vec::new();
        for b in &td.supers {
            // Which of this trait's parameters, by position.
            let of: Option<Vec<usize>> = b
                .tys
                .iter()
                .map(|t| match t.value() {
                    ast::TypeExpr::Var(v) => td.params.iter().position(|p| p.value() == v.value()),
                    _ => None,
                })
                .collect();
            let Some(of) = of else {
                self.error(
                    format!("a trait can only require traits of its own parameters, `{listed}`"),
                    "not the trait's parameters".to_string(),
                    b.tr.span,
                );
                continue;
            };
            if let Some(tr) = self.resolve_trait_name(&b.tr)
                && self.check_trait_arity(tr, of.len(), b.tr.span).is_some()
            {
                supers.push((self.node(tr, b.tr.span), of));
            }
        }
        let mut assocs = Vec::new();
        for (assoc, of) in &td.assocs {
            let same = of.len() == td.params.len()
                && of
                    .iter()
                    .zip(&td.params)
                    .all(|(a, b)| a.value() == b.value());
            if !same {
                self.error(
                    format!(
                        "`type {} …`: an associated type is of the trait's parameters, `{listed}`",
                        assoc.value(),
                    ),
                    "not the trait's parameters".to_string(),
                    assoc.span,
                );
            }
            let c = self.qualify(*assoc.value());
            assocs.push(self.node(c, assoc.span));
        }
        let mut effects = Vec::new();
        for (effect, of) in &td.effects {
            let same = of.len() == td.params.len()
                && of
                    .iter()
                    .zip(&td.params)
                    .all(|(a, b)| a.value() == b.value());
            if !same {
                self.error(
                    format!(
                        "`effect {} …`: an associated effect is of the trait's parameters, `{listed}`",
                        effect.value(),
                    ),
                    "not the trait's parameters".to_string(),
                    effect.span,
                );
            }
            let c = self.qualify(*effect.value());
            effects.push(self.node(c, effect.span));
        }
        let mut methods = Vec::new();
        for (method, ty) in &td.sigs {
            let m = *method.value();
            let id = self
                .predeclared
                .get(&(self.current.clone(), m))
                .copied()
                .unwrap_or_else(|| self.bind(m));
            // Effect variables are a method's own, so they are bound as they
            // are met; inference refuses any other variable but the trait's.
            let was = std::mem::replace(&mut self.open_tyvars, true);
            let rty = self.resolve_ty(ty);
            self.open_tyvars = was;
            methods.push(hir::TraitMethod {
                name: m,
                var: self.node(id, method.span),
                ty: rty,
                default: None,
            });
        }
        for b in &td.defaults {
            let Some(n) = method_name(b) else {
                self.error(
                    "a default is a method: `fun name … = …`".to_string(),
                    "not a name".to_string(),
                    span,
                );
                continue;
            };
            let n = n.clone();
            let Some(at) = methods.iter().position(|m| m.name == *n.value()) else {
                self.error(
                    format!(
                        "`{}` is not a method of `{name}`: declare it with `fun {} : …`",
                        n.value(),
                        n.value()
                    ),
                    "no such method".to_string(),
                    n.span,
                );
                continue;
            };
            if methods[at].default.is_some() {
                self.error(
                    format!("`{}` already has a default", n.value()),
                    "a second default".to_string(),
                    n.span,
                );
                continue;
            }
            if let Some((_, body)) = self.resolve_method(b, &format!("#default.{name}")) {
                let var = body.bound_vars()[0];
                self.hidden_exports.push(var);
                methods[at].default = Some(hir::DefaultMethod {
                    var,
                    body: Some(body),
                });
            }
        }
        self.tyvars.clear();
        self.node(
            hir::Decl::Trait(hir::TraitDecl {
                name: canonical,
                name_span: td.name.span,
                params,
                supers,
                assocs,
                effects,
                methods,
                dict: canonical_ctor(canonical, InternedString::from("#dict")),
            }),
            span,
        )
    }

    fn resolve_impl(&mut self, id: &ast::ImplDecl, span: Span) -> hir::LDecl {
        // The type's variables are the whole declaration's: `a` in a method's
        // annotation is the `a` of `impl Show [a;]`.
        self.tyvars.clear();
        let was = std::mem::replace(&mut self.open_tyvars, true);
        let tys = id.tys.iter().map(|t| self.resolve_ty(t)).collect_vec();
        let context = self.resolve_bounds(&id.context);
        self.open_tyvars = was;
        let found = self
            .resolve_trait_name(&id.tr)
            .filter(|tr| self.check_trait_arity(*tr, tys.len(), id.tr.span).is_some());
        let Some(tr) = found else {
            self.tyvars.clear();
            return self.node(hir::Decl::Error, span);
        };
        let info = self.all_traits.get(&tr).cloned().unwrap_or_default();
        let shown = hir::spelling(&tr).to_string();

        let mut assocs = Vec::new();
        for (name, at, is) in &id.assocs {
            let found = match self.tycons.get(name.value()) {
                Some(Named::One(c, _)) if info.assocs.contains(c) => Some(*c),
                _ => None,
            };
            let Some(c) = found else {
                self.error(
                    format!("`{}` is not an associated type of `{shown}`", name.value()),
                    "no such associated type".to_string(),
                    name.span,
                );
                continue;
            };
            let same =
                at.len() == id.tys.len() && at.iter().zip(&id.tys).all(|(a, b)| same_type(a, b));
            if !same {
                self.error(
                    format!(
                        "`type {} …` in an `impl` is at the implementing types",
                        name.value()
                    ),
                    "write what follows the trait's name after `impl` here".to_string(),
                    name.span,
                );
            }
            if assocs
                .iter()
                .any(|(l, _): &(hir::Label, _)| *l.value() == c)
            {
                self.error(
                    format!("`{}` is given twice", name.value()),
                    "already given".to_string(),
                    name.span,
                );
                continue;
            }
            let is = self.resolve_ty(is);
            assocs.push((self.node(c, name.span), is));
        }
        let mut effects = Vec::new();
        for (name, at, is) in &id.effects {
            let found = match self.effects.get(name.value()) {
                Some(Named::One(c, _)) if info.effects.contains(c) => Some(*c),
                _ => None,
            };
            let Some(c) = found else {
                self.error(
                    format!(
                        "`{}` is not an associated effect of `{shown}`",
                        name.value()
                    ),
                    "no such associated effect".to_string(),
                    name.span,
                );
                continue;
            };
            let same =
                at.len() == id.tys.len() && at.iter().zip(&id.tys).all(|(a, b)| same_type(a, b));
            if !same {
                self.error(
                    format!(
                        "`effect {} …` in an `impl` is at the implementing types",
                        name.value()
                    ),
                    "write what follows the trait's name after `impl` here".to_string(),
                    name.span,
                );
            }
            if effects
                .iter()
                .any(|(l, _): &(hir::Label, _)| *l.value() == c)
            {
                self.error(
                    format!("`{}` is given twice", name.value()),
                    "already given".to_string(),
                    name.span,
                );
                continue;
            }
            let is = self.resolve_effect_row(is);
            effects.push((self.node(c, name.span), is));
        }
        for want in &info.effects {
            if !effects.iter().any(|(l, _)| l.value() == want) {
                self.error(
                    format!(
                        "this `impl {shown}` does not say what `{}` is",
                        hir::spelling(want)
                    ),
                    format!("add `effect {} … = {{ … }}`", hir::spelling(want)),
                    id.tr.span,
                );
            }
        }

        for want in &info.assocs {
            if !assocs.iter().any(|(l, _)| l.value() == want) {
                self.error(
                    format!(
                        "this `impl {shown}` does not say what `{}` is",
                        hir::spelling(want)
                    ),
                    format!("add `type {} … = …`", hir::spelling(want)),
                    id.tr.span,
                );
            }
        }

        let hidden = format!("#impl.{shown}");
        let mut methods: Vec<(InternedString, hir::Bind)> = Vec::new();
        for b in &id.methods {
            let Some((name, body)) = self.resolve_method(b, &hidden) else {
                self.error(
                    "an `impl` holds methods: `fun name … = …`".to_string(),
                    "not a name".to_string(),
                    span,
                );
                continue;
            };
            let m = *name.value();
            if !info.methods.iter().any(|(n, _)| *n == m) {
                self.error(
                    format!("`{m}` is not a method of `{shown}`"),
                    "no such method".to_string(),
                    name.span,
                );
                continue;
            }
            if methods.iter().any(|(n, _)| *n == m) {
                self.error(
                    format!("`{m}` is defined twice in this `impl`"),
                    "already defined".to_string(),
                    name.span,
                );
                continue;
            }
            methods.push((m, body));
        }
        let missing: Vec<String> = info
            .methods
            .iter()
            .filter(|(n, default)| !default && !methods.iter().any(|(m, _)| m == n))
            .map(|(n, _)| format!("`{n}`"))
            .collect();
        if !missing.is_empty() {
            self.error(
                format!("this `impl {shown}` is missing {}", missing.join(", ")),
                "not every method is defined".to_string(),
                id.tr.span,
            );
        }
        self.tyvars.clear();

        let dict = self.vars.fresh();
        self.names.insert(dict, InternedString::from(hidden));
        self.hidden_exports.push(dict);
        let dict = self.node(dict, id.tr.span);
        let tr = self.node(tr, id.tr.span);
        self.node(
            hir::Decl::Impl(hir::ImplDecl {
                tr,
                tys,
                context,
                assocs,
                effects,
                methods,
                dict,
            }),
            span,
        )
    }

    // --- type expressions ---------------------------------------------------

    fn bind_tyvars(&mut self, params: &[ast::Ident]) -> Vec<hir::Ident> {
        params
            .iter()
            .map(|p| {
                let name = *p.value();
                let id = self.vars.fresh();
                self.names.insert(id, name);
                self.tyvars.push((name, id));
                self.node(id, p.span)
            })
            .collect()
    }

    /// A type variable, in the scope of the declaration being resolved: one of
    /// its parameters, one an annotation already introduced, or -- inside an
    /// annotation -- a new one. `None` once the unbound one is reported.
    ///
    /// Shared by a plain variable and a record's row tail, so that the `r` in
    /// `(p : { name : String | r })` and in `r -> Int` are one variable.
    fn type_var(&mut self, n: &ast::Ident) -> Option<hir::Ident> {
        let name = *n.value();
        let id = self
            .tyvars
            .iter()
            .rev()
            .find(|(nm, _)| *nm == name)
            .map(|(_, id)| *id)
            .or_else(|| {
                if self.open_tyvars {
                    let id = self.vars.fresh();
                    self.names.insert(id, name);
                    self.tyvars.push((name, id));
                    return Some(id);
                }
                self.error(
                    format!("unbound type variable `{name}`"),
                    "not a parameter of this type".to_string(),
                    n.span,
                );
                None
            })?;
        Some(self.node(id, n.span))
    }

    fn resolve_ty(&mut self, t: &ast::LType) -> hir::LTypeExpr {
        match t.value() {
            ast::TypeExpr::Var(n) => match self.type_var(n) {
                Some(v) => self.node(hir::TypeExpr::Var(v), t.span),
                None => self.node(hir::TypeExpr::Error, t.span),
            },
            ast::TypeExpr::Con(n, args) => {
                let name = *n.value();
                // `M.Ty`: the type `Ty` of the module `M` qualifies.
                let found = match name.rsplit_once('.') {
                    Some((q, ty)) => {
                        let q = InternedString::from(q);
                        match self.qualifier_types.get(&q) {
                            Some(types) => types
                                .get(&InternedString::from(ty))
                                .map(|(c, a)| Named::One(*c, *a)),
                            // A qualifier with no types: `unknown type` below.
                            None if self.qualifiers.contains_key(&q) => None,
                            None => {
                                self.check_qualifier(&ast::Ident::new(q, n.span));
                                return self.node(hir::TypeExpr::Error, t.span);
                            }
                        }
                    }
                    None => self.tycons.get(&name).cloned(),
                };
                let canonical = match &found {
                    Some(Named::One(c, _)) => *c,
                    _ => name,
                };
                let ok = match found {
                    Some(Named::One(_, arity)) if arity == args.len() => true,
                    Some(Named::Ambiguous(cs)) => {
                        self.ambiguous_type(name, &cs, n.span);
                        false
                    }
                    Some(Named::One(_, arity)) => {
                        self.error(
                            format!(
                                "type `{name}` takes {arity} argument(s), got {}",
                                args.len()
                            ),
                            "wrong number of type arguments".to_string(),
                            n.span,
                        );
                        false
                    }
                    None => {
                        self.error(
                            format!("unknown type `{name}`"),
                            "not defined".to_string(),
                            n.span,
                        );
                        false
                    }
                };
                // Resolved either way, so a mistake inside the arguments is
                // reported too.
                let rargs = args.iter().map(|a| self.resolve_ty(a)).collect();
                if !ok {
                    return self.node(hir::TypeExpr::Error, t.span);
                }
                let con = self.node(canonical, n.span);
                self.node(hir::TypeExpr::Con(con, rargs), t.span)
            }
            ast::TypeExpr::Fun(ps, r, eff) => {
                let rps = ps.iter().map(|p| self.resolve_ty(p)).collect();
                let rr = self.resolve_ty(r);
                let reff = eff.as_ref().map(|e| self.resolve_effect_row(e));
                self.node(hir::TypeExpr::Fun(rps, rr, reff), t.span)
            }
            ast::TypeExpr::Tuple(ts) => {
                let rts = ts.iter().map(|x| self.resolve_ty(x)).collect();
                self.node(hir::TypeExpr::Tuple(rts), t.span)
            }
            ast::TypeExpr::Vector(x) => {
                let rx = self.resolve_ty(x);
                self.node(hir::TypeExpr::Vector(rx), t.span)
            }
            ast::TypeExpr::List(x) => {
                let rx = self.resolve_ty(x);
                self.node(hir::TypeExpr::List(rx), t.span)
            }
            ast::TypeExpr::Row(row) => {
                let row = self.resolve_effect_row(row);
                self.node(hir::TypeExpr::Row(row), t.span)
            }
            ast::TypeExpr::Record(fields, tail) => {
                let mut seen: Vec<InternedString> = Vec::with_capacity(fields.len());
                let rfields = fields
                    .iter()
                    .map(|(label, ft)| {
                        let name = *label.value();
                        // A row may not say one label twice: `{ x : Int, x : Bool }`
                        // has no value, so it is a mistake rather than a type.
                        if seen.contains(&name) {
                            self.error(
                                format!("field `{name}` appears twice in this record type"),
                                "already named".to_string(),
                                label.span,
                            );
                        }
                        seen.push(name);
                        let rt = self.resolve_ty(ft);
                        (self.node(name, label.span), rt)
                    })
                    .collect();
                let rtail = tail.as_ref().and_then(|v| self.type_var(v));
                if tail.is_some() && rtail.is_none() {
                    return self.node(hir::TypeExpr::Error, t.span);
                }
                self.node(hir::TypeExpr::Record(rfields, rtail), t.span)
            }
        }
    }

    fn resolve_effect_row(&mut self, row: &ast::EffectRow) -> hir::EffectRow {
        let mut labels = Vec::new();
        let mut assoc: Option<(InternedString, Vec<hir::LTypeExpr>)> = None;
        for (name, args) in &row.labels {
            let n = *name.value();
            let found = self.effects.get(&n).cloned();
            let label = match &found {
                Some(Named::One(c, _)) => *c,
                _ => n,
            };
            match found {
                Some(Named::One(_, arity)) if arity == args.len() => {}
                Some(Named::One(_, arity)) => self.error(
                    format!("effect `{n}` takes {arity} argument(s), got {}", args.len()),
                    "wrong number of effect arguments".to_string(),
                    name.span,
                ),
                Some(Named::Ambiguous(cs)) => self.ambiguous_type(n, &cs, name.span),
                None => self.error(
                    format!("unknown effect `{n}`"),
                    "not declared".to_string(),
                    name.span,
                ),
            }
            let args: Vec<hir::LTypeExpr> = args.iter().map(|a| self.resolve_ty(a)).collect();
            // A trait's associated effect is not a label: it is whatever
            // row its implementation says, and so the rest of this one.
            if self.all_assoc_effects.contains_key(&label) {
                if assoc.is_some() || row.tail.is_some() {
                    self.error(
                        format!(
                            "`{n}` is an associated effect, which is the rest of a row: \
                             a row has one of those, or a variable, and not both"
                        ),
                        "a second rest of the row".to_string(),
                        name.span,
                    );
                } else {
                    assoc = Some((label, args));
                }
                continue;
            }
            labels.push((label, args));
        }
        let tail = row.tail.as_ref().map(|t| {
            let name = *t.value();
            let id = self
                .tyvars
                .iter()
                .rev()
                .find(|(nm, _)| *nm == name)
                .map(|(_, id)| *id)
                .unwrap_or_else(|| {
                    if self.open_tyvars {
                        let id = self.vars.fresh();
                        self.names.insert(id, name);
                        self.tyvars.push((name, id));
                        return id;
                    }
                    self.error(
                        format!("unbound effect variable `{name}`"),
                        "not a parameter of this declaration".to_string(),
                        t.span,
                    );
                    self.vars.fresh()
                });
            self.node(id, t.span)
        });
        hir::EffectRow {
            labels,
            tail,
            assoc,
        }
    }

    fn resolve_bind(&mut self, bind: &ast::Bind) -> hir::Bind {
        // `toplevel` means *this binding's own name* was predeclared, so
        // `bind_defn` should hand back the reserved id rather than a fresh one.
        // It must not leak into the sub-expressions: a `let` inside the body that
        // happens to share a name with a top-level binding is a new local, and
        // reusing the top-level id there would alias the two.
        let top = std::mem::replace(&mut self.toplevel, false);
        match bind {
            ast::Bind::Pat(pat, expr) => {
                // Right-hand side first, so it sees the *enclosing* scope and a
                // `let` can shadow: `let x = x + 1` reads the outer `x` rather
                // than the one being defined. A binding with parameters is
                // `Bind::Fun` below, which does bind its name first — that is
                // what makes `let rec go i = … go …` work, and a value binding
                // has nothing to gain from being self-recursive under eager
                // evaluation anyway.
                //
                // At the top level this changes nothing: `bind_defn` hands back
                // the id `declare_toplevel` already reserved, so both orders
                // resolve to the same binding and mutual recursion still works.
                let rexpr = self.resolve_expr(expr);
                self.toplevel = top;
                let rpat = self.resolve_pat(pat);
                self.toplevel = false;
                if top && self.binds_duplicate(pat) {
                    return hir::Bind::Error;
                }
                hir::Bind::Pat(rpat, rexpr)
            }
            ast::Bind::Fun(name, params, ret, body) => {
                self.toplevel = top;
                let id = self.bind_defn(*name.value());
                self.toplevel = false;
                let name_node = self.node(id, name.span);
                let mark = self.mark();
                let rparams = params.iter().map(|p| self.resolve_pat(p)).collect_vec();
                // Resolved after the parameters, so a type variable a parameter
                // introduced is the same one here: in
                // `fun id (x : a) : a = x` both `a`s are one variable.
                let rret = ret.as_ref().map(|t| {
                    let was = std::mem::replace(&mut self.open_tyvars, true);
                    let r = self.resolve_ty(t);
                    self.open_tyvars = was;
                    r
                });
                let rbody = self.resolve_expr(body);
                self.reset(mark);
                if top && self.duplicates.contains(&name.span) {
                    // Reported already, and it shares the first definition's
                    // id: inferring both would report the clash again as a
                    // type error. Its body was still resolved, for its own.
                    return hir::Bind::Error;
                }
                hir::Bind::Fun(name_node, rparams, rret, rbody)
            }
        }
    }

    // --- expressions -----------------------------------------------------------

    /// Warn if `q` is not an active module qualifier here (a `mod` child or a
    /// `use`d module). Constructor resolution itself is by bare name (constructor
    /// names are unique across a program), so this is only a scoping check.
    /// Does `q.Ctor` name a constructor of the *type* `q`?
    ///
    /// `Expr.Int` and `Mod.Int` look identical, so both readings are tried.
    /// The type reading is tried first: a type owns its constructors, whereas
    /// a module merely happens to contain them, and the qualified form exists
    /// precisely so a constructor can be named by its type.
    fn qualified_type_ctor(
        &mut self,
        q: &ast::Ident,
        name: InternedString,
    ) -> Option<InternedString> {
        if let Some(Named::Ambiguous(cs)) = self.tycons.get(&*q.value()).cloned() {
            let known = cs
                .iter()
                .any(|c| self.ctors.contains_key(&canonical_ctor(*c, name)));
            if known {
                self.ambiguous_type(*q.value(), &cs, q.span);
            }
        }
        let canonical = self
            .resolve_qualified_ctor(*q.value(), name)
            .or_else(|| self.module_type_ctor(*q.value(), name))?;
        // The qualifier *is* the type, written out — so it is a reference to
        // it, and a rename of the type has to rewrite it.
        self.note_ref(q.span, NameRef::Type(owner_of(canonical)));
        Some(canonical)
    }

    /// What [`Resolver::qualified_type_ctor`] would answer, without saying
    /// anything or noting a reference.
    fn qualified_type_ctor_known(
        &self,
        q: &ast::Ident,
        name: InternedString,
    ) -> Option<InternedString> {
        self.resolve_qualified_ctor(*q.value(), name)
            .or_else(|| self.module_type_ctor(*q.value(), name))
    }

    fn check_qualifier(&mut self, q: &ast::Ident) {
        if !self.qualifiers.contains_key(&*q.value()) {
            self.error(
                // A bare `use` imports names unqualified; a qualifier comes only
                // from `as`, so that is what to suggest.
                format!(
                    "module `{}` is not in scope here (add `use <path> as {}`)",
                    q.value(),
                    q.value()
                ),
                "unknown module".to_string(),
                q.span,
            );
        }
    }

    /// Resolve a constructor application `Name arg…` (bare or the tail of a
    /// qualified `Mod.Name arg…`), including the `Name { field = e, … }` form.
    fn resolve_ctor_app(
        &mut self,
        span: Span,
        name: &ast::Ident,
        args: &[ast::LExpr],
    ) -> hir::LExpr {
        // Bare here, canonical from here on: a constructor's identity downstream
        // is `Type.Ctor`, and the bare spelling is only how this module is
        // allowed to write it.
        let bare = *name.value();
        if !self.is_known_ctor(bare)
            && let Some(built) = self.synonym_expr(span, name, args)
        {
            return built;
        }
        let cands = self.ctor_candidates(bare);
        let cname = cands.first().copied().unwrap_or(bare);
        if let [only] = args {
            if let ast::Expr::Record(fields, base) = only.value() {
                if base.is_none() {
                    let labels: Vec<InternedString> =
                        fields.iter().map(|(l, _)| *l.value()).collect();
                    if let Some((c, order)) = self.named_ctor(&cands, &labels, name) {
                        return self.resolve_named_ctor(span, c, name.span, fields, &order);
                    }
                }
            }
        }
        if !self.is_known_ctor(bare) {
            self.unknown_ctor(bare, name.span);
        }
        let label = self.node(cname, name.span);
        if cands.len() > 1 {
            self.overload(label.id, cands.into_iter().map(hir::Alt::Ctor).collect());
        }
        let ra = args.iter().map(|a| self.resolve_expr(a)).collect_vec();
        self.node(hir::Expr::Cons(label, ra), span)
    }

    fn resolve_expr(&mut self, expr: &ast::LExpr) -> hir::LExpr {
        self.toplevel = false;
        match expr.value() {
            ast::Expr::Lit(lit) => {
                let l = self.resolve_lit(lit);
                self.node(hir::Expr::Lit(l), expr.span)
            }
            ast::Expr::Unit => self.node(hir::Expr::Unit, expr.span),
            // `"a ${x} b ${y:?}"` is `concatStrings #["a ", display x, " b ",
            // debug y]`: `Display`'s method and `Debug`'s, as the dependencies
            // bring them in -- not whatever a program calls `display` itself --
            // or, with no `Std`, the primitives that render anything.
            ast::Expr::Interp(texts, holes) => {
                let display = self.base_lookup("display").or_else(|| self.prim("display"));
                let debug = self.base_lookup("debug").or_else(|| self.prim("show"));
                let (Some(concat), Some(display), Some(debug)) =
                    (self.prim("concatStrings"), display, debug)
                else {
                    self.error(
                        "string interpolation needs the primitives".to_string(),
                        "no prelude here".to_string(),
                        expr.span,
                    );
                    return self.node(hir::Expr::Error, expr.span);
                };
                let mut parts = Vec::with_capacity(texts.len() + holes.len());
                for (i, text) in texts.iter().enumerate() {
                    if !text.is_empty() {
                        parts.push(self.node(hir::Expr::Lit(hir::Lit::String(*text)), expr.span));
                    }
                    if let Some((hole, fmt)) = holes.get(i) {
                        let value = self.resolve_expr(hole);
                        let render = match fmt {
                            ast::Fmt::Display => display,
                            ast::Fmt::Debug => debug,
                        };
                        let f = self.node(render, hole.span);
                        let callee = self.node(hir::Expr::Var(f), hole.span);
                        parts.push(self.node(hir::Expr::App(callee, vec![value]), hole.span));
                    }
                }
                let f = self.node(concat, expr.span);
                let callee = self.node(hir::Expr::Var(f), expr.span);
                let array = self.node(hir::Expr::Array(parts), expr.span);
                self.node(hir::Expr::App(callee, vec![array]), expr.span)
            }
            ast::Expr::MacCall(m) => {
                self.unexpanded(m, expr.span);
                self.node(hir::Expr::Error, expr.span)
            }
            ast::Expr::Hole => {
                self.error(
                    "`_` can only appear inside an operator section, e.g. `(_ + 1)`".to_string(),
                    "stray hole".to_string(),
                    expr.span,
                );
                self.node(hir::Expr::Error, expr.span)
            }
            // `$m?A`, the matcher of the pattern synonym `A`, however many
            // parameters it has: what `pattern A | B` names.
            ast::Expr::Var(name) if name.value().starts_with("$m?") => {
                let bare = InternedString::from(&name.value()[3..]);
                let found = self.synonym_arity(bare).and_then(|n| {
                    self.value_use(
                        InternedString::from(format!("$m{n}{bare}").as_str()),
                        name.span,
                    )
                });
                found.unwrap_or_else(|| {
                    self.error(
                        format!("`{bare}` is not a pattern synonym"),
                        "`pattern A | B` names pattern synonyms".to_string(),
                        name.span,
                    );
                    self.node(hir::Expr::Error, expr.span)
                })
            }
            ast::Expr::Var(name) => {
                let ids = self.lookup_all(*name.value());
                if let Some(&first) = ids.first() {
                    let v = self.node(first, name.span);
                    if ids.len() > 1 {
                        self.overload(v.id, ids.into_iter().map(hir::Alt::Value).collect());
                    }
                    self.node(hir::Expr::Var(v), expr.span)
                } else if let Some(prim) = self.operator_prim(*name.value()) {
                    // `(+)` where no `+` is in scope: the primitive.
                    let v = self.node(prim, name.span);
                    self.node(hir::Expr::Var(v), expr.span)
                } else {
                    // Without its hygiene mark: a name a macro wrote is
                    // `tmp#3` inside the compiler, and `tmp` is what was
                    // written and what the reader can look for.
                    self.error(
                        format!("undefined variable: {}", ast::hygiene::strip(*name.value())),
                        "not found in this scope".to_string(),
                        name.span,
                    );
                    self.node(hir::Expr::Error, expr.span)
                }
            }
            ast::Expr::Lam(params, body) => {
                let mark = self.mark();
                let rparams = params.iter().map(|p| self.resolve_pat(p)).collect_vec();
                let rbody = self.resolve_expr(body);
                self.reset(mark);
                self.node(hir::Expr::Lam(rparams, rbody), expr.span)
            }
            // `Mod.Ctor a b` — a qualified constructor application.
            ast::Expr::App(func, args) if matches!(func.value(), ast::Expr::Qual(_, n) if is_ctor_name(n.value())) =>
            {
                let ast::Expr::Qual(q, name) = func.value() else {
                    unreachable!()
                };
                if let Some(canonical) = self.qualified_type_ctor(q, *name.value()) {
                    // `Shape.Rect { w = 2, h = 3 }`: the braces are the
                    // constructor's named fields, exactly as they are unqualified.
                    if let [only] = args.as_slice()
                        && let ast::Expr::Record(fields, None) = only.value()
                        && let Some(order) = self.ctor_field_order(canonical)
                    {
                        return self
                            .resolve_named_ctor(expr.span, canonical, name.span, fields, &order);
                    }
                    let label = self.node(canonical, name.span);
                    let ra = args.iter().map(|a| self.resolve_expr(a)).collect_vec();
                    return self.node(hir::Expr::Cons(label, ra), expr.span);
                }
                // `Core.Lam p b`: a pattern synonym of the module `Core`,
                // building.
                let builder = InternedString::from(format!("$b{}", name.value()).as_str());
                if let Some(f) = self.qualified_value(*q.value(), builder, name.span) {
                    let ra = args.iter().map(|a| self.resolve_expr(a)).collect_vec();
                    return self.node(hir::Expr::App(f, ra), expr.span);
                }
                self.check_qualifier(q);
                self.resolve_ctor_app(expr.span, name, args)
            }
            // `includeStr "path"`, which is read at compile time. Only when
            // nothing of that name is in scope: a binding of one's own wins,
            // as it does over anything else.
            ast::Expr::App(func, args)
                if matches!(func.value(), ast::Expr::Var(n)
                    if &**n.value() == "includeStr" && self.lookup_all(*n.value()).is_empty()) =>
            {
                self.include_str(args, expr.span)
            }
            ast::Expr::App(func, args) => {
                let rf = self.resolve_expr(func);
                let ra = args.iter().map(|a| self.resolve_expr(a)).collect_vec();
                self.node(hir::Expr::App(rf, ra), expr.span)
            }
            // `Mod.name` — a value from a `use`d / `mod` child module.
            ast::Expr::Qual(q, name) => {
                let qn = *q.value();
                let nn = *name.value();
                if is_ctor_name(&nn) {
                    if let Some(canonical) = self.qualified_type_ctor(q, nn) {
                        let label = self.node(canonical, name.span);
                        return self.node(hir::Expr::Cons(label, vec![]), expr.span);
                    }
                    // `Core.Lam`, the builder of a pattern synonym of the
                    // module `Core`; applied, it is applied as any function.
                    let builder = InternedString::from(format!("$b{nn}").as_str());
                    if let Some(f) = self.qualified_value(qn, builder, name.span) {
                        return f;
                    }
                    self.check_qualifier(q);
                    return self.resolve_ctor_app(expr.span, name, &[]);
                }
                match self.qualifiers.get(&qn).and_then(|m| m.get(&nn).copied()) {
                    Some(id) => {
                        let v = self.node(id, name.span);
                        self.node(hir::Expr::Var(v), expr.span)
                    }
                    None => {
                        let msg = if self.qualifiers.contains_key(&qn) {
                            format!("`{nn}` is not exported by module `{qn}`")
                        } else {
                            format!("module `{qn}` is not in scope here (add `use <path> as {qn}`)")
                        };
                        self.error(msg, "unresolved".to_string(), expr.span);
                        self.node(hir::Expr::Error, expr.span)
                    }
                }
            }
            ast::Expr::Let(binds, body) => {
                let mark = self.mark();
                let rbinds = binds.iter().map(|b| self.resolve_bind(b)).collect_vec();
                let rbody = self.resolve_expr(body);
                self.reset(mark);
                self.node(hir::Expr::Let(rbinds, rbody), expr.span)
            }
            ast::Expr::If(cond, then_branch, else_branch) => {
                let rc = self.resolve_expr(cond);
                let rt = self.resolve_expr(then_branch);
                let re = self.resolve_expr(else_branch);
                self.node(hir::Expr::If(rc, rt, re), expr.span)
            }
            ast::Expr::Match(scrut, arms) => {
                let rs = self.resolve_expr(scrut);
                let rarms = arms
                    .iter()
                    .map(|(pat, guard, arm)| {
                        let mark = self.mark();
                        let rp = self.resolve_pat(pat);
                        // The guard sees what the pattern binds, as the body does.
                        let rg = guard.as_ref().map(|g| self.resolve_expr(g));
                        let ra = self.resolve_expr(arm);
                        self.reset(mark);
                        (rp, rg, ra)
                    })
                    .collect_vec();
                self.node(hir::Expr::Match(rs, rarms), expr.span)
            }
            ast::Expr::UnOp(op, operand) => {
                let sym = InternedString::from(op.value().to_string());
                let f = self.lookup(sym).expect("prelude never truncated");
                let ro = self.resolve_expr(operand);
                let fv = self.node(f, op.span);
                let callee = self.node(hir::Expr::Var(fv), op.span);
                self.node(hir::Expr::App(callee, vec![ro]), expr.span)
            }
            // `and` / `or` short-circuit: desugar to `if` rather than a prim call.
            ast::Expr::BinOp(op, lhs, rhs)
                if matches!(op.value(), ast::BinOp::And | ast::BinOp::Or) =>
            {
                let rl = self.resolve_expr(lhs);
                let rr = self.resolve_expr(rhs);
                let ctor = |this: &mut Self, name: &str| {
                    let label = this.node(InternedString::from(name), op.span);
                    this.node(hir::Expr::Cons(label, vec![]), op.span)
                };
                let node = match op.value() {
                    ast::BinOp::And => {
                        let f = ctor(self, "Bool.False");
                        hir::Expr::If(rl, rr, f)
                    }
                    _ => {
                        let t = ctor(self, "Bool.True");
                        hir::Expr::If(rl, t, rr)
                    }
                };
                self.node(node, expr.span)
            }
            ast::Expr::BinOp(..) => unreachable!("`and` and `or` are the only BinOps"),
            ast::Expr::Infix(first, rest) => {
                let grouped = self.reassociate(first, rest);
                self.resolve_expr(&grouped)
            }
            ast::Expr::Tuple(exprs) => {
                let res = exprs.iter().map(|e| self.resolve_expr(e)).collect_vec();
                self.node(hir::Expr::Tuple(res), expr.span)
            }
            ast::Expr::Array(exprs) => {
                let res = exprs.iter().map(|e| self.resolve_expr(e)).collect_vec();
                self.node(hir::Expr::Array(res), expr.span)
            }
            ast::Expr::List(exprs) => {
                let res = exprs.iter().map(|e| self.resolve_expr(e)).collect_vec();
                self.node(hir::Expr::List(res), expr.span)
            }
            ast::Expr::Cons(name, args) => self.resolve_ctor_app(expr.span, name, args),
            ast::Expr::Record(fields, base) => {
                let rfields = fields
                    .iter()
                    .map(|(label, val)| {
                        let l = self.node(*label.value(), label.span);
                        let v = self.resolve_expr(val);
                        (l, v)
                    })
                    .collect_vec();
                let rbase = base.as_ref().map(|b| self.resolve_expr(b));
                self.node(hir::Expr::Record(rfields, rbase), expr.span)
            }
            ast::Expr::Update(base, fields) => {
                let rbase = self.resolve_expr(base);
                let mut seen = Vec::new();
                let rfields = fields
                    .iter()
                    .map(|(label, val)| {
                        if seen.contains(label.value()) {
                            self.error(
                                format!("field `{}` is updated twice", label.value()),
                                "already updated".to_string(),
                                label.span,
                            );
                        }
                        seen.push(*label.value());
                        let l = self.node(*label.value(), label.span);
                        (l, self.resolve_expr(val))
                    })
                    .collect_vec();
                self.node(hir::Expr::Update(rbase, rfields), expr.span)
            }
            ast::Expr::Field(obj, label) => {
                let o = self.resolve_expr(obj);
                let l = self.node(*label.value(), label.span);
                self.node(hir::Expr::Field(o, l), expr.span)
            }
            ast::Expr::Handle(scrut, arms, ret) => {
                let rs = self.resolve_expr(scrut);
                let rarms = arms
                    .iter()
                    .map(|arm| {
                        let opname = *arm.op.value();
                        let effect = self.effect_ops.get(&opname).copied().unwrap_or_else(|| {
                            self.error(
                                format!("unknown operation `{opname}`"),
                                "not an effect operation".to_string(),
                                arm.op.span,
                            );
                            InternedString::default()
                        });
                        let mark = self.mark();
                        let rparam = self.resolve_pat(&arm.param);
                        let rid = self.bind(*arm.resume.value());
                        let rresume = self.node(rid, arm.resume.span);
                        let rbody = self.resolve_expr(&arm.body);
                        self.reset(mark);
                        hir::HandlerArm {
                            effect,
                            op: opname,
                            param: rparam,
                            resume: rresume,
                            body: rbody,
                        }
                    })
                    .collect_vec();
                let rret = ret.as_ref().map(|(pat, body)| {
                    let mark = self.mark();
                    let rp = self.resolve_pat(pat);
                    let rb = self.resolve_expr(body);
                    self.reset(mark);
                    (rp, rb)
                });
                self.node(hir::Expr::Handle(rs, rarms, rret), expr.span)
            }
        }
    }

    // --- patterns ------------------------------------------------------------

    /// Resolve a constructor pattern `Name p…` (bare or the tail of `Mod.Name p…`).
    fn resolve_ctor_pat(&mut self, span: Span, name: &ast::Ident, args: &[ast::LPat]) -> hir::LPat {
        // Bare here, canonical from here on: a constructor's identity downstream
        // is `Type.Ctor`, and the bare spelling is only how this module is
        // allowed to write it.
        let bare = *name.value();
        if !self.is_known_ctor(bare)
            && let Some(matched) = self.synonym_pat(span, name, args)
        {
            return matched;
        }
        let cands = self.ctor_candidates(bare);
        let cname = cands.first().copied().unwrap_or(bare);
        if let [only] = args {
            if let ast::Pat::Record(fields, _) = only.value() {
                let labels: Vec<InternedString> = fields.iter().map(|(l, _)| *l.value()).collect();
                if let Some((c, order)) = self.named_ctor(&cands, &labels, name) {
                    return self.resolve_named_ctor_pat(span, c, name.span, fields, &order);
                }
            }
        }
        if !self.is_known_ctor(bare) {
            self.unknown_ctor(bare, name.span);
        }
        let label = self.node(cname, name.span);
        if cands.len() > 1 {
            self.overload(label.id, cands.into_iter().map(hir::Alt::Ctor).collect());
        }
        let ra = args.iter().map(|p| self.resolve_pat(p)).collect_vec();
        self.node(hir::Pat::Cons(label, ra), span)
    }

    /// The functions in scope named `name`, as a use of it: the first, and an
    /// overload of them all when there are more.
    fn value_use(&mut self, name: InternedString, span: Span) -> Option<hir::LExpr> {
        let ids = self.lookup_all(name);
        let &first = ids.first()?;
        let v = self.node(first, span);
        if ids.len() > 1 {
            self.overload(v.id, ids.into_iter().map(hir::Alt::Value).collect());
        }
        Some(self.node(hir::Expr::Var(v), span))
    }

    /// `P p1 p2`, where `P` is a pattern synonym: the view of its matcher,
    /// `($m2P -> [(p1, p2);])` -- see the parser's `synonym`. `None` when no
    /// synonym `P` is in scope.
    fn synonym_pat(
        &mut self,
        span: Span,
        name: &ast::Ident,
        args: &[ast::LPat],
    ) -> Option<hir::LPat> {
        self.synonym_pat_in(None, span, name, args)
    }

    /// [`Resolver::synonym_pat`], for `Q.P p1 p2` when `q` is given: the
    /// synonym `P` of the module `Q` qualifies.
    fn synonym_pat_in(
        &mut self,
        q: Option<InternedString>,
        span: Span,
        name: &ast::Ident,
        args: &[ast::LPat],
    ) -> Option<hir::LPat> {
        let bare = *name.value();
        let matcher = InternedString::from(format!("$m{}{bare}", args.len()).as_str());
        let found = match q {
            Some(q) => self.qualified_value(q, matcher, name.span),
            None => self.value_use(matcher, name.span),
        };
        let Some(f) = found else {
            let arity = match q {
                Some(q) => (0..=MAX_SYNONYM_ARITY).find(|n| {
                    let m = InternedString::from(format!("$m{n}{bare}").as_str());
                    self.qualifiers
                        .get(&q)
                        .is_some_and(|vs| vs.contains_key(&m))
                }),
                None => self.synonym_arity(bare),
            }?;
            self.error(
                format!(
                    "the pattern `{bare}` takes {arity} argument{}, not {}",
                    plural(arity),
                    args.len()
                ),
                format!("`{bare}` is a pattern synonym of {arity}"),
                span,
            );
            args.iter().for_each(|p| {
                self.resolve_pat(p);
            });
            return Some(self.node(hir::Pat::Error, span));
        };
        self.extra_refs.push(RefSite {
            span: name.span,
            what: NameRef::Value(match f.value() {
                hir::Expr::Var(v) => *v.value(),
                _ => unreachable!("a use of a value"),
            }),
        });
        let mut ps = args.iter().map(|p| self.resolve_pat(p)).collect_vec();
        let answer = match ps.len() {
            0 => {
                let yes = self.node(InternedString::from("Bool.True"), name.span);
                hir::Pat::Cons(yes, Vec::new())
            }
            1 => hir::Pat::List(ps),
            _ => {
                let tuple = self.node(hir::Pat::Tuple(std::mem::take(&mut ps)), span);
                hir::Pat::List(vec![tuple])
            }
        };
        let answer = self.node(answer, span);
        Some(self.node(hir::Pat::View(f, answer), span))
    }

    /// `P e1 e2` in an expression, where `P` is a pattern synonym: its
    /// builder, `$bP e1 e2`. `None` when no synonym `P` is in scope; an error
    /// when one is, and only matches.
    fn synonym_expr(
        &mut self,
        span: Span,
        name: &ast::Ident,
        args: &[ast::LExpr],
    ) -> Option<hir::LExpr> {
        let bare = *name.value();
        let builder = InternedString::from(format!("$b{bare}").as_str());
        let Some(f) = self.value_use(builder, name.span) else {
            self.synonym_arity(bare)?;
            self.error(
                format!("`{bare}` is a pattern synonym that only matches"),
                format!(
                    "`pattern {bare} … = …` builds as it matches; `pattern {bare} … <- … where {bare} … = …` says how"
                ),
                name.span,
            );
            args.iter().for_each(|a| {
                self.resolve_expr(a);
            });
            return Some(self.node(hir::Expr::Error, span));
        };
        if let hir::Expr::Var(v) = f.value() {
            self.extra_refs.push(RefSite {
                span: name.span,
                what: NameRef::Value(*v.value()),
            });
        }
        if args.is_empty() {
            return Some(f);
        }
        let ra = args.iter().map(|a| self.resolve_expr(a)).collect_vec();
        Some(self.node(hir::Expr::App(f, ra), span))
    }

    /// How many arguments the pattern synonym `name` in scope takes, if there
    /// is one.
    fn synonym_arity(&self, name: InternedString) -> Option<usize> {
        (0..=MAX_SYNONYM_ARITY).find(|n| {
            !self
                .lookup_all(InternedString::from(format!("$m{n}{name}").as_str()))
                .is_empty()
        })
    }

    /// Which candidate a named-field constructor `C { a = .., b = .. }` is, and
    /// its field order.
    ///
    /// Named fields are reordered here, by the resolver, into the declared
    /// positions -- so unlike a positional constructor this cannot wait for
    /// inference to pick. The field names usually settle it; when they do not,
    /// the constructor has to be written `Type.C`.
    fn named_ctor(
        &mut self,
        cands: &[InternedString],
        labels: &[InternedString],
        name: &ast::Ident,
    ) -> Option<(InternedString, Vec<InternedString>)> {
        let named: Vec<(InternedString, Vec<InternedString>)> = cands
            .iter()
            .filter_map(|c| self.ctor_field_order(*c).map(|o| (*c, o)))
            .collect();
        if named.len() <= 1 {
            return named.into_iter().next();
        }
        let fits: Vec<&(InternedString, Vec<InternedString>)> = named
            .iter()
            .filter(|(_, order)| labels.iter().all(|l| order.contains(l)))
            .collect();
        if let [one] = fits.as_slice() {
            return Some((*one).clone());
        }
        let options = named
            .iter()
            .map(|(c, _)| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", ");
        self.error(
            format!(
                "ambiguous constructor `{}`: could be {options}",
                name.value()
            ),
            "write it qualified, as `Type.Ctor`".to_string(),
            name.span,
        );
        named.into_iter().next()
    }

    fn resolve_pat(&mut self, pat: &ast::LPat) -> hir::LPat {
        match pat.value() {
            ast::Pat::MacCall(m) => {
                self.unexpanded(m, pat.span);
                self.node(hir::Pat::Error, pat.span)
            }
            ast::Pat::Wildcard => self.node(hir::Pat::Wildcard, pat.span),
            ast::Pat::Unit => self.node(hir::Pat::Unit, pat.span),
            ast::Pat::Var(name) => {
                let id = self.bind_defn(*name.value());
                let v = self.node(id, name.span);
                self.node(hir::Pat::Var(v), pat.span)
            }
            ast::Pat::Lit(lit) => {
                let l = self.resolve_lit(lit);
                self.node(hir::Pat::Lit(l), pat.span)
            }
            ast::Pat::Ann(inner, t) => {
                // The annotation resolves in the enclosing declaration's scope,
                // so a type variable in it is the same one a sibling annotation
                // means -- and may introduce one that nothing declared.
                let rp = self.resolve_pat(inner);
                // `: R ! e` is a function's result and what its body performs;
                // a value has no body that runs when it is used.
                if let ast::TypeExpr::Fun(ps, _, Some(_)) = t.value()
                    && ps.is_empty()
                {
                    self.error(
                        "only a function's result can say what its body performs".to_string(),
                        "a value performs nothing when it is used: drop the `! …`".to_string(),
                        t.span,
                    );
                }
                let was = std::mem::replace(&mut self.open_tyvars, true);
                let rt = self.resolve_ty(t);
                self.open_tyvars = was;
                self.node(hir::Pat::Ann(Box::new(rp), rt), pat.span)
            }
            ast::Pat::As(name, sub) => {
                let id = self.bind_defn(*name.value());
                let v = self.node(id, name.span);
                let rsub = self.resolve_pat(sub);
                self.node(hir::Pat::As(v, rsub), pat.span)
            }
            ast::Pat::Cons(name, args) => self.resolve_ctor_pat(pat.span, name, args),
            ast::Pat::QualCons(q, name, args) => {
                // `Core.Lam p b`, a pattern synonym of the module `Core`.
                if self.qualified_type_ctor_known(q, *name.value()).is_none()
                    && let Some(matched) =
                        self.synonym_pat_in(Some(*q.value()), pat.span, name, args)
                {
                    return matched;
                }
                if let Some(canonical) = self.qualified_type_ctor(q, *name.value()) {
                    // `Shape.Rect { w, h }` -- named fields, as they are unqualified.
                    if let [only] = args.as_slice()
                        && let ast::Pat::Record(fields, _) = only.value()
                        && let Some(order) = self.ctor_field_order(canonical)
                    {
                        return self.resolve_named_ctor_pat(
                            pat.span, canonical, name.span, fields, &order,
                        );
                    }
                    let label = self.node(canonical, name.span);
                    let ra = args.iter().map(|p| self.resolve_pat(p)).collect_vec();
                    return self.node(hir::Pat::Cons(label, ra), pat.span);
                }
                self.check_qualifier(q);
                self.resolve_ctor_pat(pat.span, name, args)
            }
            ast::Pat::Tuple(pats) => {
                let rp = pats.iter().map(|p| self.resolve_pat(p)).collect_vec();
                self.node(hir::Pat::Tuple(rp), pat.span)
            }
            ast::Pat::Array(pats) => {
                let rp = pats.iter().map(|p| self.resolve_pat(p)).collect_vec();
                self.node(hir::Pat::Array(rp), pat.span)
            }
            ast::Pat::List(pats) => {
                let rp = pats.iter().map(|p| self.resolve_pat(p)).collect_vec();
                self.node(hir::Pat::List(rp), pat.span)
            }
            // `[]` is the empty `Vector`, which is `Vector.Empty` — the library keeps
            // that the only representation of an empty vector (`vNormalize` and
            // `fromArray` both collapse to it). A non-empty vector has no
            // structural form, so say so rather than guessing.
            ast::Pat::Vector(pats) => {
                if pats.is_empty() {
                    let label = self.node(InternedString::from("Vector.Empty"), pat.span);
                    self.node(hir::Pat::Cons(label, vec![]), pat.span)
                } else {
                    self.error(
                        "a `Vector` pattern can only be the empty `[]`".to_string(),
                        format!(
                            "write `[{}]` to match a list, or match on `len` instead",
                            vec!["_"; pats.len()].join("; ")
                        ),
                        pat.span,
                    );
                    pats.iter().for_each(|p| {
                        self.resolve_pat(p);
                    });
                    self.node(hir::Pat::Error, pat.span)
                }
            }
            ast::Pat::Record(fields, open) => {
                let rfields = fields
                    .iter()
                    .map(|(label, p)| {
                        let l = self.node(*label.value(), label.span);
                        let rp = self.resolve_pat(p);
                        (l, rp)
                    })
                    .collect_vec();
                self.node(hir::Pat::Record(rfields, *open), pat.span)
            }
            // The view is an expression in the scope the pattern is in, with
            // what the pattern has bound to its left.
            ast::Pat::View(f, p) => {
                let rf = self.resolve_expr(f);
                let rp = self.resolve_pat(p);
                self.node(hir::Pat::View(rf, rp), pat.span)
            }
        }
    }

    /// `Ctor { f2 = e2, f1 = e1 }` -> positional `Ctor e1 e2` in declared order.
    fn resolve_named_ctor(
        &mut self,
        span: Span,
        name: InternedString,
        name_span: Span,
        fields: &[(ast::Ident, ast::LExpr)],
        order: &[InternedString],
    ) -> hir::LExpr {
        let mut provided: HashMap<InternedString, &ast::LExpr> = HashMap::new();
        for (label, val) in fields {
            if provided.insert(*label.value(), val).is_some() {
                self.error(
                    format!("field `{}` given twice", label.value()),
                    "duplicate field".to_string(),
                    label.span,
                );
            }
        }
        let mut exprs = Vec::with_capacity(order.len());
        for fname in order {
            match provided.remove(fname) {
                Some(e) => exprs.push(self.resolve_expr(e)),
                None => {
                    self.error(
                        // The bare spelling: a record's constructor is
                        // canonically `Person.Person`, which would read as a
                        // stutter in a message about `Person`.
                        format!("missing field `{fname}` for `{}`", bare_ctor(name)),
                        "required here".to_string(),
                        span,
                    );
                    exprs.push(self.node(hir::Expr::Error, span));
                }
            }
        }
        for extra in provided.keys() {
            self.error(
                format!("`{}` has no field `{extra}`", bare_ctor(name)),
                "unknown field".to_string(),
                name_span,
            );
        }
        let label = self.node(name, name_span);
        self.node(hir::Expr::Cons(label, exprs), span)
    }

    fn resolve_named_ctor_pat(
        &mut self,
        span: Span,
        name: InternedString,
        name_span: Span,
        fields: &[(ast::Ident, ast::LPat)],
        order: &[InternedString],
    ) -> hir::LPat {
        let mut provided: HashMap<InternedString, &ast::LPat> = HashMap::new();
        for (label, p) in fields {
            if provided.insert(*label.value(), p).is_some() {
                self.error(
                    format!("field `{}` bound twice", label.value()),
                    "duplicate field".to_string(),
                    label.span,
                );
            }
        }
        let mut pats = Vec::with_capacity(order.len());
        for fname in order {
            match provided.remove(fname) {
                Some(p) => pats.push(self.resolve_pat(p)),
                None => pats.push(self.node(hir::Pat::Wildcard, span)),
            }
        }
        for extra in provided.keys() {
            self.error(
                format!("`{}` has no field `{extra}`", bare_ctor(name)),
                "unknown field".to_string(),
                name_span,
            );
        }
        let label = self.node(name, name_span);
        self.node(hir::Pat::Cons(label, pats), span)
    }

    fn resolve_lit(&self, lit: &ast::Lit) -> hir::Lit {
        match lit {
            ast::Lit::Int(i) => hir::Lit::Int(*i),
            ast::Lit::Float(b) => hir::Lit::Float(*b),
            ast::Lit::String(s) => hir::Lit::String(*s),
            ast::Lit::Char(c) => hir::Lit::Char(*c),
        }
    }

    fn error(&mut self, msg: String, label: String, span: Span) {
        self.errors.push(Diagnostic {
            msg,
            filename: self.filename.clone(),
            label: (label, span),
            extra_labels: vec![],
        });
    }

    /// A macro call that expansion left behind.
    ///
    /// Expansion runs before this and either replaces a call or reports why it
    /// could not, so reaching here is the compiler's own fault rather than the
    /// program's. Said as a diagnostic and not a panic: an editor asks for a
    /// tree on every keystroke, and one bad module must not take the server
    /// down with it.
    fn unexpanded(&mut self, call: &ast::MacCall, span: Span) {
        self.error(
            format!("the macro `{}!` was never expanded", call.name()),
            "this call reached name resolution".to_string(),
            span,
        );
    }
}

/// Every `VarId` an already-resolved (irrefutable) pattern binds.
/// Are two types written the same way, wherever they were written?
// --- grouping operators --------------------------------------------------------

/// Group the chain `first`, then each operator and operand of `rest`, by the
/// fixities `fixity` says, into what `apply` makes of an operator and its two
/// sides: `a + b * c == d` as `(a + (b * c)) == d`.
///
/// The usual operator-precedence parse. An operator on the stack is applied
/// before the next one is pushed when it binds tighter, or as tightly and both
/// group left; two of one level that do not group the same way -- or that
/// group neither way, as `a == b == c` -- are an error, as they are in
/// Haskell, since no reading of them is the obvious one, and read as if the
/// first went first. What the resolver groups with, and what MeadowBoot's
/// fixity pass is checked against.
pub fn reassociate<T>(
    first: T,
    rest: impl IntoIterator<Item = (ast::Ident, T)>,
    fixity: impl Fn(InternedString) -> hir::Fixity,
    mut apply: impl FnMut(ast::Ident, T, T) -> T,
    mut error: impl FnMut(String, Span),
) -> T {
    fn reduce<T>(out: &mut Vec<T>, op: ast::Ident, apply: &mut impl FnMut(ast::Ident, T, T) -> T) {
        let r = out.pop().expect("an operand for each operator");
        let l = out.pop().expect("an operand for each operator");
        out.push(apply(op, l, r));
    }

    let mut out = vec![first];
    let mut stack: Vec<(ast::Ident, hir::Fixity)> = Vec::new();
    for (op, operand) in rest {
        let here = fixity(*op.value());
        while let Some((top, there)) = stack.last() {
            let first_goes = match there.level.cmp(&here.level) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => match (there.assoc, here.assoc) {
                    (hir::Assoc::Left, hir::Assoc::Left) => true,
                    (hir::Assoc::Right, hir::Assoc::Right) => false,
                    _ => {
                        let (a, b) = (
                            ast::hygiene::strip(*top.value()),
                            ast::hygiene::strip(*op.value()),
                        );
                        let (fa, fb) = (*there, here);
                        let msg = if a == b && fa.assoc == hir::Assoc::None {
                            format!(
                                "`{a}` is `{} {}`, which does not group: `x {a} y {a} z` means nothing",
                                fa.keyword(),
                                fa.level
                            )
                        } else {
                            format!(
                                "`{a}` (`{} {}`) and `{b}` (`{} {}`) cannot be mixed without parentheses",
                                fa.keyword(),
                                fa.level,
                                fb.keyword(),
                                fb.level
                            )
                        };
                        error(msg, op.span);
                        true
                    }
                },
            };
            if !first_goes {
                break;
            }
            let (top, _) = stack.pop().expect("just looked");
            reduce(&mut out, top, &mut apply);
        }
        stack.push((op, here));
        out.push(operand);
    }
    while let Some((op, _)) = stack.pop() {
        reduce(&mut out, op, &mut apply);
    }
    out.pop().expect("one expression left")
}

fn same_type(a: &ast::LType, b: &ast::LType) -> bool {
    use ast::TypeExpr::*;
    let all = |xs: &[ast::LType], ys: &[ast::LType]| {
        xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| same_type(x, y))
    };
    match (a.value(), b.value()) {
        (Var(x), Var(y)) => x.value() == y.value(),
        (Con(x, xs), Con(y, ys)) => x.value() == y.value() && all(xs, ys),
        (Tuple(xs), Tuple(ys)) => all(xs, ys),
        (Vector(x), Vector(y)) | (List(x), List(y)) => same_type(x, y),
        // Nothing an `impl` can be of.
        _ => false,
    }
}

/// The name a method written in a `trait` or an `impl` defines: a `fun`'s, or a
/// parameterless one's, which the parser makes a pattern binding of one name.
fn method_name(bind: &ast::Bind) -> Option<&ast::Ident> {
    match bind {
        ast::Bind::Fun(name, ..) => Some(name),
        ast::Bind::Pat(pat, _) => match pat.value() {
            ast::Pat::Var(name) => Some(name),
            ast::Pat::Ann(inner, _) => match inner.value() {
                ast::Pat::Var(name) => Some(name),
                _ => None,
            },
            _ => None,
        },
    }
}

/// The most parameters a pattern synonym is looked for with, when one is
/// used with a number it does not take.
const MAX_SYNONYM_ARITY: usize = 16;

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Whether `value`, a module's function, is part of the pattern synonym
/// `name` -- its matcher `$m<arity><name>` or its builder `$b<name>` -- which
/// naming `name` in a `use` brings along.
pub fn part_of_synonym(value: &str, name: &str) -> bool {
    if let Some(rest) = value.strip_prefix("$b") {
        return rest == name;
    }
    value
        .strip_prefix("$m")
        .map(|rest| rest.trim_start_matches(|c: char| c.is_ascii_digit()))
        .is_some_and(|rest| rest == name && value.len() > 2 + name.len())
}

fn collect_hir_pat_vars(pat: &hir::LPat, out: &mut Vec<VarId>) {
    match pat.value() {
        hir::Pat::Var(id) => out.push(*id.value()),
        hir::Pat::As(id, sub) => {
            out.push(*id.value());
            collect_hir_pat_vars(sub, out);
        }
        // `@pub def x : Int = 1` is a binding of `(x : Int)`: the name is under
        // the annotation, and has to be exported all the same.
        hir::Pat::Ann(inner, _) => collect_hir_pat_vars(inner, out),
        hir::Pat::View(_, inner) => collect_hir_pat_vars(inner, out),
        hir::Pat::Tuple(ps) | hir::Pat::List(ps) | hir::Pat::Cons(_, ps) | hir::Pat::Array(ps) => {
            ps.iter().for_each(|p| collect_hir_pat_vars(p, out))
        }
        hir::Pat::Record(fs, _) => fs.iter().for_each(|(_, p)| collect_hir_pat_vars(p, out)),
        _ => {}
    }
}
